use crate::support::{
    self,
    backend::{Backend, BackendKind},
};
use awman::command::commands::daemon::actions::Request;
use serde_json::json;
use std::time::Duration;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reload_switches_admission_without_restarting_and_invalid_changes_are_atomic() {
    let sandbox = support::sandbox("daemon::reload::reload_switches_admission_without_restarting_and_invalid_changes_are_atomic");
    let backend = Backend::start(BackendKind::Daemon, &sandbox).await;
    let original = backend
        .client()
        .call(&Request::get("/v1/status"))
        .await
        .unwrap();
    assert_eq!(
        backend
            .client()
            .send(&Request::get("/v1/squad/tasks"))
            .await
            .unwrap()
            .status,
        503
    );
    for (key, value) in [
        ("daemon.features.squad.enabled", "true"),
        ("daemon.maxConcurrentJobs", "3"),
    ] {
        support::awman(&sandbox)
            .args(["config", "set", key, value, "--global"])
            .deadline(Duration::from_secs(5))
            .run()
            .assert_success();
    }
    let status = backend
        .client()
        .call(&Request::get("/v1/status"))
        .await
        .unwrap();
    assert_eq!(status["pid"], original["pid"]);
    assert_eq!(status["features"]["squad"]["state"], "on");
    assert_eq!(status["max_concurrent_jobs"], 3);
    let tasks = backend
        .client()
        .call(&Request::get("/v1/squad/tasks"))
        .await
        .unwrap();
    assert_eq!(tasks["items"], json!([]));
    let env = support::env_snapshot(&sandbox);
    let path = awman::data::config::file::global_config_path(&env).unwrap();
    let before = std::fs::read(&path).unwrap();
    for (key, value) in [
        ("daemon.maxConcurrentJobs", "0"),
        ("daemon.features.squad.enabled", "maybe"),
    ] {
        let output = support::awman(&sandbox)
            .args(["config", "set", key, value, "--global"])
            .run();
        assert!(!output.exit.success());
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }
    std::fs::write(&path, b"{broken").unwrap();
    assert!(backend
        .client()
        .call(&Request::new("POST", "/v1/config/reload", Some(json!({}))))
        .await
        .is_err());
    assert_eq!(
        backend
            .client()
            .call(&Request::get("/v1/status"))
            .await
            .unwrap()["max_concurrent_jobs"],
        3
    );
    std::fs::write(&path, before).unwrap();
    for feature in ["squad", "engine"] {
        support::awman(&sandbox)
            .args([
                "config",
                "set",
                &format!("daemon.features.{feature}.enabled"),
                "false",
                "--global",
            ])
            .run()
            .assert_success();
    }
    let status = backend
        .client()
        .call(&Request::get("/v1/status"))
        .await
        .unwrap();
    assert_eq!(status["pid"], original["pid"]);
    assert_eq!(status["status"], "no_features");
    backend.shutdown().await;
    sandbox.finish();
}

#[tokio::test]
async fn daemon_status_and_stop_keep_their_cli_and_json_contracts() {
    let s = support::sandbox(
        "daemon::reload::daemon_status_and_stop_keep_their_cli_and_json_contracts",
    );
    let backend = Backend::start(BackendKind::Daemon, &s).await;
    let status = support::awman(&s)
        .args(["daemon", "status", "--json"])
        .run();
    status.assert_success();
    let row: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(row["pid"].as_u64(), backend.pid().map(u64::from));
    let human = support::awman(&s).args(["daemon", "status"]).run();
    human.assert_success();
    assert!(human
        .stdout_text()
        .contains(&backend.pid().unwrap().to_string()));
    let stopped = support::awman(&s).args(["daemon", "stop"]).run();
    stopped.assert_success();
    assert!(
        stopped.stdout_text().contains("stopped"),
        "{}",
        stopped.stdout_text()
    );
    backend.wait_stopped();
    let absent = support::awman(&s)
        .args(["daemon", "status", "--json"])
        .run();
    absent.assert_success();
    let row: serde_json::Value = serde_json::from_slice(&absent.stdout).unwrap();
    assert_eq!(row["running"], false);
    let repeated = support::awman(&s).args(["daemon", "stop"]).run();
    assert!(!repeated.exit.success());
    assert!(repeated.stderr_text().contains("not running"));
    s.finish();
}
