//! Owner-only directories, lock files and atomically replaced JSON records.
//!
//! The conventions match the image cache's: nothing is followed through a
//! symlink, a lock or record must be a single-link regular file owned by
//! this user and closed to everyone else, and a record is replaced by
//! writing a private sibling, syncing it, and renaming it into place. A file
//! that breaks these rules is refused, never adopted.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::data::error::DataError;

/// How long a lock waiter sleeps between attempts.
const LOCK_POLL: Duration = Duration::from_millis(50);

fn io_err(path: &Path, source: std::io::Error) -> DataError {
    DataError::Io {
        path: path.to_path_buf(),
        source,
    }
}

fn refused(path: &Path, why: &str) -> DataError {
    DataError::InvalidPath {
        path: path.to_path_buf(),
        reason: format!("{why}; remove it and retry"),
    }
}

/// Create `dir` (and missing parents) and require it to be a real directory,
/// owned by this user and closed to others.
pub fn create_private_dir(dir: &Path) -> Result<(), DataError> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(dir).map_err(|e| io_err(dir, e))?;
    let meta = std::fs::symlink_metadata(dir).map_err(|e| io_err(dir, e))?;
    if !meta.file_type().is_dir() {
        return Err(refused(
            dir,
            "is not a plain directory (symlinks are refused)",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if meta.uid() != nix::unistd::geteuid().as_raw() {
            return Err(refused(dir, "is owned by another user"));
        }
        if meta.mode() & 0o077 != 0 {
            return Err(refused(dir, "is accessible to other users"));
        }
    }
    Ok(())
}

/// Create `dir`, which must not already exist, readable only by this user.
/// The parent must exist.
pub fn create_private_dir_new(dir: &Path) -> Result<(), DataError> {
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(dir).map_err(|e| io_err(dir, e))
}

/// Create `path`, which must not already exist, readable and writable only by
/// this user. Exclusive creation means a staging name can never be
/// pre-planted (or be a symlink that is followed).
pub fn create_private_file_new(path: &Path) -> Result<File, DataError> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path).map_err(|e| io_err(path, e))
}

/// Open `path` without following a symlink, creating it owner-only when
/// `create` is set, and check it is a private regular file. `Ok(None)` when
/// it is absent and `create` is not set.
fn open_private(path: &Path, create: bool, write: bool) -> Result<Option<File>, DataError> {
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(write)
        .create(create)
        .truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let file = match options.open(path) {
        Ok(file) => file,
        Err(e) if !create && e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(refused(
                path,
                &format!("cannot be opened safely (symlinks are refused): {e}"),
            ));
        }
    };
    let meta = file.metadata().map_err(|e| io_err(path, e))?;
    if !meta.is_file() {
        return Err(refused(path, "is not a regular file"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if meta.nlink() != 1 {
            return Err(refused(path, "has other hard links"));
        }
        if meta.uid() != nix::unistd::geteuid().as_raw() {
            return Err(refused(path, "is owned by another user"));
        }
        if meta.mode() & 0o077 != 0 {
            return Err(refused(path, "is accessible to other users"));
        }
    }
    Ok(Some(file))
}

/// Open the private lock or lease file at `path` (read-write), creating it
/// owner-only when `create` is set. `Ok(None)` when it is absent and not
/// created. A symlink, hard link, foreign or group/world-accessible file is
/// refused, never locked.
pub fn open_private_lock(path: &Path, create: bool) -> Result<Option<File>, DataError> {
    open_private(path, create, true)
}

/// Whether a lock is exclusive or shared among readers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockMode {
    Exclusive,
    Shared,
}

/// Take an exclusive cross-process lock on the private lock file at `path`;
/// see [`lock_private`].
pub fn lock_private_exclusive(
    path: &Path,
    check: impl FnMut() -> Result<(), DataError>,
) -> Result<File, DataError> {
    lock_private(path, LockMode::Exclusive, check)
}

/// Take a cross-process lock on the private lock file at `path`, creating it
/// if needed. Waits until it is free, calling `check` before every attempt so
/// a caller can abandon the wait (cancellation, deadline) by returning an
/// error. The lock is released when the file is dropped.
pub fn lock_private(
    path: &Path,
    mode: LockMode,
    mut check: impl FnMut() -> Result<(), DataError>,
) -> Result<File, DataError> {
    let file = open_private(path, true, true)?
        .ok_or_else(|| DataError::Other(format!("lock {} vanished", path.display())))?;
    loop {
        check()?;
        let locked = match mode {
            LockMode::Exclusive => file.try_lock(),
            LockMode::Shared => file.try_lock_shared(),
        };
        match locked {
            Ok(()) => break,
            Err(std::fs::TryLockError::WouldBlock) => std::thread::sleep(LOCK_POLL),
            Err(std::fs::TryLockError::Error(e)) => return Err(io_err(path, e)),
        }
    }
    check()?;
    Ok(file)
}

/// Like [`lock_private`], but gives up after `wait` once the lock is still
/// held by someone else: `Ok(None)` means "busy". One attempt is always made,
/// so a zero `wait` is a try-lock. `check` runs before every attempt.
pub fn lock_private_timeout(
    path: &Path,
    mode: LockMode,
    wait: Duration,
    mut check: impl FnMut() -> Result<(), DataError>,
) -> Result<Option<File>, DataError> {
    let file = open_private(path, true, true)?
        .ok_or_else(|| DataError::Other(format!("lock {} vanished", path.display())))?;
    let deadline = std::time::Instant::now() + wait;
    loop {
        check()?;
        let locked = match mode {
            LockMode::Exclusive => file.try_lock(),
            LockMode::Shared => file.try_lock_shared(),
        };
        match locked {
            Ok(()) => return Ok(Some(file)),
            Err(std::fs::TryLockError::WouldBlock) if std::time::Instant::now() < deadline => {
                std::thread::sleep(LOCK_POLL)
            }
            Err(std::fs::TryLockError::WouldBlock) => return Ok(None),
            Err(std::fs::TryLockError::Error(e)) => return Err(io_err(path, e)),
        }
    }
}

/// Read the private JSON record at `path`. `Ok(None)` when it does not
/// exist; an unsafe or unparsable file is an error, never treated as absent.
pub fn read_private_json<T: DeserializeOwned>(path: &Path) -> Result<Option<T>, DataError> {
    let Some(mut file) = open_private(path, false, false)? else {
        return Ok(None);
    };
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(&mut file, &mut bytes).map_err(|e| io_err(path, e))?;
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|e| refused(path, &format!("is not a valid record ({e})")))
}

/// Replace the record at `path` with `value`: write a private sibling, sync
/// it, call `check` one last time, then rename it into place. The previous
/// record (if any) stays intact unless the rename happens.
pub fn write_private_json_atomic<T: Serialize>(
    path: &Path,
    value: &T,
    mut check: impl FnMut() -> Result<(), DataError>,
) -> Result<(), DataError> {
    let dir = path.parent().unwrap_or(Path::new("."));
    let bytes =
        serde_json::to_vec_pretty(value).map_err(|source| DataError::ConfigSerialize { source })?;
    let tmp: PathBuf = dir.join(format!(
        ".{}.{}.tmp",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("record"),
        uuid::Uuid::new_v4().simple()
    ));
    let result = (|| {
        let mut file = open_private(&tmp, true, true)?
            .ok_or_else(|| DataError::Other(format!("{} vanished", tmp.display())))?;
        file.write_all(&bytes)
            .and_then(|_| file.sync_all())
            .map_err(|e| io_err(&tmp, e))?;
        check()?;
        std::fs::rename(&tmp, path).map_err(|e| io_err(path, e))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result?;
    #[cfg(unix)]
    if let Ok(d) = File::open(dir) {
        let _ = d.sync_all();
    }
    Ok(())
}

/// Read a small JSON record where anything unusable is a miss, not an error:
/// absent, a symlink or other non-regular file, larger than `max_bytes`, or
/// unparsable all give `Ok(None)`. The file is never followed through a
/// symlink and is read without allocating more than `max_bytes`. Only a
/// failure to open or read an otherwise ordinary file is an error.
pub fn read_private_json_or_miss<T: DeserializeOwned>(
    path: &Path,
    max_bytes: u64,
) -> Result<Option<T>, DataError> {
    use std::io::Read;
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = match options.open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        // ELOOP: a symlink in place of a record.
        #[cfg(unix)]
        Err(e) if e.raw_os_error() == Some(libc::ELOOP) => return Ok(None),
        Err(e) => return Err(io_err(path, e)),
    };
    if !file.metadata().is_ok_and(|m| m.is_file()) {
        return Ok(None);
    }
    let mut bytes = Vec::new();
    file.take(max_bytes + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| io_err(path, e))?;
    if bytes.len() as u64 > max_bytes {
        return Ok(None);
    }
    Ok(serde_json::from_slice(&bytes).ok())
}

/// Whether `path` is the leftover sibling of an interrupted
/// [`write_private_json_atomic`] (`.<name>.<uuid>.tmp`).
pub fn is_interrupted_private_write(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with('.') && n.ends_with(".tmp"))
}

/// Best-effort directory fsync so a committed rename survives a crash.
pub fn sync_private_dir(dir: &Path) {
    #[cfg(unix)]
    if let Ok(d) = File::open(dir) {
        let _ = d.sync_all();
    }
    #[cfg(not(unix))]
    let _ = dir;
}

/// Remove the record at `path`. Absent is success.
pub fn remove_private_file(path: &Path) -> Result<(), DataError> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(io_err(path, e)),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};

    #[test]
    fn create_private_file_new_is_exclusive_and_owner_only() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("stage");
        drop(create_private_file_new(&path).unwrap());
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(matches!(
            create_private_file_new(&path),
            Err(DataError::Io { .. })
        ));
        let link = root.path().join("link");
        symlink(root.path().join("elsewhere"), &link).unwrap();
        assert!(create_private_file_new(&link).is_err());
        assert!(!root.path().join("elsewhere").exists());
    }

    #[test]
    fn private_records_refuse_symlinks_and_public_permissions() {
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let record = root.path().join("record.json");
        write_private_json_atomic(&record, &serde_json::json!({"ok": true}), || Ok(())).unwrap();
        assert_eq!(
            read_private_json::<serde_json::Value>(&record).unwrap(),
            Some(serde_json::json!({"ok": true}))
        );
        let link = root.path().join("link.json");
        symlink(&record, &link).unwrap();
        assert!(read_private_json::<serde_json::Value>(&link).is_err());
        assert!(lock_private_exclusive(&link, || Ok(())).is_err());
        std::fs::set_permissions(&record, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_private_json::<serde_json::Value>(&record).is_err());
    }

    #[test]
    fn private_directory_refuses_a_symlink() {
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let real = root.path().join("real");
        create_private_dir(&real).unwrap();
        let link = root.path().join("link");
        symlink(&real, &link).unwrap();
        assert!(create_private_dir(&link).is_err());
    }
}
