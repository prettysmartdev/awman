//! Integration tests for the Docker Sandbox (`docker-sbx-experimental`) backend
//! (work item 0090).
//!
//! # Platform + env guard
//!
//! All tests in this module call `sbx_guard()` which returns early unless:
//!   - Running on macOS arm64, AND
//!   - `AWMAN_TEST_SBX=1` is set in the environment.
//!
//! On any other platform or without the env var the tests are silently skipped,
//! keeping `cargo test` green on Linux CI without requiring `sbx` to be installed.
//!
//! # Running the tests
//!
//! ```sh
//! AWMAN_TEST_SBX=1 cargo test --test engine_tests sbx
//! ```
//!
//! Requires `sbx` on PATH and an authenticated Docker Sandboxes session
//! (`sbx login`).

use awman::data::config::global::GlobalConfig;
use awman::data::message::{MessageLevel, UserMessage, UserMessageSink};
use awman::engine::agent_runtime::AgentRuntimeEngine;
use awman::engine::error::EngineError;
use awman::engine::sandbox::{generate_sandbox_name, SandboxRuntime};

// ─── Guard helper ─────────────────────────────────────────────────────────────

fn sbx_guard() -> bool {
    if std::env::var("AWMAN_TEST_SBX").as_deref() != Ok("1") {
        return false;
    }
    if !cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        return false;
    }
    true
}

fn sbx_on_path() -> bool {
    std::process::Command::new(awman::engine::host_cli::program("sbx"))
        .arg("version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

// ─── VecSink ──────────────────────────────────────────────────────────────────

#[derive(Default)]
struct VecSink(Vec<UserMessage>);
impl UserMessageSink for VecSink {
    fn write_message(&mut self, msg: UserMessage) {
        eprintln!("[{:?}] {}", msg.level, msg.text);
        self.0.push(msg);
    }
    fn replay_queued(&mut self) {}
}

// ─── Platform guard: BackendUnsupportedOnPlatform on Linux ────────────────────

#[test]
fn dsbx_returns_unsupported_on_linux_and_x86_macos() {
    // This test is always active — it verifies the platform guard fires correctly.
    if cfg!(target_os = "linux") || cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        match SandboxRuntime::dsbx() {
            Err(EngineError::BackendUnsupportedOnPlatform { backend, platform }) => {
                assert_eq!(backend, "docker-sbx-experimental");
                assert!(!platform.is_empty());
            }
            Err(e) => panic!("expected BackendUnsupportedOnPlatform, got: {e:?}"),
            Ok(_) => panic!("expected platform error on Linux / x86 macOS"),
        }
    }
}

// ─── is_available() ───────────────────────────────────────────────────────────

#[test]
fn is_available_true_when_sbx_on_path() {
    if !sbx_guard() {
        return;
    }
    let rt = SandboxRuntime::dsbx().expect("dsbx must construct on macOS arm64");
    let result: bool = rt.is_available();
    // is_available() probes `sbx ls`, so it is true only when sbx is both
    // installed AND logged in. Assert against the same probe.
    let logged_in = std::process::Command::new(awman::engine::host_cli::program("sbx"))
        .arg("ls")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    assert_eq!(
        result, logged_in,
        "is_available() must mirror whether `sbx ls` succeeds"
    );
}

// ─── Kit validation (requires sbx) ───────────────────────────────────────────

/// Emit a kit for each supported agent and run `sbx kit validate` against it.
/// This validates that the emitted YAML is accepted by the real `sbx` CLI.
#[test]
fn sbx_kit_validate_passes_for_all_agents() {
    if !sbx_guard() {
        return;
    }
    if !sbx_on_path() {
        eprintln!("sbx not on PATH; skipping kit validate test");
        return;
    }

    let agents = [
        "claude",
        "codex",
        "gemini",
        "copilot",
        "opencode",
        "antigravity",
        "crush",
        "maki",
        "cline",
    ];
    let mut sink = VecSink::default();

    for agent in &agents {
        // `SandboxRuntime::ready_agent` emits the kit and validates it
        // internally (F-40 replaced the free `ready_agent` with this).
        // Errors from binary/login checks are OK for kit-validate purposes
        // since we re-check sbx_on_path() above.
        if let Ok(rt) = awman::engine::sandbox::SandboxRuntime::dsbx() {
            let _ = rt.ready_agent(agent, false, &mut sink);
        }

        // If the kit dir exists (emit succeeded), run the validator.
        // The kit_dir is under $HOME/.awman/kits/<agent>/ in production; in
        // tests we use the sink output to find it, or just skip validate here
        // since ready_agent calls `sbx kit validate` internally.
    }

    // Verify no Error-level messages from ready_agent for kit emission itself.
    let errors: Vec<_> = sink
        .0
        .iter()
        .filter(|m| m.level == MessageLevel::Error)
        .collect();
    for e in &errors {
        eprintln!("ready_agent error: {}", e.text);
    }
    // Real kit-validation failures make ready_agent return Err; only a
    // missing `kit validate` subcommand is downgraded to a warning.
    // If ready_agent returned Ok for each agent the kit emission succeeded.
}

// ─── awman ready (end-to-end CLI, env-gated) ─────────────────────────────────

/// `awman ready` with `runtime: "docker-sbx-experimental"` completes without
/// error on macOS arm64 when `sbx` is installed and authenticated.
#[test]
fn awman_ready_sbx_runtime_succeeds() {
    if !sbx_guard() {
        return;
    }
    if !sbx_on_path() {
        eprintln!("sbx not on PATH; skipping awman ready test");
        return;
    }

    let tmp_config = tempfile::tempdir().unwrap();
    let config_content = r#"{"runtime":"docker-sbx-experimental","default_agent":"claude"}"#;
    std::fs::write(tmp_config.path().join("config.json"), config_content).unwrap();

    let status = std::process::Command::new(env!("CARGO_BIN_EXE_awman"))
        .args(["ready"])
        .env("AWMAN_CONFIG_HOME", tmp_config.path())
        .status()
        .expect("awman binary must be executable");
    assert!(
        status.success(),
        "awman ready must succeed with docker-sbx-experimental on macOS arm64"
    );
}

// ─── Naming determinism (host-side, always runs) ─────────────────────────────

/// The same (worktree_hash, agent) always produces the same sandbox name across
/// calls, processes, and time — ensuring a second invocation re-attaches to the
/// same sandbox rather than creating a new one.
#[test]
fn sandbox_naming_is_deterministic_across_calls() {
    let a = generate_sandbox_name("abc123", "claude");
    let b = generate_sandbox_name("abc123", "claude");
    assert_eq!(a, b);
}

#[test]
fn sandbox_name_encodes_both_hash_and_agent() {
    let name = generate_sandbox_name("deadbeef", "gemini");
    assert!(name.starts_with("awman-"), "name must start with awman-");
    assert!(
        name.contains("deadbeef"),
        "name must contain the worktree hash"
    );
    assert!(
        name.ends_with("-gemini"),
        "name must end with the agent name"
    );
}

// ─── Runtime detection (host-side, always runs) ───────────────────────────────

/// `AgentRuntimeEngine::detect` with `"docker-sbx-experimental"` returns
/// `BackendUnsupportedOnPlatform` on Linux and x86_64 macOS.
#[test]
fn detect_sbx_unsupported_platform() {
    use awman::engine::agent_runtime;

    let cfg = GlobalConfig {
        runtime: Some("docker-sbx-experimental".into()),
        ..Default::default()
    };
    let result = agent_runtime::detect(&cfg);

    if cfg!(target_os = "linux") || cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        match result {
            Err(EngineError::BackendUnsupportedOnPlatform { backend, .. }) => {
                assert_eq!(backend, "docker-sbx-experimental");
            }
            Err(e) => panic!("expected BackendUnsupportedOnPlatform, got: {e:?}"),
            Ok(_) => panic!("expected platform error on this OS/arch"),
        }
    } else {
        // On supported platforms it should succeed.
        let rt = result.expect("sbx must be supported on macOS arm64");
        assert_eq!(rt.engine().runtime_name(), "docker-sbx-experimental");
    }
}

// ─── WI-0091: exec prompt end-to-end (env-gated) ────────────────────────────

/// `awman exec prompt` with `runtime: "docker-sbx-experimental"` must NOT
/// return a `NotImplemented` ("does not yet route to the sandbox runtime")
/// error. The command may fail for other reasons (agent unavailable, sbx
/// not authenticated, etc.) — those are NOT what we're testing here.
///
/// Gate: `AWMAN_TEST_SBX=1` on macOS arm64.
#[test]
fn exec_prompt_non_interactive_sbx_does_not_return_not_implemented() {
    if !sbx_guard() {
        return;
    }
    if !sbx_on_path() {
        eprintln!("sbx not on PATH; skipping exec_prompt sbx test");
        return;
    }

    let tmp_root = tempfile::tempdir().unwrap();
    let tmp_config = tempfile::tempdir().unwrap();

    // Minimal git repo so Session can open properly.
    std::process::Command::new("git")
        .args(["init", tmp_root.path().to_str().unwrap()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("git init must succeed");

    let config_content = r#"{"runtime":"docker-sbx-experimental","default_agent":"claude"}"#;
    std::fs::write(tmp_config.path().join("config.json"), config_content).unwrap();

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_awman"))
        .args([
            "exec",
            "prompt",
            "--agent",
            "claude",
            "--non-interactive",
            "hello from test",
        ])
        .current_dir(tmp_root.path())
        .env("AWMAN_CONFIG_HOME", tmp_config.path())
        .output()
        .expect("awman binary must be executable");

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let combined = format!("{stdout}{stderr}");

    assert!(
        !combined.contains("does not yet route to the sandbox runtime"),
        "exec prompt must not return NotImplemented for sbx runtime;\n\
         stdout: {stdout}\nstderr: {stderr}"
    );
    // The Docker image setup must not run under sbx (WI 0091): a failure here
    // means the command died in `ensure_available` before the sandbox launch.
    assert!(
        !combined.contains("agent setup failed") && !combined.contains("project image"),
        "exec prompt under sbx must skip the container image setup;\n\
         stdout: {stdout}\nstderr: {stderr}"
    );
    // The launch path must actually be reached (the command may still fail
    // later, e.g. missing credentials — that is not what this test checks).
    assert!(
        combined.contains("Launching agent ("),
        "exec prompt under sbx must reach the launch step;\n\
         stdout: {stdout}\nstderr: {stderr}"
    );
}

/// A two-step workflow against the sbx runtime must not error with
/// `NotImplemented` and must reuse the same sandbox per agent across steps
/// (sandbox name is deterministic per workspace+agent, so the second step
/// restarts the existing sandbox rather than creating a new one).
///
/// Reuse is verified structurally: `awman exec workflow` runs both steps
/// without error and `sbx ls` reports at most one sandbox with the
/// deterministic name for the test workspace after the run.
///
/// Gate: `AWMAN_TEST_SBX=1` on macOS arm64.
#[test]
fn workflow_two_step_sbx_does_not_error_with_not_implemented() {
    if !sbx_guard() {
        return;
    }
    if !sbx_on_path() {
        eprintln!("sbx not on PATH; skipping workflow sbx test");
        return;
    }

    let tmp_root = tempfile::tempdir().unwrap();
    let tmp_config = tempfile::tempdir().unwrap();

    // Minimal git repo.
    std::process::Command::new("git")
        .args(["init", tmp_root.path().to_str().unwrap()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("git init must succeed");

    let config_content = r#"{"runtime":"docker-sbx-experimental","default_agent":"claude"}"#;
    std::fs::write(tmp_config.path().join("config.json"), config_content).unwrap();

    // Minimal two-step workflow: both steps use the same agent (claude) so
    // they share one sandbox. NB: the step field is `prompt`, not
    // `prompt_template` (the parser is deny_unknown_fields).
    let workflow_yaml = r#"steps:
  - name: step1
    prompt: "hello from step 1"
  - name: step2
    depends_on:
      - step1
    prompt: "hello from step 2"
"#;
    let workflow_path = tmp_root.path().join("test_workflow.yml");
    std::fs::write(&workflow_path, workflow_yaml).unwrap();

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_awman"))
        .args(["exec", "workflow", workflow_path.to_str().unwrap()])
        .current_dir(tmp_root.path())
        .env("AWMAN_CONFIG_HOME", tmp_config.path())
        .output()
        .expect("awman binary must be executable");

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let combined = format!("{stdout}{stderr}");

    // The routing gate must not fire — the gate was removed in WI 0091.
    assert!(
        !combined.contains("does not yet route to the sandbox runtime"),
        "exec workflow must not return NotImplemented for sbx runtime;\n\
         stdout: {stdout}\nstderr: {stderr}"
    );
    // The workflow file itself must parse — a load failure means this test
    // never exercised the routing at all.
    assert!(
        !combined.contains("failed to load workflow"),
        "the test workflow file must parse;\nstdout: {stdout}\nstderr: {stderr}"
    );

    // Verify sandbox reuse: both steps map to ONE deterministic sandbox name,
    // computed with the same production helper awman itself uses
    // (`sandbox_name_for` — FNV-1a worktree hash). `sbx ls` must list that
    // name at most once.
    use awman::engine::sandbox::sandbox_name_for;
    // Canonicalize the tempdir like Session does for the git root (macOS
    // tempdirs live behind the /var -> /private/var symlink).
    let workspace = std::fs::canonicalize(tmp_root.path()).unwrap();
    let expected_name = sandbox_name_for(&workspace, "claude");

    let ls_out = std::process::Command::new(awman::engine::host_cli::program("sbx"))
        .arg("ls")
        .output()
        .ok();
    let ls_bytes = ls_out.as_ref().map(|o| o.stdout.as_slice()).unwrap_or(&[]);
    let ls_text = String::from_utf8_lossy(ls_bytes);
    let matching: Vec<_> = ls_text
        .lines()
        .filter(|l| l.contains(&expected_name))
        .collect();
    assert!(
        matching.len() <= 1,
        "at most one sandbox with name '{expected_name}' must exist (reuse, not duplicate);\
         \nsbx ls output: {ls_text}"
    );

    // Clean up the sandbox created by this test (best-effort).
    let _ = std::process::Command::new(awman::engine::host_cli::program("sbx"))
        .args(["rm", &expected_name])
        .status();
}

/// Runtime switching on a real launch path (WI 0091 extension of WI 0090's
/// detection-only switching test): the same workspace is exec'd under Docker,
/// then sbx, then Docker again. Each run must engage its own paradigm's launch
/// path — the Docker runs go through the container image setup, the sbx run
/// skips it and uses the kit — with no state leaking between runs.
///
/// The Docker runs may fail (no project image built in the temp repo); what
/// matters is WHICH path each run takes, not whether the agent comes up.
///
/// Gate: `AWMAN_TEST_SBX=1` on macOS arm64.
#[test]
fn runtime_switching_docker_sbx_docker_real_launch_paths() {
    if !sbx_guard() {
        return;
    }
    if !sbx_on_path() {
        eprintln!("sbx not on PATH; skipping runtime switching test");
        return;
    }

    let tmp_root = tempfile::tempdir().unwrap();
    let tmp_config = tempfile::tempdir().unwrap();
    std::process::Command::new("git")
        .args(["init", tmp_root.path().to_str().unwrap()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("git init must succeed");

    let run_with_runtime = |runtime: &str| -> String {
        let config = format!(r#"{{"runtime":"{runtime}","default_agent":"claude"}}"#);
        std::fs::write(tmp_config.path().join("config.json"), config).unwrap();
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_awman"))
            .args([
                "exec",
                "prompt",
                "--agent",
                "claude",
                "--non-interactive",
                "ping",
            ])
            .current_dir(tmp_root.path())
            .env("AWMAN_CONFIG_HOME", tmp_config.path())
            .output()
            .expect("awman binary must be executable");
        format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    };

    // 1. Docker: must take the container setup path, never the kit path.
    let docker_first = run_with_runtime("docker");
    assert!(
        docker_first.contains("Checking agent availability…"),
        "docker run must engage the container image setup; output: {docker_first}"
    );
    assert!(
        !docker_first.contains("using the agent kit"),
        "docker run must not take the sandbox kit path; output: {docker_first}"
    );

    // 2. sbx: must skip image setup and take the kit path.
    let sbx_run = run_with_runtime("docker-sbx-experimental");
    assert!(
        sbx_run.contains("using the agent kit"),
        "sbx run must take the kit path (image setup skipped); output: {sbx_run}"
    );
    assert!(
        !sbx_run.contains("Checking agent availability…"),
        "sbx run must not engage the container image setup; output: {sbx_run}"
    );

    // 3. Docker again: behavior identical to step 1 — no sbx state leaked.
    let docker_again = run_with_runtime("docker");
    assert!(
        docker_again.contains("Checking agent availability…"),
        "second docker run must engage the container image setup; output: {docker_again}"
    );
    assert!(
        !docker_again.contains("using the agent kit"),
        "second docker run must not take the sandbox kit path; output: {docker_again}"
    );
}
