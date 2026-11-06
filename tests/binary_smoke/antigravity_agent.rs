//! Binary-level tests for the antigravity agent and the gemini deprecation.
//!
//! Tests that can be validated without a live Docker daemon (warnings fire
//! before agent-image checks, and model-flag errors fire before image checks
//! in exec-prompt's early-validation path).

use crate::make_git_repo;
use std::process::Command;
use tempfile::TempDir;

fn awman_bin() -> std::path::PathBuf {
    crate::awman_bin()
}

/// Build an `awman` subprocess scoped to a fresh empty config home so the
/// developer's real `~/.awman/config.json` (which on a working machine often
/// has a `runtime` and image registry configured) can never bleed into the
/// test's exit-code or warning-text assertions.
fn awman_isolated(repo: &TempDir) -> (TempDir, Command) {
    let cfg_home = TempDir::new().expect("config TempDir::new");
    let mut cmd = Command::new(awman_bin());
    cmd.current_dir(repo.path())
        .env("AWMAN_HOME", cfg_home.path())
        // `AWMAN_HOME` also roots `api/`, `squad/` and `attach/`.
        .env_remove("AWMAN_REMOTE_SESSION")
        .env_remove("AWMAN_OVERLAYS");
    (cfg_home, cmd)
}

// ─── Gemini deprecation warning — chat ───────────────────────────────────────

/// `awman chat --agent gemini` must emit a deprecation warning containing "deprecated"
/// to stderr before the container launch is attempted. The process exits
/// non-zero (no Docker images), but the warning must still be present.
#[test]
fn chat_gemini_emits_deprecation_warning_before_container_start() {
    let repo = make_git_repo();
    let (_cfg, mut cmd) = awman_isolated(&repo);
    let output = cmd
        .args(["chat", "--agent", "gemini"])
        .output()
        .expect("failed to run awman");

    // Must exit non-zero (no Docker images available in test environment).
    assert!(
        !output.status.success(),
        "`chat --agent gemini` should exit non-zero in a test environment without Docker images"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.to_lowercase().contains("deprecated"),
        "stderr must contain 'deprecated' when using the gemini agent; got:\n{stderr}"
    );
    assert!(
        stderr.contains("gemini"),
        "deprecation warning must mention 'gemini'; got:\n{stderr}"
    );
}

// ─── Gemini deprecation warning — exec-workflow ───────────────────────────────

/// `awman exec workflow --agent gemini` must emit a deprecation warning even
/// when the workflow file does not exist (warning fires before file check).
#[test]
fn exec_workflow_gemini_emits_deprecation_warning() {
    let repo = make_git_repo();
    let (_cfg, mut cmd) = awman_isolated(&repo);
    let output = cmd
        .args([
            "exec",
            "workflow",
            "--agent",
            "gemini",
            "nonexistent-workflow.toml",
        ])
        .output()
        .expect("failed to run awman");

    assert!(
        !output.status.success(),
        "`exec workflow` with missing file must exit non-zero"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.to_lowercase().contains("deprecated"),
        "stderr must contain 'deprecated' for exec-workflow with gemini agent; got:\n{stderr}"
    );
}

/// The deprecation warning must reference real `awman` commands, not a
/// non-existent `agent install` subcommand.
#[test]
fn gemini_deprecation_warning_uses_real_commands() {
    let repo = make_git_repo();
    let (_cfg, mut cmd) = awman_isolated(&repo);
    let output = cmd
        .args(["chat", "--agent", "gemini"])
        .output()
        .expect("failed to run awman");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("agent install"),
        "deprecation warning must not reference the non-existent 'agent install' subcommand; got:\n{stderr}"
    );
    assert!(
        stderr.contains("awman chat antigravity")
            || stderr.contains("awman config set agent antigravity"),
        "deprecation warning must point users at a real awman command; got:\n{stderr}"
    );
}
