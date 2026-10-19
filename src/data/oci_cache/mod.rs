//! The verified OCI image cache's on-disk layout and records — Layer 0.
//!
//! This module owns the formats and the filesystem primitives: the
//! serialized [`CacheRecord`] and [`RefRecord`], the directory layout under
//! `<builtin state dir>/oci-cache/`, and the private-file operations (locks,
//! leases, atomic record writes, safe reads) the cache is built from. Which
//! archives to acquire, how to verify them and what to prune is the engine's
//! policy in `engine::oci`.

mod record;
mod store;

pub use record::{
    CacheRecord, ImageConfigSummary, RefRecord, SourceFingerprint, MAX_RECORD_BYTES, RECORD_VERSION,
};
pub use store::{OciCacheStore, CACHE_DIR, STAGING_LEASE};
