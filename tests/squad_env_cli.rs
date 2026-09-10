//! WI 0116 §4a/§6a/§6b — the real `awman` binary against a real squad daemon
//! it started itself.
//!
//! Everything else in this work item's test suite either drives the daemon
//! in-process or puts a recording server where the daemon would be. One thing
//! neither can reach is **the spawn branch of
//! `SquadGatewayResolver::ensure_running`**: `resolve_daemon_binary` refuses to
//! re-exec a test harness (a binary under `target/.../deps`), so an integration
//! test can never take that branch in-process. Only a subprocess running the
//! built binary can, which is what this file does — and the sync-on-spawn is
//! the half of §4a that arms a daemon the user never had to think about.
//!
//! The daemon is started with an isolated `AWMAN_CONFIG_HOME`/`AWMAN_SQUAD_ROOT`
//! and stopped again by the test, so nothing here touches the developer's own
//! `~/.awman`. Skipped when the built binary cannot be found.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::OnceLock;

/// A stable, private copy of the built `awman`, for the same reason
/// `tests/squad_cli_gateway.rs` keeps one: cargo republishes the top-level
/// binary path with an unlink-then-relink, so a spawn racing a rebuild can see
/// a transient ENOENT on a binary that assuredly exists.
fn awman_binary() -> Option<PathBuf> {
    static SHARED: OnceLock<Option<PathBuf>> = OnceLock::new();
    SHARED
        .get_or_init(|| {
            let source = PathBuf::from(env!("CARGO_BIN_EXE_awman"));
            if !source.exists() {
                return None;
            }
            let dir = tempfile::Builder::new()
                .prefix("awman-env-cli-")
                .tempdir()
                .ok()?
                .keep();
            let dest = dir.join("awman");
            std::fs::copy(&source, &dest).ok()?;
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(&dest).ok()?.permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&dest, permissions).ok()?;
            Some(dest)
        })
        .clone()
}

/// One `awman` invocation against the isolated root, with `extra` added to its
/// environment — the "launching shell" whose exports WI 0116 is about.
fn run(binary: &Path, root: &Path, extra: &[(&str, &str)], args: &[&str]) -> Output {
    let mut command = Command::new(binary);
    command
        .args(args)
        .current_dir(root)
        .env("AWMAN_CONFIG_HOME", root)
        .env("AWMAN_SQUAD_ROOT", root.join("squad"))
        .env("HOME", root)
        // Deterministic: never inherit the developer's own key or tokens.
        .env_remove("AWMAN_SQUAD_KEY")
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
/// stderr so a `--json` stdout stays machine-readable; assertions about what
/// the user *sees* therefore read both.
fn combined(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    )
}

/// The plaintext bearer key the daemon minted on its first start. It is shown
/// exactly once, in the `export AWMAN_SQUAD_KEY=…` snippet.
fn minted_key(text: &str) -> Option<String> {
    text.split_once("export AWMAN_SQUAD_KEY=")
        .map(|(_, rest)| rest.split_whitespace().next().unwrap_or("").to_string())
        .filter(|key| key.len() == 64)
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

/// **`ensure_running` pushes on both of its branches.**
///
/// One process, four commands, and the daemon is stopped in between so the
/// second half genuinely takes the *spawn* branch:
///
/// 1. `squad add --overlay env(WI0116_CLI_TOKEN)` from a shell that does not
///    export it — §6a warns, **and the task is created anyway**.
/// 2. `squad stop`, so nothing is running.
/// 3. `squad env` from a shell that *does* export it — this call spawns the
///    daemon and must arm it in the same breath (**spawn branch**).
/// 4. `squad env` from a shell that does not — the daemon is already up and
///    still holds the value, reported as `pushed` (**already-running branch**),
///    and a rotation from a third shell replaces it without a restart.
#[test]
fn squad_env_arms_a_daemon_on_both_the_spawn_and_already_running_branches() {
    let Some(binary) = awman_binary() else {
        eprintln!("SKIP: the built awman binary is not available");
        return;
    };
    if Command::new("git").arg("--version").output().is_err() {
        eprintln!("SKIP: git not available");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let repo = root.join("repo");
    init_repo(&repo);

    // This test spawns a *real* daemon. Its stored keychain item is a single
    // fixed `awman-squad`/`daemon-env` pair, not keyed by storage root, so an
    // isolated `AWMAN_CONFIG_HOME` does not isolate it: with the default
    // `envPersistence: "keychain"` this test would overwrite the item a real
    // daemon on the developer's machine is using with its own fixture token.
    std::fs::write(
        root.join("config.json"),
        r#"{"squad":{"envPersistence":"none"}}"#,
    )
    .unwrap();

    // ── 1. create the task from an under-equipped shell ──────────────────────
    let created = run(
        &binary,
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
            "--non-interactive",
        ],
    );
    let text = combined(&created);
    assert!(
        created.status.success(),
        "squad add must succeed: {text}{}",
        String::from_utf8_lossy(&created.stderr)
    );
    let key = minted_key(&String::from_utf8_lossy(&created.stdout))
        .or_else(|| minted_key(&String::from_utf8_lossy(&created.stderr)))
        .expect("the first start mints and discloses a bearer key");
    let keyed = [("AWMAN_SQUAD_KEY", key.as_str())];

    // §6a, end to end: the warning is rendered *and* the task exists.
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

    // Pause it so the scheduler never tries to launch a container: this test is
    // about the env protocol, not about evaluation.
    let paused = run(&binary, root, &keyed, &["squad", "pause", "nightly"]);
    assert!(paused.status.success(), "{}", stdout(&paused));

    // §6b: the standing surfaces both say so, and name the next step.
    let listed = combined(&run(&binary, root, &keyed, &["squad", "list"]));
    assert!(listed.contains("nightly \u{26a0} env"), "{listed}");
    assert!(
        listed.contains("\u{26a0} 1 task is missing an env value; see awman squad env"),
        "{listed}"
    );
    let status = combined(&run(&binary, root, &keyed, &["squad", "status"]));
    assert!(status.contains("; 1 env value unmet"), "{status}");

    // ── 2. stop, so the next command must start one ─────────────────────────
    let stopped = run(&binary, root, &keyed, &["squad", "stop"]);
    assert!(stopped.status.success(), "{}", stdout(&stopped));

    // ── 3. the spawn branch ─────────────────────────────────────────────────
    let spawned = stdout(&run(
        &binary,
        root,
        &[
            ("AWMAN_SQUAD_KEY", key.as_str()),
            ("WI0116_CLI_TOKEN", "armed-on-spawn"),
        ],
        &["squad", "env", "--json"],
    ));
    let rows = env_rows(&spawned);
    assert_eq!(
        row(&rows, "WI0116_CLI_TOKEN")["state"],
        "set",
        "a daemon this command started must be armed by the same command: {spawned}"
    );
    assert_eq!(
        row(&rows, "WI0116_CLI_TOKEN")["source"],
        "this shell",
        "and the shell that supplied it is named: {spawned}"
    );
    assert!(
        !spawned.contains("armed-on-spawn"),
        "no surface may print the value: {spawned}"
    );

    // ── 4. the already-running branch ───────────────────────────────────────
    let held = stdout(&run(&binary, root, &keyed, &["squad", "env", "--json"]));
    let rows = env_rows(&held);
    assert_eq!(
        row(&rows, "WI0116_CLI_TOKEN")["state"],
        "set",
        "a shell with nothing to offer must not disarm the daemon: {held}"
    );
    assert_eq!(
        row(&rows, "WI0116_CLI_TOKEN")["source"],
        "pushed",
        "the value came from the daemon, not from this shell: {held}"
    );

    let rotated = stdout(&run(
        &binary,
        root,
        &[
            ("AWMAN_SQUAD_KEY", key.as_str()),
            ("WI0116_CLI_TOKEN", "rotated-value"),
        ],
        &["squad", "env", "--json"],
    ));
    assert_eq!(
        row(&env_rows(&rotated), "WI0116_CLI_TOKEN")["source"],
        "this shell",
        "a rotation reaches a long-lived daemon on the next command: {rotated}"
    );

    // With coverage complete the standing surfaces go quiet again.
    let listed = combined(&run(&binary, root, &keyed, &["squad", "list"]));
    assert!(!listed.contains("\u{26a0} env"), "{listed}");
    assert!(!listed.contains("missing an env value"), "{listed}");
    let status = combined(&run(&binary, root, &keyed, &["squad", "status"]));
    assert!(!status.contains("env value"), "{status}");

    let _ = run(&binary, root, &keyed, &["squad", "stop"]);
}
