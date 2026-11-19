//! Real Engine exports use the same validator/cache as registry and archive imports.
use super::engine::{Cleanup, Engine};
use crate::support;
use awman::data::config::image_source::{ImageSourceKind, ImageSourceSpec};
use awman::data::oci_identity::OciPlatform;
use awman::engine::oci::{
    AcquireLimits, AcquirePolicy, AcquireRequest, CachingAcquirer, DiskSpace, ImageAcquirer,
};
use std::{collections::BTreeMap, path::Path, sync::Arc};

struct Space;
impl DiskSpace for Space {
    fn available(&self, _: &Path) -> Option<u64> {
        Some(u64::MAX)
    }
}

#[test]
fn engine_export_and_docker_save_preserve_identity_and_cached_content() {
    let mut sandbox = support::sandbox(
        "docker::source::engine_export_and_docker_save_preserve_identity_and_cached_content",
    );
    let engine = Engine::grant(&mut sandbox);
    let base = engine.base_image();
    let tag = format!(
        "awman-boundary-source-{}:latest",
        engine
            .sandbox
            .root()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_lowercase()
    );
    let cleanup = Cleanup::new(&engine);
    cleanup.image(&tag);
    engine.run_ok(&["tag", &base, &tag]);
    let expected = engine.run_ok(&["image", "inspect", "--format", "{{.Id}}", &tag]);
    let state = engine.sandbox.root().join("source-cache");
    let acquirer = CachingAcquirer::new(
        &state,
        AcquireLimits {
            min_free_bytes: 0,
            ..AcquireLimits::default()
        },
        engine.snapshot(),
        Arc::new(Space),
    );
    let mut request = AcquireRequest {
        tag: tag.clone(),
        source: ImageSourceSpec::DockerStore {
            host: Some(engine.docker.host.clone()),
            tls: None,
            reference: Some(tag.clone()),
        },
        platform: OciPlatform::host_linux(),
        policy: AcquirePolicy::IfMissing,
        registries: BTreeMap::new(),
    };
    let imported = acquirer
        .acquire(&request, &mut |_| {})
        .expect("export from real Docker API");
    assert_eq!(imported.identity.config_digest.as_str(), expected);
    assert_eq!(imported.identity.source, ImageSourceKind::DockerStore);
    assert_eq!(imported.identity.platform, OciPlatform::host_linux());
    let archive = engine.sandbox.work().join("docker-save.tar");
    engine
        .command()
        .args(["image", "save", "--output"])
        .arg(&archive)
        .arg(&tag)
        .run()
        .assert_success();
    engine.run_ok(&["image", "rm", &tag]);
    // The source tag is gone, but its verified cache remains independently usable.
    request.policy = AcquirePolicy::CachedOnly;
    let cached = acquirer
        .acquire(&request, &mut |_| {})
        .expect("source-free cached acquisition");
    assert_eq!(cached.identity, imported.identity);
    request.policy = AcquirePolicy::Refresh;
    assert!(
        acquirer.acquire(&request, &mut |_| {}).is_err(),
        "removed source tag was still exported"
    );
    request.source = ImageSourceSpec::Archive { path: archive };
    let from_archive = acquirer
        .acquire(&request, &mut |_| {})
        .expect("real docker-save archive import");
    assert_eq!(
        from_archive.identity.config_digest,
        imported.identity.config_digest
    );
    assert_eq!(from_archive.identity.platform, imported.identity.platform);
    assert_eq!(from_archive.identity.source, ImageSourceKind::Archive);
    drop((from_archive, cached, imported));
    drop(cleanup);
    sandbox.finish();
}

/// Both TLS modes exercise the real Engine API and archive export. An
/// unauthenticated client must fail against mTLS before reaching that API.
#[test]
#[cfg(unix)]
fn tls_and_mutual_tls_engine_sources_verify_certificates_and_client_identity() {
    let mut sandbox = support::sandbox(
        "docker::source::tls_and_mutual_tls_engine_sources_verify_certificates_and_client_identity",
    );
    let engine = Engine::grant(&mut sandbox);
    let base = engine.base_image();
    let socket =
        engine.docker.host.strip_prefix("unix://").expect(
            "BLOCKED[docker]: TLS source proof requires a granted local Unix Engine socket",
        );
    let expected = engine.run_ok(&["image", "inspect", "--format", "{{.Id}}", &base]);
    for mutual in [false, true] {
        let endpoint = super::tls::Endpoint::start(engine.sandbox, Path::new(socket), mutual);
        let acquirer = CachingAcquirer::new(
            &engine
                .sandbox
                .root()
                .join(if mutual { "mtls-cache" } else { "tls-cache" }),
            AcquireLimits {
                min_free_bytes: 0,
                ..AcquireLimits::default()
            },
            engine.snapshot(),
            Arc::new(Space),
        );
        let mut request = AcquireRequest {
            tag: "tls-source:latest".into(),
            source: ImageSourceSpec::DockerStore {
                host: Some(endpoint.host.clone()),
                tls: Some(endpoint.tls.clone()),
                reference: Some(base.clone()),
            },
            platform: OciPlatform::host_linux(),
            policy: AcquirePolicy::Refresh,
            registries: BTreeMap::new(),
        };
        let imported = acquirer
            .acquire(&request, &mut |_| {})
            .expect("authenticated TLS Engine export");
        assert_eq!(imported.identity.config_digest.as_str(), expected);
        assert_eq!(imported.identity.source, ImageSourceKind::DockerStore);
        if mutual {
            if let ImageSourceSpec::DockerStore { tls: Some(tls), .. } = &mut request.source {
                tls.cert = None;
                tls.key = None;
            }
            assert!(
                acquirer.acquire(&request, &mut |_| {}).is_err(),
                "mTLS accepted no client certificate"
            );
        }
        drop(imported);
        drop(endpoint);
    }
    sandbox.finish();
}
