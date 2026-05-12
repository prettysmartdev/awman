/// Integration tests for the amux CLI binary.
///
/// These tests invoke the compiled binary to validate end-to-end behaviour
/// across multiple components.
use std::process::Command;

fn amux() -> Command {
    Command::new(env!("CARGO_BIN_EXE_amux"))
}

#[test]
fn help_exits_successfully() {
    let output = amux().arg("--help").output().expect("failed to run amux");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("amux"));
}

#[test]
fn version_exits_successfully() {
    let output = amux().arg("--version").output().expect("failed to run amux");
    assert!(output.status.success());
}

#[test]
fn ready_help_shows_refresh_flag() {
    let output = amux()
        .args(["ready", "--help"])
        .output()
        .expect("failed to run amux");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("--refresh"),
        "ready --help should mention --refresh flag"
    );
}

#[test]
fn ready_help_shows_non_interactive_flag() {
    let output = amux()
        .args(["ready", "--help"])
        .output()
        .expect("failed to run amux");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("--non-interactive"),
        "ready --help should mention --non-interactive flag"
    );
}

#[test]
fn new_help_shows_subcommand() {
    let output = amux()
        .args(["specs", "--help"])
        .output()
        .expect("failed to run amux");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("new"),
        "specs --help should mention 'new' subcommand"
    );
}

#[test]
fn chat_help_shows_subcommand() {
    let output = amux()
        .args(["--help"])
        .output()
        .expect("failed to run amux");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("chat"),
        "help should mention 'chat' subcommand"
    );
}

#[test]
fn chat_help_shows_non_interactive_flag() {
    let output = amux()
        .args(["chat", "--help"])
        .output()
        .expect("failed to run amux");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("--non-interactive"),
        "chat --help should mention --non-interactive flag"
    );
}

// ── config integration tests ──────────────────────────────────────────────────

use std::path::Path;
use tempfile::TempDir;

/// Build an `amux` Command with a controlled HOME directory so tests do not
/// touch the developer's real `~/.amux/config.json`.
fn amux_with_home(home: &Path) -> Command {
    let mut cmd = amux();
    cmd.env("HOME", home);
    cmd
}

/// Initialize a fresh git repo in a temp directory and return it.
fn make_git_repo() -> TempDir {
    let repo = TempDir::new().expect("TempDir::new");
    std::process::Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(repo.path())
        .status()
        .expect("git init");
    repo
}

/// Write a JSON string to `<dir>/.amux/config.json`, creating dirs as needed.
fn write_repo_config(dir: &Path, json: &str) {
    let config_dir = dir.join(".amux");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(config_dir.join("config.json"), json).unwrap();
}

/// Write a JSON string to `<home>/.amux/config.json`, creating dirs as needed.
fn write_global_config(home: &Path, json: &str) {
    let config_dir = home.join(".amux");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(config_dir.join("config.json"), json).unwrap();
}

// 1. config show — only global config present
#[test]
fn config_show_only_global_config() {
    let home = TempDir::new().unwrap();
    let repo = make_git_repo();
    write_global_config(home.path(), r#"{"default_agent":"gemini"}"#);

    let output = amux_with_home(home.path())
        .current_dir(repo.path())
        .args(["config", "show"])
        .output()
        .unwrap();

    assert!(output.status.success(), "stderr: {}", String::from_utf8_lossy(&output.stderr));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("gemini"),
        "global default_agent should appear; stdout: {}",
        stdout
    );
    assert!(
        stdout.contains("(not set)"),
        "repo column should show '(not set)' for shared fields; stdout: {}",
        stdout
    );
}

// 2. config show — only repo config present
#[test]
fn config_show_only_repo_config() {
    let home = TempDir::new().unwrap();
    let repo = make_git_repo();
    write_repo_config(repo.path(), r#"{"agent":"codex"}"#);

    let output = amux_with_home(home.path())
        .current_dir(repo.path())
        .args(["config", "show"])
        .output()
        .unwrap();

    assert!(output.status.success(), "stderr: {}", String::from_utf8_lossy(&output.stderr));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("codex"),
        "repo agent should appear; stdout: {}",
        stdout
    );
    assert!(
        stdout.contains("(built-in)"),
        "global column should show built-in defaults for unset global fields; stdout: {}",
        stdout
    );
}

// 3. config show — both configs set for terminal_scrollback_lines → Override = yes
#[test]
fn config_show_override_column_shows_yes_when_both_set_and_differ() {
    let home = TempDir::new().unwrap();
    let repo = make_git_repo();
    write_global_config(home.path(), r#"{"terminal_scrollback_lines": 10000}"#);
    write_repo_config(repo.path(), r#"{"terminal_scrollback_lines": 5000}"#);

    let output = amux_with_home(home.path())
        .current_dir(repo.path())
        .args(["config", "show"])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("yes"),
        "Override column should show 'yes' for terminal_scrollback_lines; stdout: {}",
        stdout
    );
}

// 4. config show — outside a git repo
#[test]
fn config_show_outside_git_repo_succeeds_with_note() {
    let home = TempDir::new().unwrap();
    // Use a fresh temp dir that has NOT been git-initialized.
    let not_a_repo = TempDir::new().unwrap();

    let output = amux_with_home(home.path())
        .current_dir(not_a_repo.path())
        .args(["config", "show"])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "config show must exit 0 outside a git repo; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("not inside a git repo") || stderr.contains("repo config is unavailable"),
        "should print a note about unavailable repo config; stderr: {}",
        stderr
    );
}

// 5. config get — repo overrides global annotation
#[test]
fn config_get_shows_repo_overrides_global_annotation() {
    let home = TempDir::new().unwrap();
    let repo = make_git_repo();
    // Global is at built-in (not set), repo = 5000.
    write_repo_config(repo.path(), r#"{"terminal_scrollback_lines": 5000}"#);

    let output = amux_with_home(home.path())
        .current_dir(repo.path())
        .args(["config", "get", "terminal_scrollback_lines"])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("5000"),
        "effective value should be 5000; stdout: {}",
        stdout
    );
    assert!(
        stdout.contains("← repo overrides global"),
        "should annotate repo override; stdout: {}",
        stdout
    );
}

// 6. config get — neither set shows built-in default
#[test]
fn config_get_neither_set_shows_builtin_default() {
    let home = TempDir::new().unwrap();
    let repo = make_git_repo();

    let output = amux_with_home(home.path())
        .current_dir(repo.path())
        .args(["config", "get", "terminal_scrollback_lines"])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Built-in default is 10000; should appear in Global, Repo shows (not set), Effective shows 10000.
    assert!(
        stdout.contains("10000"),
        "built-in default 10000 should appear; stdout: {}",
        stdout
    );
    assert!(
        !stdout.contains("← repo overrides"),
        "no override annotation expected; stdout: {}",
        stdout
    );
}

// 7. config set agent codex — round trip
#[test]
fn config_set_agent_writes_repo_config_and_get_returns_it() {
    let home = TempDir::new().unwrap();
    let repo = make_git_repo();

    let set_out = amux_with_home(home.path())
        .current_dir(repo.path())
        .args(["config", "set", "agent", "codex"])
        .output()
        .unwrap();
    assert!(
        set_out.status.success(),
        "config set should succeed; stderr: {}",
        String::from_utf8_lossy(&set_out.stderr)
    );

    // Verify the written JSON.
    let config_path = repo.path().join(".amux").join("config.json");
    let json = std::fs::read_to_string(&config_path).unwrap();
    assert!(
        json.contains(r#""agent""#) && json.contains("codex"),
        "config.json should contain agent=codex; got: {}",
        json
    );

    // config get should show the new value.
    let get_out = amux_with_home(home.path())
        .current_dir(repo.path())
        .args(["config", "get", "agent"])
        .output()
        .unwrap();
    assert!(get_out.status.success());
    let stdout = String::from_utf8_lossy(&get_out.stdout);
    assert!(stdout.contains("codex"), "config get agent should show codex; stdout: {}", stdout);
}

// 8. config set --global default_agent gemini — writes to global config
#[test]
fn config_set_global_default_agent_writes_to_global_config() {
    let home = TempDir::new().unwrap();
    let repo = make_git_repo();

    let output = amux_with_home(home.path())
        .current_dir(repo.path())
        .args(["config", "set", "--global", "default_agent", "gemini"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "config set --global should succeed; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let config_path = home.path().join(".amux").join("config.json");
    let json = std::fs::read_to_string(&config_path).unwrap();
    assert!(
        json.contains("default_agent") && json.contains("gemini"),
        "global config.json should contain default_agent=gemini; got: {}",
        json
    );
}

// 9. config set agent unknown_agent — exits non-zero, no file created
#[test]
fn config_set_invalid_agent_value_exits_nonzero_and_does_not_write() {
    let home = TempDir::new().unwrap();
    let repo = make_git_repo();

    let output = amux_with_home(home.path())
        .current_dir(repo.path())
        .args(["config", "set", "agent", "unknown_agent"])
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "invalid agent value should exit non-zero"
    );
    assert!(
        !repo.path().join(".amux").join("config.json").exists(),
        "config file must not be created after a failed set"
    );
}

// 10. config set auto_agent_auth_accepted — exits non-zero, no file created
#[test]
fn config_set_auto_agent_auth_accepted_exits_nonzero() {
    let home = TempDir::new().unwrap();
    let repo = make_git_repo();

    let output = amux_with_home(home.path())
        .current_dir(repo.path())
        .args(["config", "set", "auto_agent_auth_accepted", "true"])
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "setting auto_agent_auth_accepted should exit non-zero; stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        !repo.path().join(".amux").join("config.json").exists(),
        "config file must not be created after a rejected set"
    );
}

// 11. config set --global runtime apple-containers on non-macOS emits a platform warning
#[cfg(not(target_os = "macos"))]
#[test]
fn config_set_global_runtime_apple_containers_warns_on_non_macos() {
    let home = TempDir::new().unwrap();
    let repo = make_git_repo();

    let output = amux_with_home(home.path())
        .current_dir(repo.path())
        .args(["config", "set", "--global", "runtime", "apple-containers"])
        .output()
        .unwrap();

    // Should still succeed (value is written).
    assert!(
        output.status.success(),
        "should exit 0 even with platform warning; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Warning") && stderr.contains("apple-containers"),
        "should emit a platform warning on non-macOS; stderr: {}",
        stderr
    );
    // Verify value was still written.
    let config_path = home.path().join(".amux").join("config.json");
    let json = std::fs::read_to_string(&config_path).unwrap();
    assert!(
        json.contains("apple-containers"),
        "value should still be written despite warning; got: {}",
        json
    );
}

// 12. config set --global default_agent — warns when repo already overrides via `agent`
#[test]
fn config_set_global_default_agent_warns_when_repo_already_sets_agent() {
    let home = TempDir::new().unwrap();
    let repo = make_git_repo();
    // Pre-populate repo config with agent=codex.
    write_repo_config(repo.path(), r#"{"agent":"codex"}"#);

    let output = amux_with_home(home.path())
        .current_dir(repo.path())
        .args(["config", "set", "--global", "default_agent", "gemini"])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Warning") && stderr.contains("repo config overrides"),
        "should warn that repo overrides the new global value; stderr: {}",
        stderr
    );
}

// 13. config set env_passthrough "" — writes envPassthrough: [] (empty array, not omitted)
#[test]
fn config_set_env_passthrough_empty_string_writes_empty_array() {
    let home = TempDir::new().unwrap();
    let repo = make_git_repo();

    let output = amux_with_home(home.path())
        .current_dir(repo.path())
        .args(["config", "set", "env_passthrough", ""])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "config set env_passthrough '' should succeed; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let config_path = repo.path().join(".amux").join("config.json");
    let json = std::fs::read_to_string(&config_path).unwrap();
    assert!(
        json.contains("envPassthrough") && json.contains("[]"),
        "JSON must contain envPassthrough: [] (not omitted); got: {}",
        json
    );
}

// Round-trip: set → get → show
#[test]
fn config_round_trip_set_get_show() {
    let home = TempDir::new().unwrap();
    let repo = make_git_repo();

    // Set terminal_scrollback_lines to 7777 at repo level.
    let set_out = amux_with_home(home.path())
        .current_dir(repo.path())
        .args(["config", "set", "terminal_scrollback_lines", "7777"])
        .output()
        .unwrap();
    assert!(set_out.status.success(), "set failed: {}", String::from_utf8_lossy(&set_out.stderr));

    // get should reflect the new value.
    let get_out = amux_with_home(home.path())
        .current_dir(repo.path())
        .args(["config", "get", "terminal_scrollback_lines"])
        .output()
        .unwrap();
    assert!(get_out.status.success());
    assert!(
        String::from_utf8_lossy(&get_out.stdout).contains("7777"),
        "get should return 7777"
    );

    // show should reflect the new value.
    let show_out = amux_with_home(home.path())
        .current_dir(repo.path())
        .args(["config", "show"])
        .output()
        .unwrap();
    assert!(show_out.status.success());
    assert!(
        String::from_utf8_lossy(&show_out.stdout).contains("7777"),
        "show should display 7777"
    );
}

// ── --agent flag integration tests (work item 0049) ──────────────────────────

#[test]
fn chat_help_shows_agent_flag() {
    let output = amux()
        .args(["chat", "--help"])
        .output()
        .expect("failed to run amux");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("--agent"),
        "chat --help should mention --agent flag; got: {}",
        stdout
    );
}

#[test]
fn chat_unknown_agent_exits_nonzero_with_error() {
    let output = amux()
        .args(["chat", "--agent", "unknown"])
        .output()
        .expect("failed to run amux");
    assert!(
        !output.status.success(),
        "chat --agent unknown should exit non-zero"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("unknown") || stderr.contains("available agents"),
        "stderr should describe the unknown agent error; got: {}",
        stderr
    );
}

// ── exec subcommand integration tests (work item 0058) ───────────────────────

#[test]
fn exec_help_shows_prompt_and_workflow_subcommands() {
    let output = amux()
        .args(["exec", "--help"])
        .output()
        .expect("failed to run amux exec --help");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("prompt"),
        "exec --help must mention 'prompt' subcommand; got: {stdout}"
    );
    assert!(
        stdout.contains("workflow"),
        "exec --help must mention 'workflow' subcommand; got: {stdout}"
    );
}

#[test]
fn exec_prompt_help_shows_all_flags() {
    let output = amux()
        .args(["exec", "prompt", "--help"])
        .output()
        .expect("failed to run amux exec prompt --help");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    for flag in &["--non-interactive", "--plan", "--allow-docker", "--mount-ssh",
                  "--yolo", "--auto", "--agent", "--model"] {
        assert!(
            stdout.contains(flag),
            "exec prompt --help must mention {flag}; got: {stdout}"
        );
    }
}

#[test]
fn exec_prompt_help_shows_short_n_alias() {
    let output = amux()
        .args(["exec", "prompt", "--help"])
        .output()
        .expect("failed to run amux exec prompt --help");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    // clap renders short flags as `-n, --non-interactive`
    assert!(
        stdout.contains("-n"),
        "exec prompt --help must show -n short alias for --non-interactive; got: {stdout}"
    );
}

#[test]
fn exec_workflow_help_shows_all_flags() {
    let output = amux()
        .args(["exec", "workflow", "--help"])
        .output()
        .expect("failed to run amux exec workflow --help");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    for flag in &["--work-item", "--non-interactive", "--plan", "--allow-docker",
                  "--worktree", "--mount-ssh", "--yolo", "--auto", "--agent", "--model"] {
        assert!(
            stdout.contains(flag),
            "exec workflow --help must mention {flag}; got: {stdout}"
        );
    }
}

#[test]
fn exec_workflow_help_shows_short_n_alias() {
    let output = amux()
        .args(["exec", "workflow", "--help"])
        .output()
        .expect("failed to run amux exec workflow --help");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("-n"),
        "exec workflow --help must show -n short alias for --non-interactive; got: {stdout}"
    );
}

#[test]
fn exec_wf_alias_help_works() {
    let alias_out = amux()
        .args(["exec", "wf", "--help"])
        .output()
        .expect("failed to run amux exec wf --help");
    assert!(
        alias_out.status.success(),
        "`exec wf --help` must succeed (wf is an alias for workflow); \
         stderr: {}",
        String::from_utf8_lossy(&alias_out.stderr)
    );
    // Alias help must mention the same flags as the canonical command.
    let stdout = String::from_utf8_lossy(&alias_out.stdout);
    assert!(
        stdout.contains("--work-item"),
        "exec wf --help must show --work-item flag; got: {stdout}"
    );
}

#[test]
fn exec_prompt_without_git_repo_exits_with_git_error_not_unknown_subcommand() {
    // Run from a temp dir that is NOT a git repo so the binary fails at
    // find_git_root rather than at argument parsing.  The error must mention
    // "git" (or "Git"), not "unknown" — proving the subcommand was recognised.
    let not_a_repo = TempDir::new().unwrap();
    let output = amux()
        .current_dir(not_a_repo.path())
        .args(["exec", "prompt", "hello"])
        .output()
        .expect("failed to run amux exec prompt");
    assert!(
        !output.status.success(),
        "exec prompt must exit non-zero outside a git repo"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stderr_lower = stderr.to_lowercase();
    assert!(
        stderr_lower.contains("git") || stderr_lower.contains("repository"),
        "error must be a git-not-found error, not an unknown-subcommand error; \
         got: {stderr}"
    );
    // Must NOT say "unknown" or "unrecognized" (clap parse failure messages).
    assert!(
        !stderr_lower.contains("unknown command") && !stderr_lower.contains("unrecognized"),
        "exec prompt must be a recognised subcommand; got: {stderr}"
    );
}

#[test]
fn exec_workflow_without_git_repo_exits_with_git_error_not_unknown_subcommand() {
    let not_a_repo = TempDir::new().unwrap();
    let output = amux()
        .current_dir(not_a_repo.path())
        .args(["exec", "workflow", "./wf.md"])
        .output()
        .expect("failed to run amux exec workflow");
    assert!(
        !output.status.success(),
        "exec workflow must exit non-zero outside a git repo"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stderr_lower = stderr.to_lowercase();
    assert!(
        stderr_lower.contains("git") || stderr_lower.contains("repository"),
        "error must be a git-not-found error; got: {stderr}"
    );
    assert!(
        !stderr_lower.contains("unknown command") && !stderr_lower.contains("unrecognized"),
        "exec workflow must be a recognised subcommand; got: {stderr}"
    );
}

// ── headless config integration tests (work item 0058) ───────────────────────

#[test]
fn config_get_headless_always_non_interactive_default_is_false() {
    let home = TempDir::new().unwrap();
    let repo = make_git_repo();

    let output = amux_with_home(home.path())
        .current_dir(repo.path())
        .args(["config", "get", "headless.alwaysNonInteractive"])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "config get headless.alwaysNonInteractive must exit 0; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Effective value when not set should be "false" (the built-in default).
    assert!(
        stdout.contains("false"),
        "headless.alwaysNonInteractive must default to false; stdout: {stdout}"
    );
}

#[test]
fn config_set_and_get_headless_always_non_interactive_global() {
    let home = TempDir::new().unwrap();
    let repo = make_git_repo();

    // Set the flag globally.
    let set_out = amux_with_home(home.path())
        .current_dir(repo.path())
        .args(["config", "set", "--global", "headless.alwaysNonInteractive", "true"])
        .output()
        .unwrap();
    assert!(
        set_out.status.success(),
        "config set --global headless.alwaysNonInteractive true must succeed; stderr: {}",
        String::from_utf8_lossy(&set_out.stderr)
    );

    // Verify the written JSON uses the camelCase nested key.
    let config_path = home.path().join(".amux").join("config.json");
    let json = std::fs::read_to_string(&config_path).unwrap();
    assert!(
        json.contains("alwaysNonInteractive") && json.contains("true"),
        "global config.json must contain alwaysNonInteractive: true; got: {json}"
    );

    // config get must reflect the new value.
    let get_out = amux_with_home(home.path())
        .current_dir(repo.path())
        .args(["config", "get", "headless.alwaysNonInteractive"])
        .output()
        .unwrap();
    assert!(get_out.status.success());
    let stdout = String::from_utf8_lossy(&get_out.stdout);
    assert!(
        stdout.contains("true"),
        "config get must return 'true' after set --global; stdout: {stdout}"
    );
}

#[test]
fn config_get_headless_work_dirs_works() {
    let home = TempDir::new().unwrap();
    let repo = make_git_repo();

    // Query the field without setting it first — must succeed and show default.
    let output = amux_with_home(home.path())
        .current_dir(repo.path())
        .args(["config", "get", "headless.workDirs"])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "config get headless.workDirs must exit 0; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    // Default is empty (no work dirs configured).
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("empty") || stdout.contains("(not set)") || stdout.contains("[]"),
        "headless.workDirs must show empty/not-set when unconfigured; stdout: {stdout}"
    );
}

// ── `amux new` subcommand integration tests (work item 0064) ─────────────────

use std::io::Write;
use std::process::Stdio;

/// Helper: spawn amux with piped stdin and write the given bytes, then collect output.
fn run_amux_with_stdin(
    cmd: &mut std::process::Command,
    stdin_bytes: &[u8],
) -> std::process::Output {
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn amux");

    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(stdin_bytes).ok();
    }
    child.wait_with_output().expect("failed to wait for amux")
}

// 1. `amux new --help` lists spec, workflow, and skill subcommands.
#[test]
fn new_help_lists_workflow_and_skill_subcommands() {
    let output = amux()
        .args(["new", "--help"])
        .output()
        .expect("failed to run amux new --help");
    assert!(output.status.success(), "stderr: {}", String::from_utf8_lossy(&output.stderr));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("workflow"), "`new --help` must mention 'workflow'; got: {stdout}");
    assert!(stdout.contains("skill"), "`new --help` must mention 'skill'; got: {stdout}");
    assert!(stdout.contains("spec"), "`new --help` must mention 'spec'; got: {stdout}");
}

// 2. `amux new spec --help` shows --interview.
#[test]
fn new_spec_help_mentions_interview() {
    let out = amux()
        .args(["new", "spec", "--help"])
        .output()
        .expect("failed to run amux new spec --help");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("--interview"),
        "`new spec --help` must mention --interview; got: {stdout}"
    );
}

// 3. `amux new workflow --help` shows --interview, --global, --format.
#[test]
fn new_workflow_help_shows_flags() {
    let output = amux()
        .args(["new", "workflow", "--help"])
        .output()
        .expect("failed to run amux new workflow --help");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    for flag in &["--interview", "--global", "--format"] {
        assert!(
            stdout.contains(flag),
            "`new workflow --help` must mention {flag}; got: {stdout}"
        );
    }
}

// 4. `amux new skill --help` shows --interview, --global.
#[test]
fn new_skill_help_shows_flags() {
    let output = amux()
        .args(["new", "skill", "--help"])
        .output()
        .expect("failed to run amux new skill --help");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    for flag in &["--interview", "--global"] {
        assert!(
            stdout.contains(flag),
            "`new skill --help` must mention {flag}; got: {stdout}"
        );
    }
}

// 5. `amux new workflow` with stdin writes a TOML file to aspec/workflows/.
#[test]
fn new_workflow_stdin_writes_toml_to_aspec_workflows() {
    let home = TempDir::new().unwrap();
    let repo = make_git_repo();

    // Stdin sequence for non-interview, non-global workflow with one step.
    // Prompts in order:
    //   "Workflow name: "
    //   "Workflow title (human-readable): "
    //   "Step name: "
    //   "Agent (optional, ...): "
    //   "Model (optional, ...): "
    //   "Depends-on (optional, ...): "
    //   "Enter prompt text. End with a line containing only '.':"\n<prompt>\n.\n
    //   "Add another step? [y/N]: "
    let stdin = b"my-wf\nMy Workflow\nstep-one\n\n\n\nDo the thing.\n.\nn\n";

    let output = run_amux_with_stdin(
        amux_with_home(home.path())
            .current_dir(repo.path())
            .args(["new", "workflow"]),
        stdin,
    );

    assert!(
        output.status.success(),
        "amux new workflow must succeed; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let dest = repo.path().join("aspec").join("workflows").join("my-wf.toml");
    assert!(dest.exists(), "TOML workflow file must be created at {}", dest.display());

    let content = std::fs::read_to_string(&dest).unwrap();
    assert!(content.contains("My Workflow"), "title must appear in file");
    assert!(content.contains("step-one"), "step name must appear in file");
}

// 6. `amux new workflow --format yaml` writes a .yaml file.
#[test]
fn new_workflow_format_yaml_writes_yaml_file() {
    let home = TempDir::new().unwrap();
    let repo = make_git_repo();

    let stdin = b"yaml-wf\nYAML Workflow\nstep-a\n\n\n\nDo it.\n.\nn\n";

    let output = run_amux_with_stdin(
        amux_with_home(home.path())
            .current_dir(repo.path())
            .args(["new", "workflow", "--format", "yaml"]),
        stdin,
    );

    assert!(
        output.status.success(),
        "amux new workflow --format yaml must succeed; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let dest = repo.path().join("aspec").join("workflows").join("yaml-wf.yaml");
    assert!(dest.exists(), ".yaml file must be created");
    let content = std::fs::read_to_string(&dest).unwrap();
    assert!(content.contains("YAML Workflow"));
}

// 7. `amux new workflow --format md` writes a .md file.
#[test]
fn new_workflow_format_md_writes_md_file() {
    let home = TempDir::new().unwrap();
    let repo = make_git_repo();

    let stdin = b"md-wf\nMD Workflow\nstep-b\n\n\n\nDo it.\n.\nn\n";

    let output = run_amux_with_stdin(
        amux_with_home(home.path())
            .current_dir(repo.path())
            .args(["new", "workflow", "--format", "md"]),
        stdin,
    );

    assert!(
        output.status.success(),
        "amux new workflow --format md must succeed; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let dest = repo.path().join("aspec").join("workflows").join("md-wf.md");
    assert!(dest.exists(), ".md file must be created");
    let content = std::fs::read_to_string(&dest).unwrap();
    assert!(content.starts_with("# MD Workflow"), "MD file must start with title heading");
}

// 8. `amux new workflow --global` writes to ~/.amux/workflows/.
#[test]
fn new_workflow_global_writes_to_global_workflows_dir() {
    let home = TempDir::new().unwrap();
    // AMUX_CONFIG_HOME redirects global_workflows_dir() to home/.amux.
    // The integration binary uses this env var for the global dir.
    // We run outside a git repo to confirm --global works without one.
    let not_a_repo = TempDir::new().unwrap();

    let stdin = b"global-wf\nGlobal Workflow\nstep-g\n\n\n\nDo globally.\n.\nn\n";

    let output = run_amux_with_stdin(
        amux_with_home(home.path())
            .env("AMUX_CONFIG_HOME", home.path().join(".amux"))
            .current_dir(not_a_repo.path())
            .args(["new", "workflow", "--global"]),
        stdin,
    );

    assert!(
        output.status.success(),
        "amux new workflow --global must succeed outside a git repo; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let dest = home.path().join(".amux").join("workflows").join("global-wf.toml");
    assert!(dest.exists(), "global TOML file must be created at {}", dest.display());
    let content = std::fs::read_to_string(&dest).unwrap();
    assert!(content.contains("Global Workflow"));
}

// 9. `amux new workflow --interview --global` writes a skeleton file before failing on agent lookup.
#[test]
fn new_workflow_interview_global_writes_skeleton_before_agent_lookup_fails() {
    let home = TempDir::new().unwrap();
    let not_a_repo = TempDir::new().unwrap();

    // Stdin: name + summary (interview mode only prompts these two).
    let stdin = b"interview-wf\nSome brief summary.\n";

    let output = run_amux_with_stdin(
        amux_with_home(home.path())
            .env("AMUX_CONFIG_HOME", home.path().join(".amux"))
            .current_dir(not_a_repo.path())
            .args(["new", "workflow", "--interview", "--global"]),
        stdin,
    );

    // Command exits non-zero because interview requires a git repo for agent image lookup.
    assert!(
        !output.status.success(),
        "amux new workflow --interview --global must fail without a git repo"
    );
    // But the skeleton file should have been written before the failure.
    let dest = home.path().join(".amux").join("workflows").join("interview-wf.toml");
    assert!(
        dest.exists(),
        "skeleton file must be created before agent lookup fails; path: {}",
        dest.display()
    );
}

// 10. `amux new skill` writes SKILL.md to .claude/skills/<name>/.
#[test]
fn new_skill_stdin_writes_skill_md_to_claude_skills() {
    let home = TempDir::new().unwrap();
    let repo = make_git_repo();

    // Stdin: name, description, body (period-terminated), then EOF.
    let stdin = b"my-skill\nDoes something useful.\nRun the tests.\n.\n";

    let output = run_amux_with_stdin(
        amux_with_home(home.path())
            .current_dir(repo.path())
            .args(["new", "skill"]),
        stdin,
    );

    assert!(
        output.status.success(),
        "amux new skill must succeed; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let dest = repo
        .path()
        .join(".claude")
        .join("skills")
        .join("my-skill")
        .join("SKILL.md");
    assert!(dest.exists(), "SKILL.md must be created at {}", dest.display());

    let content = std::fs::read_to_string(&dest).unwrap();
    assert!(content.starts_with("---\n"), "SKILL.md must start with YAML frontmatter");
    assert!(content.contains("name: my-skill"), "frontmatter must contain name");
    assert!(content.contains("Does something useful."), "frontmatter must contain description");
    assert!(content.contains("Run the tests."), "body must be written");
}

// 11. `amux new skill --global` writes to ~/.amux/skills/<name>/SKILL.md.
#[test]
fn new_skill_global_writes_to_global_skills_dir() {
    let home = TempDir::new().unwrap();
    let not_a_repo = TempDir::new().unwrap();

    let stdin = b"global-skill\nA global skill.\nDo things globally.\n.\n";

    let output = run_amux_with_stdin(
        amux_with_home(home.path())
            .env("AMUX_CONFIG_HOME", home.path().join(".amux"))
            .current_dir(not_a_repo.path())
            .args(["new", "skill", "--global"]),
        stdin,
    );

    assert!(
        output.status.success(),
        "amux new skill --global must succeed outside a git repo; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let dest = home
        .path()
        .join(".amux")
        .join("skills")
        .join("global-skill")
        .join("SKILL.md");
    assert!(dest.exists(), "global SKILL.md must be created at {}", dest.display());
    let content = std::fs::read_to_string(&dest).unwrap();
    assert!(content.contains("name: global-skill"));
    assert!(content.contains("A global skill."));
}

// 12. `amux new skill --interview --global` writes skeleton before agent lookup fails.
#[test]
fn new_skill_interview_global_writes_skeleton_before_agent_lookup_fails() {
    let home = TempDir::new().unwrap();
    let not_a_repo = TempDir::new().unwrap();

    // Stdin: name, description, summary.
    let stdin = b"interview-skill\nDoes interview things.\nBrief summary here.\n";

    let output = run_amux_with_stdin(
        amux_with_home(home.path())
            .env("AMUX_CONFIG_HOME", home.path().join(".amux"))
            .current_dir(not_a_repo.path())
            .args(["new", "skill", "--interview", "--global"]),
        stdin,
    );

    // Must fail because interview mode requires a git repo for agent image lookup.
    assert!(
        !output.status.success(),
        "amux new skill --interview --global must fail without a git repo"
    );
    // Skeleton must be written before the failure.
    let dest = home
        .path()
        .join(".amux")
        .join("skills")
        .join("interview-skill")
        .join("SKILL.md");
    assert!(
        dest.exists(),
        "skeleton SKILL.md must be created before agent lookup fails; path: {}",
        dest.display()
    );
    let content = std::fs::read_to_string(&dest).unwrap();
    assert!(content.contains("Agent will complete"), "skeleton must contain placeholder");
}

// 13. End-to-end: `amux new workflow` output file parses without error via `amux exec workflow`.
#[test]
fn new_workflow_roundtrip_exec_workflow_parses_output() {
    let home = TempDir::new().unwrap();
    let repo = make_git_repo();

    let stdin = b"e2e-wf\nE2E Workflow\nstep-x\n\n\n\nRun the check.\n.\nn\n";
    let create_out = run_amux_with_stdin(
        amux_with_home(home.path())
            .current_dir(repo.path())
            .args(["new", "workflow"]),
        stdin,
    );
    assert!(
        create_out.status.success(),
        "workflow creation must succeed; stderr: {}",
        String::from_utf8_lossy(&create_out.stderr)
    );

    let wf_path = repo.path().join("aspec").join("workflows").join("e2e-wf.toml");
    assert!(wf_path.exists(), "workflow file must exist");

    // `exec workflow` will fail (no Docker), but must not fail at parse time.
    // A parse failure would report something like "invalid TOML" or "missing title".
    let exec_out = amux_with_home(home.path())
        .current_dir(repo.path())
        .args(["exec", "workflow", wf_path.to_str().unwrap()])
        .output()
        .expect("failed to run amux exec workflow");

    // exec workflow will fail (no git repo agent config / Docker), but must not
    // emit a TOML parse error.
    let stderr = String::from_utf8_lossy(&exec_out.stderr);
    assert!(
        !stderr.to_lowercase().contains("parse") && !stderr.to_lowercase().contains("invalid toml"),
        "exec workflow must not report a parse error on a freshly-created file; stderr: {stderr}"
    );
}

// 14. End-to-end: `amux new skill` output has valid YAML frontmatter.
#[test]
fn new_skill_output_has_parseable_yaml_frontmatter() {
    let home = TempDir::new().unwrap();
    let repo = make_git_repo();

    let stdin = b"yaml-skill\nParses correctly.\nStep one.\n.\n";
    let output = run_amux_with_stdin(
        amux_with_home(home.path())
            .current_dir(repo.path())
            .args(["new", "skill"]),
        stdin,
    );
    assert!(
        output.status.success(),
        "skill creation must succeed; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let dest = repo
        .path()
        .join(".claude")
        .join("skills")
        .join("yaml-skill")
        .join("SKILL.md");
    let content = std::fs::read_to_string(&dest).unwrap();

    // Extract YAML frontmatter between the first two "---" delimiters.
    let parts: Vec<&str> = content.splitn(3, "---").collect();
    assert!(parts.len() >= 3, "SKILL.md must contain opening and closing --- delimiters");
    let frontmatter = parts[1];
    // A minimal YAML parse: verify it's not empty and contains name/description keys.
    assert!(
        frontmatter.contains("name:"),
        "YAML frontmatter must contain 'name:'; frontmatter:\n{frontmatter}"
    );
    assert!(
        frontmatter.contains("description:"),
        "YAML frontmatter must contain 'description:'; frontmatter:\n{frontmatter}"
    );
}

