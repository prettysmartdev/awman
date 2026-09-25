//! awman's verified image-archive cache.
//!
//! Layout under `<builtin state dir>/oci-cache/` (all directories 0700, all
//! files 0600):
//!
//! ```text
//! images/<archive-sha256>.tar    the validated archive, verbatim
//! images/<archive-sha256>.json   CacheRecord: identity, format, size, source locator
//! refs/<key-sha256>.json         RefRecord: which archive a source+reference+platform resolved to
//! tmp/acq-*/                     private staging for in-flight acquisitions
//! ```
//!
//! Commit order is archive → record → ref, each by `rename` from a fully
//! written, fsynced file in the same filesystem. A reader starts from a ref,
//! so it can only ever observe a complete record whose archive already
//! exists with the recorded size. A crash leaves at worst an unreferenced
//! archive or a stale staging directory, both removed by [`OciCache::prune`].
//! `cache.lock` serialises commit and prune across processes (readers take
//! it shared), and a lookup re-hashes the archive before trusting it.
//!
//! Records hold no secrets: the locator is a registry host and repository, a
//! Docker endpoint without credentials, or an archive path. Image `Env` is
//! not recorded.

use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use crate::data::oci_identity::{Digest, ImageIdentity};
use crate::engine::error::EngineError;
use crate::engine::oci::archive::{ImageConfigSummary, ValidatedArchive};
use crate::engine::oci::verify::{create_private_dir, create_private_file, hash_file, sha256_hex};
use crate::engine::oci::{AcquiredImage, ArchiveFormat};

/// Directory name under the builtin state directory.
pub const CACHE_DIR: &str = "oci-cache";
const RECORD_VERSION: u32 = 1;
/// Staging directories older than this belong to a crashed acquisition.
const STALE_STAGING: Duration = Duration::from_secs(24 * 60 * 60);

/// What a cached archive is, persisted next to it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CacheRecord {
    pub version: u32,
    pub identity: ImageIdentity,
    /// `"oci-layout"` or `"docker-save"`.
    pub archive_format: String,
    pub bytes: u64,
    pub archive_sha256: String,
    /// RFC 3339.
    pub acquired_at: String,
    /// Non-secret description of where it came from.
    pub locator: String,
    /// For archive sources: the source file's size and mtime when imported,
    /// so a re-exported archive at the same path is re-imported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_fingerprint: Option<SourceFingerprint>,
    pub config: ImageConfigSummary,
}

/// Size and modification time of a local source file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceFingerprint {
    pub len: u64,
    pub mtime_ns: u128,
}

impl SourceFingerprint {
    pub(super) fn of(path: &Path) -> Option<Self> {
        let meta = std::fs::metadata(path).ok()?;
        let mtime_ns = meta
            .modified()
            .ok()?
            .duration_since(SystemTime::UNIX_EPOCH)
            .ok()?
            .as_nanos();
        Some(Self {
            len: meta.len(),
            mtime_ns,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RefRecord {
    version: u32,
    key: String,
    archive_sha256: String,
    // The same archive can be acquired from different sources or contain
    // several selected images. Keep provenance and selection with the ref,
    // rather than borrowing whichever record last wrote the shared archive.
    // Older refs without this field are cache misses and must be revalidated.
    record: CacheRecord,
}

/// A cache hit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedImage {
    pub image: AcquiredImage,
    pub record: CacheRecord,
}

/// What [`OciCache::prune`] removed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PruneReport {
    pub archives_removed: usize,
    pub bytes_freed: u64,
    pub refs_removed: usize,
    pub staging_removed: usize,
}

/// The cache rooted at `<state_dir>/oci-cache`.
#[derive(Debug, Clone)]
pub struct OciCache {
    root: PathBuf,
}

impl OciCache {
    /// Open (creating, owner-only) the cache under `state_dir`.
    pub fn open(state_dir: &Path) -> Result<Self, EngineError> {
        let root = state_dir.join(CACHE_DIR);
        for dir in [
            root.clone(),
            root.join("images"),
            root.join("refs"),
            root.join("tmp"),
        ] {
            create_private_dir(&dir)?;
        }
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// A fresh private staging directory, removed when dropped.
    pub(super) fn staging(&self) -> Result<tempfile::TempDir, EngineError> {
        let tmp = self.root.join("tmp");
        tempfile::Builder::new()
            .prefix("acq-")
            .tempdir_in(&tmp)
            .map_err(|e| EngineError::io(tmp, e))
    }

    /// The cache's cross-process lock: exclusive for `commit`/`prune`,
    /// shared for readers, so pruning can never observe (and delete) an
    /// archive whose record or ref is still being written. Released on drop.
    fn lock(&self, exclusive: bool) -> Result<std::fs::File, EngineError> {
        let path = self.root.join("cache.lock");
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let file = options.open(&path).map_err(|e| EngineError::io(&path, e))?;
        if !file
            .metadata()
            .map_err(|e| EngineError::io(&path, e))?
            .is_file()
        {
            return Err(EngineError::Config(format!(
                "image cache lock {} is not a regular file",
                path.display()
            )));
        }
        let locked = if exclusive {
            file.lock()
        } else {
            file.lock_shared()
        };
        locked.map_err(|e| EngineError::io(&path, e))?;
        Ok(file)
    }

    fn ref_path(&self, key: &str) -> PathBuf {
        self.root
            .join("refs")
            .join(format!("{}.json", sha256_hex(key.as_bytes())))
    }

    fn archive_path(&self, archive_sha256: &str) -> PathBuf {
        self.root
            .join("images")
            .join(format!("{archive_sha256}.tar"))
    }

    fn record_path(&self, archive_sha256: &str) -> PathBuf {
        self.root
            .join("images")
            .join(format!("{archive_sha256}.json"))
    }

    /// The cached image `key` resolved to, if its record and archive are
    /// intact: the archive is re-hashed against its recorded SHA-256, so a
    /// corrupted or replaced file is a miss. Never contacts any source.
    pub fn lookup(&self, key: &str) -> Result<Option<CachedImage>, EngineError> {
        let _guard = self.lock(false)?;
        let path = self.ref_path(key);
        let Some(reference) = read_json::<RefRecord>(&path)? else {
            return Ok(None);
        };
        if reference.version != RECORD_VERSION || reference.key != key {
            return Ok(None);
        }
        let Some(mut hit) = self.load(&reference.archive_sha256, true)? else {
            return Ok(None);
        };
        if reference.record.version != RECORD_VERSION
            || reference.record.archive_sha256 != reference.archive_sha256
            || reference.record.bytes != hit.image.bytes
            || reference.record.archive_format != hit.record.archive_format
        {
            return Ok(None);
        }
        hit.image.identity = reference.record.identity.clone();
        hit.record = reference.record;
        Ok(Some(hit))
    }

    /// The cached image stored as `archive_sha256`, if intact. With
    /// `verify_content` the archive bytes are hashed against the record.
    /// Callers hold [`Self::lock`].
    fn load(
        &self,
        archive_sha256: &str,
        verify_content: bool,
    ) -> Result<Option<CachedImage>, EngineError> {
        if archive_sha256.len() != 64 || !archive_sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Ok(None);
        }
        let Some(record) = read_json::<CacheRecord>(&self.record_path(archive_sha256))? else {
            return Ok(None);
        };
        let archive = self.archive_path(archive_sha256);
        let Ok(meta) = std::fs::metadata(&archive) else {
            return Ok(None);
        };
        if record.version != RECORD_VERSION
            || record.archive_sha256 != archive_sha256
            || meta.len() != record.bytes
        {
            return Ok(None);
        }
        if verify_content {
            match hash_file(&archive) {
                Ok((hex, len)) if hex == archive_sha256 && len == record.bytes => {}
                _ => return Ok(None),
            }
        }
        let archive_format = match record.archive_format.as_str() {
            "oci-layout" => ArchiveFormat::OciLayout,
            "docker-save" => ArchiveFormat::DockerSave,
            _ => return Ok(None),
        };
        Ok(Some(CachedImage {
            image: AcquiredImage {
                identity: record.identity.clone(),
                archive,
                archive_format,
                bytes: record.bytes,
            },
            record,
        }))
    }

    /// The record stored next to a cached archive path.
    pub fn record_for_archive(&self, archive: &Path) -> Result<Option<CacheRecord>, EngineError> {
        let Some(stem) = archive.file_stem().and_then(|s| s.to_str()) else {
            return Ok(None);
        };
        if archive.parent() != Some(self.root.join("images").as_path()) {
            return Ok(None);
        }
        let _guard = self.lock(false)?;
        Ok(self.load(stem, false)?.map(|c| c.record))
    }

    /// Move a validated, staged archive into the cache and point `key` at
    /// it. `staged` must live under this cache's `tmp/` (same filesystem).
    pub(super) fn commit(
        &self,
        staged: &Path,
        key: &str,
        identity: ImageIdentity,
        validated: &ValidatedArchive,
        locator: String,
        source_fingerprint: Option<SourceFingerprint>,
    ) -> Result<CachedImage, EngineError> {
        let (archive_sha256, bytes) = hash_file(staged)?;
        let record = CacheRecord {
            version: RECORD_VERSION,
            identity,
            archive_format: match validated.format {
                ArchiveFormat::OciLayout => "oci-layout",
                ArchiveFormat::DockerSave => "docker-save",
            }
            .to_string(),
            bytes,
            archive_sha256: archive_sha256.clone(),
            acquired_at: chrono::Utc::now().to_rfc3339(),
            locator,
            source_fingerprint,
            config: validated.config.clone(),
        };

        let _guard = self.lock(true)?;
        let archive = self.archive_path(&archive_sha256);
        std::fs::rename(staged, &archive).map_err(|e| EngineError::io(&archive, e))?;
        write_json_atomic(&self.record_path(&archive_sha256), &record)?;
        write_json_atomic(
            &self.ref_path(key),
            &RefRecord {
                version: RECORD_VERSION,
                key: key.to_string(),
                archive_sha256: archive_sha256.clone(),
                record: record.clone(),
            },
        )?;
        sync_dir(&self.root.join("images"));
        sync_dir(&self.root.join("refs"));

        self.load(&archive_sha256, false)?.ok_or_else(|| {
            EngineError::Other(format!(
                "image cache entry {archive_sha256} vanished while it was being committed"
            ))
        })
    }

    /// Remove archives whose manifest digest is not in `keep`, refs that no
    /// longer resolve, orphaned archives and stale staging directories.
    pub fn prune(&self, keep: &[Digest]) -> Result<PruneReport, EngineError> {
        let _guard = self.lock(true)?;
        let keep: BTreeSet<&str> = keep.iter().map(Digest::as_str).collect();
        let mut referenced = BTreeSet::new();
        for entry in read_dir(&self.root.join("refs"))? {
            if let Some(reference) = read_json::<RefRecord>(&entry.path())? {
                if reference.version == RECORD_VERSION
                    && keep.contains(reference.record.identity.manifest_digest.as_str())
                {
                    referenced.insert(reference.archive_sha256);
                }
            }
        }
        let mut report = PruneReport::default();
        let images = self.root.join("images");
        let mut kept = BTreeSet::new();
        for entry in read_dir(&images)? {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("tar") {
                continue;
            }
            let Some(stem) = path
                .file_stem()
                .and_then(|s| s.to_str())
                .map(str::to_string)
            else {
                continue;
            };
            let retain = referenced.contains(&stem)
                || match self.load(&stem, false)? {
                    Some(c) => keep.contains(c.record.identity.manifest_digest.as_str()),
                    None => false,
                };
            if retain {
                kept.insert(stem);
                continue;
            }
            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
            remove_file(&path)?;
            let _ = std::fs::remove_file(self.record_path(&stem));
            report.archives_removed += 1;
            report.bytes_freed += size;
        }
        // Records without an archive.
        for entry in read_dir(&images)? {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("json") {
                let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
                if !kept.contains(stem) {
                    remove_file(&path)?;
                }
            }
        }
        for entry in read_dir(&self.root.join("refs"))? {
            let path = entry.path();
            let live = read_json::<RefRecord>(&path)
                .ok()
                .flatten()
                .is_some_and(|r| kept.contains(&r.archive_sha256));
            if !live {
                remove_file(&path)?;
                report.refs_removed += 1;
            }
        }
        for entry in read_dir(&self.root.join("tmp"))? {
            let stale = entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|age| age > STALE_STAGING);
            if stale {
                let _ = std::fs::remove_dir_all(entry.path());
                report.staging_removed += 1;
            }
        }
        Ok(report)
    }
}

fn read_dir(dir: &Path) -> Result<Vec<std::fs::DirEntry>, EngineError> {
    match std::fs::read_dir(dir) {
        Ok(it) => Ok(it.flatten().collect()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(EngineError::io(dir, e)),
    }
}

fn remove_file(path: &Path) -> Result<(), EngineError> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(EngineError::io(path, e)),
    }
}

/// `Ok(None)` when absent or unparseable (a torn or foreign file is a miss,
/// not an error).
fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>, EngineError> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes).ok()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(EngineError::io(path, e)),
    }
}

/// Write `value` to a unique sibling, fsync, then rename over `path`.
fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<(), EngineError> {
    let dir = path.parent().unwrap_or(Path::new("."));
    let bytes = serde_json::to_vec_pretty(value).map_err(|e| EngineError::Other(e.to_string()))?;
    let tmp = dir.join(format!(
        ".{}.{}.tmp",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("record"),
        uuid::Uuid::new_v4().simple()
    ));
    let mut file = create_private_file(&tmp)?;
    let written = file
        .write_all(&bytes)
        .and_then(|_| file.sync_all())
        .map_err(|e| EngineError::io(&tmp, e));
    if let Err(e) =
        written.and_then(|_| std::fs::rename(&tmp, path).map_err(|e| EngineError::io(path, e)))
    {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

/// Best-effort directory fsync so a committed rename survives a crash.
fn sync_dir(dir: &Path) {
    #[cfg(unix)]
    if let Ok(d) = std::fs::File::open(dir) {
        let _ = d.sync_all();
    }
    #[cfg(not(unix))]
    let _ = dir;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::config::image_source::ImageSourceKind;
    use crate::data::oci_identity::OciPlatform;
    use crate::engine::oci::verify::sha256_digest;

    fn validated() -> ValidatedArchive {
        ValidatedArchive {
            format: ArchiveFormat::OciLayout,
            manifest_digest: sha256_digest(b"m"),
            config_digest: sha256_digest(b"c"),
            platform: OciPlatform::host_linux(),
            config: ImageConfigSummary {
                home: Some("/home/agent".into()),
                ..Default::default()
            },
            diff_ids: vec![],
        }
    }

    fn identity() -> ImageIdentity {
        ImageIdentity {
            reference: "awman-x-claude:latest".into(),
            manifest_digest: sha256_digest(b"m"),
            config_digest: sha256_digest(b"c"),
            platform: OciPlatform::host_linux(),
            source: ImageSourceKind::Archive,
        }
    }

    fn stage(cache: &OciCache, bytes: &[u8]) -> (tempfile::TempDir, PathBuf) {
        let dir = cache.staging().unwrap();
        let path = dir.path().join("archive.tar");
        std::fs::write(&path, bytes).unwrap();
        (dir, path)
    }

    #[test]
    fn commit_then_lookup_round_trips() {
        let state = tempfile::tempdir().unwrap();
        let cache = OciCache::open(state.path()).unwrap();
        assert!(cache.lookup("k").unwrap().is_none());
        let (_dir, staged) = stage(&cache, b"archive-bytes");
        let committed = cache
            .commit(
                &staged,
                "k",
                identity(),
                &validated(),
                "archive:/x".into(),
                None,
            )
            .unwrap();
        assert!(!staged.exists(), "staged file is moved, not copied");
        let hit = cache.lookup("k").unwrap().expect("hit");
        assert_eq!(hit, committed);
        assert_eq!(hit.image.bytes, 13);
        assert_eq!(hit.image.identity, identity());
        assert_eq!(hit.record.config.home.as_deref(), Some("/home/agent"));
        assert!(hit
            .image
            .archive
            .starts_with(state.path().join(CACHE_DIR).join("images")));
        assert_eq!(
            cache
                .record_for_archive(&hit.image.archive)
                .unwrap()
                .unwrap(),
            hit.record
        );
    }

    #[test]
    fn prune_waits_for_publication_of_an_in_flight_archive() {
        use std::sync::mpsc;
        use std::time::Duration;

        let state = tempfile::tempdir().unwrap();
        let cache = OciCache::open(state.path()).unwrap();
        let (_dir, staged) = stage(&cache, b"archive-bytes");
        let committed = cache
            .commit(
                &staged,
                "k",
                identity(),
                &validated(),
                "archive:/x".into(),
                None,
            )
            .unwrap();
        let record_path = cache.record_path(&committed.record.archive_sha256);
        let ref_path = cache.ref_path("k");
        let record_bytes = std::fs::read(&record_path).unwrap();
        let ref_bytes = std::fs::read(&ref_path).unwrap();
        let publication = cache.lock(true).unwrap();
        // Reproduce the state between archive rename and metadata publication.
        std::fs::remove_file(&record_path).unwrap();
        std::fs::remove_file(&ref_path).unwrap();
        let other = cache.clone();
        let (started_tx, started_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            let result = other.prune(&[identity().manifest_digest]);
            done_tx.send(result).unwrap();
        });
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let premature = done_rx.recv_timeout(Duration::from_millis(100));
        // Publish before releasing the lock, even if the regression has failed,
        // so the test never leaves a thread waiting on its own failure path.
        std::fs::write(&record_path, record_bytes).unwrap();
        std::fs::write(&ref_path, ref_bytes).unwrap();
        drop(publication);
        assert!(matches!(premature, Err(mpsc::RecvTimeoutError::Timeout)));
        let pruned = done_rx
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap();
        worker.join().unwrap();
        assert_eq!(pruned.archives_removed, 0);
        assert_eq!(cache.lookup("k").unwrap().unwrap(), committed);
    }

    #[test]
    fn shared_archive_keeps_each_references_identity_config_and_fingerprint() {
        let state = tempfile::tempdir().unwrap();
        let cache = OciCache::open(state.path()).unwrap();
        let (_d1, s1) = stage(&cache, b"multi-image-archive");
        let first = cache
            .commit(
                &s1,
                "k1",
                identity(),
                &validated(),
                "first".into(),
                Some(SourceFingerprint {
                    len: 1,
                    mtime_ns: 10,
                }),
            )
            .unwrap();
        let mut other = identity();
        other.source = ImageSourceKind::DockerStore;
        other.reference = "different:tag".into();
        other.manifest_digest = sha256_digest(b"second-selection");
        let mut config = validated();
        config.config.home = Some("/home/second".into());
        let (_d2, s2) = stage(&cache, b"multi-image-archive");
        let second = cache
            .commit(&s2, "k2", other, &config, "second".into(), None)
            .unwrap();
        assert_eq!(first.image.archive, second.image.archive);
        assert_eq!(cache.lookup("k1").unwrap().unwrap(), first);
        assert_eq!(cache.lookup("k2").unwrap().unwrap(), second);
        // Retaining the first selection must keep the shared bytes even
        // though the archive-level record names the second selection.
        assert_eq!(
            cache
                .prune(&[identity().manifest_digest])
                .unwrap()
                .archives_removed,
            0
        );
        assert_eq!(cache.lookup("k1").unwrap().unwrap(), first);
    }

    #[test]
    fn a_damaged_archive_is_a_miss_not_a_hit() {
        let state = tempfile::tempdir().unwrap();
        let cache = OciCache::open(state.path()).unwrap();
        let (_dir, staged) = stage(&cache, b"archive-bytes");
        let hit = cache
            .commit(&staged, "k", identity(), &validated(), "l".into(), None)
            .unwrap();
        std::fs::write(&hit.image.archive, b"short").unwrap();
        assert!(cache.lookup("k").unwrap().is_none());
    }

    #[test]
    fn a_same_length_corruption_is_a_miss_not_a_hit() {
        let state = tempfile::tempdir().unwrap();
        let cache = OciCache::open(state.path()).unwrap();
        let (_dir, staged) = stage(&cache, b"archive-bytes");
        let hit = cache
            .commit(&staged, "k", identity(), &validated(), "l".into(), None)
            .unwrap();
        std::fs::write(&hit.image.archive, b"ARCHIVE-BYTES").unwrap();
        assert!(cache.lookup("k").unwrap().is_none());
    }

    #[test]
    fn prune_keeps_referenced_digests_only() {
        let state = tempfile::tempdir().unwrap();
        let cache = OciCache::open(state.path()).unwrap();
        let (_d1, s1) = stage(&cache, b"one");
        cache
            .commit(&s1, "k1", identity(), &validated(), "l".into(), None)
            .unwrap();
        let mut other = identity();
        other.manifest_digest = sha256_digest(b"other");
        let (_d2, s2) = stage(&cache, b"two");
        cache
            .commit(&s2, "k2", other, &validated(), "l".into(), None)
            .unwrap();

        let report = cache.prune(&[sha256_digest(b"m")]).unwrap();
        assert_eq!(report.archives_removed, 1);
        assert_eq!(report.refs_removed, 1);
        assert!(cache.lookup("k1").unwrap().is_some());
        assert!(cache.lookup("k2").unwrap().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn cache_files_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let state = tempfile::tempdir().unwrap();
        let cache = OciCache::open(state.path()).unwrap();
        let (_dir, staged) = stage(&cache, b"x");
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o600)).unwrap();
        let hit = cache
            .commit(&staged, "k", identity(), &validated(), "l".into(), None)
            .unwrap();
        let record = cache.record_path(&hit.record.archive_sha256);
        for p in [cache.root().to_path_buf(), record] {
            let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o077;
            assert_eq!(
                mode,
                0,
                "{} must not be group/world accessible",
                p.display()
            );
        }
    }
}
