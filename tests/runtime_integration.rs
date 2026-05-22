/// Integration and end-to-end tests for the runtime abstraction layer (work item 0042).
///
/// Test categories:
/// - `DockerRuntime` trait integration (requires Docker daemon, gated by `docker_available()`)
/// - `AppleContainersRuntime` integration (macOS-only, opt-in via `AMUX_TEST_APPLE_CONTAINERS=1`)
/// - End-to-end CLI checks via the compiled `amux` binary
/// - Error path: unavailable runtime produces a user-facing message, not a panic
use awman::runtime::AgentRuntime;
use std::process::{Command, Stdio};

// ─── helpers ─────────────────────────────────────────────────────────────────

fn amux() -> Command {
    Command::new(env!("CARGO_BIN_EXE_awman"))
}

fn docker_available() -> bool {
    Command::new("docker")
        .args(["info"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

// ─── DockerRuntime unit-level integration ────────────────────────────────────

/// `DockerRuntime` must be constructible and expose the correct metadata regardless
/// of whether Docker is actually installed — these are pure in-process calls.
#[test]
fn docker_runtime_name_is_docker() {
    let rt = awman::runtime::DockerRuntime::new();
    assert_eq!(rt.name(), "docker");
}

#[test]
fn docker_runtime_cli_binary_is_docker() {
    let rt = awman::runtime::DockerRuntime::new();
    assert_eq!(rt.cli_binary(), "docker");
}

/// `is_available()` must always return a `bool` — never panic — regardless of
/// whether Docker is installed or the daemon is running.
#[test]
fn docker_runtime_is_available_returns_bool_without_panic() {
    let rt = awman::runtime::DockerRuntime::new();
    let _: bool = rt.is_available(); // must not panic
}

/// When Docker is present, `is_available()` must return `true`.
#[test]
fn docker_runtime_is_available_when_daemon_running() {
    if !docker_available() {
        eprintln!("Docker not available, skipping");
        return;
    }
    let rt = awman::runtime::DockerRuntime::new();
    assert!(rt.is_available());
}

/// `image_exists` must return `false` for an image that definitely does not
/// exist locally — and must not panic.
#[test]
fn docker_runtime_image_exists_false_for_nonexistent_image() {
    if !docker_available() {
        eprintln!("Docker not available, skipping");
        return;
    }
    let rt = awman::runtime::DockerRuntime::new();
    assert!(!rt.image_exists("amux-test-nonexistent-image-zzz-99999:latest"));
}

/// Requesting a stopped container that does not exist must return `None`, not panic.
#[test]
fn docker_runtime_find_stopped_container_returns_none_when_missing() {
    if !docker_available() {
        eprintln!("Docker not available, skipping");
        return;
    }
    let rt = awman::runtime::DockerRuntime::new();
    let result = rt.find_stopped_container("amux-test-nonexistent-xyz", "no-such-image:latest");
    assert!(result.is_none());
}

/// `list_running_containers_by_prefix` must return an empty vec (not panic) when
/// no containers match.
#[test]
fn docker_runtime_list_containers_by_prefix_returns_empty_when_none_match() {
    if !docker_available() {
        eprintln!("Docker not available, skipping");
        return;
    }
    let rt = awman::runtime::DockerRuntime::new();
    let names = rt.list_running_containers_by_prefix("amux-test-nonexistent-prefix-zzz");
    assert!(names.is_empty());
}

/// `query_container_stats` must return `None` (not panic) for a container that
/// is not running.
#[test]
fn docker_runtime_query_stats_returns_none_for_nonexistent_container() {
    if !docker_available() {
        eprintln!("Docker not available, skipping");
        return;
    }
    let rt = awman::runtime::DockerRuntime::new();
    let stats = rt.query_container_stats("amux-test-nonexistent-container-zzz");
    assert!(stats.is_none());
}

// ─── resolve_runtime integration ─────────────────────────────────────────────

/// `resolve_runtime` with `runtime: None` must produce a runtime whose name is
/// "docker" — accessible from the integration test crate.
#[test]
fn resolve_runtime_default_resolves_to_docker() {
    let config = awman::config::GlobalConfig { runtime: None, ..Default::default() };
    let rt = awman::runtime::resolve_runtime(&config).unwrap();
    assert_eq!(rt.name(), "docker");
}

/// An unknown runtime name must not panic — it falls back to Docker with a warning.
#[test]
fn resolve_runtime_unknown_string_does_not_panic() {
    let config = awman::config::GlobalConfig {
        runtime: Some("nonexistent-runtime".into()),
        ..Default::default()
    };
    let rt = awman::runtime::resolve_runtime(&config).unwrap();
    assert_eq!(rt.name(), "docker");
}

/// On non-macOS, requesting apple-containers must return Err — not a fallback.
#[cfg(not(target_os = "macos"))]
#[test]
fn resolve_runtime_apple_containers_on_non_macos_returns_err() {
    let config = awman::config::GlobalConfig {
        runtime: Some("apple-containers".into()),
        ..Default::default()
    };
    let err = awman::runtime::resolve_runtime(&config)
        .err()
        .expect("apple-containers must be rejected on non-macOS, got Ok");
    let msg = err.to_string();
    assert!(msg.contains("macOS"), "error must mention macOS: {}", msg);
}

// ─── Error path integration tests ────────────────────────────────────────────

/// start_container must return Err (not panic) for a nonexistent container.
#[test]
fn docker_runtime_start_nonexistent_container_returns_err() {
    if !docker_available() {
        eprintln!("Docker not available, skipping");
        return;
    }
    let rt = awman::runtime::DockerRuntime::new();
    let result = rt.start_container("amux-test-nonexistent-container-zzz-99999");
    assert!(result.is_err(), "start_container must return Err for nonexistent container");
    let msg = result.unwrap_err().to_string();
    assert!(!msg.is_empty(), "error message must not be empty");
}

/// stop_container must return Err (not panic) for a nonexistent container.
#[test]
fn docker_runtime_stop_nonexistent_container_returns_err() {
    if !docker_available() {
        eprintln!("Docker not available, skipping");
        return;
    }
    let rt = awman::runtime::DockerRuntime::new();
    let result = rt.stop_container("amux-test-nonexistent-container-zzz-99999");
    assert!(result.is_err(), "stop_container must return Err for nonexistent container");
}

/// remove_container must return Err (not panic) for a nonexistent container.
#[test]
fn docker_runtime_remove_nonexistent_container_returns_err() {
    if !docker_available() {
        eprintln!("Docker not available, skipping");
        return;
    }
    let rt = awman::runtime::DockerRuntime::new();
    let result = rt.remove_container("amux-test-nonexistent-container-zzz-99999");
    assert!(result.is_err(), "remove_container must return Err for nonexistent container");
}

/// get_container_workspace_mount must return None (not panic) for a nonexistent container.
#[test]
fn docker_runtime_get_workspace_mount_returns_none_for_nonexistent() {
    if !docker_available() {
        eprintln!("Docker not available, skipping");
        return;
    }
    let rt = awman::runtime::DockerRuntime::new();
    let result = rt.get_container_workspace_mount("amux-test-nonexistent-container-zzz-99999");
    assert!(result.is_none());
}

// ─── End-to-end: amux ready ──────────────────────────────────────────────────

/// `amux ready --non-interactive` must not panic regardless of whether Docker is
/// available — it should print a user-facing message and exit cleanly.
#[test]
fn amux_ready_non_interactive_does_not_panic() {
    let output = amux()
        .args(["ready", "--non-interactive"])
        .output()
        .expect("failed to invoke amux");

    // We do not assert success/failure here (depends on Docker availability),
    // but the process must terminate with a normal exit code (not a signal/panic).
    let exit_code = output.status.code();
    assert!(
        exit_code.is_some(),
        "amux ready should exit normally, not be killed by a signal"
    );
}

/// When Docker is available, `amux ready --non-interactive` must mention the
/// runtime name in its output.
#[test]
fn amux_ready_non_interactive_mentions_runtime_name_when_docker_available() {
    if !docker_available() {
        eprintln!("Docker not available, skipping");
        return;
    }

    let output = amux()
        .args(["ready", "--non-interactive"])
        .output()
        .expect("failed to invoke amux");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = format!("{}{}", stdout, stderr);

    assert!(
        combined.contains("docker"),
        "Expected 'docker' in output when runtime is docker. Got:\n{}",
        combined
    );
}

/// When the configured runtime is unavailable, `amux ready --non-interactive`
/// must produce a user-facing error message rather than an opaque OS error or panic.
#[test]
fn amux_ready_reports_user_facing_error_when_runtime_unavailable() {
    if docker_available() {
        // This test only makes sense when Docker is not running.
        // If Docker IS available the test is vacuously skipped.
        eprintln!("Docker is available, skipping unavailability error test");
        return;
    }

    let output = amux()
        .args(["ready", "--non-interactive"])
        .output()
        .expect("failed to invoke amux");

    assert!(
        !output.status.success(),
        "Expected non-zero exit when runtime unavailable"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let combined = format!("{}{}", stdout, stderr);

    // Must mention the runtime name in the error; must not be a raw OS error.
    assert!(
        combined.contains("docker") || combined.contains("runtime"),
        "Expected user-facing error mentioning runtime. Got:\n{}",
        combined
    );
    // Must not be a raw panic backtrace.
    assert!(
        !combined.contains("thread 'main' panicked"),
        "amux must not panic when runtime is unavailable. Got:\n{}",
        combined
    );
}

// ─── AppleContainersRuntime integration (macOS, opt-in) ──────────────────────

/// These tests only run on macOS and only when `AMUX_TEST_APPLE_CONTAINERS=1` is set,
/// because they require the `container` CLI (macOS 26+) to be installed.
#[cfg(target_os = "macos")]
mod apple_containers {
    use super::*;

    fn apple_available() -> bool {
        std::env::var("AMUX_TEST_APPLE_CONTAINERS").as_deref() == Ok("1")
    }

    fn container_cli_present() -> bool {
        Command::new("container")
            .args(["info"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    #[test]
    fn apple_runtime_name_and_binary_without_daemon() {
        // Metadata is always available, even if the daemon is not running.
        let rt = awman::runtime::apple::AppleContainersRuntime::new();
        assert_eq!(rt.name(), "apple-containers");
        assert_eq!(rt.cli_binary(), "container");
    }

    #[test]
    fn apple_runtime_is_available_returns_bool_without_panic() {
        let rt = awman::runtime::apple::AppleContainersRuntime::new();
        let _: bool = rt.is_available();
    }

    #[test]
    fn apple_runtime_is_available_when_installed() {
        if !apple_available() || !container_cli_present() {
            eprintln!("Apple Containers not available or AMUX_TEST_APPLE_CONTAINERS not set, skipping");
            return;
        }
        let rt = awman::runtime::apple::AppleContainersRuntime::new();
        assert!(rt.is_available());
    }

    #[test]
    fn apple_runtime_image_exists_false_for_nonexistent_image() {
        if !apple_available() || !container_cli_present() {
            eprintln!("Apple Containers not available, skipping");
            return;
        }
        let rt = awman::runtime::apple::AppleContainersRuntime::new();
        assert!(!rt.image_exists("amux-test-nonexistent-image-zzz:latest"));
    }

    #[test]
    fn apple_runtime_find_stopped_container_returns_none_when_missing() {
        if !apple_available() || !container_cli_present() {
            eprintln!("Apple Containers not available, skipping");
            return;
        }
        let rt = awman::runtime::apple::AppleContainersRuntime::new();
        let result = rt.find_stopped_container("amux-test-nonexistent", "no-such-image:latest");
        assert!(result.is_none());
    }

    #[test]
    fn apple_runtime_list_containers_by_prefix_returns_empty_when_none_match() {
        if !apple_available() || !container_cli_present() {
            eprintln!("Apple Containers not available, skipping");
            return;
        }
        let rt = awman::runtime::apple::AppleContainersRuntime::new();
        let names = rt.list_running_containers_by_prefix("amux-test-nonexistent-prefix-zzz");
        assert!(names.is_empty());
    }

    /// `amux ready --non-interactive` with Apple Containers configured must produce
    /// "apple-containers" in the output (not "docker").
    #[test]
    fn amux_ready_with_apple_containers_config_mentions_runtime() {
        if !apple_available() || !container_cli_present() {
            eprintln!("Apple Containers not available, skipping");
            return;
        }
        // Write a minimal global config pointing to apple-containers.
        // The global config lives at $HOME/.amux/config.json.
        let tmp = tempfile::tempdir().unwrap();
        let amux_dir = tmp.path().join(".amux");
        std::fs::create_dir_all(&amux_dir).unwrap();
        let config_path = amux_dir.join("config.json");
        std::fs::write(
            &config_path,
            r#"{"runtime":"apple-containers"}"#,
        )
        .unwrap();

        let output = amux()
            .args(["ready", "--non-interactive"])
            .env("HOME", tmp.path())
            .output()
            .expect("failed to invoke amux");

        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );

        assert!(
            combined.contains("apple-containers"),
            "Expected 'apple-containers' in output. Got:\n{}",
            combined
        );
    }
}
