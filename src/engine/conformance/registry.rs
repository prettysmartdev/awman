//! OCI registry conformance: awman's image acquirer
//! ([`CachingAcquirer`]) pulling from one registry (the fake distribution
//! server or a real registry).
//!
//! Contracts (`L1-OCI-*`): a pull by tag yields the published manifest and
//! config identity for the requested platform and records it in awman's
//! private cache; a cached-only acquisition is served without contacting the
//! registry; a missing tag is a typed not-found error that caches nothing;
//! an image for another platform is refused; acquisition limits (layer
//! count) refuse an image before it is cached.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use super::{ensure, Report};
use crate::data::config::env::EnvSnapshot;
use crate::data::config::image_source::{ImageSourceSpec, RegistryHostConfig};
use crate::data::oci_identity::OciPlatform;
use crate::engine::oci::{
    AcquireLimits, AcquirePolicy, AcquireRequest, CachingAcquirer, DiskSpace, ImageAcquirer,
};

/// The repository the suite pulls from on the world's registry.
pub const REPOSITORY: &str = "awman-conformance/image";
/// Tag of an image published for the host platform.
pub const HOST_TAG: &str = "host";
/// Tag of an image published only for another platform.
pub const FOREIGN_TAG: &str = "foreign";
/// A tag the registry does not have.
pub const MISSING_TAG: &str = "missing";

/// What the world published under one tag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedImage {
    /// `sha256:…` of the platform manifest awman must select.
    pub manifest_digest: String,
    /// `sha256:…` of that manifest's config blob.
    pub config_digest: String,
}

/// The world a registry suite runs in.
pub trait RegistryWorld {
    /// Name of the registry, e.g. `distribution (fake)`.
    fn subject(&self) -> String;
    /// Environment for the acquirer (no ambient credentials or proxies).
    fn env(&self) -> EnvSnapshot;
    /// `host:port` of the registry.
    fn registry(&self) -> String;
    /// How awman must reach it (plain HTTP for a loopback registry, CA...).
    fn host_config(&self) -> RegistryHostConfig;
    /// A private state directory for awman's cache.
    fn state_dir(&self) -> PathBuf;
    /// Publish a single-layer image for the host platform under
    /// [`REPOSITORY`]:[`HOST_TAG`].
    fn given_host_image(&self) -> PublishedImage;
    /// Publish an image for a platform other than the host's under
    /// [`REPOSITORY`]:[`FOREIGN_TAG`].
    fn given_foreign_image(&self);
    /// How many requests the registry has served so far.
    fn requests_served(&self) -> usize;
    /// Remove whatever the world created.
    fn cleanup(&self);
}

struct Unlimited;
impl DiskSpace for Unlimited {
    fn available(&self, _path: &std::path::Path) -> Option<u64> {
        Some(u64::MAX)
    }
}

fn acquirer(
    world: &dyn RegistryWorld,
    state: &std::path::Path,
    limits: AcquireLimits,
) -> CachingAcquirer {
    CachingAcquirer::new(state, limits, world.env(), Arc::new(Unlimited))
}

fn limits() -> AcquireLimits {
    AcquireLimits {
        min_free_bytes: 0,
        ..AcquireLimits::DEFAULT
    }
}

fn request(world: &dyn RegistryWorld, tag: &str, policy: AcquirePolicy) -> AcquireRequest {
    let registry = world.registry();
    AcquireRequest {
        tag: format!("awman-conformance-{tag}:latest"),
        source: ImageSourceSpec::Registry {
            registry: Some(registry.clone()),
            reference: Some(format!("{REPOSITORY}:{tag}")),
        },
        platform: OciPlatform::host_linux(),
        policy,
        registries: BTreeMap::from([(registry, world.host_config())]),
    }
}

/// Whether awman's cache under `state` holds any image or reference.
fn cache_is_empty(state: &std::path::Path) -> bool {
    let root = state.join("oci-cache");
    ["images", "refs"].iter().all(|dir| {
        std::fs::read_dir(root.join(dir))
            .map(|mut entries| entries.next().is_none())
            .unwrap_or(true)
    })
}

/// Run every registry check against `world`.
pub fn oci_registry(world: &dyn RegistryWorld) -> Report {
    let mut report = Report::new(world.subject());
    let published = world.given_host_image();
    world.given_foreign_image();
    let state = world.state_dir();

    report.check("a_pull_by_tag_yields_the_published_identity", {
        let acquired = acquirer(world, &state, limits()).acquire(
            &request(world, HOST_TAG, AcquirePolicy::IfMissing),
            &mut |_| {},
        );
        match acquired {
            Ok(image) => ensure(
                image.identity.manifest_digest.as_str() == published.manifest_digest
                    && image.identity.config_digest.as_str() == published.config_digest
                    && image.identity.platform == OciPlatform::host_linux()
                    && image.archive.starts_with(&state),
                || format!("acquired {:?}, published {published:?}", image.identity),
            ),
            Err(e) => Err(format!("pull failed: {e}")),
        }
    });
    report.check("cached_only_is_served_without_contacting_the_registry", {
        let before = world.requests_served();
        let cached = acquirer(world, &state, limits()).acquire(
            &request(world, HOST_TAG, AcquirePolicy::CachedOnly),
            &mut |_| {},
        );
        let after = world.requests_served();
        match cached {
            Ok(image) => ensure(
                image.identity.manifest_digest.as_str() == published.manifest_digest
                    && before == after,
                || format!("cached-only hit made {} requests", after - before),
            ),
            Err(e) => Err(format!("cached-only acquisition failed: {e}")),
        }
    });
    report.check("a_missing_tag_is_an_error_and_caches_nothing", {
        let fresh = world.state_dir();
        let result = acquirer(world, &fresh, limits()).acquire(
            &request(world, MISSING_TAG, AcquirePolicy::IfMissing),
            &mut |_| {},
        );
        match result {
            Ok(image) => Err(format!("missing tag acquired as {:?}", image.identity)),
            Err(_) => ensure(cache_is_empty(&fresh), || {
                "a failed pull left cache state".into()
            }),
        }
    });
    report.check("an_image_for_another_platform_is_refused", {
        let fresh = world.state_dir();
        let result = acquirer(world, &fresh, limits()).acquire(
            &request(world, FOREIGN_TAG, AcquirePolicy::IfMissing),
            &mut |_| {},
        );
        match result {
            Ok(image) => Err(format!("foreign image acquired as {:?}", image.identity)),
            Err(_) => ensure(cache_is_empty(&fresh), || {
                "a refused pull left cache state".into()
            }),
        }
    });
    report.check("acquisition_limits_refuse_an_image_before_caching_it", {
        let fresh = world.state_dir();
        let tight = AcquireLimits {
            max_layers: 0,
            ..limits()
        };
        let result = acquirer(world, &fresh, tight).acquire(
            &request(world, HOST_TAG, AcquirePolicy::IfMissing),
            &mut |_| {},
        );
        match result {
            Ok(_) => Err("an image over the layer limit was acquired".into()),
            Err(_) => ensure(cache_is_empty(&fresh), || {
                "a refused pull left cache state".into()
            }),
        }
    });
    world.cleanup();
    report
}
