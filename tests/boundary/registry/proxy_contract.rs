use super::{connect::Proxy, world::RealRegistryWorld};
use crate::support;
use awman::{
    data::{
        config::{env::EnvSnapshot, image_source::ImageSourceSpec},
        oci_identity::OciPlatform,
    },
    engine::{
        conformance::registry::{RegistryWorld, HOST_TAG, REPOSITORY},
        oci::{
            AcquireLimits, AcquirePolicy, AcquireRequest, CachingAcquirer, DiskSpace, ImageAcquirer,
        },
    },
};
use awman_test_support::{capability::LocalRegistry, TestSandbox};
use std::{
    collections::BTreeMap,
    path::Path,
    sync::{atomic::Ordering, Arc},
};
struct Space;
impl DiskSpace for Space {
    fn available(&self, _: &Path) -> Option<u64> {
        Some(u64::MAX)
    }
}
fn pull(
    s: &TestSandbox,
    world: &RealRegistryWorld<'_>,
    name: &str,
    proxy: &str,
    bypass: bool,
) -> Result<String, String> {
    let mut env: BTreeMap<String, String> = s
        .env()
        .iter()
        .map(|(k, v)| {
            (
                k.to_string_lossy().into_owned(),
                v.to_string_lossy().into_owned(),
            )
        })
        .collect();
    env.insert("HTTPS_PROXY".into(), proxy.into());
    env.insert("HTTP_PROXY".into(), proxy.into());
    env.insert(
        "NO_PROXY".into(),
        if bypass { "127.0.0.1" } else { "" }.into(),
    );
    let acquirer = CachingAcquirer::new(
        &s.root().join(name),
        AcquireLimits {
            min_free_bytes: 0,
            ..AcquireLimits::default()
        },
        EnvSnapshot::from_full_env(env),
        Arc::new(Space),
    );
    let registry = world.registry();
    let request = AcquireRequest {
        tag: "proxy-source:latest".into(),
        source: ImageSourceSpec::Registry {
            registry: Some(registry.clone()),
            reference: Some(format!("{REPOSITORY}:{HOST_TAG}")),
        },
        platform: OciPlatform::host_linux(),
        policy: AcquirePolicy::Refresh,
        registries: [(registry, world.host_config())].into(),
    };
    acquirer
        .acquire(&request, &mut |_| {})
        .map(|image| image.identity.config_digest.to_string())
        .map_err(|e| e.to_string())
}
#[test]
#[cfg(unix)]
fn authenticated_connect_and_no_proxy_preserve_end_to_end_registry_tls() {
    let mut s = support::sandbox("registry::proxy_contract::authenticated_connect_and_no_proxy_preserve_end_to_end_registry_tls");
    s.env_mut()
        .set("AWMAN_REGISTRY_TEST_USER", "awman-test-user");
    s.env_mut().set(
        "AWMAN_REGISTRY_TEST_PASSWORD",
        "test-secret-registry-password",
    );
    s.redact("test-secret-registry-password");
    s.redact("test-secret-proxy-password");
    let registry = s.require::<LocalRegistry>();
    let world = RealRegistryWorld::new(&s, registry.addr).with_tls();
    let image = world.given_host_image();
    let proxy = Proxy::start(&s, world.registry().parse().unwrap());
    assert_eq!(
        pull(&s, &world, "proxied", &proxy.url, false).unwrap(),
        image.config_digest
    );
    let before = proxy.accepted.load(Ordering::SeqCst);
    assert!(
        proxy.tunnels.load(Ordering::SeqCst) > 0,
        "HTTPS did not use CONNECT"
    );
    assert_eq!(
        pull(&s, &world, "bypassed", &proxy.url, true).unwrap(),
        image.config_digest
    );
    assert_eq!(
        proxy.accepted.load(Ordering::SeqCst),
        before,
        "NO_PROXY still reached the proxy"
    );
    let bad = proxy.url.replace(
        "test-secret-proxy-password",
        "test-secret-wrong-proxy-password",
    );
    let error = pull(&s, &world, "refused", &bad, false).unwrap_err();
    assert!(!error.contains("test-secret-wrong-proxy-password"));
    assert_eq!(
        proxy.tunnels.load(Ordering::SeqCst),
        before,
        "bad proxy credentials opened a tunnel"
    );
    drop(proxy);
    drop(world);
    s.finish();
}
#[test]
fn authenticated_http_proxy_forwards_only_the_selected_registry() {
    let s = support::sandbox(
        "registry::proxy_contract::authenticated_http_proxy_forwards_only_the_selected_registry",
    );
    let registry = s.require::<LocalRegistry>();
    let world = RealRegistryWorld::new(&s, registry.addr);
    let image = world.given_host_image();
    let proxy = Proxy::start(&s, world.registry().parse().unwrap());
    assert_eq!(
        pull(&s, &world, "http-proxied", &proxy.url, false).unwrap(),
        image.config_digest
    );
    assert!(proxy.forwarded.load(Ordering::SeqCst) > 0);
    assert_eq!(proxy.tunnels.load(Ordering::SeqCst), 0);
    drop(proxy);
    drop(world);
    s.finish();
}
