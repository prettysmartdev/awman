//! `engine::oci` — acquiring OCI images for runtimes that import rather than
//! build (`ImageAcquisition::Import`).
//!
//! An [`ImageAcquirer`] resolves an explicit [`ImageSourceSpec`] to content
//! identity, fetches and verifies the content, and leaves a verified archive
//! in awman's cache. The builtin runtime loads that archive into its own
//! store and records the [`ImageIdentity`].
//!
//! Sources never substitute for one another: a registry reference is only
//! pulled from a registry, a Docker Engine reference is only exported from
//! that engine, an Apple Containers reference is only exported by that
//! service's own helper through the version-gated bridge ([`apple_store`]),
//! and an archive is only read from its path.
//!
//! Every acquisition follows the same path:
//!
//! 1. resolve the source and its cache key without contacting it
//!    ([`sources`]); a cache hit is returned unless the policy is `Refresh`;
//! 2. stage the archive in a private directory inside the cache, enforcing
//!    the byte cap and the free-space floor as it arrives;
//! 3. validate it completely ([`archive`]): structure, every digest, the
//!    platform, every layer entry;
//! 4. commit it atomically ([`cache`]).
//!
//! Nothing is visible in the cache until step 4, so a wrong-platform,
//! malformed, oversized, tampered or truncated image — or a full disk —
//! leaves no partial state behind.
//!
//! Steps 2–3 are repeated for transient transport failures under a bounded
//! [`RetryPolicy`] ([`retry`]): the same source, endpoint, reference and
//! credentials every time, a fresh staging directory per attempt, never a
//! retry of an authentication, certificate, platform, digest or format
//! failure. A [`CancelToken`] stops an acquisition between attempts and
//! inside a transfer. The formats accepted at step 3 are the contract in
//! [`format`].

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::data::config::env::EnvSnapshot;
use crate::data::config::image_source::{ImageSourceKind, ImageSourceSpec, RegistryHostConfig};
use crate::data::oci_identity::{Digest, ImageIdentity, OciPlatform};
use crate::engine::error::EngineError;

pub mod apple_store;
pub mod archive;
pub mod cache;
pub mod docker_engine;
pub mod format;
pub mod registry;
pub mod resolve;
pub mod retry;
pub mod sources;
mod transport;
pub mod verify;

pub use crate::data::oci_cache::CacheRecord;
pub use crate::data::oci_cache::ImageConfigSummary;
pub use archive::{ArchiveValidator, ValidatedArchive};
pub use cache::{OciCache, PruneReport};
pub use resolve::{
    resolve_source, BuildHint, ImageBuildGuidance, ImageSourceResolver, ImageSources,
    ResolvedImageSource,
};
pub use retry::{CancelToken, RetryPolicy};
pub use verify::DiskSpace;

/// What a local source store holds under one reference, for one platform,
/// as seen without exporting it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceImage {
    /// Nothing usable: no such reference, or one for another platform.
    Absent,
    /// A single-platform image whose identity the store vouches for.
    Present(crate::data::oci_identity::BuiltImageIdentity),
    /// An image the store keeps as a multi-platform index. It can be
    /// imported (the export selects the platform), but no single-manifest
    /// identity can be bound to it without exporting it.
    Unbound,
}

/// Acquires images from explicit sources into awman's verified archive cache.
pub trait ImageAcquirer: Send + Sync {
    /// Resolve `request.source` to content identity, fetch and verify it, and
    /// produce an OCI archive in awman's cache.
    ///
    /// Never contacts a source daemon or registry when `request.policy` is
    /// [`AcquirePolicy::CachedOnly`] and the identity is already cached.
    fn acquire(
        &self,
        request: &AcquireRequest,
        progress: &mut dyn FnMut(AcquireProgress),
    ) -> Result<LeasedImage, EngineError>;
}

/// One acquisition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcquireRequest {
    /// The awman tag the image will be stored under.
    pub tag: String,
    /// The explicit source to acquire from.
    pub source: ImageSourceSpec,
    /// The platform the selected manifest must match.
    pub platform: OciPlatform,
    pub policy: AcquirePolicy,
    /// Registry host settings keyed by `host[:port]`.
    pub registries: BTreeMap<String, RegistryHostConfig>,
}

/// When an acquisition may contact its source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcquirePolicy {
    /// Use the cache only; fail if the image is not cached.
    CachedOnly,
    /// Contact the source only when the image is not cached.
    IfMissing,
    /// Always re-resolve against the source.
    Refresh,
}

/// A verified image in awman's cache.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcquiredImage {
    pub identity: ImageIdentity,
    /// The archive, digest-named, under `<state_dir>/oci-cache`.
    pub archive: PathBuf,
    pub archive_format: ArchiveFormat,
    /// Archive size in bytes.
    pub bytes: u64,
}

/// A verified acquisition whose archive cannot be pruned until the last
/// consumer drops it. Cloning preserves the lease, not just the pathname.
#[derive(Debug, Clone)]
pub struct LeasedImage {
    image: AcquiredImage,
    _lease: Arc<cache::ArchiveLease>,
}

impl std::ops::Deref for LeasedImage {
    type Target = AcquiredImage;
    fn deref(&self) -> &Self::Target {
        &self.image
    }
}

impl PartialEq for LeasedImage {
    fn eq(&self, other: &Self) -> bool {
        self.image == other.image
    }
}
impl Eq for LeasedImage {}

/// Layout of an archive on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveFormat {
    /// OCI image layout (`oci-layout`, `index.json`, `blobs/`).
    OciLayout,
    /// `docker save` layout (`manifest.json`, per-layer tars).
    DockerSave,
}

/// Progress reported while acquiring.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcquireProgress {
    Resolving {
        source: ImageSourceKind,
        reference: String,
    },
    Layer {
        digest: Digest,
        done: u64,
        total: Option<u64>,
    },
    Verifying {
        digest: Digest,
    },
    Cached {
        identity: ImageIdentity,
    },
}

/// Bounds every acquisition enforces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AcquireLimits {
    pub max_archive_bytes: u64,
    pub max_layer_bytes: u64,
    pub max_layers: usize,
    /// Free space that must remain on the cache's filesystem.
    pub min_free_bytes: u64,
}

impl AcquireLimits {
    /// 64 GiB archive, 32 GiB per layer (compressed and uncompressed), 256
    /// layers, 2 GiB left free.
    pub const DEFAULT: Self = Self {
        max_archive_bytes: 64 << 30,
        max_layer_bytes: 32 << 30,
        max_layers: 256,
        min_free_bytes: 2 << 30,
    };
}

impl Default for AcquireLimits {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// The acquirer for `state_dir` (cache under `<state_dir>/oci-cache`).
pub fn default_acquirer(
    state_dir: &Path,
    limits: AcquireLimits,
    env: &EnvSnapshot,
) -> CachingAcquirer {
    CachingAcquirer::new(
        state_dir,
        limits,
        env.clone(),
        Arc::new(verify::HostDiskSpace),
    )
}

/// The cache-backed acquirer. The disk probe, retry policy, cancellation
/// token and sleeper are injectable so the insufficient-space, retry and
/// cancellation paths are testable without a real disk or real waiting.
pub struct CachingAcquirer {
    state_dir: PathBuf,
    limits: AcquireLimits,
    env: EnvSnapshot,
    disk: Arc<dyn DiskSpace>,
    retry: RetryPolicy,
    cancel: CancelToken,
    sleep: Arc<retry::Sleeper<'static>>,
    clock: Option<&'static dyn crate::data::clock::Clock>,
    apple: Option<Arc<dyn apple_store::XpcTransport>>,
    expected_built_identity: Option<crate::data::oci_identity::BuiltImageIdentity>,
    export_selector: Option<String>,
}

impl CachingAcquirer {
    pub fn new(
        state_dir: &Path,
        limits: AcquireLimits,
        env: EnvSnapshot,
        disk: Arc<dyn DiskSpace>,
    ) -> Self {
        Self {
            state_dir: state_dir.to_path_buf(),
            limits,
            env,
            disk,
            retry: RetryPolicy::DEFAULT,
            cancel: CancelToken::new(),
            sleep: Arc::new(std::thread::sleep),
            clock: None,
            apple: apple_store::installed_transport(),
            expected_built_identity: None,
            export_selector: None,
        }
    }

    /// Bound the attempts, deadline and backoff of every acquisition.
    pub fn with_retry(mut self, policy: RetryPolicy) -> Self {
        self.retry = policy;
        self
    }

    /// Stop acquisitions when `token` is cancelled.
    pub fn with_cancel(mut self, token: CancelToken) -> Self {
        self.cancel = token;
        self
    }

    /// Validate before cache publication (also shared by recording importers).
    pub(crate) fn check_built_identity(
        expected: Option<&crate::data::oci_identity::BuiltImageIdentity>,
        identity: &ImageIdentity,
    ) -> Result<(), EngineError> {
        if let Some(expected) = expected {
            if !expected.matches(identity) {
                return Err(EngineError::ImageDigestMismatch {
                    reference: identity.reference.clone(),
                    expected: expected
                        .config_digest()
                        .or(expected.manifest_digest())
                        .expect("validated build receipt")
                        .to_string(),
                    actual: if expected.config_digest().is_some() {
                        identity.config_digest.to_string()
                    } else {
                        identity.manifest_digest.to_string()
                    },
                });
            }
        }
        Ok(())
    }

    /// Require fresh content matching a builder receipt before cache
    /// publication. A caller must also select `AcquirePolicy::Refresh`.
    pub fn with_expected_built_identity(
        mut self,
        expected: Option<crate::data::oci_identity::BuiltImageIdentity>,
    ) -> Self {
        self.expected_built_identity = expected;
        self
    }

    /// Pin a source export to an immutable image ID while retaining the
    /// configured reference as the cache key and installed identity.
    pub fn with_export_selector(mut self, selector: Option<String>) -> Self {
        self.export_selector = selector;
        self
    }

    /// Replace the backoff sleeper (tests).
    pub fn with_sleeper(mut self, sleep: Arc<retry::Sleeper<'static>>) -> Self {
        self.sleep = sleep;
        self
    }

    /// Measure acquisition deadlines on `clock` instead of the system clock
    /// (virtual time in tests).
    pub fn with_clock(mut self, clock: &'static dyn crate::data::clock::Clock) -> Self {
        self.clock = Some(clock);
        self
    }

    /// Replace the Apple Containers transport (tests; `None` removes it).
    pub fn with_apple_transport(
        mut self,
        transport: Option<Arc<dyn apple_store::XpcTransport>>,
    ) -> Self {
        self.apple = transport;
        self
    }

    /// The token this acquirer watches.
    pub fn cancel_token(&self) -> CancelToken {
        self.cancel.clone()
    }
}

impl ImageAcquirer for CachingAcquirer {
    fn acquire(
        &self,
        request: &AcquireRequest,
        progress: &mut dyn FnMut(AcquireProgress),
    ) -> Result<LeasedImage, EngineError> {
        let deadline = match self.clock {
            Some(clock) => retry::Deadline::start_on(self.retry.deadline, clock),
            None => retry::Deadline::start(self.retry.deadline),
        };
        let control = retry::OperationControl::new(self.cancel.clone(), deadline);
        control.check()?;
        if self.expected_built_identity.is_some() && request.policy != AcquirePolicy::Refresh {
            return Err(EngineError::Config(
                "a build-bound image acquisition must refresh its source".into(),
            ));
        }
        if self.export_selector.is_some() && self.expected_built_identity.is_none() {
            return Err(EngineError::Config(
                "an immutable export selector requires a build receipt".into(),
            ));
        }
        let kind = request.source.kind();
        let reference = request.source.reference_or(&request.tag);
        let mut resolved = sources::Resolved::resolve(
            &request.source,
            &reference,
            &request.registries,
            &self.env,
        )?;
        let source_key = resolved.key(kind, &reference, &request.platform);
        let key = source_key.canonical_key();
        progress(AcquireProgress::Resolving {
            source: kind,
            reference: reference.clone(),
        });

        let cache = OciCache::open(&self.state_dir)?.controlled(control.clone());
        if request.policy != AcquirePolicy::Refresh {
            for candidate in source_key.lookup_keys() {
                if let Some((hit, lease)) = cache.lookup_leased(&candidate)? {
                    let archive_changed = match (&resolved, &hit.record.source_fingerprint) {
                        // An archive re-exported at the same path is re-imported
                        // (only when the source is allowed to be read at all).
                        (sources::Resolved::Archive { path }, Some(fp))
                            if request.policy == AcquirePolicy::IfMissing =>
                        {
                            crate::data::oci_cache::SourceFingerprint::of(path)
                                .is_some_and(|now| now != *fp)
                        }
                        _ => false,
                    };
                    if !archive_changed
                        && hit.image.identity.source == kind
                        && hit.image.identity.reference == reference
                        && hit.record.locator == resolved.locator()
                        && request.platform.matches(&hit.image.identity.platform)
                    {
                        progress(AcquireProgress::Cached {
                            identity: hit.image.identity.clone(),
                        });
                        control.check()?;
                        return Ok(LeasedImage {
                            image: hit.image,
                            _lease: Arc::new(lease),
                        });
                    }
                }
            }
            if request.policy == AcquirePolicy::CachedOnly {
                return Err(EngineError::ImageNotCached {
                    tag: request.tag.clone(),
                    source_desc: format!("{} {}", kind, resolved.locator()),
                });
            }
        }
        self.cancel.check()?;

        resolved.prepare_credentials(&self.env)?;

        // Every attempt is the same request against the same resolved source
        // and endpoint, staged into a fresh private directory. A failed or
        // cancelled attempt drops its staging directory with it.
        let attempt = |number: u32| -> Result<
            (cache::Staging, sources::Fetched, ValidatedArchive),
            EngineError,
        > {
            let staging = cache.staging_leased()?;
            verify::ensure_space(
                self.disk.as_ref(),
                staging.path(),
                0,
                self.limits.min_free_bytes,
            )?;
            let ctx = sources::FetchContext {
                staging_dir: staging.path(),
                limits: self.limits,
                disk: self.disk.as_ref(),
                platform: &request.platform,
                export_selector: self.export_selector.as_deref(),
                cancel: &self.cancel,
                deadline,
                attempt: number,
                apple: self.apple.as_deref(),
            };
            let fetch = |report: sources::Report<'_>| {
                let fetched = resolved.fetch(&request.tag, &reference, &ctx, report)?;
                self.cancel.check()?;
                let validated = archive::ArchiveValidator {
                    platform: &request.platform,
                    wanted_refs: &fetched.wanted_refs,
                    limits: &self.limits,
                }
                .validate_controlled(
                    &fetched.path,
                    &mut |p| report(p),
                    &control,
                )?;
                Ok::<_, EngineError>((fetched, validated))
            };
            let (fetched, validated) = if resolved.is_network() {
                sources::run_isolated(&mut *progress, fetch)?
            } else {
                let events = std::sync::Mutex::new(Vec::new());
                let out = fetch(&|p| {
                    if let Ok(mut e) = events.lock() {
                        e.push(p)
                    }
                });
                for p in events.into_inner().unwrap_or_default() {
                    progress(p);
                }
                out?
            };
            Ok((staging, fetched, validated))
        };
        let (staging, fetched, validated) = retry::run_bounded(
            self.retry,
            deadline,
            &self.cancel,
            self.sleep.as_ref(),
            attempt,
        )
        .map_err(retry::finish)?;
        if let Some(expected) = &fetched.expected_manifest {
            if *expected != validated.manifest_digest {
                return Err(EngineError::ImageDigestMismatch {
                    reference: reference.clone(),
                    expected: expected.to_string(),
                    actual: validated.manifest_digest.to_string(),
                });
            }
        }
        let identity = ImageIdentity {
            reference: reference.clone(),
            manifest_digest: validated.manifest_digest.clone(),
            config_digest: validated.config_digest.clone(),
            platform: validated.platform.clone(),
            source: kind,
        };
        Self::check_built_identity(self.expected_built_identity.as_ref(), &identity)?;
        control.check()?;
        let (committed, lease) = cache.commit_leased(
            &fetched.path,
            &key,
            identity,
            &validated,
            resolved.locator(),
            fetched.fingerprint,
        )?;
        drop(staging);
        progress(AcquireProgress::Cached {
            identity: committed.image.identity.clone(),
        });
        Ok(LeasedImage {
            image: committed.image,
            _lease: Arc::new(lease),
        })
    }
}
