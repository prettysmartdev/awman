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

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::data::config::image_source::ImageSourceKind;
    use crate::data::oci_cache::record::{ImageConfigSummary, RECORD_VERSION};
    use crate::data::oci_identity::{Digest, ImageIdentity, OciPlatform};
    use std::os::unix::fs::{symlink, PermissionsExt};

    fn record(sha: &str) -> CacheRecord {
        CacheRecord {
            version: RECORD_VERSION,
            identity: ImageIdentity {
                reference: "r".into(),
                manifest_digest: Digest::parse(&format!("sha256:{}", "a".repeat(64))).unwrap(),
                config_digest: Digest::parse(&format!("sha256:{}", "b".repeat(64))).unwrap(),
                platform: OciPlatform::host_linux(),
                source: ImageSourceKind::Registry,
            },
            archive_format: "oci-layout".into(),
            bytes: 1,
            archive_sha256: sha.into(),
            acquired_at: "2026-01-01T00:00:00+00:00".into(),
            locator: "l".into(),
            source_fingerprint: None,
            config: ImageConfigSummary::default(),
        }
    }

    fn open() -> (tempfile::TempDir, OciCacheStore) {
        let state = tempfile::tempdir().unwrap();
        std::fs::set_permissions(state.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let store = OciCacheStore::open(state.path()).unwrap();
        (state, store)
    }

    #[test]
    fn open_creates_the_private_layout() {
        let (state, store) = open();
        assert_eq!(store.root(), state.path().join(CACHE_DIR));
        for dir in [
            store.images_dir(),
            store.refs_dir(),
            store.leases_dir(),
            store.tmp_dir(),
        ] {
            let mode = std::fs::metadata(&dir).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700, "{}", dir.display());
        }
    }

    #[test]
    fn paths_follow_the_documented_layout() {
        let (_state, store) = open();
        let sha = "f".repeat(64);
        assert_eq!(
            store.archive_path(&sha),
            store.root().join("images").join(format!("{sha}.tar"))
        );
        assert_eq!(
            store.record_path(&sha),
            store.root().join("images").join(format!("{sha}.json"))
        );
        assert_eq!(
            store.lease_path(&sha),
            store.root().join("leases").join(format!("{sha}.lock"))
        );
        assert_eq!(
            store.ref_path("k"),
            store
                .root()
                .join("refs")
                .join(format!("{}.json", sha256_hex("k")))
        );
        assert_eq!(
            store.archive_stem(&store.archive_path(&sha)),
            Some(sha.clone())
        );
        assert_eq!(store.archive_stem(&store.record_path(&sha)), None);
        assert_eq!(store.archive_stem(Path::new("/elsewhere/x.tar")), None);
    }

    #[test]
    fn a_substituted_cache_directory_is_refused_at_open() {
        let (state, store) = open();
        let refs = store.refs_dir();
        std::fs::remove_dir(&refs).unwrap();
        let elsewhere = state.path().join("elsewhere");
        std::fs::create_dir(&elsewhere).unwrap();
        symlink(&elsewhere, &refs).unwrap();
        let err = OciCacheStore::open(state.path()).unwrap_err().to_string();
        assert!(err.contains("symlinks are refused"), "{err}");
    }

    #[test]
    fn records_and_refs_round_trip_and_replace_atomically() {
        let (_state, store) = open();
        let sha = "c".repeat(64);
        assert!(store.read_record(&sha).unwrap().is_none());
        store.write_record(&sha, &record(&sha), || Ok(())).unwrap();
        assert_eq!(store.read_record(&sha).unwrap().unwrap(), record(&sha));

        let reference = RefRecord {
            version: RECORD_VERSION,
            key: "k".into(),
            archive_sha256: sha.clone(),
            record: record(&sha),
        };
        store.write_ref("k", &reference, || Ok(())).unwrap();
        assert_eq!(store.read_ref("k").unwrap().unwrap().key, "k");

        // A cancelled replacement keeps the previous ref and leaves no
        // sibling behind.
        let before = std::fs::read(store.ref_path("k")).unwrap();
        let mut replacement = reference.clone();
        replacement.key = "k-replacement".into();
        let err = store.write_ref("k", &replacement, || Err(DataError::Other("stop".into())));
        assert!(err.is_err());
        assert_eq!(store.read_ref("k").unwrap().unwrap().key, "k");
        assert_eq!(std::fs::read(store.ref_path("k")).unwrap(), before);
        assert!(store
            .read_dir(&store.refs_dir())
            .unwrap()
            .iter()
            .all(|e| !store.is_interrupted_write(&e.path())));
    }

    #[test]
    fn unusable_records_are_misses() {
        let (state, store) = open();
        let sha = "d".repeat(64);
        let path = store.record_path(&sha);
        std::fs::write(&path, b"{ not json").unwrap();
        assert!(store.read_record(&sha).unwrap().is_none(), "torn");
        std::fs::remove_file(&path).unwrap();

        let target = state.path().join("target.json");
        std::fs::write(&target, serde_json::to_vec(&record(&sha)).unwrap()).unwrap();
        symlink(&target, &path).unwrap();
        assert!(store.read_record(&sha).unwrap().is_none(), "symlink");
        std::fs::remove_file(&path).unwrap();

        let mut padded = serde_json::to_vec(&record(&sha)).unwrap();
        padded.pop();
        padded.extend(std::iter::repeat_n(b' ', MAX_RECORD_BYTES as usize));
        padded.push(b'}');
        std::fs::write(&path, &padded).unwrap();
        assert!(store.read_record(&sha).unwrap().is_none(), "oversized");
    }

    #[test]
    fn the_cache_lock_is_shared_for_readers_and_exclusive_for_writers() {
        let (_state, store) = open();
        let a = store.lock(false, || Ok(())).unwrap();
        let _b = store.lock(false, || Ok(())).unwrap();
        let mut attempts = 0;
        let blocked = store.lock(true, || {
            attempts += 1;
            if attempts > 2 {
                Err(DataError::Other("gave up".into()))
            } else {
                Ok(())
            }
        });
        assert!(blocked.is_err(), "writers wait for readers");
        drop(a);
    }

    #[test]
    fn substituted_locks_are_refused_not_followed() {
        let (state, store) = open();
        let target = state.path().join("target");
        std::fs::write(&target, b"").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
        let lock = store.lock_path();

        symlink(&target, &lock).unwrap();
        let err = store.lock(false, || Ok(())).unwrap_err().to_string();
        assert!(err.contains("symlinks are refused"), "{err}");
        std::fs::remove_file(&lock).unwrap();

        std::fs::hard_link(&target, &lock).unwrap();
        let err = store.lock(true, || Ok(())).unwrap_err().to_string();
        assert!(err.contains("hard links"), "{err}");
        std::fs::remove_file(&lock).unwrap();

        std::fs::write(&lock, b"").unwrap();
        std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o644)).unwrap();
        let err = store.lock(false, || Ok(())).unwrap_err().to_string();
        assert!(err.contains("other users"), "{err}");
    }

    #[test]
    fn a_held_lease_cannot_be_reclaimed_and_a_free_one_can() {
        let (_state, store) = open();
        let sha = "e".repeat(64);
        assert!(
            matches!(store.reclaim_lease(&sha).unwrap(), Some(None)),
            "absent"
        );
        let lease = store.lease_shared(&sha).unwrap();
        assert!(store.reclaim_lease(&sha).unwrap().is_none(), "held");
        drop(lease);
        assert!(
            matches!(store.reclaim_lease(&sha).unwrap(), Some(Some(_))),
            "free"
        );
    }

    #[test]
    fn a_planted_lease_symlink_is_refused() {
        let (state, store) = open();
        let sha = "e".repeat(64);
        let target = state.path().join("target");
        std::fs::write(&target, b"").unwrap();
        symlink(&target, store.lease_path(&sha)).unwrap();
        assert!(store.lease_shared(&sha).is_err());
        assert!(store.reclaim_lease(&sha).is_err());
        assert!(target.exists());
    }

    #[test]
    fn staging_with_a_live_lease_is_not_abandoned_until_it_is_released() {
        let (_state, store) = open();
        let dir = store.new_staging_dir().unwrap();
        let lease = store.lock_staging_lease(dir.path()).unwrap();
        assert!(
            store.lock_staging_lease(dir.path()).is_err(),
            "already held"
        );
        let stale = std::time::Duration::from_secs(3600);
        let entry = || store.read_dir(&store.tmp_dir()).unwrap().pop().unwrap();
        assert!(!store.staging_is_abandoned(&entry(), stale).unwrap());
        drop(lease);
        assert!(store.staging_is_abandoned(&entry(), stale).unwrap());
        assert!(store.remove_staging_entry(&entry()));
    }

    #[test]
    fn archive_len_ignores_symlinks_and_adopt_moves_the_staged_file() {
        let (state, store) = open();
        let sha = "9".repeat(64);
        assert_eq!(store.archive_len(&sha), None);
        assert!(!store.archive_exists(&sha));
        let staging = store.new_staging_dir().unwrap();
        let staged = staging.path().join("a.tar");
        std::fs::write(&staged, b"12345").unwrap();
        store.adopt_archive(&staged, &sha).unwrap();
        assert_eq!(store.archive_len(&sha), Some(5));

        let outside = state.path().join("outside.tar");
        std::fs::rename(store.archive_path(&sha), &outside).unwrap();
        symlink(&outside, store.archive_path(&sha)).unwrap();
        assert!(store.archive_exists(&sha));
        assert_eq!(store.archive_len(&sha), None, "a symlink is not an archive");
        store.discard_entry(&sha);
        assert!(!store.archive_exists(&sha));
    }
}
