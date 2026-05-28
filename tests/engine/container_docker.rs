//! Real-Docker tests for `ContainerRuntime` against a live daemon.
//!
//! Every test in this file is gated by `helpers::docker_available()` and skips
//! cleanly when Docker is not reachable. `make test-fast` skips them via the
//! `--skip docker` filter; `make test-full` includes them.
//!
//! Coverage (from WI 0073 §2e items 36–40):
//!   - `ContainerRuntime::is_available` matches reality
//!   - `image_exists(unknown)` returns false; round-trip after a pull returns true
//!   - `list_running_sync` succeeds and returns a vector (possibly empty)
//!   - End-to-end run-and-stop of the `hello-world` image via a raw `docker run`,
//!     then verify the runtime's view of running containers stays consistent.

use std::collections::HashMap;
use std::process::{Command, Stdio};

use awman::engine::container::runtime::ContainerRuntime;

use crate::helpers::docker_available;

fn try_pull(image: &str) -> bool {
    Command::new("docker")
        .args(["pull", image])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[test]
fn docker_runtime_is_available_matches_helpers_docker_available() {
    let runtime = ContainerRuntime::docker();
    assert_eq!(runtime.is_available(), docker_available());
}

#[test]
fn docker_runtime_runtime_name_is_docker() {
    let runtime = ContainerRuntime::docker();
    assert_eq!(runtime.runtime_name(), "docker");
}

#[test]
fn docker_image_exists_false_for_unknown_tag() {
    if !docker_available() {
        eprintln!("SKIP: Docker not available");
        return;
    }
    let runtime = ContainerRuntime::docker();
    assert!(
        !runtime.image_exists("amux-test-image-that-does-not-exist:latest"),
        "image_exists must return false for an unknown tag"
    );
}

#[test]
fn docker_image_exists_true_after_pull_hello_world() {
    if !docker_available() {
        eprintln!("SKIP: Docker not available");
        return;
    }
    if !try_pull("hello-world:latest") {
        eprintln!("SKIP: docker pull hello-world failed (no network?)");
        return;
    }
    let runtime = ContainerRuntime::docker();
    assert!(
        runtime.image_exists("hello-world:latest"),
        "hello-world should exist after pull"
    );
}

#[test]
fn docker_list_running_sync_returns_ok() {
    if !docker_available() {
        eprintln!("SKIP: Docker not available");
        return;
    }
    let runtime = ContainerRuntime::docker();
    let listed = runtime.list_running_sync();
    assert!(
        listed.is_ok(),
        "list_running_sync must succeed against a live daemon: {:?}",
        listed.err()
    );
}

// ─── BackgroundContainer integration tests ───────────────────────────────────

/// Pull `alpine:latest` if it is not already local. Returns false when the
/// pull fails (no network), allowing callers to skip gracefully.
fn try_pull_alpine() -> bool {
    try_pull("alpine:latest")
}

/// Start a background container, exec a command, kill it, and verify it is
/// gone from the Docker container list.
#[test]
fn docker_background_container_start_exec_kill() {
    if !docker_available() {
        eprintln!("SKIP: Docker not available");
        return;
    }
    if !try_pull_alpine() {
        eprintln!("SKIP: docker pull alpine:latest failed (no network?)");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let runtime = ContainerRuntime::docker();
    let env = HashMap::new();
    let overlays = [];

    let container = runtime
        .start_background("alpine:latest", tmp.path(), &env, &overlays)
        .expect("start_background must succeed");

    let container_id = container.container_id().to_string();

    let output = container
        .exec("echo hello", None)
        .expect("exec must succeed");
    assert_eq!(output.stdout, "hello\n");
    assert_eq!(output.exit_code, 0);

    container.kill().expect("kill must succeed");

    // After kill, the container must no longer appear in `docker ps -a`.
    let ps = Command::new("docker")
        .args([
            "ps",
            "-a",
            "--filter",
            &format!("id={}", &container_id[..12]),
            "--format",
            "{{.ID}}",
        ])
        .output()
        .expect("docker ps");
    let listed = String::from_utf8_lossy(&ps.stdout).trim().to_string();
    assert!(
        listed.is_empty(),
        "container must not appear in docker ps -a after kill; got: {listed:?}"
    );
}

/// Exec a command with a non-zero exit code inside a background container;
/// assert the exit code is surfaced correctly.
#[test]
fn docker_background_container_exec_nonzero_exit() {
    if !docker_available() {
        eprintln!("SKIP: Docker not available");
        return;
    }
    if !try_pull_alpine() {
        eprintln!("SKIP: docker pull alpine:latest failed");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let runtime = ContainerRuntime::docker();
    let env = HashMap::new();
    let overlays = [];

    let container = runtime
        .start_background("alpine:latest", tmp.path(), &env, &overlays)
        .expect("start_background must succeed");

    let output = container
        .exec("exit 1", None)
        .expect("exec call must not error");
    assert_eq!(output.exit_code, 1, "exit code must be 1");

    container.kill().expect("kill must succeed");
}

/// Env vars passed to `start_background` must be visible inside the container.
#[test]
fn docker_background_container_env_var_injected() {
    if !docker_available() {
        eprintln!("SKIP: Docker not available");
        return;
    }
    if !try_pull_alpine() {
        eprintln!("SKIP: docker pull alpine:latest failed");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let runtime = ContainerRuntime::docker();
    let mut env = HashMap::new();
    env.insert("FOO".to_string(), "bar".to_string());
    let overlays = [];

    let container = runtime
        .start_background("alpine:latest", tmp.path(), &env, &overlays)
        .expect("start_background must succeed");

    let output = container
        .exec("printenv FOO", None)
        .expect("exec must succeed");
    assert_eq!(
        output.stdout, "bar\n",
        "env var FOO must be bar inside container"
    );
    assert_eq!(output.exit_code, 0);

    container.kill().expect("kill must succeed");
}

/// Overlay specs passed to `start_background` must mount inside the container
/// at the requested container path. Mounts a host directory containing a known
/// file, then asserts the file is readable from inside via `cat`.
#[test]
fn docker_background_container_overlay_mount_applied() {
    use awman::engine::container::options::{OverlayPermission, OverlaySpec};

    if !docker_available() {
        eprintln!("SKIP: Docker not available");
        return;
    }
    if !try_pull_alpine() {
        eprintln!("SKIP: docker pull alpine:latest failed");
        return;
    }

    // Host: write a marker file the container should be able to read.
    let host_tmp = tempfile::tempdir().unwrap();
    let host_marker = host_tmp.path().join("marker.txt");
    std::fs::write(&host_marker, b"overlay-ok\n").unwrap();

    let workdir = tempfile::tempdir().unwrap();
    let runtime = ContainerRuntime::docker();
    let env = HashMap::new();
    let overlays = vec![OverlaySpec {
        host_path: host_tmp.path().to_path_buf(),
        container_path: std::path::PathBuf::from("/mnt/overlay-test"),
        permission: OverlayPermission::ReadOnly,
    }];

    let container = runtime
        .start_background("alpine:latest", workdir.path(), &env, &overlays)
        .expect("start_background must succeed with overlays");

    let output = container
        .exec("cat /mnt/overlay-test/marker.txt", None)
        .expect("exec must succeed");
    assert_eq!(output.exit_code, 0, "cat exit: stderr={}", output.stderr);
    assert_eq!(
        output.stdout, "overlay-ok\n",
        "overlay file must be readable inside container"
    );

    container.kill().expect("kill must succeed");
}

/// Attempting to start a background container with a non-existent image must
/// return an error whose message names the missing image.
#[test]
fn docker_background_container_image_not_found() {
    if !docker_available() {
        eprintln!("SKIP: Docker not available");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let runtime = ContainerRuntime::docker();
    let env = HashMap::new();
    let overlays = [];
    let bad_image = "amux-nonexistent-image-xyzzy:no-such-tag";

    let result = runtime.start_background(bad_image, tmp.path(), &env, &overlays);

    let err = match result {
        Ok(_) => panic!("start_background must fail for missing image"),
        Err(e) => e,
    };
    match &err {
        awman::engine::error::EngineError::ContainerImageNotFound { image } => {
            assert_eq!(image, bad_image, "variant must name the missing image");
        }
        other => panic!("expected ContainerImageNotFound, got: {other:?}"),
    }
    // Display should still mention the image so users see actionable text.
    assert!(err.to_string().contains(bad_image));
}

/// Run hello-world directly via `docker run`, wait for it to exit, then
/// confirm the runtime's view of running amux-labeled containers is unaffected
/// (hello-world is not amux-labeled, so it must NOT show up).
#[test]
fn docker_hello_world_run_does_not_appear_in_amux_listing() {
    if !docker_available() {
        eprintln!("SKIP: Docker not available");
        return;
    }
    if !try_pull("hello-world:latest") {
        eprintln!("SKIP: docker pull hello-world failed (no network?)");
        return;
    }

    let before = ContainerRuntime::docker()
        .list_running_sync()
        .expect("list_running_sync before");

    let status = Command::new("docker")
        .args(["run", "--rm", "hello-world:latest"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("docker run hello-world");
    assert!(status.success(), "docker run hello-world must succeed");

    let after = ContainerRuntime::docker()
        .list_running_sync()
        .expect("list_running_sync after");

    // hello-world isn't amux-labeled and exits immediately; the amux listing
    // should be unchanged in size.
    assert_eq!(
        before.len(),
        after.len(),
        "non-amux container must not appear in amux's labeled listing"
    );
}
