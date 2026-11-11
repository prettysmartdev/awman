//! Isolation canary: a bare `cargo test` never touches the real HOME, git
//! configuration, keychain or clipboard — after a passing child, a failing
//! child and a child killed at its deadline.
//!
//! The test process's own environment is never changed (that is forbidden
//! in tests). Instead the canary snapshots the real files a leaking child
//! would touch, drives children that try to read and write HOME and git
//! configuration, and proves nothing outside the sandbox changed and no
//! child saw a real path.

use crate::{expect_panic, fake_cli_exe, sandbox};
use awman_test_support::{Exit, TestSandbox};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Files under the real HOME that a leaking child would change.
const WATCHED: &[&str] = &[
    ".gitconfig",
    ".config/git/config",
    ".awman",
    ".awman/config.json",
    ".claude",
    ".codex",
    ".docker/config.json",
    ".ssh",
];

fn snapshot(home: &Path) -> BTreeMap<PathBuf, Option<(u64, SystemTime)>> {
    WATCHED
        .iter()
        .map(|rel| {
            let path = home.join(rel);
            let state = match std::fs::symlink_metadata(&path) {
                Ok(m) => Some((m.len(), m.modified().unwrap_or(SystemTime::UNIX_EPOCH))),
                Err(_) => None,
            };
            (path, state)
        })
        .collect()
}

/// The real home directory (HOME / USERPROFILE), which every CI runner and
/// developer machine sets; the canary cannot run without knowing it.
fn real_home() -> PathBuf {
    awman_test_support::runtime_env::get()
        .real_home
        .clone()
        .expect("HOME (USERPROFILE on Windows) must be set for the isolation canary")
}

fn assert_no_real_paths(sandbox: &TestSandbox, env: &BTreeMap<String, String>) {
    let home = real_home().to_string_lossy().into_owned();
    for (k, v) in env {
        // The sandbox may itself live under the real HOME (TMPDIR there);
        // only values outside the sandbox root count.
        let outside = v
            .split(if cfg!(windows) { ';' } else { ':' })
            .any(|part| part.starts_with(&home) && !Path::new(part).starts_with(sandbox.root()));
        assert!(!outside, "child saw the real HOME through {k}={v}");
    }
}

#[test]
fn children_never_see_or_change_the_real_home_or_git_config() {
    let home = real_home();
    let before = snapshot(&home);
    let sandbox = sandbox("canary::children_never_see_or_change_the_real_home_or_git_config");
    assert_ne!(sandbox.home(), home);
    let fake = sandbox.install_fake(fake_cli_exe(), "agentish");
    fake.on(&["ok"]).stdout("fine\n");
    fake.on(&["fail"]).stderr("bad\n").exit(1);
    fake.on(&["hang"]).hold_until_released();

    // Pass, fail and timeout.
    assert_eq!(
        sandbox.command("agentish").arg("ok").run().exit,
        Exit::Code(0)
    );
    assert_eq!(
        sandbox.command("agentish").arg("fail").run().exit,
        Exit::Code(1)
    );
    let hung = sandbox
        .command("agentish")
        .arg("hang")
        .deadline(Duration::from_millis(300))
        .run();
    assert_eq!(hung.exit, Exit::KilledAtDeadline);
    for call in fake.calls() {
        assert_eq!(
            call.env.get("HOME").map(PathBuf::from),
            Some(sandbox.home())
        );
        assert_eq!(
            call.env.get("AWMAN_TEST_ISOLATION").map(String::as_str),
            Some("1")
        );
        for var in [
            "SSH_AUTH_SOCK",
            "DISPLAY",
            "WAYLAND_DISPLAY",
            "DBUS_SESSION_BUS_ADDRESS",
            "XDG_RUNTIME_DIR",
            "GITHUB_TOKEN",
            "GH_TOKEN",
            "DOCKER_HOST",
        ] {
            assert!(!call.env.contains_key(var), "child saw {var}");
        }
        assert_no_real_paths(&sandbox, &call.env);
    }

    // A child writing "global" git config writes the sandbox's file.
    let out = sandbox
        .command("git")
        .args(["config", "--global", "user.canary", "written-by-test"])
        .run();
    out.assert_success();
    let sandbox_config = std::fs::read_to_string(sandbox.root().join("gitconfig")).unwrap();
    assert!(sandbox_config.contains("written-by-test"));
    let out = sandbox
        .command("sh")
        .args([
            "-c",
            "echo x > \"$HOME/.gitconfig\"; mkdir -p \"$HOME/.awman\"",
        ])
        .run();
    out.assert_success();
    assert!(sandbox.home().join(".gitconfig").is_file());

    // Keychain and clipboard tools are refused outright.
    for tool in ["security", "secret-tool", "pbcopy", "xclip", "wl-copy"] {
        let text = expect_panic(|| {
            sandbox
                .command("sh")
                .args(["-c", &format!("{tool} --help")])
                .run();
        });
        assert!(text.starts_with("REFUSED: "), "{tool}: {text}");
    }

    sandbox.finish();
    assert_eq!(snapshot(&home), before, "the real HOME changed");
}

#[test]
fn sandboxes_live_under_the_test_temp_root() {
    let sandbox = sandbox("canary::sandboxes_live_under_the_test_temp_root");
    let runtime = awman_test_support::runtime_env::get();
    let parent = runtime
        .tmproot
        .clone()
        .unwrap_or_else(|| runtime.system_temp.clone());
    let canonical = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    assert!(canonical(sandbox.root()).starts_with(canonical(&parent)));
    for dir in [
        sandbox.home(),
        sandbox.awman_home(),
        sandbox.xdg_config(),
        sandbox.tmp(),
    ] {
        assert!(dir.starts_with(sandbox.root()));
    }
}
