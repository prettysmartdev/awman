//! `awman config set daemon.features.*` switches features on a running daemon
//! without a restart; disabling drains.

use serde_json::json;

use crate::api_process::{AgentMode, ApiHome};
use crate::support::*;

fn set(home: &ApiHome, key: &str, value: &str) -> String {
    home.cli_ok(&["config", "set", key, value, "--global"])
}

fn feature_state(status: &serde_json::Value, feature: &str) -> String {
    status["features"][feature]["state"]
        .as_str()
        .unwrap_or("?")
        .to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn disabling_the_worker_refuses_new_work_while_in_flight_work_finishes() {
    if !git_available() {
        return;
    }
    let home = ApiHome::new();
    home.set_agent(AgentMode::Slow);
    let repo = Repo::new();
    // Squad stays on so the daemon outlives the worker being switched off.
    repo.allow_in(
        &home,
        json!({"daemon":{"features":{"squad":{"enabled":true,"envPersistence":"none"}}}}),
    );
    let server = home.start().await;
    let session = repo.session(&server).await;
    let command = submit(&server, &session, &["in flight when disabled"]).await;
    wait_slow_agent(&home).await;
    wait_status(&server, &command, &["running"]).await;

    // The change is applied while the command is in flight (finding F5: the CLI
    // may report a reload timeout because the reload waits for the drain).
    let _ = home.cli(&[
        "config",
        "set",
        "daemon.features.worker.enabled",
        "false",
        "--global",
    ]);

    // New submissions are refused …
    wait_for_status(&server, "worker draining", |s| {
        matches!(
            s["features"]["worker"]["state"].as_str(),
            Some("draining" | "off")
        )
    })
    .await;
    let (code, body) = http(
        &server,
        reqwest::Method::POST,
        "/v1/worker/commands",
        Some(&session),
        Some(json!({"subcommand": "exec prompt", "args": ["refused"]})),
    )
    .await;
    assert_eq!(code, 503, "{body}");
    assert_eq!(body["code"], "feature_disabled", "{body}");
    // … the squad feature is untouched …
    let (code, _) = http(&server, reqwest::Method::GET, "/v1/squad/tasks", None, None).await;
    assert_eq!(code, 200);
    // … and the in-flight command is allowed to finish.
    let record = server.get(&format!("/v1/worker/commands/{command}")).await;
    assert_eq!(record["status"], "running", "{record}");
    home.set_agent(AgentMode::Edit);
    release_slow_agent(&home);
    wait_for_status(&server, "worker off after finishing accepted work", |s| {
        feature_state(s, "worker") == "off"
    })
    .await;
    let store = awman::data::fs::api_db::SqliteSessionStore::open_at(
        &home.home().join(".local/share/awman/data/awman.db"),
    )
    .unwrap();
    let record = store.get_command(&command).unwrap().unwrap();
    assert!(matches!(
        record.status,
        awman::data::command_status::CommandStatus::Done
            | awman::data::command_status::CommandStatus::Error
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn jobs_cancel_for_a_feature_cancels_its_running_commands() {
    if !git_available() {
        return;
    }
    let home = ApiHome::new();
    home.set_agent(AgentMode::Slow);
    let repo = Repo::new();
    repo.allow_in(&home, json!({}));
    let server = home.start().await;
    let session = repo.session(&server).await;
    let command = submit(&server, &session, &["to be cancelled"]).await;
    wait_slow_agent(&home).await;
    wait_status(&server, &command, &["running"]).await;

    let out = home.cli_ok(&["daemon", "api", "jobs.cancel", "feature=worker"]);
    assert!(!out.contains("error"), "{out}");
    let record = wait_status(&server, &command, &["cancelled", "done", "error"]).await;
    assert_eq!(record["status"], "cancelled", "{record}");
}

#[tokio::test(flavor = "multi_thread")]
async fn disabling_the_only_feature_keeps_the_daemon_up_until_stopped() {
    let home = ApiHome::new();
    let server = home.start().await;
    set(&home, "daemon.features.worker.enabled", "false");
    wait_for_status(&server, "worker off", |s| {
        feature_state(s, "worker") == "off"
    })
    .await;

    // The daemon is still up and answering; stop ends it.
    let status = server.get("/v1/status").await;
    assert_eq!(
        status["status"], "no_features",
        "status says no feature is left: {status}"
    );
    let text = home.cli_ok(&["daemon", "status"]);
    assert!(text.contains("none enabled"), "{text}");
    let out = home.cli_ok(&["daemon", "stop"]);
    assert!(out.contains("stopped"), "{out}");
    assert!(home.cli_ok(&["daemon", "status"]).contains("not running"));
}

#[tokio::test(flavor = "multi_thread")]
async fn squad_can_be_switched_on_live_and_serves_without_a_restart() {
    let home = ApiHome::new();
    let server = home.start().await;
    let (code, body) = http(&server, reqwest::Method::GET, "/v1/squad/tasks", None, None).await;
    assert_eq!(code, 503, "{body}");
    assert_eq!(body["code"], "feature_disabled");

    set(&home, "daemon.features.squad.enabled", "true");
    wait_for_status(&server, "squad on", |s| feature_state(s, "squad") == "on").await;
    let (code, body) = http(&server, reqwest::Method::GET, "/v1/squad/tasks", None, None).await;
    assert_eq!(code, 200, "{body}");
}

#[tokio::test(flavor = "multi_thread")]
async fn invalid_config_values_are_refused_and_not_persisted() {
    let home = ApiHome::new();
    let server = home.start().await;
    let error = home.cli_err(&[
        "config",
        "set",
        "daemon.features.worker.enabled",
        "maybe",
        "--global",
    ]);
    assert!(!error.is_empty());
    let error = home.cli_err(&["config", "set", "daemon.maxConcurrentJobs", "0", "--global"]);
    assert!(!error.is_empty());
    let config = server.get("/v1/config").await;
    assert_ne!(
        config["config"]["daemon"]["maxConcurrentJobs"],
        json!(0),
        "{config}"
    );
    assert_eq!(
        server.get("/v1/status").await["features"]["worker"]["state"],
        "on"
    );
}

/// 0140 finding F5 (see 0140-test-findings.md): after the in-flight command
/// finishes, a disabled worker stays `draining` forever (running 0) instead of
/// reaching `off`, and `config set` reports "did not reload: timed out"
/// because the reload request waits for that drain.
#[tokio::test(flavor = "multi_thread")]
async fn a_draining_worker_reaches_off_and_config_set_reports_applied() {
    if !git_available() {
        return;
    }
    let home = ApiHome::new();
    home.set_agent(AgentMode::Slow);
    let repo = Repo::new();
    repo.allow_in(
        &home,
        json!({"daemon":{"features":{"squad":{"enabled":true,"envPersistence":"none"}}}}),
    );
    let server = home.start().await;
    let session = repo.session(&server).await;
    let command = submit(&server, &session, &["in flight when disabled"]).await;
    wait_slow_agent(&home).await;
    wait_status(&server, &command, &["running"]).await;

    let out = set(&home, "daemon.features.worker.enabled", "false");
    assert!(out.contains("applied to the running daemon"), "{out}");
    home.set_agent(AgentMode::Edit);
    release_slow_agent(&home);
    wait_for_status(&server, "worker off", |s| {
        feature_state(s, "worker") == "off"
    })
    .await;
    // Off routes deliberately answer 503. Inspect the durable result after
    // the drain instead of racing the final GET against the Off transition.
    let store = awman::data::fs::api_db::SqliteSessionStore::open_at(
        &home.home().join(".local/share/awman/data/awman.db"),
    )
    .unwrap();
    let record = store.get_command(&command).unwrap().unwrap();
    assert!(matches!(
        record.status,
        awman::data::command_status::CommandStatus::Done
            | awman::data::command_status::CommandStatus::Error
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn an_idle_worker_switches_off_and_serves_again_when_re_enabled() {
    let home = ApiHome::new();
    // Squad keeps the daemon's feature set non-empty either way.
    home.write_config(
        json!({"daemon":{"features":{"squad":{"enabled":true,"envPersistence":"none"}}}}),
    );
    let server = home.start().await;

    let out = set(&home, "daemon.features.worker.enabled", "false");
    assert!(out.contains("applied to the running daemon"), "{out}");
    wait_for_status(&server, "worker off", |s| {
        feature_state(s, "worker") == "off"
    })
    .await;
    let (code, body) = http(
        &server,
        reqwest::Method::GET,
        "/v1/worker/workdirs",
        None,
        None,
    )
    .await;
    assert_eq!(code, 503, "{body}");
    assert_eq!(body["code"], "feature_disabled");

    let out = set(&home, "daemon.features.worker.enabled", "true");
    assert!(out.contains("applied to the running daemon"), "{out}");
    wait_for_status(&server, "worker on", |s| feature_state(s, "worker") == "on").await;
    let (code, body) = http(
        &server,
        reqwest::Method::GET,
        "/v1/worker/workdirs",
        None,
        None,
    )
    .await;
    assert_eq!(code, 200, "{body}");
}

#[tokio::test(flavor = "multi_thread")]
async fn push_enabled_switches_admission_alone_and_preserves_pull_status() {
    let home = ApiHome::new();
    let repo = Repo::new();
    repo.allow_in(&home, json!({}));
    let server = home.start().await;
    let session = repo.session(&server).await;
    let _ = set(&home, "daemon.features.worker.push.enabled", "false");
    let (code, body) = http(
        &server,
        reqwest::Method::POST,
        "/v1/worker/commands",
        None,
        Some(json!({"session_id":session,"subcommand":"exec prompt","args":["hello"]})),
    )
    .await;
    assert_eq!(code, 503, "{body}");
    assert_eq!(body["code"], "feature_disabled");
    let status = server.get("/v1/status").await;
    assert_eq!(status["features"]["worker"]["state"], "on");
    assert!(
        status["capabilities"]
            .as_array()
            .unwrap()
            .contains(&json!("worker_pull")),
        "{status}"
    );
    let _ = set(&home, "daemon.features.worker.push.enabled", "true");
    let id = submit(&server, &session, &["hello again"]).await;
    assert_eq!(
        wait_status(&server, &id, &["done", "error"]).await["status"],
        "done"
    );
}
