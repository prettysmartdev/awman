//! The on-disk side of an in-place catalog upgrade — Layer 0.
//!
//! Backups of `db/`, restoring `db/` from one, the retired-sandbox quarantine
//! and the lock probes the engine's liveness gate uses. The engine decides
//! when each runs; everything here only moves, copies and checks files.
//!
//! Every tree walk holds a descriptor for each directory it visits and opens
//! children relative to it (`openat`/`mkdirat` with `O_NOFOLLOW`), so a
//! directory swapped for a symlink mid-walk is refused rather than followed.
//! Directory fsync failures are reported, except where the filesystem cannot
//! sync a directory at all.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use nix::errno::Errno;
use nix::fcntl::{open, openat, renameat, AtFlags, OFlag};
use nix::sys::stat::{fstatat, mkdirat, Mode, SFlag};
use nix::unistd::{unlinkat, UnlinkatFlags};
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};

use super::{io_err, BuiltinState, RUNTIME_VERSION_FILE};
use crate::data::error::DataError;

/// The manifest inside every backup made by [`BuiltinState::backup_catalog`].
/// A directory without a matching one is not treated as a backup.
pub const CATALOG_BACKUP_MANIFEST: &str = "awman-catalog-backup.json";
/// Present while `db/` is being restored from a backup; names that backup.
pub const CATALOG_RESTORE_JOURNAL: &str = "awman-catalog-restore";
/// Per-sandbox directories of retired sandboxes, kept until the upgrade
/// commits so a rollback can put them back.
pub const RETIRED_SANDBOXES_DIR: &str = ".sandboxes.retired";

/// What a backup's manifest records.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogBackup {
    /// The protocol whose catalog was backed up.
    pub from: String,
    /// The migrations applied to the backed-up database; `None` when it had
    /// no database or no migration ledger.
    pub migrations: Option<BTreeSet<String>>,
    /// Every other file in the backup, by relative path, with its size.
    files: BTreeMap<String, u64>,
}

fn errno(path: &Path, error: Errno) -> DataError {
    io_err(path, error.into())
}

fn dir_flags() -> OFlag {
    OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC | OFlag::O_RDONLY
}

fn private_dir() -> Mode {
    Mode::from_bits_truncate(0o700)
}

/// Open the directory `name` inside `parent` without following a symlink.
fn open_dir_at(parent: &File, name: &OsStr, path: &Path, what: &str) -> Result<File, DataError> {
    openat(parent, name, dir_flags(), Mode::empty())
        .map(File::from)
        .map_err(|error| match error {
            Errno::ELOOP | Errno::ENOTDIR => DataError::BuiltinState(format!(
                "{what} {} is not a real directory (symlinks are refused)",
                path.display()
            )),
            error => errno(path, error),
        })
}

/// Test seam: what an injected fault does when its point is reached.
#[cfg(test)]
pub(crate) enum Fault {
    /// Fail with EIO, as a failing disk would.
    Fail,
    /// Panic, which models a crash: no cleanup runs while unwinding.
    Crash,
    /// Run a closure (to change the filesystem at that moment), then go on.
    Run(Box<dyn FnMut()>),
}

#[cfg(test)]
thread_local! {
    static FAULTS: std::cell::RefCell<Vec<(String, Fault)>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Test seam: fire `fault` the next time `point` is reached on this thread.
#[cfg(test)]
pub(crate) fn inject_fault(point: &str, fault: Fault) {
    FAULTS.with(|faults| faults.borrow_mut().push((point.into(), fault)));
}

/// A named point where tests inject faults; a no-op otherwise.
fn fault_point(point: &str, path: &Path) -> Result<(), DataError> {
    #[cfg(test)]
    {
        let fault = FAULTS.with(|faults| {
            let mut faults = faults.borrow_mut();
            let index = faults.iter().position(|(name, _)| name == point)?;
            Some(faults.remove(index).1)
        });
        match fault {
            Some(Fault::Fail) => {
                return Err(io_err(path, std::io::Error::from_raw_os_error(libc::EIO)))
            }
            Some(Fault::Crash) => panic!("simulated crash at {point}"),
            Some(Fault::Run(mut run)) => run(),
            None => {}
        }
    }
    let _ = (point, path);
    Ok(())
}

/// Sync a directory so the entries just created or renamed in it survive a
/// power loss. Only a filesystem that cannot sync directories is excused.
fn sync_dir(directory: &File, path: &Path) -> Result<(), DataError> {
    match directory.sync_all() {
        Ok(()) => Ok(()),
        Err(error)
            if error.raw_os_error().is_some_and(|code| {
                code == libc::EINVAL || code == libc::ENOTSUP || code == libc::EOPNOTSUPP
            }) =>
        {
            Ok(())
        }
        Err(error) => Err(io_err(path, error)),
    }
}

/// The names in an open directory, sorted, without `.` and `..`.
fn entries(directory: &File, path: &Path) -> Result<Vec<OsString>, DataError> {
    let listing = directory.try_clone().map_err(|e| io_err(path, e))?;
    let mut listing = nix::dir::Dir::from_fd(OwnedFd::from(listing)).map_err(|e| errno(path, e))?;
    let mut names = Vec::new();
    for entry in listing.iter() {
        let entry = entry.map_err(|e| errno(path, e))?;
        let name = entry.file_name().to_bytes();
        if name != b"." && name != b".." {
            names.push(OsStr::from_bytes(name).to_os_string());
        }
    }
    names.sort();
    Ok(names)
}

enum Kind {
    Directory,
    File,
    Symlink,
    Other,
}

fn kind_at(parent: &File, name: &OsStr, path: &Path) -> Result<Kind, DataError> {
    let stat = fstatat(parent, name, AtFlags::AT_SYMLINK_NOFOLLOW).map_err(|e| errno(path, e))?;
    let format = SFlag::from_bits_truncate(stat.st_mode as _) & SFlag::S_IFMT;
    Ok(if format == SFlag::S_IFDIR {
        Kind::Directory
    } else if format == SFlag::S_IFREG {
        Kind::File
    } else if format == SFlag::S_IFLNK {
        Kind::Symlink
    } else {
        Kind::Other
    })
}

/// Open the regular file `name` in `parent` for reading, never following a
/// symlink or blocking on a FIFO swapped in after it was checked.
fn open_file_at(parent: &File, name: &OsStr, path: &Path) -> Result<File, DataError> {
    let flags = OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC;
    let file = openat(parent, name, flags, Mode::empty())
        .map(File::from)
        .map_err(|error| match error {
            Errno::ELOOP => DataError::BuiltinState(format!(
                "builtin catalog contains a symlink: {}",
                path.display()
            )),
            error => errno(path, error),
        })?;
    if !file.metadata().map_err(|e| io_err(path, e))?.is_file() {
        return Err(DataError::BuiltinState(format!(
            "builtin catalog entry is not a regular file: {}",
            path.display()
        )));
    }
    Ok(file)
}

/// Create the private file `name` in `directory` (never replacing anything),
/// write `bytes` and sync it.
fn write_new_file_at(
    directory: &File,
    name: &str,
    path: &Path,
    bytes: &[u8],
) -> Result<(), DataError> {
    let flags =
        OFlag::O_WRONLY | OFlag::O_CREAT | OFlag::O_EXCL | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC;
    let mut file = openat(directory, name, flags, Mode::from_bits_truncate(0o600))
        .map(File::from)
        .map_err(|e| errno(path, e))?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|e| io_err(path, e))
}

/// Copy the tree under the open directory `source` into the new, empty
/// directory `destination` with private permissions. Symlinks and anything
/// that is neither a file nor a directory are refused. `skip` names one
/// top-level entry to leave out.
fn copy_tree(
    source: &File,
    source_path: &Path,
    destination: &File,
    destination_path: &Path,
    skip: Option<&str>,
) -> Result<(), DataError> {
    for name in entries(source, source_path)? {
        if skip.is_some_and(|skip| name == skip) {
            continue;
        }
        let from = source_path.join(&name);
        let to = destination_path.join(&name);
        match kind_at(source, &name, &from)? {
            Kind::Directory => {
                fault_point("copy:open-dir", &from)?;
                let child = open_dir_at(source, &name, &from, "builtin catalog directory")?;
                mkdirat(destination, name.as_os_str(), private_dir()).map_err(|e| errno(&to, e))?;
                let copy = open_dir_at(destination, &name, &to, "builtin catalog copy")?;
                copy_tree(&child, &from, &copy, &to, None)?;
                sync_dir(&copy, &to)?;
            }
            Kind::File => {
                fault_point("copy:open-file", &from)?;
                let mut input = open_file_at(source, &name, &from)?;
                let flags = OFlag::O_WRONLY
                    | OFlag::O_CREAT
                    | OFlag::O_EXCL
                    | OFlag::O_NOFOLLOW
                    | OFlag::O_CLOEXEC;
                let mut output = openat(
                    destination,
                    name.as_os_str(),
                    flags,
                    Mode::from_bits_truncate(0o600),
                )
                .map(File::from)
                .map_err(|e| errno(&to, e))?;
                std::io::copy(&mut input, &mut output).map_err(|e| io_err(&to, e))?;
                output.sync_all().map_err(|e| io_err(&to, e))?;
            }
            Kind::Symlink => {
                return Err(DataError::BuiltinState(format!(
                    "builtin catalog contains a symlink: {}",
                    from.display()
                )))
            }
            Kind::Other => {
                return Err(DataError::BuiltinState(format!(
                    "builtin catalog contains an unsupported filesystem entry: {}",
                    from.display()
                )))
            }
        }
    }
    Ok(())
}

/// Every file under the open directory `directory` with its size, keyed by
/// `/`-separated relative path. `skip` names one top-level entry to leave out.
fn tree_sizes(
    directory: &File,
    path: &Path,
    prefix: &str,
    skip: Option<&str>,
    sizes: &mut BTreeMap<String, u64>,
) -> Result<(), DataError> {
    for name in entries(directory, path)? {
        if skip.is_some_and(|skip| name == skip) {
            continue;
        }
        let child = path.join(&name);
        let Some(text) = name.to_str() else {
            return Err(DataError::BuiltinState(format!(
                "builtin catalog entry name is not UTF-8: {}",
                child.display()
            )));
        };
        let relative = format!("{prefix}{text}");
        match kind_at(directory, &name, &child)? {
            Kind::Directory => {
                let nested = open_dir_at(directory, &name, &child, "builtin catalog directory")?;
                tree_sizes(&nested, &child, &format!("{relative}/"), None, sizes)?;
            }
            Kind::File => {
                let size = open_file_at(directory, &name, &child)?
                    .metadata()
                    .map_err(|e| io_err(&child, e))?
                    .len();
                sizes.insert(relative, size);
            }
            Kind::Symlink | Kind::Other => {
                return Err(DataError::BuiltinState(format!(
                    "builtin catalog contains an unsupported filesystem entry: {}",
                    child.display()
                )))
            }
        }
    }
    Ok(())
}

impl BuiltinState {
    fn open_home(&self) -> Result<File, DataError> {
        open(&self.home, dir_flags(), Mode::empty())
            .map(File::from)
            .map_err(|e| errno(&self.home, e))
    }

    /// Replace (or create) the file `name` directly in the state directory
    /// with `bytes`: a private temporary file is written and synced, renamed
    /// over `name`, and the directory synced. An error before the rename
    /// leaves the old file untouched; an error after it means the new file
    /// is visible but may not be durable.
    fn publish_file(&self, home: &File, name: &str, bytes: &[u8]) -> Result<(), DataError> {
        let path = self.home.join(name);
        let temporary = format!(".{name}.{}.tmp", uuid::Uuid::new_v4().simple());
        let temporary_path = self.home.join(&temporary);
        let staged = write_new_file_at(home, &temporary, &temporary_path, bytes)
            .and_then(|()| fault_point(&format!("publish:{name}:rename"), &self.home))
            .and_then(|()| {
                renameat(home, temporary.as_str(), home, name).map_err(|e| errno(&path, e))
            });
        if let Err(error) = staged {
            let _ = unlinkat(home, temporary.as_str(), UnlinkatFlags::NoRemoveDir);
            return Err(error);
        }
        fault_point(&format!("publish:{name}:sync"), &self.home)?;
        sync_dir(home, &self.home)
    }

    /// Copy the catalog to a new private `db.pre-<sanitised from>/` backup.
    ///
    /// The caller must hold the catalog lock and call this before the SDK
    /// opens the database. This method never overwrites an existing backup.
    ///
    /// The copy is staged in a hidden directory. There its SQLite database
    /// `database` has its WAL folded into the main file, and the migrations
    /// in its ledger table `ledger` are recorded with every file's size in
    /// a manifest ([`CATALOG_BACKUP_MANIFEST`]). The staged copy is then
    /// published under the backup name, which is claimed with an exclusive
    /// `mkdir` first. So a directory at the returned path with a matching
    /// manifest is always a complete copy. If anything fails (for example a
    /// full disk, or a failed fsync) the staged or published copy is removed
    /// and `db/` is left untouched; it is only read.
    pub fn backup_catalog(
        &self,
        from: &str,
        database: &str,
        ledger: &str,
    ) -> Result<PathBuf, DataError> {
        let home = self.open_home()?;
        let source_path = self.home.join("db");
        let source = open_dir_at(&home, OsStr::new("db"), &source_path, "builtin catalog")?;
        let backup = self.catalog_backup_path(from);
        let backup_name = backup.file_name().unwrap_or_default().to_os_string();
        let exists = || {
            DataError::BuiltinState(format!(
                "builtin catalog backup already exists: {}",
                backup.display()
            ))
        };
        match fstatat(&home, backup_name.as_os_str(), AtFlags::AT_SYMLINK_NOFOLLOW) {
            Ok(_) => return Err(exists()),
            Err(Errno::ENOENT) => {}
            Err(error) => return Err(errno(&backup, error)),
        }

        let staging_name = format!(
            ".{}.partial-{}",
            backup_name.to_string_lossy(),
            uuid::Uuid::new_v4().simple()
        );
        let staging_path = self.home.join(&staging_name);
        let mut published = false;
        let result = (|| -> Result<(), DataError> {
            mkdirat(&home, staging_name.as_str(), private_dir())
                .map_err(|e| errno(&staging_path, e))?;
            let staging = open_dir_at(
                &home,
                OsStr::new(&staging_name),
                &staging_path,
                "builtin catalog copy",
            )?;
            copy_tree(&source, &source_path, &staging, &staging_path, None)?;
            let migrations = checkpoint_database(&staging_path.join(database), Some(ledger))?;
            let mut files = BTreeMap::new();
            tree_sizes(&staging, &staging_path, "", None, &mut files)?;
            let manifest = CatalogBackup {
                from: from.into(),
                migrations,
                files,
            };
            let bytes = serde_json::to_vec_pretty(&manifest).map_err(|e| {
                DataError::BuiltinState(format!("cannot record catalog backup: {e}"))
            })?;
            write_new_file_at(
                &staging,
                CATALOG_BACKUP_MANIFEST,
                &staging_path.join(CATALOG_BACKUP_MANIFEST),
                &bytes,
            )?;
            sync_dir(&staging, &staging_path)?;
            fault_point("backup:claim", &backup)?;
            // Claim the name with an exclusive mkdir: a rename alone would
            // silently replace an empty directory created meanwhile.
            match mkdirat(&home, backup_name.as_os_str(), private_dir()) {
                Ok(()) => {}
                Err(Errno::EEXIST) => return Err(exists()),
                Err(error) => return Err(errno(&backup, error)),
            }
            if let Err(error) =
                renameat(&home, staging_name.as_str(), &home, backup_name.as_os_str())
            {
                // Only an empty directory can be removed here: ours.
                let _ = unlinkat(&home, backup_name.as_os_str(), UnlinkatFlags::RemoveDir);
                return Err(match error {
                    Errno::ENOTEMPTY | Errno::EEXIST => exists(),
                    error => errno(&backup, error),
                });
            }
            published = true;
            fault_point("backup:sync", &self.home)?;
            sync_dir(&home, &self.home)
        })();
        if let Err(error) = result {
            // Both are this call's own private copies.
            let _ = std::fs::remove_dir_all(if published { &backup } else { &staging_path });
            return Err(error);
        }
        Ok(backup)
    }

    /// Where [`Self::backup_catalog`] puts the backup taken before upgrading
    /// from protocol `from`.
    pub fn catalog_backup_path(&self, from: &str) -> PathBuf {
        self.home
            .join(format!("db.pre-{}", sanitise_backup_component(from)))
    }

    /// The manifest of the backup at `backup`, after checking that the
    /// backup is a real `db.pre-*` directory in this state directory whose
    /// files are exactly the ones its manifest lists, with the same sizes.
    /// Anything else (no manifest, an empty or truncated directory, an
    /// extra or missing file, a symlink) is an error: such a directory is
    /// never restored.
    pub fn verified_catalog_backup(&self, backup: &Path) -> Result<CatalogBackup, DataError> {
        let invalid = |why: &str| {
            DataError::BuiltinState(format!(
                "{} is not a complete awman catalog backup: {why}",
                backup.display()
            ))
        };
        let name = backup
            .file_name()
            .filter(|name| {
                backup.parent() == Some(self.home.as_path())
                    && name.as_bytes().starts_with(b"db.pre-")
            })
            .ok_or_else(|| invalid("not a db.pre-* directory of this state directory"))?;
        let home = self.open_home()?;
        let directory = open_dir_at(&home, name, backup, "builtin catalog backup")?;
        let manifest_path = backup.join(CATALOG_BACKUP_MANIFEST);
        let mut bytes = Vec::new();
        match openat(
            &directory,
            CATALOG_BACKUP_MANIFEST,
            OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC | OFlag::O_NONBLOCK,
            Mode::empty(),
        ) {
            Ok(file) => File::from(file)
                .take(1 << 20)
                .read_to_end(&mut bytes)
                .map_err(|e| io_err(&manifest_path, e))?,
            Err(Errno::ENOENT) => return Err(invalid("it has no manifest")),
            Err(error) => return Err(errno(&manifest_path, error)),
        };
        let manifest: CatalogBackup =
            serde_json::from_slice(&bytes).map_err(|_| invalid("its manifest cannot be read"))?;
        let mut files = BTreeMap::new();
        tree_sizes(
            &directory,
            backup,
            "",
            Some(CATALOG_BACKUP_MANIFEST),
            &mut files,
        )
        .map_err(|_| invalid("it contains an entry that cannot be checked"))?;
        if files != manifest.files {
            return Err(invalid("its files do not match its manifest"));
        }
        Ok(manifest)
    }

    /// Replace `db/` with a copy of the verified backup at `backup`.
    ///
    /// The caller must hold the catalog lock, and no SDK handle may have the
    /// database open. The backup itself is only read.
    ///
    /// The restore is journalled: [`CATALOG_RESTORE_JOURNAL`] is published
    /// before `db/` is touched and removed only once the restored `db/` is
    /// durable. A copy is staged, its SQLite database `database` has its WAL
    /// folded into the main file (a connection to the replaced catalog that
    /// closes late removes `db/<database>-wal` by path, which then cannot
    /// drop committed transactions), and it is swapped in with two renames.
    /// If the second rename fails the displaced `db/` is put back. If the
    /// process dies in between, `db/` may be missing or empty, but the
    /// journal remains, so [`Self::pending_catalog_restore`] tells the next
    /// upgrade attempt to repeat the restore.
    pub fn restore_catalog(&self, backup: &Path, database: &str) -> Result<(), DataError> {
        self.verified_catalog_backup(backup)?;
        let home = self.open_home()?;
        let backup_name = backup.file_name().unwrap_or_default();
        let source = open_dir_at(&home, backup_name, backup, "builtin catalog backup")?;
        self.publish_file(&home, CATALOG_RESTORE_JOURNAL, backup_name.as_bytes())?;

        let id = uuid::Uuid::new_v4().simple();
        let staging_name = format!(".db.restore-{id}");
        let staging_path = self.home.join(&staging_name);
        let staged = (|| -> Result<(), DataError> {
            mkdirat(&home, staging_name.as_str(), private_dir())
                .map_err(|e| errno(&staging_path, e))?;
            let staging = open_dir_at(
                &home,
                OsStr::new(&staging_name),
                &staging_path,
                "builtin catalog copy",
            )?;
            copy_tree(
                &source,
                backup,
                &staging,
                &staging_path,
                Some(CATALOG_BACKUP_MANIFEST),
            )?;
            checkpoint_database(&staging_path.join(database), None)?;
            sync_dir(&staging, &staging_path)
        })();
        if let Err(error) = staged {
            let _ = std::fs::remove_dir_all(&staging_path);
            return Err(error);
        }
        let db = self.home.join("db");
        let discarded = format!(".db.discard-{id}");
        let displaced = match renameat(&home, "db", &home, discarded.as_str()) {
            Ok(()) => true,
            Err(Errno::ENOENT) => false,
            Err(error) => {
                let _ = std::fs::remove_dir_all(&staging_path);
                return Err(errno(&db, error));
            }
        };
        let swapped = fault_point("restore:swap", &db).and_then(|()| {
            renameat(&home, staging_name.as_str(), &home, "db").map_err(|e| errno(&db, e))
        });
        if let Err(error) = swapped {
            if displaced {
                let _ = renameat(&home, discarded.as_str(), &home, "db");
            }
            let _ = std::fs::remove_dir_all(&staging_path);
            return Err(error);
        }
        fault_point("restore:sync", &self.home)?;
        sync_dir(&home, &self.home)?;
        let journal = self.home.join(CATALOG_RESTORE_JOURNAL);
        unlinkat(&home, CATALOG_RESTORE_JOURNAL, UnlinkatFlags::NoRemoveDir)
            .map_err(|e| errno(&journal, e))?;
        sync_dir(&home, &self.home)?;
        let _ = std::fs::remove_dir_all(self.home.join(discarded));
        Ok(())
    }

    /// The backup an interrupted [`Self::restore_catalog`] was restoring
    /// from, if its journal is still present.
    pub fn pending_catalog_restore(&self) -> Result<Option<PathBuf>, DataError> {
        let journal = self.home.join(CATALOG_RESTORE_JOURNAL);
        let home = self.open_home()?;
        let file = match openat(
            &home,
            CATALOG_RESTORE_JOURNAL,
            OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC | OFlag::O_NONBLOCK,
            Mode::empty(),
        ) {
            Ok(file) => File::from(file),
            Err(Errno::ENOENT) => return Ok(None),
            Err(error) => return Err(errno(&journal, error)),
        };
        let mut name = String::new();
        file.take(256)
            .read_to_string(&mut name)
            .map_err(|e| io_err(&journal, e))?;
        if !name.starts_with("db.pre-") || name.contains('/') || name.contains('\0') {
            return Err(DataError::BuiltinState(format!(
                "builtin catalog restore journal {} does not name a backup",
                journal.display()
            )));
        }
        Ok(Some(self.home.join(name)))
    }

    /// Remove a catalog backup made by [`Self::backup_catalog`] once `db/`
    /// has been restored from it. Only a real `db.pre-*` directory directly
    /// inside this state directory is accepted.
    pub fn discard_catalog_backup(&self, backup: &Path) -> Result<(), DataError> {
        let named_backup = backup.parent() == Some(self.home.as_path())
            && backup
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("db.pre-"));
        let metadata = std::fs::symlink_metadata(backup).map_err(|e| io_err(backup, e))?;
        if !named_backup || !metadata.file_type().is_dir() {
            return Err(DataError::BuiltinState(format!(
                "{} is not a builtin catalog backup",
                backup.display()
            )));
        }
        std::fs::remove_dir_all(backup).map_err(|e| io_err(backup, e))?;
        sync_dir(&self.open_home()?, &self.home)
    }

    /// Move the per-sandbox directory `sandboxes/<name>` into
    /// [`RETIRED_SANDBOXES_DIR`] before the SDK removes the sandbox, so a
    /// rolled-back upgrade can put it back. A missing directory is fine.
    pub fn retire_sandbox_files(&self, name: &str) -> Result<(), DataError> {
        if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\0']) {
            return Err(DataError::BuiltinState(format!(
                "invalid sandbox name {name:?}"
            )));
        }
        let home = self.open_home()?;
        let sandboxes_path = self.home.join("sandboxes");
        let sandboxes = open_dir_at(
            &home,
            OsStr::new("sandboxes"),
            &sandboxes_path,
            "builtin sandboxes directory",
        )?;
        match fstatat(&sandboxes, name, AtFlags::AT_SYMLINK_NOFOLLOW) {
            Ok(_) => {}
            Err(Errno::ENOENT) => return Ok(()),
            Err(error) => return Err(errno(&sandboxes_path.join(name), error)),
        }
        let retired_path = self.home.join(RETIRED_SANDBOXES_DIR);
        match mkdirat(&home, RETIRED_SANDBOXES_DIR, private_dir()) {
            Ok(()) | Err(Errno::EEXIST) => {}
            Err(error) => return Err(errno(&retired_path, error)),
        }
        let retired = open_dir_at(
            &home,
            OsStr::new(RETIRED_SANDBOXES_DIR),
            &retired_path,
            "retired sandboxes directory",
        )?;
        renameat(&sandboxes, name, &retired, name)
            .map_err(|e| errno(&retired_path.join(name), e))?;
        sync_dir(&retired, &retired_path)?;
        sync_dir(&sandboxes, &sandboxes_path)?;
        sync_dir(&home, &self.home)
    }

    /// Put every directory [`Self::retire_sandbox_files`] moved aside back
    /// under `sandboxes/` and remove the quarantine. Nothing to do when it
    /// does not exist.
    pub fn restore_retired_sandbox_files(&self) -> Result<(), DataError> {
        let home = self.open_home()?;
        let retired_path = self.home.join(RETIRED_SANDBOXES_DIR);
        let retired = match openat(&home, RETIRED_SANDBOXES_DIR, dir_flags(), Mode::empty()) {
            Ok(fd) => File::from(fd),
            Err(Errno::ENOENT) => return Ok(()),
            Err(error) => return Err(errno(&retired_path, error)),
        };
        let sandboxes_path = self.home.join("sandboxes");
        let sandboxes = open_dir_at(
            &home,
            OsStr::new("sandboxes"),
            &sandboxes_path,
            "builtin sandboxes directory",
        )?;
        for name in entries(&retired, &retired_path)? {
            renameat(&retired, name.as_os_str(), &sandboxes, name.as_os_str()).map_err(
                |error| match error {
                    Errno::EEXIST | Errno::ENOTEMPTY => DataError::BuiltinState(format!(
                        "cannot put back retired sandbox files {}: {} already exists",
                        retired_path.join(&name).display(),
                        sandboxes_path.join(&name).display()
                    )),
                    error => errno(&sandboxes_path.join(&name), error),
                },
            )?;
        }
        sync_dir(&sandboxes, &sandboxes_path)?;
        unlinkat(&home, RETIRED_SANDBOXES_DIR, UnlinkatFlags::RemoveDir)
            .map_err(|e| errno(&retired_path, e))?;
        sync_dir(&home, &self.home)
    }

    /// Delete the retired sandbox directories once the upgrade committed.
    pub fn discard_retired_sandbox_files(&self) -> Result<(), DataError> {
        let path = self.home.join(RETIRED_SANDBOXES_DIR);
        match std::fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(io_err(&path, error)),
            Ok(metadata) if metadata.file_type().is_dir() => {
                std::fs::remove_dir_all(&path).map_err(|e| io_err(&path, e))
            }
            Ok(_) => Err(DataError::BuiltinState(format!(
                "{} is not a directory",
                path.display()
            ))),
        }
    }

    /// Lock files directly under `<state dir>/<subdir>` that some open file
    /// description currently holds.
    ///
    /// Each regular file is opened read-only without following symlinks and
    /// probed with a non-blocking exclusive `flock`, released again at once.
    /// A lock left behind by a crashed process is free, so it is not
    /// reported. Symlinks and other non-regular entries are skipped: no
    /// runtime creates them there and they cannot be held. A missing
    /// directory has no held locks.
    pub fn held_locks(&self, subdir: &str) -> Result<Vec<PathBuf>, DataError> {
        let directory = self.home.join(subdir);
        let entries = match std::fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(io_err(&directory, error)),
        };
        let mut held = Vec::new();
        for entry in entries {
            let path = entry.map_err(|e| io_err(&directory, e))?.path();
            let metadata = std::fs::symlink_metadata(&path).map_err(|e| io_err(&path, e))?;
            if !metadata.file_type().is_file() {
                continue;
            }
            let mut options = std::fs::OpenOptions::new();
            options.read(true);
            std::os::unix::fs::OpenOptionsExt::custom_flags(
                &mut options,
                libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
            );
            let file = match options.open(&path) {
                Ok(file) => file,
                // Removed between listing and opening: nobody holds it.
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(io_err(&path, error)),
            };
            match file.try_lock() {
                Ok(()) => {}
                Err(std::fs::TryLockError::WouldBlock) => held.push(path),
                Err(std::fs::TryLockError::Error(error)) => return Err(io_err(&path, error)),
            }
        }
        held.sort();
        Ok(held)
    }

    /// Take every existing image lease (`image-*.lock`) exclusively without
    /// waiting, and keep them while the returned files are alive. A launch
    /// holds its image's lease, shared, from planning until it exits, and an
    /// import or image removal holds it exclusively, so while these are held
    /// none of them can start. A lease someone holds is reported as
    /// [`DataError::BuiltinStateBusy`].
    pub fn lock_image_leases(&self) -> Result<Vec<File>, DataError> {
        let mut names = Vec::new();
        for entry in std::fs::read_dir(&self.home).map_err(|e| io_err(&self.home, e))? {
            let entry = entry.map_err(|e| io_err(&self.home, e))?;
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            if name.starts_with("image-")
                && name.ends_with(".lock")
                && entry.file_type().is_ok_and(|kind| kind.is_file())
            {
                names.push(name);
            }
        }
        names.sort();
        names
            .iter()
            .map(|name| self.lock_controlled(name, Duration::ZERO, false, || Ok(())))
            .collect()
    }

    /// Atomically replace the runtime protocol marker with `expected`.
    ///
    /// The temporary file is private, synced before rename, and created in
    /// the marker's directory so the rename stays on the same filesystem. An
    /// error can come after the rename (the directory sync): callers that
    /// must know whether the new marker is visible read it back.
    pub fn replace_runtime_marker(&self, expected: &str) -> Result<(), DataError> {
        let home = self.open_home()?;
        self.publish_file(&home, RUNTIME_VERSION_FILE, expected.as_bytes())
    }
}

fn sanitise_backup_component(from: &str) -> String {
    let sanitised: String = from
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-') {
                character
            } else {
                '_'
            }
        })
        .collect();
    if sanitised.is_empty() {
        "_".into()
    } else {
        sanitised
    }
}

fn check_ledger_name(ledger: &str) -> Result<(), DataError> {
    if ledger.is_empty()
        || !ledger
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return Err(DataError::BuiltinState(format!(
            "invalid migration ledger table name {ledger:?}"
        )));
    }
    Ok(())
}

/// The `version` column of the ledger table `ledger`, or `None` when the
/// database has no such table.
fn read_ledger(
    connection: &rusqlite::Connection,
    ledger: &str,
) -> Result<Option<BTreeSet<String>>, DataError> {
    let exists: Option<i64> = connection
        .query_row(
            "SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = ?1",
            [ledger],
            |row| row.get(0),
        )
        .optional()?;
    if exists.is_none() {
        return Ok(None);
    }
    let mut statement = connection.prepare(&format!("SELECT version FROM \"{ledger}\""))?;
    let versions = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<BTreeSet<_>, _>>()?;
    Ok(Some(versions))
}

/// Whether `database` is a regular file (`false` when it does not exist).
fn database_exists(database: &Path) -> Result<bool, DataError> {
    match std::fs::symlink_metadata(database) {
        Ok(metadata) if metadata.file_type().is_file() => Ok(true),
        Ok(_) => Err(DataError::BuiltinState(format!(
            "builtin catalog database {} is not a regular file",
            database.display()
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(io_err(database, error)),
    }
}

/// The applied-migration ids in the SQLite database `database`: the `version`
/// column of its migration ledger table `ledger` (the engine names the SDK's
/// table). Read through a read-only connection that never migrates or writes
/// rows; SQLite may still create its `-shm`/`-wal` side files next to the
/// database. `None` when the file or the ledger table does not exist.
pub fn applied_migrations(
    database: &Path,
    ledger: &str,
) -> Result<Option<BTreeSet<String>>, DataError> {
    check_ledger_name(ledger)?;
    if !database_exists(database)? {
        return Ok(None);
    }
    let connection = rusqlite::Connection::open_with_flags(
        database,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    read_ledger(&connection, ledger)
}

/// Fold the WAL of the SQLite database at `database`, if the file exists,
/// into its main file and truncate the WAL; closing the connection then
/// removes the side files. With `ledger`, also return the migrations that
/// ledger table records.
fn checkpoint_database(
    database: &Path,
    ledger: Option<&str>,
) -> Result<Option<BTreeSet<String>>, DataError> {
    if let Some(ledger) = ledger {
        check_ledger_name(ledger)?;
    }
    if !database_exists(database)? {
        return Ok(None);
    }
    let connection = rusqlite::Connection::open_with_flags(
        database,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let migrations = match ledger {
        Some(ledger) => read_ledger(&connection, ledger)?,
        None => None,
    };
    connection.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))?;
    connection
        .close()
        .map_err(|(_, error)| DataError::Sqlite(error))?;
    Ok(migrations)
}
