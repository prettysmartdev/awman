//! The builtin runtime's private state directory — Layer 0.
//!
//! Owns the on-disk layout of `<state dir>/` (`run`, `cache`, `logs`,
//! `sandboxes`, `volumes`, `snapshots`, `secrets`, `attach`, `db`), the
//! race-free creation and validation of that directory, the cross-process
//! locks and image leases that live in it, the attach-socket paths and the
//! runtime-version marker.
//!
//! The engine keeps the policy: socket-path budgets (passed in as a `check`
//! hook), the protocol string the marker must hold, how long a caller is
//! willing to wait, and everything SDK-specific. Nothing here starts, stops
//! or talks to a sandbox.

use std::fs::File;
use std::io::{Read, Write};

use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use crate::data::error::DataError;
use crate::data::fs::hash::sha256_hex;
use crate::data::fs::private_files::{lock_private_timeout, LockMode};

/// Subdirectories created (0700) under the state directory.
pub const STATE_SUBDIRS: [&str; 9] = [
    "run",
    "cache",
    "logs",
    "sandboxes",
    "volumes",
    "snapshots",
    "secrets",
    "attach",
    "db",
];

/// The short catalog lock (version marker, image identities).
pub const CATALOG_LOCK: &str = "awman.lock";
/// Serialises image imports without blocking catalog readers.
pub const IMPORT_LOCK: &str = "awman-import.lock";
/// How long the catalog lock is awaited before reporting "busy".
pub const LOCK_WAIT: Duration = Duration::from_secs(30);
/// How long the import lock is awaited; imports can take minutes.
pub const IMPORT_LOCK_WAIT: Duration = Duration::from_secs(30 * 60);
/// File holding the worker protocol the catalog was created under.
pub const RUNTIME_VERSION_FILE: &str = "awman-runtime-version";

/// What [`BuiltinState::check_runtime_marker`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeMarker {
    /// No marker existed and the catalog was empty: the marker was written.
    Created,
    /// The marker holds the expected protocol.
    Matches,
    /// The marker holds an exact protocol listed in `UPGRADABLE_FROM`.
    Upgradable { found: String },
    /// The catalog belongs to another protocol; nothing was changed.
    Mismatch { found: String },
}

/// A resolved, private builtin state directory.
#[derive(Debug, Clone)]
pub struct BuiltinState {
    pub home: PathBuf,
}

fn io_err(path: &Path, source: std::io::Error) -> DataError {
    DataError::Io {
        path: path.to_path_buf(),
        source,
    }
}

impl BuiltinState {
    /// Handle for an already resolved state directory.
    pub fn at(home: impl Into<PathBuf>) -> Self {
        Self { home: home.into() }
    }

    /// Resolve `root` into a private state directory, creating it and its
    /// subdirectories. `check` runs on the root before anything is created
    /// and again on its canonical form (the engine's socket-path budget);
    /// an error from it aborts the resolution.
    pub fn resolve(
        root: &Path,
        mut check: impl FnMut(&Path) -> Result<(), DataError>,
    ) -> Result<Self, DataError> {
        // Resolve symlinks in the parent chain (e.g. macOS `/tmp` ->
        // `/private/tmp`); `secure_create` then validates every canonical
        // component. The state directory itself must not be a symlink.
        let root = &canonical_parent(root)?;
        check(root)?;
        BuiltinState::at(root).secure_create()?;
        let home = root.canonicalize().map_err(|e| io_err(root, e))?;
        check(&home)?;
        for sub in STATE_SUBDIRS {
            BuiltinState::at(home.join(sub)).secure_create()?;
        }
        Ok(Self { home })
    }

    /// Short catalog lock. Never held across VM boot or stop; waits a
    /// bounded time, then reports busy.
    pub fn lock(&self) -> Result<File, DataError> {
        self.lock_controlled(CATALOG_LOCK, LOCK_WAIT, false, || Ok(()))
    }

    /// Serialises image imports, which can take minutes, without blocking
    /// catalog readers such as `awman agents status`.
    pub fn import_lock(&self) -> Result<File, DataError> {
        self.import_lock_cancellable(|| Ok(()))
    }

    /// [`Self::import_lock`] whose wait is abandoned when `check` errors.
    pub fn import_lock_cancellable(
        &self,
        check: impl FnMut() -> Result<(), DataError>,
    ) -> Result<File, DataError> {
        self.lock_controlled(IMPORT_LOCK, IMPORT_LOCK_WAIT, false, check)
    }

    /// Protect the interval before the SDK publishes a sandbox's rootfs
    /// reference. Afterwards its transactional ImageInUse check owns
    /// retention. Mutations (`exclusive`) fail promptly rather than waiting
    /// for a planned launch.
    pub fn image_lease(&self, image: &str, exclusive: bool) -> Result<File, DataError> {
        let name = format!("image-{}.lock", sha256_hex(image));
        self.lock_controlled(&name, Duration::ZERO, !exclusive, || Ok(()))
    }

    /// Take the lock file `name` in the state directory, waiting up to
    /// `wait` (shared when `shared`). `check` runs before every attempt. The
    /// lock must be a private regular file; anything else is refused.
    pub fn lock_controlled(
        &self,
        name: &str,
        wait: Duration,
        shared: bool,
        check: impl FnMut() -> Result<(), DataError>,
    ) -> Result<File, DataError> {
        let path = self.home.join(name);
        let mode = if shared {
            LockMode::Shared
        } else {
            LockMode::Exclusive
        };
        lock_private_timeout(&path, mode, wait, check)
            .map_err(|e| match e {
                DataError::InvalidPath { path, reason } => DataError::BuiltinState(format!(
                    "builtin lock {} must be a private regular file: {reason}",
                    path.display()
                )),
                e => e,
            })?
            .ok_or_else(|| DataError::BuiltinStateBusy {
                lock: name.to_string(),
                waited_secs: wait.as_secs(),
            })
    }

    /// The attach socket for the sandbox named `name` (already derived from
    /// its id and owner by the engine).
    pub fn attach_socket(&self, name: &str) -> PathBuf {
        self.home.join("attach").join(format!("{name}.sock"))
    }

    /// Refuse a catalog created under another protocol before the SDK runs
    /// migrations on it, and write the marker for a fresh one. Holds the
    /// catalog lock throughout.
    pub fn check_runtime_marker(&self, expected: &str) -> Result<RuntimeMarker, DataError> {
        let _lock = self.lock()?;
        self.check_runtime_marker_locked(expected)
    }

    /// [`Self::check_runtime_marker`] for a caller that already holds the
    /// catalog lock (the in-place upgrade re-reads the marker under the lock
    /// it keeps for the whole upgrade).
    pub fn check_runtime_marker_locked(&self, expected: &str) -> Result<RuntimeMarker, DataError> {
        let path = self.home.join(RUNTIME_VERSION_FILE);
        let db = self.home.join("db");
        if !path.exists()
            && std::fs::read_dir(&db)
                .map_err(|e| io_err(&db, e))?
                .next()
                .is_some()
        {
            return Ok(RuntimeMarker::Mismatch {
                found: "unversioned existing catalog".into(),
            });
        }
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        match options.open(&path) {
            Ok(mut file) => {
                file.write_all(expected.as_bytes())
                    .map_err(|e| io_err(&path, e))?;
                file.sync_all().map_err(|e| io_err(&path, e))?;
                Ok(RuntimeMarker::Created)
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let mut found = String::new();
                let mut read = std::fs::OpenOptions::new();
                read.read(true);
                #[cfg(unix)]
                std::os::unix::fs::OpenOptionsExt::custom_flags(&mut read, libc::O_NOFOLLOW);
                read.open(&path)
                    .map_err(|e| io_err(&path, e))?
                    .take(128)
                    .read_to_string(&mut found)
                    .map_err(|e| io_err(&path, e))?;
                if found == expected {
                    Ok(RuntimeMarker::Matches)
                } else if crate::data::builtin_runtime::UPGRADABLE_FROM.contains(&found.as_str()) {
                    Ok(RuntimeMarker::Upgradable { found })
                } else {
                    Ok(RuntimeMarker::Mismatch { found })
                }
            }
            Err(e) => Err(io_err(&path, e)),
        }
    }
}

mod catalog_backup;
pub use catalog_backup::{
    applied_migrations, CatalogBackup, CATALOG_BACKUP_MANIFEST, CATALOG_RESTORE_JOURNAL,
    RETIRED_SANDBOXES_DIR,
};

/// Canonicalise the longest existing ancestor of `root`'s parent and re-append
/// the missing components. The final component is kept as given, so a
/// symlinked state directory is still refused by [`secure_create`].
fn canonical_parent(root: &Path) -> Result<PathBuf, DataError> {
    let (Some(parent), Some(leaf)) = (root.parent(), root.file_name()) else {
        return Ok(root.to_path_buf());
    };
    if !root.is_absolute() || root.components().any(|c| matches!(c, Component::ParentDir)) {
        // secure_create reports the precise error.
        return Ok(root.to_path_buf());
    }
    let mut existing = parent.to_path_buf();
    let mut missing = Vec::new();
    while !existing.exists() {
        match (existing.parent(), existing.file_name()) {
            (Some(up), Some(name)) => {
                missing.push(name.to_os_string());
                existing = up.to_path_buf();
            }
            _ => break,
        }
    }
    let mut resolved = existing.canonicalize().map_err(|e| io_err(&existing, e))?;
    resolved.extend(missing.into_iter().rev());
    resolved.push(leaf);
    Ok(resolved)
}

/// Create `path` (and missing parents) one component at a time with
/// `openat`/`mkdirat`, never following a symlink, and require every
/// component to be owned by this user or root with no group/world write
/// (a root-owned sticky parent is trusted), and the last one to be 0700.
impl BuiltinState {
    pub fn secure_create(&self) -> Result<(), DataError> {
        let path = self.home.as_path();
        use nix::fcntl::{open, openat, OFlag};
        use nix::sys::stat::{mkdirat, Mode};
        use std::os::unix::fs::MetadataExt;
        if !path.is_absolute() || path.components().any(|c| matches!(c, Component::ParentDir)) {
            return Err(DataError::BuiltinState(
                "builtin state directory must be an absolute path without '..'".into(),
            ));
        }
        let uid = nix::unistd::geteuid().as_raw();
        let flags = OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC | OFlag::O_RDONLY;
        let mut directory: File = open("/", flags, Mode::empty())
            .map_err(|e| io_err(path, e.into()))?
            .into();
        let parts: Vec<_> = path
            .components()
            .filter_map(|c| {
                if let Component::Normal(n) = c {
                    Some(n)
                } else {
                    None
                }
            })
            .collect();
        if parts.is_empty() {
            return Err(DataError::BuiltinState(
                "builtin state directory cannot be /".into(),
            ));
        }
        for (index, component) in parts.iter().enumerate() {
            match mkdirat(
                &directory,
                Path::new(component),
                Mode::from_bits_truncate(0o700),
            ) {
                Ok(()) | Err(nix::errno::Errno::EEXIST) => (),
                Err(e) => return Err(io_err(path, e.into())),
            }
            directory = openat(&directory, Path::new(component), flags, Mode::empty())
            .map_err(|e| match e {
                nix::errno::Errno::ELOOP | nix::errno::Errno::ENOTDIR => {
                    DataError::BuiltinState(format!(
                        "builtin state directory {} contains a symlink or non-directory component; use a real directory path",
                        path.display()
                    ))
                }
                e => io_err(path, e.into()),
            })?
            .into();
            let metadata = directory.metadata().map_err(|e| io_err(path, e))?;
            let last = index + 1 == parts.len();
            let trusted_sticky = metadata.uid() == 0 && metadata.mode() & 0o1000 != 0 && !last;
            if (metadata.uid() != 0 && metadata.uid() != uid)
                || (metadata.mode() & 0o022 != 0 && !trusted_sticky)
                || (last && (metadata.uid() != uid || metadata.mode() & 0o077 != 0))
            {
                return Err(DataError::BuiltinState(
                    "builtin state directory requires private ownership (0700) and trusted parents"
                        .into(),
                ));
            }
        }
        Ok(())
    }
}
