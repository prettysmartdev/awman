/// Integration tests for the overlay system (work item 0063).
///
/// These tests invoke the compiled `amux` binary to validate overlay-related
/// CLI behaviour: flag presence in help output, fatal errors on malformed input,
/// and non-fatal treatment of missing host paths.
///
/// Tests that would require a live Docker daemon (verifying that `-v` flags
/// appear in actual `docker run` invocations) are covered at the unit level in
/// `src/runtime/docker.rs` via `build_run_args_pty_with_overlays_adds_volume_flags`.
use std::process::Command;
use tempfile::TempDir;

fn amux() -> Command {
    Command::new(env!("CARGO_BIN_EXE_amux"))
}

/// Initialise a fresh git repo in a temp directory and return the TempDir.
fn make_git_repo() -> TempDir {
    let repo = TempDir::new().expect("TempDir::new");
    std::process::Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(repo.path())
        .status()
        .expect("git init");
    repo
}

// ─── --overlay flag presence in help text ────────────────────────────────────

#[test]
fn chat_help_shows_overlay_flag() {
    let output = amux()
        .args(["chat", "--help"])
        .output()
        .expect("failed to run amux chat --help");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("--overlay"),
        "chat --help must mention --overlay flag; got: {stdout}"
    );
}

#[test]
fn exec_prompt_help_shows_overlay_flag() {
    let output = amux()
        .args(["exec", "prompt", "--help"])
        .output()
        .expect("failed to run amux exec prompt --help");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("--overlay"),
        "exec prompt --help must mention --overlay flag; got: {stdout}"
    );
}

#[test]
fn exec_workflow_help_shows_overlay_flag() {
    let output = amux()
        .args(["exec", "workflow", "--help"])
        .output()
        .expect("failed to run amux exec workflow --help");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("--overlay"),
        "exec workflow --help must mention --overlay flag; got: {stdout}"
    );
}

// ─── malformed --overlay value is recognised (not an unknown flag) ────────────
//
// These tests verify that the `--overlay` flag is accepted by the CLI argument
// parser (clap). A malformed overlay *value* causes a fatal error somewhere in
// the command pipeline; the exact error depends on what check runs first
// (overlay parsing vs. agent-image availability). What matters at the
// integration level is:
//   1. The exit code is non-zero (no silent failure).
//   2. clap does NOT report the flag as "unrecognized" — that would mean
//      the flag was never wired up.
// Detailed overlay-error message checking is covered by the unit tests in
// `src/overlays/parser.rs` and `src/overlays/mod.rs`.

#[test]
fn malformed_overlay_flag_exits_nonzero_and_flag_is_recognized() {
    // "notvalid" has no opening parenthesis — a fatal parse error if overlay
    // parsing is reached.  Even if another check (e.g. agent availability) fires
    // first, the exit code must be non-zero and the flag must be recognized.
    let repo = make_git_repo();
    let output = amux()
        .current_dir(repo.path())
        .args(["chat", "--non-interactive", "--overlay", "notvalid"])
        .output()
        .expect("failed to run amux");

    assert!(
        !output.status.success(),
        "malformed --overlay must cause a non-zero exit"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    // Must NOT say the flag itself is unknown/unrecognized (that would mean
    // the --overlay flag was never declared in the CLI).
    assert!(
        !stderr.contains("unrecognized argument --overlay")
            && !stderr.contains("unexpected argument '--overlay'"),
        "--overlay must be a recognised flag; got: {stderr}"
    );
}

#[test]
fn unknown_type_tag_exits_nonzero_and_overlay_flag_is_recognized() {
    // "secret(...)" uses an unsupported type tag.  The --overlay flag itself
    // must be wired up (not "unrecognized"); exit must be non-zero.
    let repo = make_git_repo();
    let output = amux()
        .current_dir(repo.path())
        .args(["chat", "--non-interactive", "--overlay", "secret(/foo:/bar)"])
        .output()
        .expect("failed to run amux");

    assert!(
        !output.status.success(),
        "unknown overlay type tag must cause a non-zero exit"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("unrecognized argument --overlay")
            && !stderr.contains("unexpected argument '--overlay'"),
        "--overlay must be a recognised flag; got: {stderr}"
    );
}

#[test]
fn malformed_permission_exits_nonzero_and_overlay_flag_is_recognized() {
    // "rw2" is not a valid permission string.
    let repo = make_git_repo();
    let output = amux()
        .current_dir(repo.path())
        .args(["chat", "--non-interactive", "--overlay", "dir(/a:/b:rw2)"])
        .output()
        .expect("failed to run amux");

    assert!(
        !output.status.success(),
        "malformed permission 'rw2' must cause a non-zero exit"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("unrecognized argument --overlay")
            && !stderr.contains("unexpected argument '--overlay'"),
        "--overlay must be a recognised flag; got: {stderr}"
    );
}

// ─── syntactically valid overlay with missing host path is not a parse error ──

#[test]
fn valid_overlay_with_missing_host_path_does_not_cause_parse_error() {
    // /nonexistent-amux-test-overlay-path should not exist on any test machine.
    // The overlay is syntactically valid; host-path validation later warns and drops it.
    // The command may still fail (docker unavailable, etc.) but must not fail because of
    // an overlay *parse* error.
    let repo = make_git_repo();
    let output = amux()
        .current_dir(repo.path())
        .args([
            "chat",
            "--non-interactive",
            "--overlay",
            "dir(/nonexistent-amux-test-overlay-path:/mnt/x:ro)",
        ])
        .output()
        .expect("failed to run amux");

    let stderr = String::from_utf8_lossy(&output.stderr);
    // Must NOT report a *parse* error for the overlay expression.
    assert!(
        !stderr.contains("invalid permission") && !stderr.contains("malformed overlay"),
        "syntactically valid overlay must not trigger a parse error; stderr: {stderr}"
    );
}

// ─── AMUX_OVERLAYS env var ────────────────────────────────────────────────────

#[test]
fn malformed_amux_overlays_env_var_does_not_prevent_help() {
    // AMUX_OVERLAYS is only parsed when a command actually launches an agent.
    // The --help flag causes clap to exit before any overlay parsing happens,
    // so even a completely garbled env var must not prevent help from succeeding.
    let output = amux()
        .env("AMUX_OVERLAYS", "###not-an-overlay###")
        .args(["chat", "--help"])
        .output()
        .expect("failed to run amux chat --help");
    assert!(
        output.status.success(),
        "amux chat --help must exit 0 regardless of AMUX_OVERLAYS content; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn valid_amux_overlays_env_var_does_not_prevent_help() {
    // A well-formed AMUX_OVERLAYS also must not interfere with help.
    let output = amux()
        .env("AMUX_OVERLAYS", "dir(/tmp:/mnt/tmp:ro)")
        .args(["chat", "--help"])
        .output()
        .expect("failed to run amux chat --help");
    assert!(
        output.status.success(),
        "amux chat --help must exit 0 with a valid AMUX_OVERLAYS; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

// ─── malformed AMUX_OVERLAYS is now a fatal error (binary-level) ─────────────

#[test]
fn malformed_amux_overlays_env_var_causes_fatal_exit_on_command_run() {
    // Previously AMUX_OVERLAYS parse errors were silently dropped (warning only).
    // Now they must be fatal: the command must exit non-zero and the error message
    // must mention AMUX_OVERLAYS so the user knows what to fix.
    let repo = make_git_repo();
    let output = amux()
        .current_dir(repo.path())
        .env("AMUX_OVERLAYS", "###not-an-overlay###")
        .args(["chat", "--non-interactive"])
        .output()
        .expect("failed to run amux");

    assert!(
        !output.status.success(),
        "malformed AMUX_OVERLAYS must cause a non-zero exit when running a command"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("AMUX_OVERLAYS"),
        "error message must mention AMUX_OVERLAYS; got: {stderr}"
    );
}

// ─── overlay flag forwarding through help for exec workflow alias ─────────────

#[test]
fn exec_wf_alias_help_shows_overlay_flag() {
    // `exec wf` is an alias for `exec workflow`; the --overlay flag must appear there too.
    let output = amux()
        .args(["exec", "wf", "--help"])
        .output()
        .expect("failed to run amux exec wf --help");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("--overlay"),
        "exec wf --help must mention --overlay flag; got: {stdout}"
    );
}

// ─── docker args integration tests (library-level, no daemon required) ────────
//
// These tests exercise the full path from overlay resolution through to the
// docker run arguments that would be passed to the Docker daemon.  They do NOT
// require a live Docker daemon because they call `build_run_args_pty` (which
// assembles the argument list without executing anything).
mod docker_args {
    use amux::config::{save_repo_config, DirectoryOverlayConfig, OverlaysConfig, RepoConfig};
    use amux::overlays::resolve_overlays;
    use amux::runtime::{AgentRuntime, DockerRuntime, HostSettings};
    use std::path::PathBuf;
    use tempfile::TempDir;

    /// Serialise tests that mutate process-global env vars.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn rt() -> DockerRuntime {
        DockerRuntime::new()
    }

    /// `--overlay` flag resolves and appears as `-v host:container:ro` in docker run args.
    #[test]
    fn overlay_flag_produces_v_flag_in_docker_run_args() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let repo = TempDir::new().unwrap();
        let host_dir = TempDir::new().unwrap(); // must exist so resolve_overlays keeps it

        let fake_home = TempDir::new().unwrap();
        unsafe { std::env::set_var("AMUX_CONFIG_HOME", fake_home.path().to_str().unwrap()) };
        let prev_env = std::env::var("AMUX_OVERLAYS").ok();
        unsafe { std::env::remove_var("AMUX_OVERLAYS") };

        let flag = format!("dir({}:/mnt/test:ro)", host_dir.path().display());
        let overlays = resolve_overlays(repo.path(), &[flag]).unwrap();

        unsafe { std::env::remove_var("AMUX_CONFIG_HOME") };
        if let Some(v) = prev_env {
            unsafe { std::env::set_var("AMUX_OVERLAYS", v) };
        }

        assert_eq!(overlays.len(), 1, "expected 1 overlay; got {overlays:?}");

        let mut settings = HostSettings::from_paths(
            PathBuf::from("/fake/claude.json"),
            PathBuf::from("/fake/dot-claude"),
        );
        settings.set_overlays(overlays);

        let args = rt().build_run_args_pty("img", "/h", &[], &[], Some(&settings), false, None, None);
        let expected = format!("{}:/mnt/test:ro", host_dir.path().display());
        assert!(
            args.windows(2).any(|w| w[0] == "-v" && w[1] == expected),
            "expected -v {expected} in docker args; got {args:?}"
        );
    }

    /// `AMUX_OVERLAYS` env var resolves and appears as `-v` in docker run args.
    #[test]
    fn amux_overlays_env_var_produces_v_flag_in_docker_run_args() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let repo = TempDir::new().unwrap();
        let host_dir = TempDir::new().unwrap();

        let fake_home = TempDir::new().unwrap();
        unsafe { std::env::set_var("AMUX_CONFIG_HOME", fake_home.path().to_str().unwrap()) };
        let env_val = format!("dir({}:/mnt/env:ro)", host_dir.path().display());
        unsafe { std::env::set_var("AMUX_OVERLAYS", &env_val) };

        let overlays = resolve_overlays(repo.path(), &[]).unwrap();

        unsafe { std::env::remove_var("AMUX_OVERLAYS") };
        unsafe { std::env::remove_var("AMUX_CONFIG_HOME") };

        assert_eq!(overlays.len(), 1, "expected 1 overlay from AMUX_OVERLAYS; got {overlays:?}");

        let mut settings = HostSettings::from_paths(
            PathBuf::from("/fake/claude.json"),
            PathBuf::from("/fake/dot-claude"),
        );
        settings.set_overlays(overlays);

        let args = rt().build_run_args_pty("img", "/h", &[], &[], Some(&settings), false, None, None);
        let expected = format!("{}:/mnt/env:ro", host_dir.path().display());
        assert!(
            args.windows(2).any(|w| w[0] == "-v" && w[1] == expected),
            "expected -v {expected} in docker args from AMUX_OVERLAYS; got {args:?}"
        );
    }

    /// `--overlay` flag overrides project config for the same host path;
    /// only the flag's container path appears in the docker args.
    #[test]
    fn flag_overlay_overrides_project_config_for_same_host_path() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let repo = TempDir::new().unwrap();
        let host_dir = TempDir::new().unwrap();

        let fake_home = TempDir::new().unwrap();
        unsafe { std::env::set_var("AMUX_CONFIG_HOME", fake_home.path().to_str().unwrap()) };
        let prev_env = std::env::var("AMUX_OVERLAYS").ok();
        unsafe { std::env::remove_var("AMUX_OVERLAYS") };

        // Project config maps host_dir → /mnt/from-config
        let config = RepoConfig {
            overlays: Some(OverlaysConfig {
                directories: Some(vec![DirectoryOverlayConfig {
                    host: host_dir.path().to_string_lossy().to_string(),
                    container: "/mnt/from-config".to_string(),
                    permission: Some("ro".to_string()),
                }]),
            }),
            ..Default::default()
        };
        save_repo_config(repo.path(), &config).unwrap();

        // Flag maps same host_dir → /mnt/from-flag (higher priority)
        let flag = format!("dir({}:/mnt/from-flag:ro)", host_dir.path().display());
        let overlays = resolve_overlays(repo.path(), &[flag]).unwrap();

        unsafe { std::env::remove_var("AMUX_CONFIG_HOME") };
        if let Some(v) = prev_env {
            unsafe { std::env::set_var("AMUX_OVERLAYS", v) };
        }

        assert_eq!(overlays.len(), 1, "same host path must deduplicate; got {overlays:?}");
        assert_eq!(
            overlays[0].container_path,
            PathBuf::from("/mnt/from-flag"),
            "flag container_path must win over project config"
        );

        let mut settings = HostSettings::from_paths(
            PathBuf::from("/fake/claude.json"),
            PathBuf::from("/fake/dot-claude"),
        );
        settings.set_overlays(overlays);

        let args = rt().build_run_args_pty("img", "/h", &[], &[], Some(&settings), false, None, None);
        let flag_v = format!("{}:/mnt/from-flag:ro", host_dir.path().display());
        let config_v = format!("{}:/mnt/from-config:ro", host_dir.path().display());

        assert!(
            args.windows(2).any(|w| w[0] == "-v" && w[1] == flag_v),
            "flag container path must appear in docker args; got {args:?}"
        );
        assert!(
            !args.windows(2).any(|w| w[0] == "-v" && w[1] == config_v),
            "config container path must not appear in docker args; got {args:?}"
        );
    }

    /// Missing host path is dropped and does not appear as a `-v` flag.
    #[test]
    fn missing_host_path_absent_from_docker_args() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let repo = TempDir::new().unwrap();

        let fake_home = TempDir::new().unwrap();
        unsafe { std::env::set_var("AMUX_CONFIG_HOME", fake_home.path().to_str().unwrap()) };
        let prev_env = std::env::var("AMUX_OVERLAYS").ok();
        unsafe { std::env::remove_var("AMUX_OVERLAYS") };

        let flags = vec!["dir(/nonexistent-amux-path-xyz:/mnt/x:ro)".to_string()];
        let overlays = resolve_overlays(repo.path(), &flags).unwrap();

        unsafe { std::env::remove_var("AMUX_CONFIG_HOME") };
        if let Some(v) = prev_env {
            unsafe { std::env::set_var("AMUX_OVERLAYS", v) };
        }

        assert!(overlays.is_empty(), "non-existent host path must be dropped; got {overlays:?}");

        // With empty overlays the docker args must have no /mnt/x -v mount.
        let settings = HostSettings::from_paths(
            PathBuf::from("/fake/claude.json"),
            PathBuf::from("/fake/dot-claude"),
        );
        let args = rt().build_run_args_pty("img", "/h", &[], &[], Some(&settings), false, None, None);
        assert!(
            !args.windows(2).any(|w| w[0] == "-v" && w[1].contains("/mnt/x")),
            "missing-path overlay must not appear in docker args; got {args:?}"
        );
    }

    /// Malformed `AMUX_OVERLAYS` is a fatal error (not silently dropped).
    #[test]
    fn malformed_amux_overlays_env_var_is_fatal_error() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let repo = TempDir::new().unwrap();

        let fake_home = TempDir::new().unwrap();
        unsafe { std::env::set_var("AMUX_CONFIG_HOME", fake_home.path().to_str().unwrap()) };
        unsafe { std::env::set_var("AMUX_OVERLAYS", "###not-an-overlay###") };

        let result = resolve_overlays(repo.path(), &[]);

        unsafe { std::env::remove_var("AMUX_OVERLAYS") };
        unsafe { std::env::remove_var("AMUX_CONFIG_HOME") };

        assert!(result.is_err(), "malformed AMUX_OVERLAYS must be a fatal error; got Ok");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("AMUX_OVERLAYS"),
            "error must mention AMUX_OVERLAYS; got: {msg}"
        );
    }

    /// Malformed permission in project config is a fatal error (not silently defaulted).
    #[test]
    fn malformed_config_permission_is_fatal_error() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let repo = TempDir::new().unwrap();
        let host_dir = TempDir::new().unwrap();

        let fake_home = TempDir::new().unwrap();
        unsafe { std::env::set_var("AMUX_CONFIG_HOME", fake_home.path().to_str().unwrap()) };
        let prev_env = std::env::var("AMUX_OVERLAYS").ok();
        unsafe { std::env::remove_var("AMUX_OVERLAYS") };

        let config = RepoConfig {
            overlays: Some(OverlaysConfig {
                directories: Some(vec![DirectoryOverlayConfig {
                    host: host_dir.path().to_string_lossy().to_string(),
                    container: "/mnt/data".to_string(),
                    permission: Some("rwx".to_string()), // invalid
                }]),
            }),
            ..Default::default()
        };
        save_repo_config(repo.path(), &config).unwrap();

        let result = resolve_overlays(repo.path(), &[]);

        unsafe { std::env::remove_var("AMUX_CONFIG_HOME") };
        if let Some(v) = prev_env {
            unsafe { std::env::set_var("AMUX_OVERLAYS", v) };
        }

        assert!(
            result.is_err(),
            "malformed permission in project config must be a fatal error; got Ok"
        );
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("rwx") || msg.contains("permission"),
            "error must mention the bad value; got: {msg}"
        );
    }
}
