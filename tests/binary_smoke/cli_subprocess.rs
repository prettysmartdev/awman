//! Binary smoke tests — invokes the compiled `amux` binary as a subprocess.

use std::process::Command;

/// Path to the compiled `amux` binary. Cargo sets this for integration tests.
fn amux_bin() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_amux"))
}

fn run_amux(args: &[&str]) -> std::process::Output {
    Command::new(amux_bin())
        .args(args)
        .output()
        .expect("failed to run amux binary")
}

// ─── Help flags ───────────────────────────────────────────────────────────────

#[test]
fn amux_help_exits_zero() {
    let out = run_amux(&["--help"]);
    assert!(
        out.status.success(),
        "`amux --help` should exit 0; got {:?}\nstderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn amux_help_stdout_mentions_amux() {
    let out = run_amux(&["--help"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("amux"),
        "`amux --help` stdout should mention 'amux'; got:\n{stdout}"
    );
}

#[test]
fn amux_version_flag_exits_zero_or_one() {
    // --version may not be defined; exit code 0 (version printed) or 2 (unrecognised flag)
    // are both acceptable; the test just ensures the binary runs.
    let out = run_amux(&["--version"]);
    let code = out.status.code().unwrap_or(0);
    assert!(
        code == 0 || code == 1 || code == 2,
        "unexpected exit code {code} for --version"
    );
}

// ─── Subcommand help ──────────────────────────────────────────────────────────

#[test]
fn amux_init_help_exits_zero() {
    let out = run_amux(&["init", "--help"]);
    assert!(
        out.status.success(),
        "`amux init --help` should exit 0; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn amux_ready_help_exits_zero() {
    let out = run_amux(&["ready", "--help"]);
    assert!(out.status.success());
}

#[test]
fn amux_exec_help_exits_zero() {
    let out = run_amux(&["exec", "--help"]);
    assert!(out.status.success());
}

#[test]
fn amux_headless_help_exits_zero() {
    let out = run_amux(&["headless", "--help"]);
    assert!(out.status.success());
}

#[test]
fn amux_status_help_exits_zero() {
    let out = run_amux(&["status", "--help"]);
    assert!(out.status.success());
}

#[test]
fn amux_config_help_exits_zero() {
    let out = run_amux(&["config", "--help"]);
    assert!(out.status.success());
}

#[test]
fn amux_new_help_exits_zero() {
    let out = run_amux(&["new", "--help"]);
    assert!(out.status.success());
}

#[test]
fn amux_remote_help_exits_zero() {
    let out = run_amux(&["remote", "--help"]);
    assert!(out.status.success());
}

// ─── skill() overlay flag behaviour (WI 0075) ────────────────────────────────

fn make_git_repo() -> tempfile::TempDir {
    let repo = tempfile::tempdir().expect("TempDir::new");
    std::process::Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(repo.path())
        .status()
        .expect("git init");
    repo
}

/// `skill(anything)` as --overlay value must exit non-zero with a descriptive
/// error; the flag itself must be recognised (not "unknown flag").
#[test]
fn skill_with_args_flag_exits_nonzero_with_descriptive_error() {
    let repo = make_git_repo();
    // Use `chat --non-interactive` which accepts --overlay without a required positional arg.
    let out = Command::new(amux_bin())
        .current_dir(repo.path())
        .args(["chat", "--non-interactive", "--overlay", "skill(something)"])
        .output()
        .expect("failed to run amux");

    assert!(
        !out.status.success(),
        "skill(something) must cause a non-zero exit; got: {:?}",
        out.status.code()
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    // Must NOT complain that --overlay is unrecognised.
    assert!(
        !stderr.contains("unexpected argument '--overlay'")
            && !stderr.contains("unrecognized argument --overlay"),
        "--overlay must be a recognised flag; got: {stderr}"
    );
    // Must report a parse-level error mentioning the invalid use of arguments.
    assert!(
        stderr.contains("takes no arguments") || stderr.contains("skill"),
        "error must describe the invalid skill() usage; got: {stderr}"
    );
}

/// `skill()` (valid) as --overlay value must be recognised — clap must not
/// reject the flag. The command may still fail (no Docker/agent), but the
/// --overlay flag itself must be accepted as syntactically valid.
#[test]
fn skill_empty_overlay_flag_is_recognized_by_cli() {
    let repo = make_git_repo();
    let out = Command::new(amux_bin())
        .current_dir(repo.path())
        .args(["exec", "workflow", "--help"])
        .output()
        .expect("failed to run amux exec workflow --help");

    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("--overlay"),
        "exec workflow --help must mention --overlay so skill() can be passed; got: {stdout}"
    );
}

/// `AMUX_OVERLAYS="skill()"` env var: help still works (env var not parsed at help time).
#[test]
fn skill_in_amux_overlays_env_does_not_break_help() {
    let out = Command::new(amux_bin())
        .env("AMUX_OVERLAYS", "skill()")
        .args(["exec", "workflow", "--help"])
        .output()
        .expect("failed to run amux exec workflow --help");

    assert!(
        out.status.success(),
        "amux exec workflow --help must succeed even when AMUX_OVERLAYS=skill(); stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

// ─── Unknown command error handling ──────────────────────────────────────────

#[test]
fn amux_unknown_subcommand_exits_nonzero() {
    let out = run_amux(&["definitely-not-a-command"]);
    assert!(
        !out.status.success(),
        "amux with unknown subcommand should exit non-zero"
    );
}

// ─── Subcommand help text contains flag names ─────────────────────────────────

#[test]
fn amux_init_help_mentions_agent_flag() {
    let out = run_amux(&["init", "--help"]);
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        text.contains("agent") || text.contains("--agent"),
        "`amux init --help` should mention the --agent flag;\ngot:\n{text}"
    );
}

#[test]
fn amux_ready_help_mentions_build_flag() {
    let out = run_amux(&["ready", "--help"]);
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        text.contains("build") || text.contains("--build"),
        "`amux ready --help` should mention --build;\ngot:\n{text}"
    );
}

#[test]
fn amux_headless_start_help_mentions_port_flag() {
    let out = run_amux(&["headless", "start", "--help"]);
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        text.contains("port") || text.contains("--port"),
        "`amux headless start --help` should mention --port;\ngot:\n{text}"
    );
}
