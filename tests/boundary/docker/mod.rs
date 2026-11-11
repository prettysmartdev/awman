//! Boundary tests: awman's container runtime against the real Docker Engine.
//! One bounded proof per behaviour that only a real engine can show; the
//! shared conformance suite (`conformance::`) covers the rest of the
//! runtime contract against the same engine.
//!
//! Every test requires the `docker` capability (`BLOCKED[docker]` when the
//! run names none) and removes what it created, even when it fails.

pub mod engine;

use crate::support;
use awman::engine::auth::credential::SecretString;
use awman::engine::container::background::BackgroundLaunchRequest;
use awman::engine::container::options::{OverlayPermission, OverlaySpec};
use awman::engine::container::ContainerRuntime;
use engine::Engine;
use std::collections::HashMap;
use std::path::PathBuf;

/// The runtime reaches the granted engine, names itself `docker`, reports
/// an unknown image as absent and lists running awman containers without
/// error. (Replaces the legacy `docker_runtime_*`, `docker_image_exists_*`
/// and `docker_list_running_all_*` tests.)
#[test]
fn the_engine_is_reachable_and_unknown_images_are_absent() {
    let mut sandbox =
        support::sandbox("docker::the_engine_is_reachable_and_unknown_images_are_absent");
    let engine = Engine::grant(&mut sandbox);
    let runtime = ContainerRuntime::docker_in(engine.snapshot());

    runtime
        .availability()
        .unwrap_or_else(|e| panic!("the granted engine is reported unavailable: {e}"));
    assert_eq!(runtime.runtime_name(), "docker");
    assert!(!runtime.image_exists("awman-boundary/unknown:none"));
    let base = engine.base_image();
    assert!(runtime.image_exists(&base), "{base} is in the engine");
    runtime
        .list_running_all()
        .unwrap_or_else(|e| panic!("listing awman containers failed: {e}"));
    sandbox.finish();
}

/// An overlay passed to a background container is mounted at the requested
/// container path and is readable there. (Replaces
/// `docker_background_container_overlay_mount_applied`.)
#[test]
fn an_overlay_is_mounted_read_only_at_the_requested_path() {
    let mut sandbox =
        support::sandbox("docker::an_overlay_is_mounted_read_only_at_the_requested_path");
    let engine = Engine::grant(&mut sandbox);
    let base = engine.base_image();
    let host_dir = engine.sandbox.root().join("overlay-source");
    std::fs::create_dir_all(&host_dir).expect("overlay source directory");
    std::fs::write(host_dir.join("marker.txt"), b"overlay-ok\n").expect("marker file");

    let runtime = ContainerRuntime::docker_in(engine.snapshot());
    let env = HashMap::new();
    let overlays = [OverlaySpec {
        host_path: host_dir,
        container_path: PathBuf::from("/mnt/overlay-test"),
        permission: OverlayPermission::ReadOnly,
    }];
    let workdir = engine.sandbox.work();
    let container = runtime
        .launch_background(&BackgroundLaunchRequest {
            secret_env: &[],
            name: Some("awman-boundary-overlay"),
            image: &base,
            workdir: &workdir,
            env: &env,
            overlays: &overlays,
            builtin_launch: None,
        })
        .expect("a background container starts with an overlay");
    let cleanup = engine::Cleanup::new(&engine);
    cleanup.container("awman-boundary-overlay");

    let read = container.exec("cat /mnt/overlay-test/marker.txt", None);
    let write = container.exec("touch /mnt/overlay-test/new 2>/dev/null; echo $?", None);
    container.kill().expect("the container is removed");
    let read = read.expect("exec reads the overlay");
    assert_eq!(read.exit_code, 0, "stderr: {}", read.stderr);
    assert_eq!(read.stdout, "overlay-ok\n");
    let write = write.expect("exec tries to write the overlay");
    assert_ne!(
        write.stdout.trim(),
        "0",
        "a read-only overlay accepted a write"
    );
    assert!(!engine.container_exists("awman-boundary-overlay"));
    drop(cleanup);
    sandbox.finish();
}

/// Secret values containing `=` or a newline reach the container byte for
/// byte although only their names are on the docker command line.
/// (Replaces the legacy `docker_credential_*` value tests; the argv side is
/// asserted by the conformance suite's argv check.)
#[test]
fn secret_values_with_equals_and_newline_arrive_byte_for_byte() {
    let mut sandbox =
        support::sandbox("docker::secret_values_with_equals_and_newline_arrive_byte_for_byte");
    let engine = Engine::grant(&mut sandbox);
    let base = engine.base_image();
    let runtime = ContainerRuntime::docker_in(engine.snapshot());
    let secrets = vec![
        (
            "BOUNDARY_PLAIN".to_string(),
            SecretString::new("plain-7f3a"),
        ),
        (
            "BOUNDARY_EQUALS".to_string(),
            SecretString::new("a=b=c-7f3a"),
        ),
        (
            "BOUNDARY_NEWLINE".to_string(),
            SecretString::new("line1\nline2-7f3a"),
        ),
    ];
    for (_, value) in &secrets {
        engine.sandbox.redact(value.expose());
    }
    let env = HashMap::new();
    let workdir = engine.sandbox.work();
    let container = runtime
        .launch_background(&BackgroundLaunchRequest {
            secret_env: &secrets,
            name: Some("awman-boundary-secrets"),
            image: &base,
            workdir: &workdir,
            env: &env,
            overlays: &[],
            builtin_launch: None,
        })
        .expect("a background container starts with secrets");
    let cleanup = engine::Cleanup::new(&engine);
    cleanup.container("awman-boundary-secrets");

    let seen = container.exec(
        "printf '%s|%s|%s' \"$BOUNDARY_PLAIN\" \"$BOUNDARY_EQUALS\" \"$BOUNDARY_NEWLINE\"",
        None,
    );
    container.kill().expect("the container is removed");
    assert_eq!(
        seen.expect("exec reads the variables").stdout,
        "plain-7f3a|a=b=c-7f3a|line1\nline2-7f3a"
    );
    drop(cleanup);
    sandbox.finish();
}

/// The engine store source exports an image from the real engine over its
/// Unix socket, reuses it from awman's cache, and reports a missing image as
/// final. (Replaces `store_docker_real_engine_unix_roundtrip`; the TLS and
/// mutual-TLS variants need a TLS-enabled engine no runner provisions yet.)
#[test]
fn the_engine_store_exports_an_image_and_serves_it_from_cache() {
    use awman::data::config::image_source::{ImageSourceKind, ImageSourceSpec};
    use awman::data::oci_identity::OciPlatform;
    use awman::engine::error::EngineError;
    use awman::engine::oci::{
        default_acquirer, AcquireLimits, AcquirePolicy, AcquireRequest, ImageAcquirer,
    };
    use std::collections::BTreeMap;

    let mut sandbox =
        support::sandbox("docker::the_engine_store_exports_an_image_and_serves_it_from_cache");
    let engine = Engine::grant(&mut sandbox);
    assert!(
        engine.docker.host.starts_with("unix://"),
        "BLOCKED[docker]: the engine store test needs a unix:// engine, got {}",
        engine.docker.host
    );
    let base = engine.base_image();
    let state = engine.sandbox.tmp().join("engine-store-state");
    let env = engine.snapshot();
    let request = |policy, reference: &str| AcquireRequest {
        tag: "awman-boundary-store:latest".into(),
        source: ImageSourceSpec::DockerStore {
            host: Some(engine.docker.host.clone()),
            tls: None,
            reference: Some(reference.to_string()),
        },
        platform: OciPlatform::host_linux(),
        policy,
        registries: BTreeMap::new(),
    };
    let acquirer = default_acquirer(&state, AcquireLimits::DEFAULT, &env);
    let got = acquirer
        .acquire(&request(AcquirePolicy::IfMissing, &base), &mut |_| {})
        .unwrap_or_else(|e| panic!("exporting {base} from the engine failed: {e}"));
    assert_eq!(got.identity.source, ImageSourceKind::DockerStore);
    assert_eq!(got.identity.platform, OciPlatform::host_linux());
    let cached = acquirer
        .acquire(&request(AcquirePolicy::CachedOnly, &base), &mut |_| {})
        .expect("a cached-only acquisition is served from awman's cache");
    assert_eq!(cached, got);
    let missing = acquirer
        .acquire(
            &request(AcquirePolicy::IfMissing, "awman-test/does-not-exist:none"),
            &mut |_| {},
        )
        .unwrap_err();
    assert!(
        matches!(missing, EngineError::SourceImageNotFound { .. }),
        "{missing:?}"
    );
    assert!(missing.to_string().contains("does-not-exist"));
    sandbox.finish();
}
