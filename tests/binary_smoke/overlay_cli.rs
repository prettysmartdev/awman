//! Binary-level `--overlay` CLI tests: the removed `--mount-ssh` flag and
//! `skills()` plural form, fatal handling of a malformed `AWMAN_OVERLAYS`, and
//! parsing of `context(...)` overlay expressions.

use crate::{awman, make_git_repo};

// ─── --mount-ssh removed ──────────────────────────────────────────────────────

#[test]
fn mount_ssh_flag_rejected_with_nonzero_exit() {
    let repo = make_git_repo();
    let output = awman()
        .current_dir(repo.path())
        .args(["chat", "--non-interactive", "--mount-ssh"])
        .output()
        .expect("failed to run awman");

    assert!(
        !output.status.success(),
        "--mount-ssh must be rejected with a non-zero exit; got 0"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("unrecognized argument --overlay")
            && !stderr.contains("unexpected argument '--overlay'"),
        "--overlay must remain a recognised flag; stderr: {stderr}"
    );
    assert!(
        stderr.contains("--mount-ssh"),
        "rejection message must name the removed --mount-ssh flag; stderr: {stderr}"
    );
    assert!(
        stderr.contains("ssh()") || stderr.contains("--overlay"),
        "rejection message must point users at `--overlay ssh()` as the replacement; stderr: {stderr}"
    );
}

#[test]
fn mount_ssh_with_value_form_also_rejected_with_guidance() {
    let repo = make_git_repo();
    let output = awman()
        .current_dir(repo.path())
        .args(["chat", "--non-interactive", "--mount-ssh=true"])
        .output()
        .expect("failed to run awman");

    assert!(
        !output.status.success(),
        "--mount-ssh=true must be rejected with a non-zero exit; got 0"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--mount-ssh")
            && (stderr.contains("ssh()") || stderr.contains("--overlay")),
        "--mount-ssh=true must produce the same guidance message; stderr: {stderr}"
    );
}

// ─── skills() plural form removed ────────────────────────────────────────────

#[test]
fn skills_plural_named_in_overlay_flag_exits_nonzero() {
    let repo = make_git_repo();
    let output = awman()
        .current_dir(repo.path())
        .args(["chat", "--non-interactive", "--overlay", "skills(lint)"])
        .output()
        .expect("failed to run awman");

    assert!(
        !output.status.success(),
        "--overlay skills(lint) must exit non-zero (removed form)"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("skill(") || stderr.contains("removed"),
        "error must mention the replacement skill() form or that skills() was removed; got: {stderr}"
    );
}

#[test]
fn skills_plural_empty_in_overlay_flag_exits_nonzero() {
    let repo = make_git_repo();
    let output = awman()
        .current_dir(repo.path())
        .args(["chat", "--non-interactive", "--overlay", "skills()"])
        .output()
        .expect("failed to run awman");

    assert!(
        !output.status.success(),
        "--overlay skills() must exit non-zero (removed form)"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("skill(") || stderr.contains("removed"),
        "error must mention skill(*) replacement or that skills() was removed; got: {stderr}"
    );
}

#[test]
fn skills_plural_in_awman_overlays_env_exits_nonzero() {
    let repo = make_git_repo();
    let output = awman()
        .current_dir(repo.path())
        .env("AWMAN_OVERLAYS", "skills(foo)")
        .args(["chat", "--non-interactive"])
        .output()
        .expect("failed to run awman");

    assert!(
        !output.status.success(),
        "skills(foo) in AWMAN_OVERLAYS must exit non-zero (removed form)"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("skill(") || stderr.contains("removed"),
        "error must mention migration guidance; got: {stderr}"
    );
}

// ─── Malformed AWMAN_OVERLAYS ─────────────────────────────────────────────────

#[test]
fn malformed_awman_overlays_causes_fatal_exit_and_names_env_var() {
    let repo = make_git_repo();
    let output = awman()
        .current_dir(repo.path())
        .env("AWMAN_OVERLAYS", "###not-an-overlay###")
        .args(["chat", "--non-interactive"])
        .output()
        .expect("failed to run awman");

    assert!(
        !output.status.success(),
        "malformed AWMAN_OVERLAYS must cause a non-zero exit when running a command"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("AWMAN_OVERLAYS"),
        "error message must mention AWMAN_OVERLAYS so the user knows the source; got: {stderr}"
    );
}

#[test]
fn malformed_awman_overlays_does_not_prevent_help() {
    let output = awman()
        .env("AWMAN_OVERLAYS", "###not-an-overlay###")
        .args(["chat", "--help"])
        .output()
        .expect("failed to run awman chat --help");

    assert!(
        output.status.success(),
        "`chat --help` must exit 0 regardless of AWMAN_OVERLAYS content; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

// ─── --overlay flag still present in help ────────────────────────────────────

#[test]
fn chat_help_shows_overlay_flag() {
    let output = awman()
        .args(["chat", "--help"])
        .output()
        .expect("failed to run awman chat --help");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("--overlay"),
        "chat --help must mention --overlay flag; got: {stdout}"
    );
}

#[test]
fn exec_workflow_help_shows_overlay_flag() {
    let output = awman()
        .args(["exec", "workflow", "--help"])
        .output()
        .expect("failed to run awman exec workflow --help");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("--overlay"),
        "exec workflow --help must mention --overlay flag; got: {stdout}"
    );
}

// ─── CLI dispatch roundtrip ───────────────────────────────────────────────────

/// Verify that `--overlay context(global)` is accepted by the CLI without a
/// parse error. The command may fail for other reasons (no Docker, no project
/// image), but the overlay expression itself must not trigger an "unknown
/// overlay type" or similar parse-level rejection.
#[test]
fn exec_prompt_context_global_overlay_accepted_by_cli() {
    let repo = make_git_repo();
    let output = awman()
        .current_dir(repo.path())
        .args([
            "exec",
            "prompt",
            "--non-interactive",
            "--overlay",
            "context(global)",
            "hello",
        ])
        .output()
        .expect("failed to run awman");

    let stderr = String::from_utf8_lossy(&output.stderr);
    // The CLI must not reject the overlay expression with a parse error.
    assert!(
        !stderr.contains("unknown overlay type"),
        "--overlay context(global) must not trigger 'unknown overlay type'; stderr: {stderr}"
    );
    assert!(
        !stderr.contains("context() requires"),
        "--overlay context(global) must not trigger 'context() requires'; stderr: {stderr}"
    );
    assert!(
        !stderr.contains("unknown context scope"),
        "--overlay context(global) must not trigger 'unknown context scope'; stderr: {stderr}"
    );
    // Any failure must be for a reason unrelated to overlay parsing (e.g. no
    // Docker / no project image / non-interactive limitation).
}

/// Verify that `--overlay context(repo)` is also accepted by the CLI.
#[test]
fn exec_prompt_context_repo_overlay_accepted_by_cli() {
    let repo = make_git_repo();
    let output = awman()
        .current_dir(repo.path())
        .args([
            "exec",
            "prompt",
            "--non-interactive",
            "--overlay",
            "context(repo)",
            "hello",
        ])
        .output()
        .expect("failed to run awman");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("unknown overlay type"),
        "--overlay context(repo) must not trigger 'unknown overlay type'; stderr: {stderr}"
    );
    assert!(
        !stderr.contains("context() requires"),
        "--overlay context(repo) must not trigger a parse error; stderr: {stderr}"
    );
}

/// Verify that `--overlay context(global:ro)` is accepted by the CLI.
#[test]
fn exec_prompt_context_global_ro_overlay_accepted_by_cli() {
    let repo = make_git_repo();
    let output = awman()
        .current_dir(repo.path())
        .args([
            "exec",
            "prompt",
            "--non-interactive",
            "--overlay",
            "context(global:ro)",
            "hello",
        ])
        .output()
        .expect("failed to run awman");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("unknown overlay type"),
        "--overlay context(global:ro) must not trigger 'unknown overlay type'; stderr: {stderr}"
    );
    assert!(
        !stderr.contains("unknown permission"),
        "--overlay context(global:ro) must not trigger 'unknown permission'; stderr: {stderr}"
    );
}

/// Verify that `--overlay context(unknown_scope)` is REJECTED with a parse error.
#[test]
fn exec_prompt_context_unknown_scope_rejected_with_parse_error() {
    let repo = make_git_repo();
    let output = awman()
        .current_dir(repo.path())
        .args([
            "exec",
            "prompt",
            "--non-interactive",
            "--overlay",
            "context(notascope)",
            "hello",
        ])
        .output()
        .expect("failed to run awman");

    assert!(
        !output.status.success(),
        "awman must exit non-zero for context(notascope)"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("notascope") || stderr.contains("unknown") || stderr.contains("scope"),
        "rejection must name the bad scope or say 'unknown'; stderr: {stderr}"
    );
}

// ─── parse_overlay_list roundtrip (library level) ────────────────────────────

/// Roundtrip test using the library API directly — ensures the CLI dispatch
/// would correctly plumb `context(global)` through `parse_overlay_list`.
#[test]
fn parse_overlay_list_context_global_roundtrip() {
    use awman::command::commands::{parse_overlay_list, ContextOverlaySpec, TypedOverlay};
    use awman::engine::container::options::OverlayPermission;
    use awman::engine::overlay::ContextScope;

    let result = parse_overlay_list("context(global)").expect("context(global) must parse");
    assert_eq!(result.len(), 1);
    match &result[0] {
        TypedOverlay::Context(ContextOverlaySpec { scope, permission }) => {
            assert_eq!(*scope, ContextScope::Global);
            assert_eq!(*permission, OverlayPermission::ReadWrite);
        }
        other => panic!("expected TypedOverlay::Context, got {other:?}"),
    }
}

#[test]
fn parse_overlay_list_context_workflow_ro_roundtrip() {
    use awman::command::commands::{parse_overlay_list, ContextOverlaySpec, TypedOverlay};
    use awman::engine::container::options::OverlayPermission;
    use awman::engine::overlay::ContextScope;

    let result =
        parse_overlay_list("context(workflow:ro)").expect("context(workflow:ro) must parse");
    assert_eq!(result.len(), 1);
    match &result[0] {
        TypedOverlay::Context(ContextOverlaySpec { scope, permission }) => {
            assert_eq!(*scope, ContextScope::Workflow);
            assert_eq!(*permission, OverlayPermission::ReadOnly);
        }
        other => panic!("expected TypedOverlay::Context, got {other:?}"),
    }
}
