//! Docker-gated binary end-to-end coverage for WI 0128 agent commands.

use std::process::{Command, Output};

fn run_awman(args: &[&str], root: &std::path::Path) -> Output {
    let config = root.join(".awman");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(config.join("config.json"), br#"{"runtime":"docker"}"#).unwrap();
    Command::new(crate::awman_bin())
        .current_dir(root)
        .env("HOME", root)
        .env("AWMAN_CONFIG_HOME", &config)
        .env_remove("XDG_CONFIG_HOME")
        .args(args)
        .output()
        .expect("run awman subprocess")
}

#[test]
fn docker_agents_list_aliases_and_status_are_single_pass() {
    if std::env::var("AWMAN_TEST_DOCKER").as_deref() != Ok("1") {
        eprintln!("SKIP: set AWMAN_TEST_DOCKER=1 to run Docker agent CLI e2e");
        return;
    }
    if !crate::helpers::docker_available() {
        eprintln!("SKIP: Docker daemon not available");
        return;
    }

    let root = tempfile::tempdir().unwrap();
    for args in [["agents", "ls"].as_slice(), ["agents", "status"].as_slice()] {
        let output = run_awman(args, root.path());
        assert!(
            output.status.success(),
            "{args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        if args[1] == "status" {
            assert_eq!(
                stdout
                    .lines()
                    .filter(|line| line.starts_with("Docker ·"))
                    .count(),
                1,
                "{stdout}"
            );
            assert!(stdout.matches('┌').count() <= 1, "{stdout}");
            assert!(stdout.matches("AGENT ID").count() <= 1, "{stdout}");
        } else {
            assert_eq!(stdout.matches('┌').count(), 1, "{stdout}");
            assert_eq!(stdout.matches("AGENT ID").count(), 1, "{stdout}");
        }
    }

    let output = run_awman(&["agents", "ps", "--json"], root.path());
    assert!(
        output.status.success(),
        "agents ps --json failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("ps --json emits JSON");
    assert!(json.is_array(), "ps --json must return an array: {json}");
    assert!(!String::from_utf8_lossy(&output.stdout).contains("AGENT ID"));
}
