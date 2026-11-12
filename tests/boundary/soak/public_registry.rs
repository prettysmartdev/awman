//! Pulls from real public registries (anonymous Bearer tokens from a realm
//! on another host, multi-platform index selection, blob redirects to CDN
//! hosts, digest pinning, a missing tag, cached reuse without contact).
//! Needs the public network; nightly only.

use super::snapshot_with;
use crate::support;
use awman::data::config::image_source::{ImageSourceKind, ImageSourceSpec};
use awman::data::oci_identity::OciPlatform;
use awman::engine::oci::{
    default_acquirer, AcquireLimits, AcquirePolicy, AcquireRequest, ImageAcquirer,
};
use awman_test_support::capability::PublicNetwork;
use std::collections::BTreeMap;

fn request(registry: &str, reference: &str, policy: AcquirePolicy) -> AcquireRequest {
    AcquireRequest {
        tag: format!("awman-public-{}:latest", registry.replace('.', "-")),
        source: ImageSourceSpec::Registry {
            registry: Some(registry.into()),
            reference: Some(reference.into()),
        },
        platform: OciPlatform::host_linux(),
        policy,
        registries: BTreeMap::new(),
    }
}

/// Replaces `store_registry_public_normal_cases`.
#[test]
fn public_registries_serve_pinned_cached_and_missing_references() {
    let sandbox = support::soak_sandbox(
        "soak::public_registry::public_registries_serve_pinned_cached_and_missing_references",
    );
    let _ = sandbox.require::<PublicNetwork>();
    let env = support::env_snapshot(&sandbox);
    // A proxy nothing listens on: any request a "cached only" pull made
    // would fail, so success proves it did not contact the registry.
    let closed = {
        let listener = sandbox.ports().tcp_listener();
        listener.local_addr().expect("listener address")
    };
    let offline = snapshot_with(&sandbox, &[("HTTPS_PROXY", format!("http://{closed}"))]);

    let mut summary = Vec::new();
    for (registry, reference) in [
        ("docker.io", "library/alpine:3.22"),
        ("ghcr.io", "linuxcontainers/alpine:latest"),
    ] {
        let state = sandbox
            .tmp()
            .join(format!("state-{}", registry.replace('.', "-")));
        let acquirer = default_acquirer(&state, AcquireLimits::DEFAULT, &env);
        let got = acquirer
            .acquire(
                &request(registry, reference, AcquirePolicy::IfMissing),
                &mut |_| {},
            )
            .unwrap_or_else(|e| panic!("{registry}/{reference}: {e}"));
        assert_eq!(got.identity.source, ImageSourceKind::Registry);
        assert!(OciPlatform::host_linux().matches(&got.identity.platform));

        let cached = default_acquirer(&state, AcquireLimits::DEFAULT, &offline)
            .acquire(
                &request(registry, reference, AcquirePolicy::CachedOnly),
                &mut |_| {},
            )
            .unwrap_or_else(|e| panic!("{registry}/{reference} cached-only: {e}"));
        assert_eq!(cached, got);

        let name = reference.split(':').next().expect("repository name");
        let pinned = format!("{name}@{}", got.identity.manifest_digest);
        let fresh = sandbox
            .tmp()
            .join(format!("pinned-{}", registry.replace('.', "-")));
        let by_digest = default_acquirer(&fresh, AcquireLimits::DEFAULT, &env)
            .acquire(
                &request(registry, &pinned, AcquirePolicy::IfMissing),
                &mut |_| {},
            )
            .unwrap_or_else(|e| panic!("{registry}/{pinned}: {e}"));
        assert_eq!(
            by_digest.identity.manifest_digest,
            got.identity.manifest_digest
        );
        summary.push(format!(
            "{registry}/{reference} -> {}",
            got.identity.manifest_digest
        ));
    }
    sandbox.note("pulled", &summary.join("; "));

    // A missing tag is reported as such (not as auth or transport) and
    // caches nothing.
    let state = sandbox.tmp().join("state-missing");
    let missing = default_acquirer(&state, AcquireLimits::DEFAULT, &env)
        .acquire(
            &request(
                "docker.io",
                "library/alpine:awman-no-such-tag-7f3a",
                AcquirePolicy::IfMissing,
            ),
            &mut |_| {},
        )
        .unwrap_err();
    let detail = missing.to_string().to_ascii_lowercase();
    assert!(
        detail.contains("not found") || detail.contains("manifest unknown"),
        "{missing}"
    );
    sandbox.finish();
}
