//! Boundary tests: image acquisition from a real OCI registry on loopback.
//!
//! The registry is the run's `local-registry` capability (a registry
//! container the job started on the granted Docker Engine). Both
//! capabilities are required: `BLOCKED[docker]` / `BLOCKED[local-registry]`
//! when the run does not name them.

mod client;
mod connect;
mod faults;
mod proxy;
mod proxy_contract;
mod resilience;
mod world;

use crate::support;
use awman::engine::conformance::registry::oci_registry;
use awman_test_support::capability::LocalRegistry;

/// awman's image acquirer pulling from a real registry conforms: a pull by
/// tag yields the published manifest and config identity, cached-only
/// acquisition makes no request, a missing tag or another platform is an
/// error that caches nothing, and the layer limit refuses before caching.
#[test]
fn a_real_registry_conforms_to_the_acquirer_contract() {
    let sandbox = support::sandbox("registry::a_real_registry_conforms_to_the_acquirer_contract");
    let registry = sandbox.require::<LocalRegistry>();
    let addr = registry.addr;
    let world = world::RealRegistryWorld::new(&sandbox, addr);
    let report = oci_registry(&world);
    report.assert_conforms();
    assert!(
        report.not_applicable.is_empty(),
        "a real registry declares every check applicable: {:?}",
        report.not_applicable
    );
    drop(world);
    sandbox.finish();
}

/// A real registry reached through a private-CA TLS terminator must obey the
/// same content, platform, cache isolation and failure contract as HTTP.
#[test]
#[cfg(unix)]
fn a_private_ca_registry_conforms_to_the_acquirer_contract() {
    let mut sandbox =
        support::sandbox("registry::a_private_ca_registry_conforms_to_the_acquirer_contract");
    sandbox
        .env_mut()
        .set("AWMAN_REGISTRY_TEST_USER", "awman-test-user");
    sandbox.env_mut().set(
        "AWMAN_REGISTRY_TEST_PASSWORD",
        "test-secret-registry-password",
    );
    sandbox.redact("test-secret-registry-password");
    let registry = sandbox.require::<LocalRegistry>();
    let world = world::RealRegistryWorld::new(&sandbox, registry.addr).with_tls();
    let report = oci_registry(&world);
    report.assert_conforms();
    assert!(report.not_applicable.is_empty());
    drop(world);
    sandbox.finish();
}

#[test]
#[cfg(unix)]
fn private_registry_rejects_wrong_credentials_without_publishing_cached_success() {
    use awman::data::config::image_source::ImageSourceSpec;
    use awman::data::oci_identity::OciPlatform;
    use awman::engine::conformance::registry::{RegistryWorld, HOST_TAG, REPOSITORY};
    use awman::engine::oci::{
        AcquireLimits, AcquirePolicy, AcquireProgress, AcquireRequest, CachingAcquirer, DiskSpace,
        ImageAcquirer,
    };
    let mut sandbox = support::sandbox(
        "registry::private_registry_rejects_wrong_credentials_without_publishing_cached_success",
    );
    sandbox
        .env_mut()
        .set("AWMAN_REGISTRY_TEST_USER", "awman-test-user");
    sandbox
        .env_mut()
        .set("AWMAN_REGISTRY_TEST_PASSWORD", "test-secret-wrong-password");
    sandbox.redact("test-secret-wrong-password");
    let registry = sandbox.require::<LocalRegistry>();
    let world = world::RealRegistryWorld::new(&sandbox, registry.addr).with_tls();
    world.given_host_image();
    struct Space;
    impl DiskSpace for Space {
        fn available(&self, _: &std::path::Path) -> Option<u64> {
            Some(u64::MAX)
        }
    }
    let acquirer = CachingAcquirer::new(
        &world.state_dir(),
        AcquireLimits {
            min_free_bytes: 0,
            ..AcquireLimits::default()
        },
        world.env(),
        std::sync::Arc::new(Space),
    );
    let registry = world.registry();
    let mut request = AcquireRequest {
        tag: "private-registry:latest".into(),
        source: ImageSourceSpec::Registry {
            registry: Some(registry.clone()),
            reference: Some(format!("{REPOSITORY}:{HOST_TAG}")),
        },
        platform: OciPlatform::host_linux(),
        policy: AcquirePolicy::Refresh,
        registries: [(registry, world.host_config())].into(),
    };
    let mut published = false;
    let error = acquirer
        .acquire(&request, &mut |p| {
            published |= matches!(p, AcquireProgress::Cached { .. });
        })
        .unwrap_err();
    assert!(!published, "failed authentication emitted cached success");
    assert!(!error.to_string().contains("test-secret-wrong-password"));
    assert_eq!(
        world.requests_served(),
        0,
        "unauthenticated request reached the registry"
    );
    request.policy = AcquirePolicy::CachedOnly;
    assert!(
        acquirer.acquire(&request, &mut |_| {}).is_err(),
        "failed pull populated cache"
    );
    drop(world);
    sandbox.finish();
}
