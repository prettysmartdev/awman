//! awman's verified image-archive cache.
//!
//! Layout under `<builtin state dir>/oci-cache/` (all directories 0700, all
//! files 0600):
//!
//! ```text
//! images/<archive-sha256>.tar    the validated archive, verbatim
//! images/<archive-sha256>.json   CacheRecord: identity, format, size, source locator
//! refs/<key-sha256>.json         RefRecord: which archive a source+reference+platform resolved to
//! leases/<archive-sha256>.lock   held (shared flock) while an archive is in use
//! tmp/acq-*/                     private staging for in-flight acquisitions
//! tmp/acq-*/.lease               held (exclusive flock) by the live acquisition
//! ```
//!
//! Commit order is archive → record → ref, each by `rename` from a fully
//! written, fsynced file in the same filesystem. A reader starts from a ref,
//! so it can only ever observe a complete record whose archive already
//! exists with the recorded size. A crash leaves at worst an unreferenced
//! archive, an unrenamed `.*.tmp` record or an unlocked staging directory,
//! all removed by [`OciCache::prune`]. A failed commit removes an archive it
//! newly introduced, so an interrupted publication never leaves a partial
//! ref. `cache.lock` serialises commit and prune across processes (readers
//! take it shared), and a lookup re-hashes the archive before trusting it.
//!
//! Use after lookup is protected by leases, not by the path alone: an
//! [`ArchiveLease`] (from [`OciCache::lookup_leased`] or
//! [`OciCache::lease_archive`]) is taken under the shared cache lock and
//! prune only deletes an archive whose lease file it can lock exclusively.
//! Holders keep the lease for the whole import/materialisation interval.
//! Likewise a [`Staging`] directory from [`OciCache::staging_leased`] is
//! removed by prune only once its owner has gone (crashed or finished).
//!
//! Every lock, lease and record is opened without following symlinks and
//! must be a private single-link regular file owned by this user; records
//! are read with a size bound. These checks defeat substitution by another
//! user or a stale artefact; a hostile process running as the same user can
//! still race path checks and is outside this cache's threat model.
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
use crate::engine::oci::verify::{create_private_dir, create_private_file, sha256_hex};
use crate::engine::oci::{AcquiredImage, ArchiveFormat};

/// Directory name under the builtin state directory.
pub const CACHE_DIR: &str = "oci-cache";
const RECORD_VERSION: u32 = 1;
/// Staging directories without a lease file that are older than this belong
/// to a crashed acquisition. Leased staging is judged by its lock instead.
const STALE_STAGING: Duration = Duration::from_secs(24 * 60 * 60);
/// Records and refs are small JSON documents; anything larger is foreign or
/// damaged and is a miss, read without allocating its full size.
const MAX_RECORD_BYTES: u64 = 1 << 20;
/// The lease file inside a leased staging directory.
const STAGING_LEASE: &str = ".lease";

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
    /// Archives prune would have removed but kept because a lease holds them.
    pub archives_in_use: usize,
    /// Unrenamed `.*.tmp` records left by an interrupted commit.
    pub interrupted_writes_removed: usize,
}

/// A cached archive held in use. While any lease on an archive is alive,
/// [`OciCache::prune`] keeps that archive, its record and its refs. Dropping
/// the lease (or the holder's process exiting) releases it.
#[derive(Debug)]
pub struct ArchiveLease {
    archive: PathBuf,
    archive_sha256: String,
    _file: std::fs::File,
}

impl ArchiveLease {
    /// The leased archive, valid for as long as the lease lives.
    pub fn archive(&self) -> &Path {
        &self.archive
    }

    pub fn archive_sha256(&self) -> &str {
        &self.archive_sha256
    }
}

/// A private staging directory under the cache's `tmp/`, removed when
/// dropped. Its `.lease` file is locked for the directory's lifetime, so a
/// concurrent [`OciCache::prune`] never removes a live acquisition's bytes
/// but does reclaim the directory of one that crashed.
#[derive(Debug)]
pub struct Staging {
    // Field order: unlock (close) only after the directory is gone.
    dir: tempfile::TempDir,
    _lease: std::fs::File,
}

impl Staging {
    pub fn path(&self) -> &Path {
        self.dir.path()
    }
}

/// The cache rooted at `<state_dir>/oci-cache`.
#[derive(Debug, Clone)]
pub struct OciCache {
    root: PathBuf,
    control: super::retry::OperationControl,
}

impl OciCache {
    /// Open (creating, owner-only) the cache under `state_dir`.
    pub fn open(state_dir: &Path) -> Result<Self, EngineError> {
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
        Ok(Self {
            root,
            control: Default::default(),
        })
    }

    pub(crate) fn controlled(mut self, control: super::retry::OperationControl) -> Self {
        self.control = control;
        self
    }

    /// A leased private staging directory; see [`Staging`]. Created under
    /// the shared cache lock so prune never sees it between creation and
    /// locking.
    pub fn staging_leased(&self) -> Result<Staging, EngineError> {
        let _guard = self.lock(false)?;
        let dir = self.staging()?;
        let path = dir.path().join(STAGING_LEASE);
        let file = open_private_lock(&path, true)?.ok_or_else(|| {
            EngineError::Other(format!("staging lease {} vanished", path.display()))
        })?;
        file.try_lock().map_err(|e| match e {
            std::fs::TryLockError::Error(e) => EngineError::io(&path, e),
            std::fs::TryLockError::WouldBlock => {
                EngineError::Other(format!("staging lease {} is already held", path.display()))
            }
        })?;
        Ok(Staging { dir, _lease: file })
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
        let file = open_private_lock(&path, true)?.ok_or_else(|| {
            EngineError::Other(format!("image cache lock {} vanished", path.display()))
        })?;
        loop {
            self.control.check()?;
            let locked = if exclusive {
                file.try_lock()
            } else {
                file.try_lock_shared()
            };
            match locked {
                Ok(()) => break,
                Err(std::fs::TryLockError::Error(e)) => return Err(EngineError::io(&path, e)),
                Err(std::fs::TryLockError::WouldBlock) => {
                    std::thread::sleep(std::time::Duration::from_millis(20))
                }
            }
        }
        self.control.check()?;
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

    fn lease_path(&self, archive_sha256: &str) -> PathBuf {
        self.root
            .join("leases")
            .join(format!("{archive_sha256}.lock"))
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
        self.lookup_locked(key)
    }

    /// [`Self::lookup`] plus a lease taken before the cache lock is released,
    /// so no prune can remove the archive between the hit and its use.
    pub fn lookup_leased(
        &self,
        key: &str,
    ) -> Result<Option<(CachedImage, ArchiveLease)>, EngineError> {
        let _guard = self.lock(false)?;
        let Some(hit) = self.lookup_locked(key)? else {
            return Ok(None);
        };
        let lease = self.lease_locked(&hit.record.archive_sha256)?;
        Ok(Some((hit, lease)))
    }

    /// Lease the cached archive at `archive` (an [`AcquiredImage::archive`]
    /// path). `Ok(None)` when it is not (or no longer) an intact cache
    /// entry: the caller must acquire again rather than use the path.
    pub fn lease_archive(&self, archive: &Path) -> Result<Option<ArchiveLease>, EngineError> {
        let Some(stem) = archive.file_stem().and_then(|s| s.to_str()) else {
            return Ok(None);
        };
        if archive.parent() != Some(self.root.join("images").as_path())
            || archive.extension().and_then(|e| e.to_str()) != Some("tar")
        {
            return Ok(None);
        }
        let _guard = self.lock(false)?;
        if self.load(stem, false)?.is_none() {
            return Ok(None);
        }
        self.lease_locked(stem).map(Some)
    }

    /// Callers hold the cache lock (shared suffices): prune, which deletes
    /// lease files, holds it exclusively.
    fn lease_locked(&self, archive_sha256: &str) -> Result<ArchiveLease, EngineError> {
        let path = self.lease_path(archive_sha256);
        let file = open_private_lock(&path, true)?
            .ok_or_else(|| EngineError::Other(format!("lease {} vanished", path.display())))?;
        file.lock_shared().map_err(|e| EngineError::io(&path, e))?;
        Ok(ArchiveLease {
            archive: self.archive_path(archive_sha256),
            archive_sha256: archive_sha256.to_string(),
            _file: file,
        })
    }

    fn lookup_locked(&self, key: &str) -> Result<Option<CachedImage>, EngineError> {
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
        // A symlink (or anything but a plain file) in place of an archive is
        // a miss, never followed.
        let Ok(meta) = std::fs::symlink_metadata(&archive) else {
            return Ok(None);
        };
        if !meta.file_type().is_file()
            || record.version != RECORD_VERSION
            || record.archive_sha256 != archive_sha256
            || meta.len() != record.bytes
        {
            return Ok(None);
        }
        if verify_content {
            let result = super::verify::hash_file_controlled(&archive, &self.control);
            self.control.check()?;
            match result {
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
    #[cfg(test)]
    pub(super) fn commit(
        &self,
        staged: &Path,
        key: &str,
        identity: ImageIdentity,
        validated: &ValidatedArchive,
        locator: String,
        source_fingerprint: Option<SourceFingerprint>,
    ) -> Result<CachedImage, EngineError> {
        let meta = (identity, validated, locator, source_fingerprint);
        self.publish(staged, key, meta, false).map(|(hit, _)| hit)
    }

    /// Commit while leasing the published archive before the
    /// exclusive lock is released, for callers that import it next.
    pub(super) fn commit_leased(
        &self,
        staged: &Path,
        key: &str,
        identity: ImageIdentity,
        validated: &ValidatedArchive,
        locator: String,
        source_fingerprint: Option<SourceFingerprint>,
    ) -> Result<(CachedImage, ArchiveLease), EngineError> {
        let meta = (identity, validated, locator, source_fingerprint);
        let (hit, lease) = self.publish(staged, key, meta, true)?;
        let lease = lease.ok_or_else(|| EngineError::Other("image cache lease missing".into()))?;
        Ok((hit, lease))
    }

    fn publish(
        &self,
        staged: &Path,
        key: &str,
        (identity, validated, locator, source_fingerprint): (
            ImageIdentity,
            &ValidatedArchive,
            String,
            Option<SourceFingerprint>,
        ),
        lease: bool,
    ) -> Result<(CachedImage, Option<ArchiveLease>), EngineError> {
        let (archive_sha256, bytes) = super::verify::hash_file_controlled(staged, &self.control)?;
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
        let record_path = self.record_path(&archive_sha256);
        // Bytes another ref already publishes are shared and must survive a
        // failure here; bytes this commit introduced must not linger as an
        // unreferenced partial entry.
        let introduced = std::fs::symlink_metadata(&archive).is_err();
        let published = (|| {
            std::fs::rename(staged, &archive).map_err(|e| EngineError::io(&archive, e))?;
            fault::check(fault::Stage::ArchivePublished, &archive)?;
            write_json_atomic(&record_path, &record)?;
            fault::check(fault::Stage::RecordPublished, &record_path)?;
            self.control.check()?;
            write_json_atomic_controlled(
                &self.ref_path(key),
                &RefRecord {
                    version: RECORD_VERSION,
                    key: key.to_string(),
                    archive_sha256: archive_sha256.clone(),
                    record: record.clone(),
                },
                &self.control,
            )
        })();
        if let Err(e) = published {
            if introduced {
                let _ = std::fs::remove_file(&record_path);
                let _ = std::fs::remove_file(&archive);
            }
            return Err(e);
        }
        sync_dir(&self.root.join("images"));
        sync_dir(&self.root.join("refs"));

        let hit = self.load(&archive_sha256, false)?.ok_or_else(|| {
            EngineError::Other(format!(
                "image cache entry {archive_sha256} vanished while it was being committed"
            ))
        })?;
        let lease = if lease {
            Some(self.lease_locked(&archive_sha256)?)
        } else {
            None
        };
        Ok((hit, lease))
    }

    /// Remove archives whose manifest digest is not in `keep`, refs that no
    /// longer resolve, orphaned archives, interrupted record writes and the
    /// staging of acquisitions that are no longer running. Archives held by
    /// an [`ArchiveLease`] and staging held by a live [`Staging`] are kept.
    pub fn prune(&self, keep: &[Digest]) -> Result<PruneReport, EngineError> {
        let _guard = self.lock(true)?;
        let mut report = PruneReport::default();
        // Commits hold the exclusive lock while writing, so any `.*.tmp`
        // left now is from a process that died mid-write.
        for dir in ["images", "refs"] {
            for entry in read_dir(&self.root.join(dir))? {
                let path = entry.path();
                if is_interrupted_write(&path) {
                    remove_file(&path)?;
                    report.interrupted_writes_removed += 1;
                }
            }
        }
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
            let Some(reclaimed) = self.reclaim_lease(&stem)? else {
                report.archives_in_use += 1;
                kept.insert(stem);
                continue;
            };
            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
            remove_file(&path)?;
            let _ = std::fs::remove_file(self.record_path(&stem));
            // No one can open the lease file without the cache lock, which
            // this prune holds exclusively, so removing it cannot strand a
            // future lease on an unlinked inode.
            if reclaimed.is_some() {
                remove_file(&self.lease_path(&stem))?;
            }
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
        // Leases whose archive is gone and no one holds.
        for entry in read_dir(&self.root.join("leases"))? {
            let path = entry.path();
            let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
            if !kept.contains(stem) && self.reclaim_lease(stem)?.is_some() {
                remove_file(&path)?;
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
            if staging_is_abandoned(&entry)? {
                let path = entry.path();
                let removed = match entry.file_type() {
                    Ok(t) if t.is_dir() => std::fs::remove_dir_all(&path),
                    _ => std::fs::remove_file(&path),
                };
                if removed.is_ok() {
                    report.staging_removed += 1;
                }
            }
        }
        Ok(report)
    }

    /// Callers hold the exclusive cache lock. `Ok(None)` when a lease on
    /// `archive_sha256` is held; otherwise the (possibly absent) lease file,
    /// locked exclusively so it cannot be taken while it is removed.
    fn reclaim_lease(
        &self,
        archive_sha256: &str,
    ) -> Result<Option<Option<std::fs::File>>, EngineError> {
        let path = self.lease_path(archive_sha256);
        let Some(file) = open_private_lock(&path, false)? else {
            return Ok(Some(None));
        };
        match file.try_lock() {
            Ok(()) => Ok(Some(Some(file))),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(std::fs::TryLockError::Error(e)) => Err(EngineError::io(&path, e)),
        }
    }
}

/// `.<name>.<uuid>.tmp`, as written by [`write_json_atomic`].
fn is_interrupted_write(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with('.') && n.ends_with(".tmp"))
}

/// A staging entry prune may remove: a leased directory whose lease no one
/// holds (its acquisition ended or crashed), or an unleased entry older than
/// [`STALE_STAGING`]. Callers hold the exclusive cache lock.
fn staging_is_abandoned(entry: &std::fs::DirEntry) -> Result<bool, EngineError> {
    let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
    if is_dir {
        if let Some(lease) = open_private_lock(&entry.path().join(STAGING_LEASE), false)? {
            return match lease.try_lock() {
                Ok(()) => Ok(true),
                Err(std::fs::TryLockError::WouldBlock) => Ok(false),
                Err(std::fs::TryLockError::Error(e)) => Err(EngineError::io(entry.path(), e)),
            };
        }
    }
    Ok(entry
        .metadata()
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|age| age > STALE_STAGING))
}

/// Open (optionally creating) a lock or lease file without following a
/// symlink. It must be a single-link, owner-only regular file owned by this
/// user, so a planted symlink, hardlink or foreign file is refused rather
/// than locked (and later removed). `Ok(None)` when absent and not created.
fn open_private_lock(path: &Path, create: bool) -> Result<Option<std::fs::File>, EngineError> {
    let mut options = std::fs::OpenOptions::new();
    options
        .read(true)
        .write(true)
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
            return Err(EngineError::Config(format!(
                "image cache lock {} cannot be opened safely (symlinks are refused): {e}",
                path.display()
            )))
        }
    };
    let meta = file.metadata().map_err(|e| EngineError::io(path, e))?;
    let refused = |why: &str| {
        Err(EngineError::Config(format!(
            "image cache lock {} {why}; remove it and retry",
            path.display()
        )))
    };
    if !meta.is_file() {
        return refused("is not a regular file");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if meta.nlink() != 1 {
            return refused("has other hard links");
        }
        if meta.uid() != nix::unistd::geteuid().as_raw() {
            return refused("is owned by another user");
        }
        if meta.mode() & 0o077 != 0 {
            return refused("is accessible to other users");
        }
    }
    Ok(Some(file))
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
/// not an error). A symlink, a non-regular file or a record larger than
/// [`MAX_RECORD_BYTES`] is also a miss; it is neither followed nor read in
/// full.
fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>, EngineError> {
    use std::io::Read;
    let mut options = std::fs::OpenOptions::new();
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
        Err(e) => return Err(EngineError::io(path, e)),
    };
    if !file.metadata().is_ok_and(|m| m.is_file()) {
        return Ok(None);
    }
    let mut bytes = Vec::new();
    file.take(MAX_RECORD_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| EngineError::io(path, e))?;
    if bytes.len() as u64 > MAX_RECORD_BYTES {
        return Ok(None);
    }
    Ok(serde_json::from_slice(&bytes).ok())
}

/// Write `value` to a unique sibling, fsync, then rename over `path`.
fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<(), EngineError> {
    write_json_atomic_controlled(path, value, &Default::default())
}

fn write_json_atomic_controlled<T: Serialize>(
    path: &Path,
    value: &T,
    control: &super::retry::OperationControl,
) -> Result<(), EngineError> {
    control.check()?;
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
    if let Err(e) = written.and_then(|_| {
        control.check()?;
        std::fs::rename(&tmp, path).map_err(|e| EngineError::io(path, e))
    }) {
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

/// Commit fault injection for the interrupted-publication and ENOSPC tests.
/// Outside `cfg(test)` every check is a no-op the optimiser removes.
mod fault {
    use super::*;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) enum Stage {
        ArchivePublished,
        RecordPublished,
    }

    #[cfg(test)]
    thread_local! {
        pub(super) static FAIL_AFTER: std::cell::Cell<Option<Stage>> =
            const { std::cell::Cell::new(None) };
    }

    #[cfg(test)]
    pub(super) fn check(stage: Stage, path: &Path) -> Result<(), EngineError> {
        if FAIL_AFTER.with(|f| f.get()) == Some(stage) {
            return Err(EngineError::io(
                path,
                std::io::Error::from(std::io::ErrorKind::StorageFull),
            ));
        }
        Ok(())
    }

    #[cfg(not(test))]
    #[inline(always)]
    pub(super) fn check(_: Stage, _: &Path) -> Result<(), EngineError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::config::image_source::ImageSourceKind;
    use crate::data::oci_identity::OciPlatform;
    use crate::engine::oci::verify::sha256_digest;

    #[test]
    fn acquisition_budget_expires_while_waiting_for_cache_lock() {
        use crate::engine::oci::retry::{CancelToken, Deadline, OperationControl};
        let dir = tempfile::tempdir().unwrap();
        let cache = OciCache::open(dir.path()).unwrap();
        let held = cache.lock(true).unwrap();
        let bounded = cache.clone().controlled(OperationControl::new(
            CancelToken::new(),
            Deadline::start(std::time::Duration::from_millis(20)),
        ));
        let err = bounded.lookup_leased("missing").unwrap_err();
        assert!(err.to_string().contains("deadline"), "{err}");
        drop(held);
        assert!(cache.lookup_leased("missing").unwrap().is_none());
    }

    #[test]
    fn cancelled_reference_write_keeps_previous_reference() {
        use crate::engine::oci::retry::{CancelToken, Deadline, OperationControl};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ref.json");
        write_json_atomic(&path, &"previous").unwrap();
        let token = CancelToken::new();
        token.cancel();
        let control =
            OperationControl::new(token, Deadline::start(std::time::Duration::from_secs(1)));
        assert!(write_json_atomic_controlled(&path, &"new", &control).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "\"previous\"");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

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

    fn commit(cache: &OciCache, bytes: &[u8], key: &str, identity: ImageIdentity) -> CachedImage {
        let (_dir, staged) = stage(cache, bytes);
        cache
            .commit(&staged, key, identity, &validated(), "l".into(), None)
            .unwrap()
    }

    fn other_identity(name: &[u8]) -> ImageIdentity {
        let mut other = identity();
        other.manifest_digest = sha256_digest(name);
        other
    }

    fn entries(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    struct FailAfter;
    impl FailAfter {
        fn set(stage: fault::Stage) -> Self {
            fault::FAIL_AFTER.with(|f| f.set(Some(stage)));
            FailAfter
        }
    }
    impl Drop for FailAfter {
        fn drop(&mut self) {
            fault::FAIL_AFTER.with(|f| f.set(None));
        }
    }

    #[test]
    fn a_leased_archive_survives_prune_until_the_lease_is_dropped() {
        let state = tempfile::tempdir().unwrap();
        let cache = OciCache::open(state.path()).unwrap();
        let hit = commit(&cache, b"leased", "k", identity());
        let (leased, lease) = cache.lookup_leased("k").unwrap().expect("hit");
        assert_eq!(leased, hit);
        assert_eq!(lease.archive(), hit.image.archive);
        assert_eq!(lease.archive_sha256(), hit.record.archive_sha256);

        // Keep nothing: without the lease the archive would go.
        let report = cache.prune(&[]).unwrap();
        assert_eq!(report.archives_removed, 0);
        assert_eq!(report.archives_in_use, 1);
        assert_eq!(report.refs_removed, 0, "refs of an in-use archive stay");
        assert_eq!(
            super::super::verify::hash_file(lease.archive()).unwrap().0,
            hit.record.archive_sha256
        );
        assert_eq!(cache.lookup("k").unwrap().unwrap(), hit);

        drop(lease);
        let report = cache.prune(&[]).unwrap();
        assert_eq!(report.archives_removed, 1);
        assert_eq!(report.archives_in_use, 0);
        assert!(!hit.image.archive.exists());
        assert!(entries(&cache.root().join("leases")).is_empty());
        assert!(cache.lookup("k").unwrap().is_none());
    }

    #[test]
    fn a_lease_by_path_only_covers_intact_cache_entries() {
        let state = tempfile::tempdir().unwrap();
        let cache = OciCache::open(state.path()).unwrap();
        let hit = commit(&cache, b"by-path", "k", identity());
        let lease = cache.lease_archive(&hit.image.archive).unwrap().unwrap();
        assert_eq!(cache.prune(&[]).unwrap().archives_in_use, 1);
        drop(lease);

        let outside = state.path().join("elsewhere.tar");
        std::fs::write(&outside, b"x").unwrap();
        assert!(cache.lease_archive(&outside).unwrap().is_none());
        cache.prune(&[]).unwrap();
        // Pruned between acquisition and use: the caller must acquire again.
        assert!(cache.lease_archive(&hit.image.archive).unwrap().is_none());
    }

    #[test]
    fn two_leases_share_an_archive_and_both_must_go() {
        let state = tempfile::tempdir().unwrap();
        let cache = OciCache::open(state.path()).unwrap();
        commit(&cache, b"shared", "k", identity());
        let (_, first) = cache.lookup_leased("k").unwrap().unwrap();
        let (_, second) = cache.lookup_leased("k").unwrap().unwrap();
        drop(first);
        assert_eq!(cache.prune(&[]).unwrap().archives_in_use, 1);
        drop(second);
        assert_eq!(cache.prune(&[]).unwrap().archives_removed, 1);
    }

    #[test]
    fn commit_leased_publishes_and_holds_the_archive() {
        let state = tempfile::tempdir().unwrap();
        let cache = OciCache::open(state.path()).unwrap();
        let (_dir, staged) = stage(&cache, b"leased-commit");
        let (hit, lease) = cache
            .commit_leased(&staged, "k", identity(), &validated(), "l".into(), None)
            .unwrap();
        assert_eq!(lease.archive(), hit.image.archive);
        assert_eq!(cache.prune(&[]).unwrap().archives_in_use, 1);
        drop(lease);
        assert_eq!(cache.prune(&[]).unwrap().archives_removed, 1);
    }

    #[test]
    fn enospc_after_the_archive_rename_leaves_no_partial_entry() {
        let state = tempfile::tempdir().unwrap();
        let cache = OciCache::open(state.path()).unwrap();
        let (_dir, staged) = stage(&cache, b"fresh-bytes");
        let failed = {
            let _fault = FailAfter::set(fault::Stage::ArchivePublished);
            cache.commit(&staged, "k", identity(), &validated(), "l".into(), None)
        };
        let err = failed.unwrap_err().to_string();
        assert!(err.contains("no storage space"), "{err}");
        assert!(cache.lookup("k").unwrap().is_none());
        assert!(
            entries(&cache.root().join("images")).is_empty(),
            "no orphan archive"
        );
        assert!(
            entries(&cache.root().join("refs")).is_empty(),
            "no partial ref"
        );
        // The next attempt publishes normally.
        let hit = commit(&cache, b"fresh-bytes", "k", identity());
        assert_eq!(cache.lookup("k").unwrap().unwrap(), hit);
    }

    #[test]
    fn an_interrupted_commit_of_shared_bytes_keeps_the_other_reference() {
        let state = tempfile::tempdir().unwrap();
        let cache = OciCache::open(state.path()).unwrap();
        let first = commit(&cache, b"shared-bytes", "k1", identity());
        let (_dir, staged) = stage(&cache, b"shared-bytes");
        let failed = {
            let _fault = FailAfter::set(fault::Stage::RecordPublished);
            cache.commit(
                &staged,
                "k2",
                other_identity(b"second"),
                &validated(),
                "l".into(),
                None,
            )
        };
        assert!(failed.is_err());
        assert!(cache.lookup("k2").unwrap().is_none(), "no partial ref");
        assert_eq!(cache.lookup("k1").unwrap().unwrap().image, first.image);
        assert!(
            first.image.archive.is_file(),
            "shared bytes are not deleted"
        );
    }

    #[test]
    fn prune_recovers_what_a_crashed_commit_and_acquisition_left_behind() {
        let state = tempfile::tempdir().unwrap();
        let cache = OciCache::open(state.path()).unwrap();
        let live = commit(&cache, b"live", "k", identity());
        let images = cache.root().join("images");
        // A process died after the archive rename and mid record/ref write.
        std::fs::write(images.join(format!("{}.tar", "a".repeat(64))), b"orphan").unwrap();
        std::fs::write(images.join(".x.json.0123.tmp"), b"{\"torn").unwrap();
        std::fs::write(cache.root().join("refs/.y.json.0123.tmp"), b"{}").unwrap();
        // One acquisition died (its lease file remains, unlocked); another
        // is still running.
        let crashed_path = cache.root().join("tmp/acq-crashed");
        std::fs::create_dir(&crashed_path).unwrap();
        std::fs::write(crashed_path.join("archive.tar"), b"partial").unwrap();
        drop(open_private_lock(&crashed_path.join(STAGING_LEASE), true).unwrap());
        let running = cache.staging_leased().unwrap();

        let report = cache.prune(&[identity().manifest_digest]).unwrap();
        assert_eq!(report.interrupted_writes_removed, 2);
        assert_eq!(report.archives_removed, 1, "the orphan archive");
        assert_eq!(report.staging_removed, 1);
        assert!(
            !crashed_path.exists(),
            "the crashed acquisition's staging is reclaimed"
        );
        assert!(
            running.path().is_dir(),
            "a live acquisition's staging is kept"
        );
        assert_eq!(cache.lookup("k").unwrap().unwrap(), live);
        drop(running);
        assert_eq!(
            cache.prune(&[]).unwrap().staging_removed,
            0,
            "dropped staging removes itself"
        );
    }

    #[cfg(unix)]
    #[test]
    fn substituted_locks_and_leases_are_refused_not_followed() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let state = tempfile::tempdir().unwrap();
        let cache = OciCache::open(state.path()).unwrap();
        let hit = commit(&cache, b"x", "k", identity());
        let lock = cache.root().join("cache.lock");
        let target = state.path().join("target");
        std::fs::write(&target, b"").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();

        std::fs::remove_file(&lock).unwrap();
        symlink(&target, &lock).unwrap();
        let err = cache.lookup("k").unwrap_err().to_string();
        assert!(err.contains("symlinks are refused"), "{err}");
        std::fs::remove_file(&lock).unwrap();

        std::fs::hard_link(&target, &lock).unwrap();
        let err = cache.prune(&[]).unwrap_err().to_string();
        assert!(err.contains("hard links"), "{err}");
        std::fs::remove_file(&lock).unwrap();

        std::fs::write(&lock, b"").unwrap();
        std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o644)).unwrap();
        let err = cache.lookup("k").unwrap_err().to_string();
        assert!(err.contains("other users"), "{err}");
        std::fs::remove_file(&lock).unwrap();

        // A planted lease symlink is neither locked nor deleted through.
        let lease = cache.lease_path(&hit.record.archive_sha256);
        symlink(&target, &lease).unwrap();
        assert!(cache.lookup_leased("k").is_err());
        assert!(cache.prune(&[]).is_err(), "prune fails closed");
        assert!(target.exists());
        assert!(hit.image.archive.exists());
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_or_oversized_entries_are_misses() {
        use std::os::unix::fs::symlink;
        let state = tempfile::tempdir().unwrap();
        let cache = OciCache::open(state.path()).unwrap();
        let hit = commit(&cache, b"archive", "k", identity());
        let outside = state.path().join("outside.tar");
        std::fs::copy(&hit.image.archive, &outside).unwrap();
        std::fs::remove_file(&hit.image.archive).unwrap();
        symlink(&outside, &hit.image.archive).unwrap();
        assert!(cache.lookup("k").unwrap().is_none(), "symlinked archive");
        assert!(cache.lease_archive(&hit.image.archive).unwrap().is_none());
        std::fs::remove_file(&hit.image.archive).unwrap();
        std::fs::copy(&outside, &hit.image.archive).unwrap();
        assert!(cache.lookup("k").unwrap().is_some());

        let ref_path = cache.ref_path("k");
        let original = std::fs::read(&ref_path).unwrap();
        let moved = state.path().join("ref.json");
        std::fs::write(&moved, &original).unwrap();
        std::fs::remove_file(&ref_path).unwrap();
        symlink(&moved, &ref_path).unwrap();
        assert!(cache.lookup("k").unwrap().is_none(), "symlinked ref");
        std::fs::remove_file(&ref_path).unwrap();

        let mut padded = original.clone();
        padded.truncate(padded.len() - 1);
        padded.extend(std::iter::repeat_n(b' ', MAX_RECORD_BYTES as usize));
        padded.push(b'}');
        std::fs::write(&ref_path, &padded).unwrap();
        assert!(cache.lookup("k").unwrap().is_none(), "oversized ref");
        std::fs::write(&ref_path, &original).unwrap();
        assert!(cache.lookup("k").unwrap().is_some());
    }

    #[cfg(unix)]
    #[test]
    fn a_substituted_cache_directory_is_refused_at_open() {
        use std::os::unix::fs::symlink;
        let state = tempfile::tempdir().unwrap();
        OciCache::open(state.path()).unwrap();
        let refs = state.path().join(CACHE_DIR).join("refs");
        std::fs::remove_dir(&refs).unwrap();
        let elsewhere = state.path().join("elsewhere");
        std::fs::create_dir(&elsewhere).unwrap();
        symlink(&elsewhere, &refs).unwrap();
        let err = OciCache::open(state.path()).unwrap_err().to_string();
        assert!(err.contains("symlinks are refused"), "{err}");
    }
}
