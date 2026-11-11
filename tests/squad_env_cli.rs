//! WI 0116 §4a/§6a/§6b — the real `awman` binary against a real daemon.
//!
//! The daemon is an `awman daemon start` process with only the squad feature
//! on (isolated `AWMAN_HOME`, fake `docker` on its PATH); every `awman squad …`
//! command in the test is a separate subprocess that reaches it over the
//! owner-only local socket. Nothing starts a daemon on the CLI's behalf: with
//! none running a squad command refuses with "not running".

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

#[path = "helpers/api_process.rs"]
mod api_process;
use api_process::RootDaemon;

fn awman_binary() -> PathBuf {
    PathBuf::from(awman_test_support::cargo_bin!("awman"))
}

/// One `awman` invocation against the isolated root, with `extra` added to its
/// environment — the "launching shell" whose exports WI 0116 is about.
fn run(root: &Path, extra: &[(&str, &str)], args: &[&str]) -> Output {
    let mut command = Command::new(awman_binary());
    command
        .args(args)
        .current_dir(root)
        .env("AWMAN_HOME", root)
        .env("HOME", root)
        .env("AWMAN_TEST_ISOLATION", "1")
        .env_remove("WI0116_CLI_TOKEN")
        .env_remove("AWMAN_OVERLAYS");
    for (key, value) in extra {
        command.env(key, value);
    }
    command.output().expect("awman must be executable")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// stdout plus stderr. `UserMessage`s (the §6a warning among them) go to
/// stderr so a `--json` stdout stays machine-readable.
fn combined(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    )
}

fn init_repo(path: &Path) {
    std::fs::create_dir_all(path).unwrap();
    let ok = Command::new("git")
        .args(["init", "-q"])
        .current_dir(path)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    assert!(ok, "git init must succeed");
}

fn env_rows(json: &str) -> serde_json::Value {
    let start = json.find('{').expect("a JSON object in the output");
    serde_json::from_str::<serde_json::Value>(&json[start..]).expect("parsable --json output")
        ["payload"]["rows"]
        .clone()
}

fn row<'a>(rows: &'a serde_json::Value, name: &str) -> &'a serde_json::Value {
    rows.as_array()
        .expect("rows is an array")
        .iter()
        .find(|row| row["name"] == name)
        .unwrap_or_else(|| panic!("no row for {name} in {rows}"))
}

fn squad_config(root: &Path) {
    std::fs::write(
        root.join("config.json"),
        r#"{"daemon":{"features":{"squad":{"enabled":true,"envPersistence":"none"}}}}"#,
    )
    .unwrap();
}

fn add_nightly(root: &Path, repo: &Path) -> Output {
    run(
        root,
        &[],
        &[
            "squad",
            "add",
            "--name",
            "nightly",
            "--description",
            "declares an env() overlay",
            "--repo",
            repo.to_str().unwrap(),
            "--interval",
            "3600",
            "--mount-scope",
            "gitroot",
            "--overlay",
            "env(WI0116_CLI_TOKEN)",
            "--headless",
        ],
    )
}

/// **The sync arms the daemon on every command.**
///
/// 1. `squad add --overlay env(WI0116_CLI_TOKEN)` from a shell that does not
///    export it — §6a warns, **and the task is created anyway**.
/// 2. `squad env` from a shell that *does* export it arms the running daemon.
/// 3. `squad env` from a shell that does not leaves it armed (`pushed`), and a
///    rotation from a third shell replaces it without a restart.
#[test]
fn squad_env_arms_the_running_daemon_and_rotation_replaces_it() {
    if Command::new("git").arg("--version").output().is_err() {
        eprintln!("SKIP: git not available");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    squad_config(root);
    let repo = root.join("repo");
    init_repo(&repo);
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let daemon = runtime.block_on(RootDaemon::start(root));

    // ── 1. create the task from an under-equipped shell ──────────────────────
    let created = add_nightly(root, &repo);
    let text = combined(&created);
    assert!(created.status.success(), "squad add must succeed: {text}");
    assert!(
        text.contains(
            "\u{26a0} Task \"nightly\" was created, but the squad daemon has no value for \
             WI0116_CLI_TOKEN."
        ),
        "the §6a warning must render: {text}"
    );
    assert!(
        text.contains("Created task nightly."),
        "a warning is never a rejection — the task is created: {text}"
    );

    // Pause it so the scheduler never tries to launch a container.
    let paused = run(root, &[], &["squad", "pause", "nightly"]);
    assert!(paused.status.success(), "{}", stdout(&paused));

    // §6b: the standing list surface says so, and names the next step.
    let listed = combined(&run(root, &[], &["squad", "list"]));
    assert!(listed.contains("nightly \u{26a0} env"), "{listed}");
    assert!(
        listed.contains("\u{26a0} 1 task is missing an env value; see awman squad env"),
        "{listed}"
    );

    // ── 2. a shell that has the value arms the daemon ────────────────────────
    let armed = stdout(&run(
        root,
        &[("WI0116_CLI_TOKEN", "armed-on-first-use")],
        &["squad", "env", "--json"],
    ));
    let rows = env_rows(&armed);
    assert_eq!(row(&rows, "WI0116_CLI_TOKEN")["state"], "set", "{armed}");
    assert_eq!(row(&rows, "WI0116_CLI_TOKEN")["source"], "this shell");
    assert!(!armed.contains("armed-on-first-use"), "{armed}");

    // ── 3. a shell with nothing to offer must not disarm it ──────────────────
    let held = stdout(&run(root, &[], &["squad", "env", "--json"]));
    let rows = env_rows(&held);
    assert_eq!(row(&rows, "WI0116_CLI_TOKEN")["state"], "set", "{held}");
    assert_eq!(
        row(&rows, "WI0116_CLI_TOKEN")["source"],
        "pushed",
        "the value came from the daemon, not from this shell: {held}"
    );
    let rotated = stdout(&run(
        root,
        &[("WI0116_CLI_TOKEN", "rotated-value")],
        &["squad", "env", "--json"],
    ));
    assert_eq!(
        row(&env_rows(&rotated), "WI0116_CLI_TOKEN")["source"],
        "this shell",
        "a rotation reaches a long-lived daemon on the next command: {rotated}"
    );

    // With coverage complete the standing list goes quiet again.
    let listed = combined(&run(root, &[], &["squad", "list"]));
    assert!(!listed.contains("\u{26a0} env"), "{listed}");
    assert!(!listed.contains("missing an env value"), "{listed}");

    daemon.abort();
}

/// Nothing starts a daemon on the CLI's behalf: with none running a squad
/// command that needs one is refused and creates nothing.
#[test]
fn squad_commands_refuse_when_no_daemon_is_running() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    squad_config(root);
    let repo = root.join("repo");
    init_repo(&repo);

    let refused = add_nightly(root, &repo);
    assert!(!refused.status.success());
    let text = combined(&refused);
    assert!(text.contains("not running"), "{text}");
    assert!(
        !root.join("daemon").join("awman.pid").exists(),
        "no daemon may have been started"
    );
}

/// 0140 finding F2: bare `awman squad --headless` reads the squad feature's
/// `detail` from `GET /v1/status` as a `DaemonStatus`, which the feature does
/// not fill in (`missing field running`).
#[test]
fn bare_squad_headless_prints_the_daemon_status_and_unmet_env_count() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    squad_config(root);
    let repo = root.join("repo");
    init_repo(&repo);
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let daemon = runtime.block_on(RootDaemon::start(root));
    assert!(add_nightly(root, &repo).status.success());

    let status = run(root, &[], &["squad", "--headless"]);
    let text = combined(&status);
    assert!(status.status.success(), "{text}");
    assert!(text.contains("1 env value unmet"), "{text}");
    daemon.abort();
}
