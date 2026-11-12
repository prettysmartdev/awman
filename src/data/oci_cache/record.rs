//! Serialized records of the image cache. The JSON shape is a stable on-disk
//! format: caches written by earlier releases must keep hitting.

use std::path::Path;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::data::oci_identity::ImageIdentity;

/// Version written into, and required of, every record and ref.
pub const RECORD_VERSION: u32 = 1;

/// Records and refs are small JSON documents; anything larger is foreign or
/// damaged and is a miss, read without allocating its full size.
pub const MAX_RECORD_BYTES: u64 = 1 << 20;

/// The non-secret image defaults the runtime needs before first boot.
/// Image `Env` is deliberately not kept (it may carry baked-in secrets);
/// only `HOME` is extracted from it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageConfigSummary {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub home: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<String>,
}

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
    /// The fingerprint of the file at `path`, or `None` when it cannot be
    /// read.
    pub fn of(path: &Path) -> Option<Self> {
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

/// Which archive a source+reference+platform key resolved to.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RefRecord {
    pub version: u32,
    pub key: String,
    pub archive_sha256: String,
    // The same archive can be acquired from different sources or contain
    // several selected images. Keep provenance and selection with the ref,
    // rather than borrowing whichever record last wrote the shared archive.
    // Older refs without this field are cache misses and must be revalidated.
    pub record: CacheRecord,
}
