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

/// An image with Git for the session credential socket proof: the
/// `docker-git-image` host resource, or a pinned `alpine/git`.
fn git_image(engine: &Engine) -> String {
    let reference = awman_test_support::runtime_env::get()
        .host_resource("docker-git-image")
        .filter(|r| !r.is_empty())
        .unwrap_or("alpine/git:v2.45.2")
        .to_string();
    let present =
        engine.has_image(&reference) || engine.run(&["pull", "--quiet", &reference]).exit.success();
    assert!(
        present,
        "BLOCKED[docker]: Git image {reference} is neither present nor pullable; \
         name one with docker-git-image=<reference> in AWMAN_TEST_HOST_RESOURCES"
    );
    reference
}

/// WI 0145: a teardown/setup Git step of a `git_auth` push session reaches
/// a container-local `credential-cache` socket seeded over private stdin, gets
/// the credential for the session's repository only, and the token is never
/// in the container's environment. Runs the exact helper configuration
/// `push_branch` renders (`PushAuth::SessionSocket`).
#[cfg(target_os = "linux")]
#[test]
fn teardown_git_uses_the_private_credential_cache_only_for_the_session_host() {
    use awman::data::git_transport::{GitAuthKind, GitAuthSpec};
    use awman::engine::git::credential_socket::{CacheSocketListener, CONTAINER_CACHE_SOCKET};
    use awman::engine::git::lease_transport::CredentialScope;
    use awman::engine::workflow::step_commands::{transport_git, PushAuth, StepCommandOptions};

    let mut sandbox = support::sandbox(
        "docker::teardown_git_uses_the_private_credential_cache_only_for_the_session_host",
    );
    let engine = Engine::grant(&mut sandbox);
    let image = git_image(&engine);
    let token = "test-secret-container-socket";
    engine.sandbox.redact(token);
    let listener = CacheSocketListener::start(
        &engine.sandbox.root().join("r"),
        std::sync::Arc::new(
            GitAuthSpec::new(GitAuthKind::Token, None, SecretString::new(token)).unwrap(),
        ),
        CredentialScope::for_http_url("https://git.example/team/repo.git").unwrap(),
    )
    .expect("the per-command listener starts");
    let runtime = ContainerRuntime::docker_in(engine.snapshot());
    let env = HashMap::from([("GIT_TERMINAL_PROMPT".to_string(), "0".to_string())]);
    let workdir = engine.sandbox.work();
    let name = "awman-boundary-git-socket";
    let container = runtime
        .launch_background(&BackgroundLaunchRequest {
            secret_env: &[],
            name: Some(name),
            image: &image,
            workdir: &workdir,
            env: &env,
            overlays: &[],
            builtin_launch: None,
        })
        .expect("a background container starts without host socket mounts");
    let cleanup = engine::Cleanup::new(&engine);
    cleanup.container(name);
    container
        .seed_git_credentials(&listener.cache_seed())
        .unwrap();
    let git = transport_git(&StepCommandOptions {
        push_auth: PushAuth::SessionSocket {
            socket: CONTAINER_CACHE_SOCKET.into(),
        },
        remote_url: Some("https://git.example/team/repo.git".into()),
    });
    let fill = |request: &str| {
        container
            .exec(
                &format!("printf '{request}' | {git} credential fill 2>&1; echo \"exit=$?\""),
                None,
            )
            .expect("exec runs git credential fill")
            .stdout
    };
    let session = fill("protocol=https\\nhost=git.example\\npath=team/repo.git\\n\\n");
    let other_host = fill("protocol=https\\nhost=evil.example\\npath=team/repo.git\\n\\n");
    let other_repo = fill("protocol=https\\nhost=git.example\\npath=team/other.git\\n\\n");
    let environment = engine.run_ok(&["inspect", "--format", "{{json .Config.Env}}", name]);
    container.kill().expect("the container is removed");
    assert!(
        session.contains(&format!("password={token}")) && session.contains("exit=0"),
        "the session repository got no credential: {session}"
    );
    for refused in [&other_host, &other_repo] {
        assert!(!refused.contains(token), "another repository got the token");
        assert!(!refused.contains("exit=0"), "{refused}");
    }
    assert!(
        !environment.contains(token),
        "the token is in the container env"
    );
    drop(cleanup);
    drop(listener);
    sandbox.finish();
}
