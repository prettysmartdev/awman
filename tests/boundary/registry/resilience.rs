use super::{
    faults::{Endpoint, Fault},
    world::RealRegistryWorld,
};
use crate::support;
use awman::{
    data::{
        config::image_source::{ImageSourceSpec, RegistryAuthSource, RegistryHostConfig},
        oci_identity::OciPlatform,
    },
    engine::{
        conformance::registry::{PublishedImage, RegistryWorld, HOST_TAG, REPOSITORY},
        oci::{
            AcquireLimits, AcquirePolicy, AcquireProgress, AcquireRequest, CachingAcquirer,
            CancelToken, DiskSpace, ImageAcquirer, RetryPolicy,
        },
    },
};
use awman_test_support::{capability::LocalRegistry, TestSandbox};
use std::{
    net::SocketAddr,
    path::Path,
    sync::{atomic::Ordering, Arc},
    time::{Duration, Instant},
};

struct Space;
impl DiskSpace for Space {
    fn available(&self, _: &Path) -> Option<u64> {
        Some(u64::MAX)
    }
}
fn prepare(id: &'static str) -> (TestSandbox, SocketAddr, PublishedImage) {
    let mut s = support::sandbox(id);
    s.env_mut()
        .set("AWMAN_REGISTRY_TEST_USER", "awman-test-user");
    s.env_mut().set(
        "AWMAN_REGISTRY_TEST_PASSWORD",
        "test-secret-registry-password",
    );
    s.redact("test-secret-registry-password");
    let registry = s.require::<LocalRegistry>();
    let world = RealRegistryWorld::new(&s, registry.addr);
    let image = world.given_host_image();
    drop(world);
    (s, registry.addr, image)
}
fn pull(
    s: &TestSandbox,
    endpoint: &Endpoint,
    deadline: Duration,
) -> (CachingAcquirer, AcquireRequest) {
    let registry = endpoint.addr.to_string();
    let acquirer = CachingAcquirer::new(
        &s.root().join("fault-cache"),
        AcquireLimits {
            min_free_bytes: 0,
            ..AcquireLimits::default()
        },
        support::env_snapshot(s),
        Arc::new(Space),
    )
    .with_retry(RetryPolicy {
        max_attempts: 3,
        deadline,
        initial_backoff: Duration::ZERO,
        max_backoff: Duration::ZERO,
    });
    let request = AcquireRequest {
        tag: "fault-source:latest".into(),
        source: ImageSourceSpec::Registry {
            registry: Some(registry.clone()),
            reference: Some(format!("{REPOSITORY}:{HOST_TAG}")),
        },
        platform: OciPlatform::host_linux(),
        policy: AcquirePolicy::Refresh,
        registries: [(
            registry,
            RegistryHostConfig {
                insecure: true,
                ca_cert: None,
                auth: Some(RegistryAuthSource::Env {
                    username_var: "AWMAN_REGISTRY_TEST_USER".into(),
                    password_var: "AWMAN_REGISTRY_TEST_PASSWORD".into(),
                }),
            },
        )]
        .into(),
    };
    (acquirer, request)
}
fn no_cached_success(
    acquirer: &CachingAcquirer,
    request: &AcquireRequest,
) -> awman::engine::error::EngineError {
    let mut published = false;
    let error = acquirer
        .acquire(request, &mut |progress| {
            published |= matches!(progress, AcquireProgress::Cached { .. });
        })
        .unwrap_err();
    assert!(!published, "failed acquisition reported cached success");
    let mut cached = request.clone();
    cached.policy = AcquirePolicy::CachedOnly;
    assert!(
        acquirer.acquire(&cached, &mut |_| {}).is_err(),
        "failed acquisition published a cache reference"
    );
    for secret in ["test-secret-registry-password", "test-secret-token-"] {
        assert!(
            !error.to_string().contains(secret),
            "error exposed a credential"
        );
    }
    error
}

#[test]
fn bearer_token_expiring_between_manifest_and_blob_is_renewed() {
    let (s, registry, image) =
        prepare("registry::resilience::bearer_token_expiring_between_manifest_and_blob_is_renewed");
    let endpoint = Endpoint::start(&s, registry, Fault::BearerExpiry);
    let (acquirer, request) = pull(&s, &endpoint, Duration::from_secs(10));
    let acquired = acquirer.acquire(&request, &mut |_| {}).unwrap();
    assert_eq!(
        acquired.identity.config_digest.as_str(),
        image.config_digest
    );
    assert_eq!(
        endpoint.tokens.load(Ordering::SeqCst),
        2,
        "expired token was not renewed exactly once"
    );
    drop(acquired);
    drop(endpoint);
    s.finish();
}
#[test]
fn transient_server_errors_and_mid_blob_disconnect_retry_the_same_source() {
    for (fault, id) in [
        (Fault::Unavailable(2), "registry::resilience::transient_server_errors_and_mid_blob_disconnect_retry_the_same_source"),
        (Fault::DisconnectBlob, "registry::resilience::transient_server_errors_and_mid_blob_disconnect_retry_the_same_source"),
    ] {
        let (s, registry, image) = prepare(id);
        let endpoint = Endpoint::start(&s, registry, fault);
        let (acquirer, request) = pull(&s, &endpoint, Duration::from_secs(10));
        let acquired = acquirer.acquire(&request, &mut |_| {}).unwrap();
        assert_eq!(acquired.identity.config_digest.as_str(), image.config_digest);
        assert!(endpoint.requests.load(Ordering::SeqCst) >= 5, "transfer did not retry");
        drop(acquired);
        drop(endpoint);
        s.finish();
    }
}
#[test]
fn retry_exhaustion_and_bad_digest_never_publish_partial_images() {
    for (fault, expected, id) in [
        (
            Fault::Unavailable(usize::MAX),
            3,
            "registry::resilience::retry_exhaustion_and_bad_digest_never_publish_partial_images",
        ),
        (
            Fault::CorruptBlob,
            2,
            "registry::resilience::retry_exhaustion_and_bad_digest_never_publish_partial_images",
        ),
    ] {
        let (s, registry, _) = prepare(id);
        let endpoint = Endpoint::start(&s, registry, fault);
        let (acquirer, request) = pull(&s, &endpoint, Duration::from_secs(10));
        no_cached_success(&acquirer, &request);
        assert_eq!(
            endpoint.requests.load(Ordering::SeqCst),
            expected,
            "wrong retry classification"
        );
        drop(endpoint);
        s.finish();
    }
}
#[test]
fn stalled_blob_obeys_the_overall_deadline_without_publishing_success() {
    let (s, registry, _) = prepare(
        "registry::resilience::stalled_blob_obeys_the_overall_deadline_without_publishing_success",
    );
    let endpoint = Endpoint::start(&s, registry, Fault::StallBlob);
    let (acquirer, request) = pull(&s, &endpoint, Duration::from_secs(1));
    let start = Instant::now();
    let error = no_cached_success(&acquirer, &request);
    assert!(error.to_string().contains("deadline"), "{error}");
    assert!(
        start.elapsed() < Duration::from_secs(10),
        "deadline did not bound a stalled body"
    );
    assert_eq!(endpoint.blobs.load(Ordering::SeqCst), 1);
    drop(endpoint);
    s.finish();
}
#[test]
fn cancellation_interrupts_a_stalled_blob_without_publishing_success() {
    let (s, registry, _) = prepare(
        "registry::resilience::cancellation_interrupts_a_stalled_blob_without_publishing_success",
    );
    let endpoint = Endpoint::start(&s, registry, Fault::StallBlob);
    let (acquirer, request) = pull(&s, &endpoint, Duration::from_secs(10));
    let token = CancelToken::new();
    let acquirer = acquirer.with_cancel(token.clone());
    std::thread::scope(|scope| {
        let pending = scope.spawn(|| no_cached_success(&acquirer, &request));
        awman_test_support::wait_for("registry body stall", Duration::from_secs(5), || {
            (endpoint.blobs.load(Ordering::SeqCst) == 1).then_some(())
        });
        token.cancel();
        let error = pending.join().unwrap();
        assert!(matches!(
            error,
            awman::engine::error::EngineError::ImageAcquisitionCancelled
        ));
    });
    let references = s.root().join("fault-cache/oci-cache/refs");
    assert!(
        !references.exists() || std::fs::read_dir(references).unwrap().next().is_none(),
        "cancelled transfer published a cache reference"
    );
    drop(endpoint);
    s.finish();
}
