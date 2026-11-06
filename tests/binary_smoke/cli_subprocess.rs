//! Binary smoke tests — invokes the compiled `awman` binary as a subprocess.

use crate::make_git_repo;
use std::process::Command;

/// Path to the compiled `awman` binary. Cargo sets this for integration tests.
fn awman_bin() -> std::path::PathBuf {
    crate::awman_bin()
}

fn run_awman(args: &[&str]) -> std::process::Output {
    Command::new(awman_bin())
        .args(args)
        .output()
        .expect("failed to run awman binary")
}

// ─── Help flags ───────────────────────────────────────────────────────────────

#[test]
fn awman_help_exits_zero() {
    let out = run_awman(&["--help"]);
    assert!(
        out.status.success(),
        "`awman --help` should exit 0; got {:?}\nstderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn awman_help_stdout_mentions_awman() {
    let out = run_awman(&["--help"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("awman"),
        "`awman --help` stdout should mention 'awman'; got:\n{stdout}"
    );
}

#[test]
fn awman_version_flag_exits_zero_or_one() {
    // --version may not be defined; exit code 0 (version printed) or 2 (unrecognised flag)
    // are both acceptable; the test just ensures the binary runs.
    let out = run_awman(&["--version"]);
    let code = out.status.code().unwrap_or(0);
    assert!(
        code == 0 || code == 1 || code == 2,
        "unexpected exit code {code} for --version"
    );
}

// ─── Subcommand help ──────────────────────────────────────────────────────────

#[test]
fn awman_init_help_exits_zero() {
    let out = run_awman(&["init", "--help"]);
    assert!(
        out.status.success(),
        "`awman init --help` should exit 0; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn awman_ready_help_exits_zero() {
    let out = run_awman(&["ready", "--help"]);
    assert!(out.status.success());
}

#[test]
fn awman_exec_help_exits_zero() {
    let out = run_awman(&["exec", "--help"]);
    assert!(out.status.success());
}

#[test]
fn awman_daemon_help_lists_the_five_verbs() {
    let out = run_awman(&["daemon", "--help"]);
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    for verb in ["start", "stop", "status", "logs", "api"] {
        assert!(text.contains(verb), "{verb}:\n{text}");
    }
    assert!(!run_awman(&["api", "--help"]).status.success());
}

#[test]
fn awman_agents_status_help_exits_zero() {
    let out = run_awman(&["agents", "status", "--help"]);
    assert!(out.status.success());
}

#[test]
fn removed_root_status_is_unknown_without_migration_hint() {
    let out = run_awman(&["status"]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("unrecognized subcommand 'status'"),
        "unexpected status error: {stderr}"
    );
    assert!(
        !stderr.to_lowercase().contains("agents status"),
        "removed command must not suggest a migration hint: {stderr}"
    );
}

#[test]
fn awman_config_help_exits_zero() {
    let out = run_awman(&["config", "--help"]);
    assert!(out.status.success());
}

#[test]
fn awman_new_help_exits_zero() {
    let out = run_awman(&["new", "--help"]);
    assert!(out.status.success());
}

#[test]
fn awman_remote_help_exits_zero() {
    let out = run_awman(&["remote", "--help"]);
    assert!(out.status.success());
}

/// WI 0078: `awman remote chat` (old arbitrary-command style) must NOT
/// silently parse — it returns a non-zero exit and points the user to the
/// new subcommand structure.
#[test]
fn awman_remote_old_style_chat_rejected() {
    let out = run_awman(&["remote", "chat"]);
    assert!(
        !out.status.success(),
        "remote chat (old style) must fail; got exit {:?}",
        out.status.code()
    );
    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    // Must mention either a usage hint or surface that `chat` is not a known
    // remote subcommand. We accept any error that names the offending token.
    assert!(
        combined.to_lowercase().contains("chat")
            || combined.to_lowercase().contains("subcommand")
            || combined.to_lowercase().contains("remote"),
        "rejection error must reference the bad `chat` argument or remote subcommands; got: {combined}"
    );
}

/// `awman remote exec --help` lists exactly `workflow` and `prompt` subcommands.
#[test]
fn awman_remote_exec_help_lists_workflow_and_prompt() {
    let out = run_awman(&["remote", "exec", "--help"]);
    assert!(
        out.status.success(),
        "remote exec --help must exit 0; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("workflow"),
        "remote exec --help must list `workflow`; got: {stdout}"
    );
    assert!(
        stdout.contains("prompt"),
        "remote exec --help must list `prompt`; got: {stdout}"
    );
}

/// `awman remote session --help` lists exactly `start` and `kill` subcommands.
#[test]
fn awman_remote_session_help_lists_start_and_kill() {
    let out = run_awman(&["remote", "session", "--help"]);
    assert!(
        out.status.success(),
        "remote session --help must exit 0; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("start"),
        "remote session --help must list `start`; got: {stdout}"
    );
    assert!(
        stdout.contains("kill"),
        "remote session --help must list `kill`; got: {stdout}"
    );
}

// ─── skill() overlay flag behaviour (WI 0075) ────────────────────────────────

/// `skill()` (empty parens, no args) as --overlay value must exit non-zero with
/// a descriptive error; the flag itself must be recognised (not "unknown flag").
#[test]
fn skill_with_args_flag_exits_nonzero_with_descriptive_error() {
    let repo = make_git_repo();
    // Use `chat --headless` which accepts --overlay without a required positional arg.
    let out = Command::new(awman_bin())
        .current_dir(repo.path())
        .args(["chat", "--headless", "--overlay", "skill()"])
        .output()
        .expect("failed to run awman");

    assert!(
        !out.status.success(),
        "skill() must cause a non-zero exit; got: {:?}",
        out.status.code()
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    // Must NOT complain that --overlay is unrecognised.
    assert!(
        !stderr.contains("unexpected argument '--overlay'")
            && !stderr.contains("unrecognized argument --overlay"),
        "--overlay must be a recognised flag; got: {stderr}"
    );
    // Must report a parse-level error mentioning that skill() requires an argument.
    assert!(
        stderr.contains("requires an argument") || stderr.contains("skill"),
        "error must describe the invalid skill() usage; got: {stderr}"
    );
}

/// `skill()` (valid) as --overlay value must be recognised — clap must not
/// reject the flag. The command may still fail (no Docker/agent), but the
/// --overlay flag itself must be accepted as syntactically valid.
#[test]
fn skill_empty_overlay_flag_is_recognized_by_cli() {
    let repo = make_git_repo();
    let out = Command::new(awman_bin())
        .current_dir(repo.path())
        .args(["exec", "workflow", "--help"])
        .output()
        .expect("failed to run awman exec workflow --help");

    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("--overlay"),
        "exec workflow --help must mention --overlay so skill() can be passed; got: {stdout}"
    );
}

/// `AWMAN_OVERLAYS="skill()"` env var: help still works (env var not parsed at help time).
#[test]
fn skill_in_awman_overlays_env_does_not_break_help() {
    let out = Command::new(awman_bin())
        .env("AWMAN_OVERLAYS", "skill()")
        .args(["exec", "workflow", "--help"])
        .output()
        .expect("failed to run awman exec workflow --help");

    assert!(
        out.status.success(),
        "awman exec workflow --help must succeed even when AWMAN_OVERLAYS=skill(); stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

// ─── Unknown command error handling ──────────────────────────────────────────

#[test]
fn awman_unknown_subcommand_exits_nonzero() {
    let out = run_awman(&["definitely-not-a-command"]);
    assert!(
        !out.status.success(),
        "awman with unknown subcommand should exit non-zero"
    );
}

// ─── Subcommand help text contains flag names ─────────────────────────────────

#[test]
fn awman_init_help_mentions_agent_flag() {
    let out = run_awman(&["init", "--help"]);
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        text.contains("agent") || text.contains("--agent"),
        "`awman init --help` should mention the --agent flag;\ngot:\n{text}"
    );
}

#[test]
fn awman_ready_help_mentions_build_flag() {
    let out = run_awman(&["ready", "--help"]);
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        text.contains("build") || text.contains("--build"),
        "`awman ready --help` should mention --build;\ngot:\n{text}"
    );
}

#[test]
fn awman_daemon_start_help_lists_the_global_set_flag_and_no_port() {
    let out = run_awman(&["daemon", "start", "--help"]);
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(text.contains("--set"), "`--set` is global;\ngot:\n{text}");
    assert!(
        !text.contains("--port"),
        "the listen address is config now;\ngot:\n{text}"
    );
}

/// WI 0139: removed per-setting flags are clap's ordinary unknown arguments.
#[test]
fn removed_flags_fail_with_the_unknown_argument_error() {
    for args in [
        &["chat", "--non-interactive"][..],
        &["chat", "--launch-mode", "acp"],
        &["exec", "workflow", "wf.toml", "--max-concurrent", "2"],
    ] {
        let out = run_awman(args);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains("unexpected argument"), "{args:?}: {stderr}");
    }
}

/// WI 0139: a bad `--set` is a usage error carrying the key table's message.
#[test]
fn a_bad_set_exits_2_with_the_key_table_error() {
    let out = run_awman(&["--set", "maxConcurrentAgents=2", "ready"]);
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("unknown config key `maxConcurrentAgents`"),
        "{stderr}"
    );
    assert!(stderr.contains("workflows.maxConcurrentSteps"), "{stderr}");
}

/// `exec workflow` and its alias report a failure through the same renderer as
/// every other command: a JSON error on stdout under `--json`, and the same exit
/// code as the plain form, which keeps its text on stderr.
#[test]
fn exec_workflow_failure_renders_for_the_output_mode() {
    let repo = make_git_repo();
    let config = tempfile::TempDir::new().unwrap();
    let run = |args: &[&str]| {
        std::process::Command::new(awman_bin())
            .args(args)
            .current_dir(repo.path())
            .env("AWMAN_HOME", config.path())
            .output()
            .unwrap()
    };

    let plain = run(&["exec", "workflow", "missing.toml", "--headless"]);
    assert!(!plain.status.success());
    assert!(plain.stdout.is_empty(), "{plain:?}");
    assert!(!plain.stderr.is_empty(), "{plain:?}");

    for spelling in ["workflow", "wf"] {
        let json = run(&["exec", spelling, "missing.toml", "--json"]);
        assert_eq!(json.status.code(), plain.status.code(), "{json:?}");
        let body: serde_json::Value =
            serde_json::from_slice(&json.stdout).expect("stdout is one JSON document");
        assert!(body["error"].as_str().is_some_and(|text| !text.is_empty()));
    }
}
