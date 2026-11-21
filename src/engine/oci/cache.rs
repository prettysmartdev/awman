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
//!
//! The layout, record types and private-file primitives live in Layer 0
//! (`data::oci_cache`); this module is the acquisition-facing policy over
//! them: leases, commit order, verification on lookup and pruning.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::data::error::DataError;
use crate::data::oci_cache::{
    CacheRecord, OciCacheStore, RefRecord, SourceFingerprint, RECORD_VERSION,
};
use crate::data::oci_identity::{Digest, ImageIdentity};
use crate::engine::error::EngineError;
use crate::engine::oci::archive::ValidatedArchive;
use crate::engine::oci::{AcquiredImage, ArchiveFormat};

pub use crate::data::oci_cache::CACHE_DIR;

/// Staging directories without a lease file that are older than this belong
/// to a crashed acquisition. Leased staging is judged by its lock instead.
const STALE_STAGING: Duration = Duration::from_secs(24 * 60 * 60);

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

/// The acquisition-facing cache: policy over the Layer 0 [`OciCacheStore`].
#[derive(Debug, Clone)]
pub struct OciCache {
    store: OciCacheStore,
    control: super::retry::OperationControl,
}

impl OciCache {
    /// Non-secret defaults recorded for a cached archive.
    pub fn image_config(
        &self,
        archive: &Path,
    ) -> Result<Option<super::ImageConfigSummary>, EngineError> {
        Ok(self
            .record_for_archive(archive)?
            .map(|record| record.config))
    }

    /// Open (creating, owner-only) the cache under `state_dir`.
    pub fn open(state_dir: &Path) -> Result<Self, EngineError> {
        Ok(Self {
            store: OciCacheStore::open(state_dir)?,
            control: Default::default(),
        })
    }

    pub(crate) fn controlled(mut self, control: super::retry::OperationControl) -> Self {
        self.control = control;
        self
    }

    /// Run a Layer 0 operation that takes a `check` hook. The hook polls this
    /// cache's cancellation and deadline; the engine error that stopped it is
    /// returned as-is rather than flattened into a `DataError`.
    fn with_control<T>(
        &self,
        op: impl FnOnce(&mut dyn FnMut() -> Result<(), DataError>) -> Result<T, DataError>,
    ) -> Result<T, EngineError> {
        crate::engine::error::run_checked(|| self.control.check(), op)
    }

    /// A leased private staging directory; see [`Staging`]. Created under
    /// the shared cache lock so prune never sees it between creation and
    /// locking.
    pub fn staging_leased(&self) -> Result<Staging, EngineError> {
        let _guard = self.lock(false)?;
        let dir = self.staging()?;
        let file = self.store.lock_staging_lease(dir.path())?;
        Ok(Staging { dir, _lease: file })
    }

    pub fn root(&self) -> &Path {
        self.store.root()
    }

    /// A fresh private staging directory, removed when dropped.
    pub(super) fn staging(&self) -> Result<tempfile::TempDir, EngineError> {
        Ok(self.store.new_staging_dir()?)
    }

    /// The cache's cross-process lock: exclusive for `commit`/`prune`,
    /// shared for readers, so pruning can never observe (and delete) an
    /// archive whose record or ref is still being written. Released on drop.
    fn lock(&self, exclusive: bool) -> Result<std::fs::File, EngineError> {
        self.with_control(|check| self.store.lock(exclusive, check))
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
        let Some(stem) = self.store.archive_stem(archive) else {
            return Ok(None);
        };
        let _guard = self.lock(false)?;
        if self.load(&stem, false)?.is_none() {
            return Ok(None);
        }
        self.lease_locked(&stem).map(Some)
    }

    /// Callers hold the cache lock (shared suffices): prune, which deletes
    /// lease files, holds it exclusively.
    fn lease_locked(&self, archive_sha256: &str) -> Result<ArchiveLease, EngineError> {
        Ok(ArchiveLease {
            archive: self.store.archive_path(archive_sha256),
            archive_sha256: archive_sha256.to_string(),
            _file: self.store.lease_shared(archive_sha256)?,
        })
    }

    fn lookup_locked(&self, key: &str) -> Result<Option<CachedImage>, EngineError> {
        let Some(reference) = self.store.read_ref(key)? else {
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
        let Some(record) = self.store.read_record(archive_sha256)? else {
            return Ok(None);
        };
        let archive = self.store.archive_path(archive_sha256);
        // A symlink (or anything but a plain file) in place of an archive is
        // a miss, never followed.
        let Some(len) = self.store.archive_len(archive_sha256) else {
            return Ok(None);
        };
        if record.version != RECORD_VERSION
            || record.archive_sha256 != archive_sha256
            || len != record.bytes
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
        let Some(stem) = self.store.image_stem(archive) else {
            return Ok(None);
        };
        let _guard = self.lock(false)?;
        Ok(self.load(&stem, false)?.map(|c| c.record))
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
        let archive = self.store.archive_path(&archive_sha256);
        let record_path = self.store.record_path(&archive_sha256);
        // Bytes another ref already publishes are shared and must survive a
        // failure here; bytes this commit introduced must not linger as an
        // unreferenced partial entry.
        let introduced = !self.store.archive_exists(&archive_sha256);
        let published = (|| {
            self.store.adopt_archive(staged, &archive_sha256)?;
            fault::check(fault::Stage::ArchivePublished, &archive)?;
            self.store
                .write_record(&archive_sha256, &record, || Ok(()))?;
            fault::check(fault::Stage::RecordPublished, &record_path)?;
            self.control.check()?;
            self.with_control(|check| {
                self.store.write_ref(
                    key,
                    &RefRecord {
                        version: RECORD_VERSION,
                        key: key.to_string(),
                        archive_sha256: archive_sha256.clone(),
                        record: record.clone(),
                    },
                    check,
                )
            })
        })();
        if let Err(e) = published {
            if introduced {
                self.store.discard_entry(&archive_sha256);
            }
            return Err(e);
        }
        self.store.sync_committed();

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
        let store = &self.store;
        let mut report = PruneReport::default();
        // Commits hold the exclusive lock while writing, so any `.*.tmp`
        // left now is from a process that died mid-write.
        for dir in [store.images_dir(), store.refs_dir()] {
            for entry in store.read_dir(&dir)? {
                let path = entry.path();
                if store.is_interrupted_write(&path) {
                    store.remove_file(&path)?;
                    report.interrupted_writes_removed += 1;
                }
            }
        }
        let keep: BTreeSet<&str> = keep.iter().map(Digest::as_str).collect();
        let mut referenced = BTreeSet::new();
        for entry in store.read_dir(&store.refs_dir())? {
            if let Some(reference) = store.read_ref_at(&entry.path())? {
                if reference.version == RECORD_VERSION
                    && keep.contains(reference.record.identity.manifest_digest.as_str())
                {
                    referenced.insert(reference.archive_sha256);
                }
            }
        }
        let images = store.images_dir();
        let mut kept = BTreeSet::new();
        for entry in store.read_dir(&images)? {
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
            let Some(reclaimed) = store.reclaim_lease(&stem)? else {
                report.archives_in_use += 1;
                kept.insert(stem);
                continue;
            };
            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
            store.remove_file(&path)?;
            store.discard_record(&stem);
            // No one can open the lease file without the cache lock, which
            // this prune holds exclusively, so removing it cannot strand a
            // future lease on an unlinked inode.
            if reclaimed.is_some() {
                store.remove_file(&store.lease_path(&stem))?;
            }
            report.archives_removed += 1;
            report.bytes_freed += size;
        }
        // Records without an archive.
        for entry in store.read_dir(&images)? {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("json") {
                let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
                if !kept.contains(stem) {
                    store.remove_file(&path)?;
                }
            }
        }
        // Leases whose archive is gone and no one holds.
        for entry in store.read_dir(&store.leases_dir())? {
            let path = entry.path();
            let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
            if !kept.contains(stem) && store.reclaim_lease(stem)?.is_some() {
                store.remove_file(&path)?;
            }
        }
        for entry in store.read_dir(&store.refs_dir())? {
            let path = entry.path();
            let live = store
                .read_ref_at(&path)
                .ok()
                .flatten()
                .is_some_and(|r| kept.contains(&r.archive_sha256));
            if !live {
                store.remove_file(&path)?;
                report.refs_removed += 1;
            }
        }
        for entry in store.read_dir(&store.tmp_dir())? {
            if store.staging_is_abandoned(&entry, STALE_STAGING)?
                && store.remove_staging_entry(&entry)
            {
                report.staging_removed += 1;
            }
        }
        Ok(report)
    }
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

    #[inline(always)]
    pub(super) fn check(_: Stage, _: &Path) -> Result<(), EngineError> {
        Ok(())
    }
}
