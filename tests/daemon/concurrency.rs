//! Squad and the worker feature share one daemon and one database: neither
//! excludes the other (the pre-0140 "cannot run at the same time" is gone).

use serde_json::json;

use crate::api_process::{AgentMode, ApiHome};
use crate::support::*;

fn both_features(home: &ApiHome, repo: &Repo) {
    repo.allow_in(
        home,
        json!({"daemon":{"features":{"squad":{"enabled":true,"envPersistence":"none"}}}}),
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn both_features_run_in_one_process_over_one_database() {
    if !git_available() {
        return;
    }
    let home = ApiHome::new();
    let repo = Repo::new();
    both_features(&home, &repo);
    let server = home.start().await;

    let status = server.get("/v1/status").await;
    assert_eq!(status["features"]["worker"]["state"], "on", "{status}");
    assert_eq!(status["features"]["squad"]["state"], "on", "{status}");
    assert_eq!(
        status["pid"],
        serde_json::json!(server_pid(&home)),
        "one daemon process serves both"
    );

    // Both resource trees answer from the same process.
    let (code, tasks) = http(&server, reqwest::Method::GET, "/v1/squad/tasks", None, None).await;
    assert_eq!(code, 200, "{tasks}");
    let (code, sessions) = http(
        &server,
        reqwest::Method::GET,
        "/v1/worker/sessions",
        None,
        None,
    )
    .await;
    assert_eq!(code, 200, "{sessions}");

    // A second `daemon start` is refused because a daemon is running — not
    // because the other feature "cannot run at the same time".
    let error = home.cli_err(&["daemon", "start"]);
    assert!(error.contains("already running"), "{error}");
    assert!(!error.contains("same time"), "{error}");

    // One database file holds the tables of both features.
    let session = repo.session(&server).await;
    let task = create_squad_task(&server, "shared-db", &repo.path).await;
    let db = home.home().join(".local/share/awman/data/awman.db");
    let conn = rusqlite::Connection::open(&db).unwrap();
    let count = |table: &str| -> i64 {
        conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .unwrap_or_else(|e| panic!("{table}: {e}"))
    };
    assert_eq!(count("squad_tasks"), 1);
    assert_eq!(count("sessions"), 1);
    let _ = (session, task);
}

fn server_pid(home: &ApiHome) -> u32 {
    let out = home.cli_ok(&["daemon", "status", "--json"]);
    serde_json::from_str::<serde_json::Value>(&out).unwrap()["pid"]
        .as_u64()
        .unwrap() as u32
}

/// The core guarantee: a squad trigger and a pushed worker command are both
/// in flight at once on the same database.
#[tokio::test(flavor = "multi_thread")]
async fn squad_trigger_and_pushed_command_run_concurrently_on_one_database() {
    if !git_available() {
        return;
    }
    let home = ApiHome::new();
    home.set_agent(AgentMode::Slow);
    let repo = Repo::new();
    both_features(&home, &repo);
    let server = home.start().await;

    // A pushed worker command occupies the (slow) fake agent …
    let session = repo.session(&server).await;
    let command = submit(&server, &session, &["slow worker job"]).await;
    wait_slow_agent(&home).await;
    wait_status(&server, &command, &["running"]).await;

    // … and a squad trigger starts a run while it is still going.
    let task = create_squad_task(&server, "concurrent", &repo.path).await;
    let (code, run) = http(
        &server,
        reqwest::Method::POST,
        &format!("/v1/squad/tasks/{task}/runs"),
        None,
        Some(json!({})),
    )
    .await;
    assert_eq!(code, 202, "{run}\n{}", server.log());
    let run_id = run["id"].as_str().unwrap().to_string();
    assert!(run_id.starts_with("run_"), "{run}");

    // The squad run is running (its leader is in the fake container) while the
    // worker command is still running: both features have work in flight on
    // the one database.
    let status = wait_for_status(&server, "the worker busy", |s| {
        s["features"]["worker"]["running"].as_u64().unwrap_or(0) >= 1
    })
    .await;
    let (_, runs) = http(
        &server,
        reqwest::Method::GET,
        &format!("/v1/squad/tasks/{task}/runs"),
        None,
        None,
    )
    .await;
    assert_eq!(runs["items"][0]["id"], run_id.as_str(), "{runs}");
    assert_eq!(runs["items"][0]["status"], "running", "{runs}");
    assert!(
        status["max_concurrent_jobs"].as_u64().unwrap() >= 2,
        "the shared cap leaves room for both: {status}"
    );
    let record = server.get(&format!("/v1/worker/commands/{command}")).await;
    assert_eq!(record["status"], "running", "{record}");

    // Neither is blocking the other: cancelling the squad run leaves the
    // worker command running; releasing the worker command lets it finish.
    let (code, _) = http(
        &server,
        reqwest::Method::POST,
        &format!("/v1/squad/runs/{run_id}/cancel"),
        None,
        None,
    )
    .await;
    assert!(code == 200 || code == 202, "cancel run: {code}");
    let record = server.get(&format!("/v1/worker/commands/{command}")).await;
    assert_eq!(record["status"], "running", "{record}");
    let (code, _) = http(
        &server,
        reqwest::Method::POST,
        &format!("/v1/worker/commands/{command}/cancel"),
        None,
        Some(json!(null)),
    )
    .await;
    assert!(code == 200 || code == 202, "cancel command: {code}");
    wait_status(&server, &command, &["cancelled", "done", "error"]).await;
}

/// 0140 finding F6 (see 0140-test-findings.md): `GET /v1/status` reports
/// `features.squad.running == 0` while a triggered squad run is `running`
/// (the count comes from the scheduler's tick bookkeeping, not the shared
/// executor), so `awman daemon status` under-reports squad's load.
#[tokio::test(flavor = "multi_thread")]
async fn status_counts_a_triggered_squad_run_as_running() {
    if !git_available() {
        return;
    }
    let home = ApiHome::new();
    std::fs::write(home.control().join("hold-leader"), "").unwrap();
    home.set_agent(AgentMode::Slow);
    let repo = Repo::new();
    both_features(&home, &repo);
    let server = home.start().await;
    let task = create_squad_task(&server, "counted", &repo.path).await;
    let (code, _) = http(
        &server,
        reqwest::Method::POST,
        &format!("/v1/squad/tasks/{task}/runs"),
        None,
        Some(json!({})),
    )
    .await;
    assert_eq!(code, 202);
    wait_for_status(&server, "squad running", |s| {
        s["features"]["squad"]["running"].as_u64().unwrap_or(0) >= 1
    })
    .await;
}
