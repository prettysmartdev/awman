//! The cache directory: layout, locks, leases and private record I/O.
//!
//! ```text
//! images/<archive-sha256>.tar    the validated archive, verbatim
//! images/<archive-sha256>.json   CacheRecord
//! refs/<key-sha256>.json         RefRecord
//! leases/<archive-sha256>.lock   held (shared flock) while an archive is in use
//! tmp/acq-*/                     private staging for in-flight acquisitions
//! tmp/acq-*/.lease               held (exclusive flock) by the live acquisition
//! cache.lock                     serialises commit and prune (readers share it)
//! ```
//!
//! Every directory is 0700 and every file 0600. Locks, leases and records
//! are opened without following symlinks and must be single-link regular
//! files owned by this user (`data::fs::private_files`). A hostile process
//! running as the same user can still race path checks and is outside the
//! cache's threat model.
//!
//! This type does no policy: it never decides what is valid, current or
//! prunable. The engine's `OciCache` composes these operations.

use std::fs::{DirEntry, File};
use std::path::{Path, PathBuf};

use crate::data::error::DataError;
use crate::data::fs::hash::sha256_hex;
use crate::data::fs::private_files::{
    create_private_dir, is_interrupted_private_write, lock_private, open_private_lock,
    read_private_json_or_miss, remove_private_file, sync_private_dir, write_private_json_atomic,
    LockMode,
};
use crate::data::oci_cache::record::{CacheRecord, RefRecord, MAX_RECORD_BYTES};

/// Directory name under the builtin state directory.
pub const CACHE_DIR: &str = "oci-cache";
/// The lease file inside a leased staging directory.
pub const STAGING_LEASE: &str = ".lease";

fn io_err(path: &Path, source: std::io::Error) -> DataError {
    DataError::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// The cache rooted at `<state_dir>/oci-cache`.
#[derive(Debug, Clone)]
pub struct OciCacheStore {
    root: PathBuf,
}

impl OciCacheStore {
    /// Open (creating, owner-only) the cache directories under `state_dir`.
    pub fn open(state_dir: &Path) -> Result<Self, DataError> {
        let root = state_dir.join(CACHE_DIR);
        for dir in [
            root.clone(),
            root.join("images"),
            root.join("refs"),
            root.join("leases"),
            root.join("tmp"),
        ] {
            create_private_dir(&dir)?;
        }
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn images_dir(&self) -> PathBuf {
        self.root.join("images")
    }

    pub fn refs_dir(&self) -> PathBuf {
        self.root.join("refs")
    }

    pub fn leases_dir(&self) -> PathBuf {
        self.root.join("leases")
    }

    pub fn tmp_dir(&self) -> PathBuf {
        self.root.join("tmp")
    }

    pub fn lock_path(&self) -> PathBuf {
        self.root.join("cache.lock")
    }

    pub fn ref_path(&self, key: &str) -> PathBuf {
        self.refs_dir().join(format!("{}.json", sha256_hex(key)))
    }

    pub fn archive_path(&self, archive_sha256: &str) -> PathBuf {
        self.images_dir().join(format!("{archive_sha256}.tar"))
    }

    pub fn record_path(&self, archive_sha256: &str) -> PathBuf {
        self.images_dir().join(format!("{archive_sha256}.json"))
    }

    pub fn lease_path(&self, archive_sha256: &str) -> PathBuf {
        self.leases_dir().join(format!("{archive_sha256}.lock"))
    }

    /// The archive's SHA-256 when `archive` is exactly a path this cache
    /// stores archives at (`images/<sha>.tar`).
    pub fn archive_stem(&self, archive: &Path) -> Option<String> {
        if archive.parent() != Some(self.images_dir().as_path())
            || archive.extension().and_then(|e| e.to_str()) != Some("tar")
        {
            return None;
        }
        archive.file_stem()?.to_str().map(str::to_string)
    }

    /// The archive stem of any path directly under `images/`.
    pub fn image_stem(&self, path: &Path) -> Option<String> {
        if path.parent() != Some(self.images_dir().as_path()) {
            return None;
        }
        path.file_stem()?.to_str().map(str::to_string)
    }

    /// The cache's cross-process lock: exclusive for commit/prune, shared for
    /// readers. `check` runs before each attempt; an error abandons the wait.
    /// Released on drop.
    pub fn lock(
        &self,
        exclusive: bool,
        check: impl FnMut() -> Result<(), DataError>,
    ) -> Result<File, DataError> {
        let mode = if exclusive {
            LockMode::Exclusive
        } else {
            LockMode::Shared
        };
        lock_private(&self.lock_path(), mode, check)
    }

    /// Lock the lease of `archive_sha256` shared. Callers hold the cache lock
    /// (shared suffices): prune, which deletes lease files, holds it
    /// exclusively.
    pub fn lease_shared(&self, archive_sha256: &str) -> Result<File, DataError> {
        let path = self.lease_path(archive_sha256);
        let file = open_private_lock(&path, true)?
            .ok_or_else(|| DataError::Other(format!("lease {} vanished", path.display())))?;
        file.lock_shared().map_err(|e| io_err(&path, e))?;
        Ok(file)
    }

    /// Callers hold the exclusive cache lock. `Ok(None)` when a lease on
    /// `archive_sha256` is held; otherwise the (possibly absent) lease file,
    /// locked exclusively so it cannot be taken while it is removed.
    pub fn reclaim_lease(&self, archive_sha256: &str) -> Result<Option<Option<File>>, DataError> {
        let path = self.lease_path(archive_sha256);
        let Some(file) = open_private_lock(&path, false)? else {
            return Ok(Some(None));
        };
        match file.try_lock() {
            Ok(()) => Ok(Some(Some(file))),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(std::fs::TryLockError::Error(e)) => Err(io_err(&path, e)),
        }
    }

    /// A fresh private staging directory (`tmp/acq-*`), removed when dropped.
    pub fn new_staging_dir(&self) -> Result<tempfile::TempDir, DataError> {
        let tmp = self.tmp_dir();
        tempfile::Builder::new()
            .prefix("acq-")
            .tempdir_in(&tmp)
            .map_err(|e| io_err(&tmp, e))
    }

    /// Create and exclusively lock the `.lease` file of the staging directory
    /// `dir`. It stays locked for as long as the returned file is held.
    pub fn lock_staging_lease(&self, dir: &Path) -> Result<File, DataError> {
        let path = dir.join(STAGING_LEASE);
        let file = open_private_lock(&path, true)?.ok_or_else(|| {
            DataError::Other(format!("staging lease {} vanished", path.display()))
        })?;
        file.try_lock().map_err(|e| match e {
            std::fs::TryLockError::Error(e) => io_err(&path, e),
            std::fs::TryLockError::WouldBlock => {
                DataError::Other(format!("staging lease {} is already held", path.display()))
            }
        })?;
        Ok(file)
    }

    /// Whether `entry` of `tmp/` may be removed by prune: a leased directory
    /// whose lease no one holds, or an unleased entry not modified within
    /// `stale_after`. Callers hold the exclusive cache lock.
    pub fn staging_is_abandoned(
        &self,
        entry: &DirEntry,
        stale_after: std::time::Duration,
    ) -> Result<bool, DataError> {
        let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
        if is_dir {
            if let Some(lease) = open_private_lock(&entry.path().join(STAGING_LEASE), false)? {
                return match lease.try_lock() {
                    Ok(()) => Ok(true),
                    Err(std::fs::TryLockError::WouldBlock) => Ok(false),
                    Err(std::fs::TryLockError::Error(e)) => Err(io_err(&entry.path(), e)),
                };
            }
        }
        Ok(entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > stale_after))
    }

    /// Remove a staging entry (directory or file); `true` when it is gone.
    pub fn remove_staging_entry(&self, entry: &DirEntry) -> bool {
        let path = entry.path();
        match entry.file_type() {
            Ok(t) if t.is_dir() => std::fs::remove_dir_all(&path),
            _ => std::fs::remove_file(&path),
        }
        .is_ok()
    }

    /// The archive's size when it is a plain file. A symlink or anything but
    /// a regular file is `None`, never followed.
    pub fn archive_len(&self, archive_sha256: &str) -> Option<u64> {
        let meta = std::fs::symlink_metadata(self.archive_path(archive_sha256)).ok()?;
        meta.file_type().is_file().then_some(meta.len())
    }

    /// Whether anything (even a dangling symlink) occupies the archive path.
    pub fn archive_exists(&self, archive_sha256: &str) -> bool {
        std::fs::symlink_metadata(self.archive_path(archive_sha256)).is_ok()
    }

    /// Move the staged archive into `images/`. `staged` must be on the same
    /// filesystem (under this cache's `tmp/`).
    pub fn adopt_archive(&self, staged: &Path, archive_sha256: &str) -> Result<(), DataError> {
        let archive = self.archive_path(archive_sha256);
        std::fs::rename(staged, &archive).map_err(|e| io_err(&archive, e))
    }

    /// Best-effort removal of an entry's record and archive, for a commit
    /// that failed after introducing them.
    pub fn discard_entry(&self, archive_sha256: &str) {
        let _ = std::fs::remove_file(self.record_path(archive_sha256));
        let _ = std::fs::remove_file(self.archive_path(archive_sha256));
    }

    /// Best-effort removal of the record next to a removed archive.
    pub fn discard_record(&self, archive_sha256: &str) {
        let _ = std::fs::remove_file(self.record_path(archive_sha256));
    }

    /// The record stored for `archive_sha256`. Absent, unsafe, oversized or
    /// unparsable is a miss.
    pub fn read_record(&self, archive_sha256: &str) -> Result<Option<CacheRecord>, DataError> {
        read_private_json_or_miss(&self.record_path(archive_sha256), MAX_RECORD_BYTES)
    }

    /// The ref stored at `path` (as listed by [`Self::read_dir`] of `refs/`,
    /// or from [`Self::ref_path`]); a miss as for [`Self::read_record`].
    pub fn read_ref_at(&self, path: &Path) -> Result<Option<RefRecord>, DataError> {
        read_private_json_or_miss(path, MAX_RECORD_BYTES)
    }

    /// The ref `key` resolved to, if any.
    pub fn read_ref(&self, key: &str) -> Result<Option<RefRecord>, DataError> {
        self.read_ref_at(&self.ref_path(key))
    }

    /// Atomically replace the record of `archive_sha256`.
    pub fn write_record(
        &self,
        archive_sha256: &str,
        record: &CacheRecord,
        check: impl FnMut() -> Result<(), DataError>,
    ) -> Result<(), DataError> {
        write_private_json_atomic(&self.record_path(archive_sha256), record, check)
    }

    /// Atomically replace the ref for `key`.
    pub fn write_ref(
        &self,
        key: &str,
        reference: &RefRecord,
        check: impl FnMut() -> Result<(), DataError>,
    ) -> Result<(), DataError> {
        write_private_json_atomic(&self.ref_path(key), reference, check)
    }

    /// fsync `images/` and `refs/` after a commit so its renames survive a
    /// crash.
    pub fn sync_committed(&self) {
        sync_private_dir(&self.images_dir());
        sync_private_dir(&self.refs_dir());
    }

    /// The entries of `dir`; a missing directory is empty.
    pub fn read_dir(&self, dir: &Path) -> Result<Vec<DirEntry>, DataError> {
        match std::fs::read_dir(dir) {
            Ok(it) => Ok(it.flatten().collect()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(io_err(dir, e)),
        }
    }

    /// Remove `path`; absent is success.
    pub fn remove_file(&self, path: &Path) -> Result<(), DataError> {
        remove_private_file(path)
    }

    /// Whether `path` is the leftover of an interrupted atomic write.
    pub fn is_interrupted_write(&self, path: &Path) -> bool {
        is_interrupted_private_write(path)
    }
}
