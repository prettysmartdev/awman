//! The catalog contract: refuse unknown catalogs before asking the SDK to run
//! migrations, and upgrade an allow-listed older catalog in place.
//!
//! The marker file, its lock, the backup/restore copies and the "unversioned
//! catalog" rule live in Layer 0 (`BuiltinState`); the engine supplies the
//! protocol it speaks, decides when an upgrade may run and drives the SDK.
//!
//! # In-place upgrade
//!
//! [`BuiltinPaths::prepare_catalog`] reports [`CatalogState::NeedsUpgrade`]
//! when the marker names a protocol in `UPGRADABLE_FROM`. The driver then
//! calls [`BuiltinPaths::upgrade_catalog`], which holds the catalog lock for
//! the whole sequence:
//!
//! 1. Re-read the marker under the lock. A concurrent awman may have upgraded
//!    already (`Matches`: nothing to do) or the marker may have changed.
//! 2. Liveness gate and fence: refuse with `BuiltinCatalogUpgradeBlocked`
//!    when any SDK lifecycle lock under `run/locks/`, any image lease or the
//!    image-import lock is held, and keep the leases and the import lock
//!    until the upgrade ends (see `fence_older_awman`).
//! 3. Put back sandbox files an interrupted attempt retired, then back up
//!    `db/` to `db.pre-<found>/` with a manifest, or, when that backup
//!    already exists, recover from the interrupted attempt that left it (see
//!    `recover_interrupted_upgrade`).
//! 4. Migrate: open the catalog through the SDK, which applies its forward
//!    migrations without booting or touching any VM.
//! 5. Retire every awman sandbox whose protocol label is not `PROTOCOL`:
//!    after step 2 they all belong to dead sessions. Its per-sandbox
//!    directory is first moved to `.sandboxes.retired/`, so it can be put
//!    back. Images and the OCI cache are kept.
//! 6. Close every SDK database handle, probe the lifecycle locks again, then
//!    commit by replacing the marker, and delete the retired directories.
//!
//! A failure in steps 4–6 puts the retired directories back, restores `db/`
//! from the backup (a journalled restore) and removes the backup, so the
//! marker, `db/` and `sandboxes/` are as they were before the attempt. A
//! failure after the new marker became visible is not rolled back. A crash
//! leaves the old marker in place, so the next run tries again and takes the
//! recovery path in step 3.
use std::collections::BTreeMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::naming;
use super::paths::{state_error, BuiltinPaths};
use crate::data::builtin_state::{applied_migrations, RuntimeMarker, IMPORT_LOCK};
use crate::data::error::DataError;
use crate::engine::error::EngineError;

/// Where the SDK keeps each sandbox's lifecycle lock, relative to the state
/// directory (`microsandbox_runtime::ipc::lifecycle_lock_path`:
/// `<run>/locks/<hash>.lock`, the same in SDK 0.7.2 and 0.7.8). The worker
/// running a VM holds it for the VM's whole lifetime.
pub const LIFECYCLE_LOCK_DIR: &str = "run/locks";
/// The SDK's catalog database inside `db/`.
pub const CATALOG_DATABASE: &str = "msb.db";
/// The SDK migrator's ledger of applied migrations in [`CATALOG_DATABASE`].
pub const MIGRATION_LEDGER: &str = "seaql_migrations";

/// An awman sandbox id with its labels, as the upgrade discovers it.
pub type LabelledSandbox = (String, BTreeMap<String, String>);

/// What [`BuiltinPaths::prepare_catalog`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatalogState {
    /// The catalog is fresh or already uses this protocol.
    Ready,
    /// The catalog uses the older protocol `found`, which this build may
    /// upgrade in place with [`BuiltinPaths::upgrade_catalog`].
    NeedsUpgrade { found: String },
}

/// The SDK side of an in-place upgrade. `MsbDriver` implements it with the
/// pinned SDK; hermetic tests use the in-memory `FakeDriver`.
///
/// None of these may boot, stop or otherwise touch a VM.
pub trait CatalogUpgrade {
    /// Open the catalog, which applies the SDK's forward migrations to `db/`.
    fn migrate(&self) -> Result<(), EngineError>;
    /// Every awman sandbox in the migrated catalog, with its labels.
    fn sandbox_labels(&self) -> Result<Vec<LabelledSandbox>, EngineError>;
    /// Remove a stale sandbox's record and per-sandbox directories. Unlike
    /// every other removal path this skips `naming::check_protocol`: it is
    /// called only from the upgrade, only for sandboxes of the old protocol.
    fn retire_stale_sandbox(&self, id: &str) -> Result<(), EngineError>;
    /// Close every database handle the calls above opened. Always called
    /// before `db/` is committed or restored, whether or not they succeeded.
    fn close(&self) -> Result<(), EngineError>;
    /// Test seam, called after `close` and immediately before the marker is
    /// committed. An error here is an ordinary failure (rolled back); a panic
    /// models a crash before commit, because no rollback runs while
    /// unwinding.
    fn before_commit(&self) -> Result<(), EngineError> {
        Ok(())
    }
}

/// A completed in-place upgrade.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogUpgraded {
    pub state_dir: PathBuf,
    pub found: String,
    pub expected: String,
    pub backup: PathBuf,
    /// Stale sandboxes whose records were removed.
    pub retired: usize,
}

impl CatalogUpgraded {
    /// The one-line notice every frontend shows after an upgrade.
    pub fn notice(&self) -> String {
        format!(
            "upgraded builtin state {} from {} to {}; previous catalog saved in {}",
            self.state_dir.display(),
            self.found,
            self.expected,
            self.backup.display()
        )
    }
}

/// Whether a sandbox's labels name a protocol other than this build's.
fn is_stale(labels: &BTreeMap<String, String>) -> bool {
    labels.get(naming::LABEL_PROTOCOL).map(String::as_str) != Some(naming::PROTOCOL)
}

impl BuiltinPaths {
    /// Check the state directory's catalog against this worker protocol,
    /// writing the marker for a fresh one. An unknown protocol, a newer one
    /// (a downgraded awman) and an unversioned existing catalog are refused.
    pub fn prepare_catalog(&self) -> Result<CatalogState, EngineError> {
        match self.check_runtime_marker(naming::PROTOCOL)? {
            RuntimeMarker::Created | RuntimeMarker::Matches => {
                // Sandbox files retired by an upgrade that committed but
                // stopped before deleting them.
                let _ = self.state().discard_retired_sandbox_files();
                Ok(CatalogState::Ready)
            }
            RuntimeMarker::Upgradable { found } => Ok(CatalogState::NeedsUpgrade { found }),
            RuntimeMarker::Mismatch { found } => Err(self.catalog_mismatch(found)),
        }
    }

    fn catalog_mismatch(&self, found: String) -> EngineError {
        EngineError::BuiltinCatalogMismatch {
            expected: naming::PROTOCOL.into(),
            found,
            state_dir: self.home.clone(),
        }
    }

    /// Upgrade an older catalog in place (see the module docs). Returns
    /// `None` when, once the lock was taken, there was nothing to upgrade.
    pub fn upgrade_catalog(
        &self,
        upgrade: &dyn CatalogUpgrade,
    ) -> Result<Option<CatalogUpgraded>, EngineError> {
        let _catalog = self.lock()?;
        let state = self.state();
        let found = match state
            .check_runtime_marker_locked(naming::PROTOCOL)
            .map_err(state_error)?
        {
            RuntimeMarker::Created | RuntimeMarker::Matches => return Ok(None),
            RuntimeMarker::Mismatch { found } => return Err(self.catalog_mismatch(found)),
            RuntimeMarker::Upgradable { found } => found,
        };
        // Held until this function returns, whether it commits or rolls back.
        let _fence = self.fence_older_awman(&found)?;

        // An earlier attempt may have stopped after retiring sandboxes.
        state.restore_retired_sandbox_files().map_err(state_error)?;
        let backup = state.catalog_backup_path(&found);
        if backup.symlink_metadata().is_ok()
            || state
                .pending_catalog_restore()
                .map_err(state_error)?
                .is_some()
        {
            self.recover_interrupted_upgrade(&found, &backup)?;
        } else {
            state
                .backup_catalog(&found, CATALOG_DATABASE, MIGRATION_LEDGER)
                .map_err(state_error)?;
        }

        // From here on `db/` and `sandboxes/` may change: any failure before
        // the marker is replaced puts them back.
        let retired = match self.migrate_and_retire(upgrade).and_then(|retired| {
            upgrade.before_commit()?;
            // A VM the older awman started after the gate (for an image
            // whose lease did not exist yet) must not see its catalog
            // committed under it.
            self.check_no_live_sandboxes(&found)?;
            Ok(retired)
        }) {
            Ok(retired) => retired,
            Err(error) => {
                self.roll_back_upgrade(&backup);
                return Err(error);
            }
        };
        if let Err(error) = state.replace_runtime_marker(naming::PROTOCOL) {
            // The error may come after the rename (the directory sync): the
            // new marker is then visible next to the migrated catalog, which
            // is consistent, so it must not be rolled back under it. The
            // backup and the retired files are kept.
            if !self.marker_committed() {
                self.roll_back_upgrade(&backup);
            }
            return Err(state_error(error));
        }
        // Committed. A leftover is removed by the next `prepare_catalog`.
        let _ = state.discard_retired_sandbox_files();
        Ok(Some(CatalogUpgraded {
            state_dir: self.home.clone(),
            found,
            expected: naming::PROTOCOL.into(),
            backup,
            retired,
        }))
    }

    /// Refuse while any session of the older awman may still be alive, and
    /// keep it from starting new work until the returned locks are dropped.
    ///
    /// The check needs no knowledge of the old schema, and relies only on
    /// locks the older awman already takes:
    /// - Every running worker holds its sandbox's SDK lifecycle lock for the
    ///   VM's whole lifetime (verified for SDK 0.7.2: `machine_cmd` adopts
    ///   the inherited `--lifecycle-lock-fd` into
    ///   `runner::vm::Config::lifecycle_guard`, which lives until the process
    ///   exits), so a lock that cannot be taken means a live VM. These are
    ///   only probed; `check_no_live_sandboxes` probes them again before the
    ///   commit.
    /// - A launch holds its image's lease (shared) from planning until the
    ///   session ends, and an import or image removal holds it exclusively.
    ///   Every existing lease is taken exclusively and kept, so a launch or
    ///   image change that starts now fails at once instead of writing to
    ///   the catalog being migrated.
    /// - An import holds the import lock while it writes the catalog. It is
    ///   taken without waiting and kept, so an import that starts now waits
    ///   for the upgrade instead.
    ///
    /// Locks left by crashed processes are free, so they count as dead.
    fn fence_older_awman(&self, found: &str) -> Result<Vec<File>, EngineError> {
        let busy = |error: DataError| match error {
            DataError::BuiltinStateBusy { .. } => self.upgrade_blocked(found),
            error => state_error(error),
        };
        self.check_no_live_sandboxes(found)?;
        let state = self.state();
        let mut held = vec![state
            .lock_controlled(IMPORT_LOCK, Duration::ZERO, false, || Ok(()))
            .map_err(busy)?];
        held.extend(state.lock_image_leases().map_err(busy)?);
        Ok(held)
    }

    fn upgrade_blocked(&self, found: &str) -> EngineError {
        EngineError::BuiltinCatalogUpgradeBlocked {
            state_dir: self.home.clone(),
            found: found.into(),
        }
    }

    /// Refuse when any SDK lifecycle lock is held: some VM is alive.
    fn check_no_live_sandboxes(&self, found: &str) -> Result<(), EngineError> {
        if self
            .state()
            .held_locks(LIFECYCLE_LOCK_DIR)
            .map_err(state_error)?
            .is_empty()
        {
            Ok(())
        } else {
            Err(self.upgrade_blocked(found))
        }
    }

    /// Whether the marker now names this build's protocol.
    fn marker_committed(&self) -> bool {
        matches!(
            self.state().check_runtime_marker_locked(naming::PROTOCOL),
            Ok(RuntimeMarker::Matches)
        )
    }

    /// The backup exists (or a restore journal does) although the marker
    /// still names `found`: an earlier attempt stopped before committing (a
    /// crash, or a rollback that could not finish).
    ///
    /// Nothing is restored unless the backup's manifest proves it is a
    /// complete awman backup of the `found` catalog. Then:
    /// - A restore from it was interrupted (its journal is present): repeat
    ///   the restore, whatever state `db/` was left in.
    /// - `db/` records every migration the backup does and more: the SDK had
    ///   started migrating it, so the schema is newer than the marker says.
    ///   Restore `db/` from the backup.
    ///
    /// Either way the caller then migrates again from the restored copy.
    /// Anything else — an empty, partial or foreign directory, `db/` never
    /// migrated, or a database that cannot be read — is refused with
    /// `BuiltinCatalogBackupExists`. The backup may be the only good copy,
    /// so it is never overwritten or removed here.
    fn recover_interrupted_upgrade(&self, found: &str, backup: &Path) -> Result<(), EngineError> {
        let state = self.state();
        let refuse = || EngineError::BuiltinCatalogBackupExists {
            state_dir: self.home.clone(),
            found: found.into(),
            backup: backup.to_path_buf(),
        };
        let saved = match state.verified_catalog_backup(backup) {
            Ok(saved) if saved.from == found => saved,
            _ => return Err(refuse()),
        };
        let interrupted_restore = match state.pending_catalog_restore() {
            Ok(Some(journal)) if journal == backup => true,
            Ok(None) => false,
            _ => return Err(refuse()),
        };
        let partially_migrated = || match applied_migrations(
            &self.home.join("db").join(CATALOG_DATABASE),
            MIGRATION_LEDGER,
        ) {
            Ok(Some(current)) => {
                let saved = saved.migrations.clone().unwrap_or_default();
                current.len() > saved.len() && current.is_superset(&saved)
            }
            _ => false,
        };
        if !interrupted_restore && !partially_migrated() {
            return Err(refuse());
        }
        state
            .restore_catalog(backup, CATALOG_DATABASE)
            .map_err(state_error)
    }

    /// Steps 4–5 and closing the SDK. Returns how many stale sandboxes were
    /// retired.
    fn migrate_and_retire(&self, upgrade: &dyn CatalogUpgrade) -> Result<usize, EngineError> {
        let state = self.state();
        let steps = (|| -> Result<usize, EngineError> {
            upgrade.migrate()?;
            let mut retired = 0;
            for (id, labels) in upgrade.sandbox_labels()? {
                if is_stale(&labels) {
                    // The SDK deletes the per-sandbox directory before the
                    // row; moving it aside first keeps it for a rollback.
                    state.retire_sandbox_files(&id).map_err(state_error)?;
                    upgrade.retire_stale_sandbox(&id)?;
                    retired += 1;
                }
            }
            Ok(retired)
        })();
        // No SDK connection may stay open while `db/` is committed or
        // swapped back: SQLite removes its side files by path on close.
        let closed = upgrade.close();
        let retired = steps?;
        closed?;
        Ok(retired)
    }

    /// Put `sandboxes/` and `db/` back as they were before this attempt and
    /// drop the backup, so the next run starts clean. If either cannot be
    /// put back the backup is kept: the next run finds it (or the restore
    /// journal) and takes the recovery path.
    fn roll_back_upgrade(&self, backup: &Path) {
        let state = self.state();
        let sandboxes = state.restore_retired_sandbox_files();
        let catalog = state.restore_catalog(backup, CATALOG_DATABASE);
        if sandboxes.is_ok() && catalog.is_ok() {
            let _ = state.discard_catalog_backup(backup);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::OpenOptions;
    #[test]
    fn incompatible_catalog_is_preserved_without_migration() {
        let temp = tempfile::tempdir_in("/tmp").unwrap();
        let paths =
            BuiltinPaths::resolve(&temp.path().canonicalize().unwrap().join("state")).unwrap();
        let marker = paths.home.join("awman-runtime-version");
        std::fs::write(&marker, b"future-protocol").unwrap();
        assert!(matches!(
            paths.prepare_catalog(),
            Err(EngineError::BuiltinCatalogMismatch { .. })
        ));
        assert_eq!(std::fs::read(&marker).unwrap(), b"future-protocol");
        assert_eq!(std::fs::read_dir(paths.home.join("db")).unwrap().count(), 0);
    }
    #[test]
    fn matching_catalog_reopens_and_released_locks_are_reusable() {
        let temp = tempfile::tempdir_in("/tmp").unwrap();
        let paths =
            BuiltinPaths::resolve(&temp.path().canonicalize().unwrap().join("state")).unwrap();
        paths.prepare_catalog().unwrap();
        paths.prepare_catalog().unwrap();
        let guard = paths.lock().unwrap();
        let other = OpenOptions::new()
            .read(true)
            .write(true)
            .open(paths.home.join("awman.lock"))
            .unwrap();
        assert!(other.try_lock().is_err());
        drop(guard);
        assert!(other.try_lock().is_ok());
    }
    #[test]
    fn unversioned_catalog_is_not_adopted() {
        let temp = tempfile::tempdir_in("/tmp").unwrap();
        let paths =
            BuiltinPaths::resolve(&temp.path().canonicalize().unwrap().join("state")).unwrap();
        std::fs::write(paths.home.join("db/catalog.sqlite"), b"existing data").unwrap();
        assert!(paths.prepare_catalog().is_err());
        assert!(!paths.home.join("awman-runtime-version").exists());
    }
}

#[cfg(test)]
mod hardening_tests;
