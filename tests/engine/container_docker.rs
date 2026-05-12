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

use std::process::{Command, Stdio};

use amux::engine::container::runtime::ContainerRuntime;

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
