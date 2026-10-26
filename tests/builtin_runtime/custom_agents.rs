//! WI0130 actual guest coverage through the existing hardware/fixture gate.
use std::path::Path;
use std::process::Command;
use std::time::Duration;

// Exactly the same custom definition as the Docker and Apple runtime tests.
#[path = "../helpers/custom_agents.rs"]
mod fixtures;

#[test]
fn builtin_hw_custom_pi_ready_and_chat_runtime_smoke() {
    const TEST: &str = "builtin_hw_custom_pi_ready_and_chat_runtime_smoke";
    let Some((binary, archive)) = crate::lifecycle::gate(TEST) else {
        return;
    };
    // Keep state/socket paths short, as required by the existing builtin harness.
    let temp = tempfile::Builder::new()
        .prefix("wi130.")
        .tempdir_in("/tmp")
        .unwrap();
    let repo = temp.path().join("repo");
    let home = temp.path().join("home");
    let bin = temp.path().join("bin");
    let state = tempfile::Builder::new()
        .prefix("awb.")
        .tempdir_in("/tmp")
        .unwrap();
    for dir in [&repo, &home.join(".awman/agents"), &bin] {
        std::fs::create_dir_all(dir).unwrap();
    }
    let git = Command::new("git")
        .args(["init", "-q"])
        .arg(&repo)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .unwrap();
    assert!(git.status.success(), "{git:?}");
    std::fs::create_dir_all(repo.join(".awman")).unwrap();
    std::fs::write(repo.join("Dockerfile.dev"), "FROM scratch\n").unwrap();
    std::fs::write(home.join(".awman/agents/pi.toml"), fixtures::RUNTIME_SMOKE).unwrap();
    std::fs::write(
        repo.join(".awman/config.json"),
        r#"{"agent":"pi","auth":"none"}"#,
    )
    .unwrap();
    std::fs::write(home.join(".awman/config.json"), serde_json::to_vec(&serde_json::json!({
        "runtime":"builtin-experimental", "default_agent":"pi",
        "builtin": { "stateDir":state.path().join("s"), "imageSource":{"type":"archive","path":archive}, "network":{"mode":"none"} }
    })).unwrap()).unwrap();
    // Trap accidental dependence on any host container CLI.
    let marker = temp.path().join("host-helper-called");
    for name in ["docker", "container", "sbx"] {
        use std::os::unix::fs::PermissionsExt;
        let path = bin.join(name);
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\nprintf invoked > {}\nexit 91\n",
                shell_words::quote(marker.to_str().unwrap())
            ),
        )
        .unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    fn command(binary: &Path, repo: &Path, home: &Path, bin: &Path) -> Command {
        let mut command = crate::lifecycle::awman(binary, repo, home, bin);
        command
            .env("AWMAN_TEST_ISOLATION", "1")
            .env("AWMAN_TEST_BUILTIN", "1");
        command
    }
    let mut ready = command(&binary, &repo, &home, &bin);
    ready.args(["ready", "--agent", "pi", "--json"]);
    let output = crate::lifecycle::run_bounded(ready, Duration::from_secs(180));
    assert!(output.status.success(), "{output:?}");
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["ready"], true, "{json}");
    let mut chat = command(&binary, &repo, &home, &bin);
    chat.args(["chat", "--agent", "pi", "--non-interactive"]);
    let output = crate::lifecycle::run_bounded(chat, Duration::from_secs(120));
    assert_eq!(output.status.code(), Some(37), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("WI0130_CUSTOM_AGENT"),
        "{output:?}"
    );
    assert!(
        !marker.exists(),
        "builtin must execute without a host container CLI"
    );
    eprintln!("EXECUTED: {TEST}: custom pi guest exited 37 with expected marker");
}
