//! Actual awman imports an image from the live Apple Containers store through
//! its in-process bridge, the Apple service is stopped, and the cached image
//! still runs offline in a real builtin guest.
//!
//! Needs, besides the builtin hardware gate: `AWMAN_TEST_APPLE_CONTAINER=1`,
//! a running Apple Containers service holding the promoted fixture under
//! `AWMAN_TEST_APPLE_FIXTURE_REF` (e.g. after `container image load -i
//! fixture-oci.tar`). The harness — never awman — uses the `container` CLI
//! (`AWMAN_TEST_APPLE_CLI`, default `/usr/local/bin/container`) to stop the
//! service and to start it again afterwards.
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use crate::lifecycle::{awman, gate, install_ready_ping_stub, run_bounded};

fn apple_cli() -> PathBuf {
    std::env::var_os("AWMAN_TEST_APPLE_CLI")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/usr/local/bin/container"))
}

fn service(action: &str) {
    let status = Command::new(apple_cli())
        .args(["system", action])
        .status()
        .expect("run the Apple Containers CLI (test harness only)");
    assert!(status.success(), "container system {action} failed");
}

/// Restarts the Apple service however the test ends.
struct Restart;
impl Drop for Restart {
    fn drop(&mut self) {
        let _ = Command::new(apple_cli()).args(["system", "start"]).status();
    }
}

const TEST: &str = "builtin_hw_actual_awman_apple_store_import_runs_offline_after_service_stops";

#[test]
fn builtin_hw_actual_awman_apple_store_import_runs_offline_after_service_stops() {
    let Some((binary, _archive)) = gate(TEST) else {
        return;
    };
    let required = std::env::var("AWMAN_TEST_BUILTIN_REQUIRE_HW").is_ok();
    let reference = std::env::var("AWMAN_TEST_APPLE_FIXTURE_REF").ok();
    let (true, Some(reference)) = (
        std::env::var("AWMAN_TEST_APPLE_CONTAINER").is_ok_and(|v| v == "1"),
        reference,
    ) else {
        eprintln!(
            "BLOCKED: {TEST}: set AWMAN_TEST_APPLE_CONTAINER=1 and AWMAN_TEST_APPLE_FIXTURE_REF"
        );
        assert!(!required, "{TEST}: Apple store prerequisites missing");
        return;
    };

    let temp = tempfile::Builder::new()
        .prefix("awman-apple.")
        .tempdir_in("/tmp")
        .unwrap();
    let repo = temp.path().join("repo");
    let home = temp.path().join("home");
    let private_bin = temp.path().join("bin");
    let state = tempfile::Builder::new()
        .prefix("awb.")
        .tempdir_in("/tmp")
        .unwrap();
    std::fs::create_dir_all(repo.join(".awman")).unwrap();
    std::fs::create_dir_all(home.join(".awman")).unwrap();
    std::fs::create_dir_all(&private_bin).unwrap();
    assert!(Command::new("git")
        .args(["init", "-q"])
        .arg(&repo)
        .status()
        .unwrap()
        .success());
    std::fs::write(repo.join("Dockerfile.dev"), "FROM scratch\n").unwrap();
    std::fs::write(
        repo.join(".awman/Dockerfile.claude"),
        "FROM awman-repo:latest\n",
    )
    .unwrap();
    let agent = temp.path().join("claude");
    std::fs::write(
        &agent,
        b"#!/bin/sh\nIFS= read -r prompt\nprintf 'SYNTHETIC_UID=%s\\n' \"$(id -u)\"\nprintf 'SYNTHETIC_PROMPT=%s\\n' \"$prompt\"\nexit 37\n",
    )
    .unwrap();
    std::fs::set_permissions(&agent, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(
        repo.join(".awman/config.json"),
        serde_json::to_vec(&serde_json::json!({
            "agent": "claude", "auth": "none", "autoAgentAuthAccepted": true,
            "overlays": [format!("dir({}:/usr/local/bin/claude:ro)", agent.display())]
        }))
        .unwrap(),
    )
    .unwrap();
    std::fs::write(
        home.join(".awman/config.json"),
        serde_json::to_vec(&serde_json::json!({
            "runtime": "builtin", "default_agent": "claude",
            "builtin": {
                "stateDir": state.path().join("s"),
                "imageSource": {"type": "apple-store", "reference": reference},
                "network": {"mode": "none"}
            }
        }))
        .unwrap(),
    )
    .unwrap();
    // Any host container-helper use by awman is recorded and fails the test.
    let marker = temp.path().join("helper-called");
    for helper in ["docker", "container", "sbx"] {
        let path = private_bin.join(helper);
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\nprintf '%s\\n' {helper} >> '{}'\nexit 91\n",
                marker.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let ping_log = temp.path().join("ready-pings");
    install_ready_ping_stub(&private_bin, "claude", &ping_log);

    // 1. Import through the in-process bridge while the service runs.
    let mut ready = awman(&binary, &repo, &home, &private_bin);
    ready.args(["ready", "--json"]);
    let ready = run_bounded(ready, Duration::from_secs(300));
    assert!(
        ready.status.success(),
        "ready with the Apple store failed: {}",
        String::from_utf8_lossy(&ready.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&ready.stdout).unwrap();
    assert_eq!(json["steps"]["image_source"]["status"], "ok", "{json}");

    // 2. Stop the Apple service; from here nothing may reach it.
    let _restart = Restart;
    service("stop");

    // 3. The cached import is still ready, and a real guest runs from it
    //    with egress denied and only the private PATH.
    let mut again = awman(&binary, &repo, &home, &private_bin);
    again.args(["ready", "--json"]);
    let again = run_bounded(again, Duration::from_secs(300));
    assert!(
        again.status.success(),
        "cached ready after the service stopped failed: {}",
        String::from_utf8_lossy(&again.stderr)
    );
    let mut exec = awman(&binary, &repo, &home, &private_bin);
    exec.args([
        "exec",
        "prompt",
        "--non-interactive",
        "--agent",
        "claude",
        "apple offline prompt",
    ]);
    let exec = run_bounded(exec, Duration::from_secs(180));
    let stdout = String::from_utf8_lossy(&exec.stdout);
    assert_eq!(
        exec.status.code(),
        Some(37),
        "offline exec failed: {}",
        String::from_utf8_lossy(&exec.stderr)
    );
    assert!(stdout.contains("SYNTHETIC_UID=1234"), "{stdout}");
    assert!(
        stdout.contains("SYNTHETIC_PROMPT=apple offline prompt"),
        "{stdout}"
    );
    // 4. With nothing cached, the stopped service is reported, not bypassed.
    let fresh_state = tempfile::Builder::new()
        .prefix("awb.")
        .tempdir_in("/tmp")
        .unwrap();
    let fresh_home = temp.path().join("fresh-home");
    std::fs::create_dir_all(fresh_home.join(".awman")).unwrap();
    std::fs::write(
        fresh_home.join(".awman/config.json"),
        serde_json::to_vec(&serde_json::json!({
            "runtime": "builtin", "default_agent": "claude",
            "builtin": {
                "stateDir": fresh_state.path().join("s"),
                "imageSource": {"type": "apple-store", "reference": reference},
                "network": {"mode": "none"}
            }
        }))
        .unwrap(),
    )
    .unwrap();
    let mut stopped = awman(&binary, &repo, &fresh_home, &private_bin);
    stopped.args(["ready", "--json"]);
    let stopped = run_bounded(stopped, Duration::from_secs(300));
    let json: serde_json::Value = serde_json::from_slice(&stopped.stdout).unwrap();
    assert_eq!(json["ready"], false, "{json}");
    assert_eq!(json["steps"]["image_source"]["status"], "failed", "{json}");
    let message = json["steps"]["image_source"]["message"]
        .as_str()
        .unwrap_or("");
    assert!(message.contains("service is not running"), "{message}");
    assert!(message.contains("container system start"), "{message}");
    assert!(!marker.exists(), "awman invoked a host container helper");
    eprintln!("PASS: {TEST}: {reference} imported from the Apple store ran offline");
}
