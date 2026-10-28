//! WI 0135 command lifecycle over the API: cancel, labels, `Idempotency-Key`
//! replay and `POST /v1/runs`.
//!
//! Runs the real `awman api start` binary with the fake `docker` of
//! `helpers/api_process.rs`. Tests that need a `git daemon` origin carry
//! `real_git`; the ones that only need loopback HTTP carry `real_network`.

#![cfg(unix)]

use crate::{api_process, git_daemon};

use std::time::{Duration, Instant};

use reqwest::Response;
use serde_json::{json, Value};

use api_process::{AgentMode, ApiHome, ApiServer};
use git_daemon::Origin;

async fn send(
    server: &ApiServer,
    method: reqwest::Method,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<&Value>,
) -> Response {
    let mut request = reqwest::Client::new().request(method, format!("{}{path}", server.url));
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    if let Some(body) = body {
        request = request.json(body);
    }
    request.send().await.expect("request")
}

async fn json_of(response: Response) -> (u16, Value) {
    let status = response.status().as_u16();
    (status, response.json().await.unwrap_or(Value::Null))
}

async fn post(
    server: &ApiServer,
    path: &str,
    headers: &[(&str, &str)],
    body: Value,
) -> (u16, Value) {
    json_of(send(server, reqwest::Method::POST, path, headers, Some(&body)).await).await
}

async fn get(server: &ApiServer, path: &str) -> (u16, Value) {
    json_of(send(server, reqwest::Method::GET, path, &[], None).await).await
}

async fn cancel(server: &ApiServer, id: &str) -> (u16, Value) {
    post(
        server,
        &format!("/v1/commands/{id}/cancel"),
        &[],
        Value::Null,
    )
    .await
}

async fn submit(server: &ApiServer, session: &str, subcommand: &str, args: &[&str]) -> String {
    let (status, body) = post(
        server,
        "/v1/commands",
        &[("x-awman-session", session)],
        json!({"subcommand": subcommand, "args": args}),
    )
    .await;
    assert_eq!(status, 202, "{body}");
    body["command_id"].as_str().unwrap().to_string()
}

async fn wait_command(server: &ApiServer, id: &str, wanted: &[&str]) -> Value {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let (_, record) = get(server, &format!("/v1/commands/{id}/status")).await;
        if wanted.contains(&record["status"].as_str().unwrap_or("")) {
            return record;
        }
        assert!(
            Instant::now() < deadline,
            "command {id} never reached {wanted:?}: {record}\n{}",
            server.log()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn wait_session_closed(server: &ApiServer, id: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let (_, session) = get(server, &format!("/v1/sessions/{id}")).await;
        if session["status"] == "closed" {
            return session;
        }
        assert!(
            Instant::now() < deadline,
            "session {id} never closed: {session}\n{}",
            server.log()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn remote(url: &str) -> Value {
    json!({"session_type": "remote", "repo_url": url})
}

/// A repo URL whose clone fails fast.
const BAD_REPO: &str = "git://127.0.0.1:1/missing.git";

/// A loopback listener that accepts git connections and never answers, so a
/// clone hangs in setup until it is cancelled.
fn hanging_origin() -> (std::net::TcpListener, String) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("git://{}/hang.git", listener.local_addr().unwrap());
    let accept = listener.try_clone().unwrap();
    std::thread::spawn(move || {
        let mut held = Vec::new();
        while let Ok((stream, _)) = accept.accept() {
            held.push(stream);
        }
    });
    (listener, url)
}

async fn wait_slow_agent(home: &ApiHome) {
    let pid = home.control().join("slow.pid");
    let deadline = Instant::now() + Duration::from_secs(60);
    while !pid.exists() {
        assert!(Instant::now() < deadline, "slow agent never started");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

// ─── cancel ──────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn real_network_cancel_unknown_command_is_404() {
    let home = ApiHome::new();
    let server = home.start().await;
    let (status, body) = cancel(&server, "no-such-command").await;
    assert_eq!(status, 404, "{body}");
    assert!(body["error"].as_str().unwrap().contains("no-such-command"));
}

#[tokio::test(flavor = "multi_thread")]
async fn real_git_cancel_queued_command_marks_it_cancelled_without_running() {
    crate::real_git_skip!();
    let Some(origin) = Origin::start() else {
        return;
    };
    let home = ApiHome::new();
    home.set_agent(AgentMode::Slow);
    let server = home.start().await;
    let id = server.ready_session(remote(&origin.url)).await;

    // The first command occupies the session's worker; the second stays queued.
    let first = submit(&server, &id, "exec prompt", &["slow"]).await;
    wait_slow_agent(&home).await;
    let (status, created) = post(
        &server,
        "/v1/commands",
        &[("x-awman-session", &id)],
        json!({
            "subcommand": "exec prompt", "args": ["never runs"], "labels": {"node": "queued"}
        }),
    )
    .await;
    assert_eq!(status, 202, "{created}");
    let queued = created["command_id"].as_str().unwrap().to_owned();
    let (_, before) = get(&server, &format!("/v1/commands/{queued}/status")).await;
    assert_eq!(before["status"], "queued", "{before}");
    let (_, queue) = get(&server, &format!("/v1/sessions/{id}/queue")).await;
    assert_eq!(queue["running"]["labels"], json!({}), "{queue}");
    assert_eq!(
        queue["queued"][0]["labels"],
        json!({"node": "queued"}),
        "{queue}"
    );

    let (status, body) = cancel(&server, &queued).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["status"], "cancelled", "{body}");
    assert_eq!(body["exit_code"], 130, "{body}");
    assert_eq!(body["result"]["error"], "cancelled", "{body}");
    let (_, queue) = get(&server, &format!("/v1/sessions/{id}/queue")).await;
    assert_eq!(
        queue["recent_completed"][0]["labels"],
        json!({"node": "queued"}),
        "{queue}"
    );

    // The other command is untouched.
    let (_, other) = get(&server, &format!("/v1/commands/{first}/status")).await;
    assert_eq!(other["status"], "running", "{other}");
    let (_, listed) = get(&server, "/v1/commands?status=cancelled").await;
    let ids: Vec<_> = listed["commands"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].clone())
        .collect();
    assert_eq!(ids, vec![json!(queued)]);
    let _ = cancel(&server, &first).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn real_git_cancel_running_command_returns_202_then_cancelled_without_push_or_pr() {
    crate::real_git_skip!();
    let Some(origin) = Origin::start() else {
        return;
    };
    let home = ApiHome::new();
    home.set_agent(AgentMode::Slow);
    home.set_teardown(
        r#"
[[teardown]]
type = "run_shell"
command = "git config user.name Test && git config user.email test@localhost"
[[teardown]]
type = "commit_changes"
message = "workflow commit"
add_all = true
[[teardown]]
type = "push_branch"
[[teardown]]
type = "create_pull_request"
"#,
    );
    let server = home.start().await;
    let mut spec = remote(&origin.url);
    spec["branch"] = json!("cancelled-work");
    spec["push"] = json!(true);
    let id = server.ready_session(spec).await;

    let command = submit(
        &server,
        &id,
        "exec workflow",
        &["--dynamic", "--work-item", "0042"],
    )
    .await;
    wait_slow_agent(&home).await;
    let (_, running) = get(&server, &format!("/v1/commands/{command}/status")).await;
    assert_eq!(running["status"], "running", "{running}");

    let (status, body) = cancel(&server, &command).await;
    assert_eq!(status, 202, "{body}");
    assert_eq!(body["command_id"], json!(command));
    assert_eq!(body["session_id"], json!(id));
    assert!(body["cancel_requested_at"].is_string(), "{body}");
    assert!(
        body["message"].as_str().unwrap().contains("cancelled"),
        "{body}"
    );

    let record = wait_command(&server, &command, &["cancelled", "done", "error"]).await;
    assert_eq!(record["status"], "cancelled", "{record}\n{}", server.log());
    assert_eq!(record["exit_code"], 130, "{record}");
    assert_eq!(record["result"]["error"], "cancelled", "{record}");

    // A second cancel on the terminal command is an idempotent 200.
    let (status, again) = cancel(&server, &command).await;
    assert_eq!(status, 200, "{again}");
    assert_eq!(again["status"], "cancelled");

    // No push, no PR, and no on_failure retry of the agent.
    assert_eq!(origin.branch_sha("cancelled-work"), None);
    assert!(!home.control().join("pr-head").exists(), "PR step ran");
    let runs = std::fs::read_to_string(home.control().join("docker.log"))
        .unwrap()
        .lines()
        .filter(|l| l.starts_with("docker run") && !l.contains("-d "))
        .count();
    assert!(
        runs <= 2,
        "cancelled workflow relaunched agents ({runs} docker runs)"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn real_git_cancel_after_done_returns_200_and_leaves_status_unchanged() {
    crate::real_git_skip!();
    let Some(origin) = Origin::start() else {
        return;
    };
    let home = ApiHome::new();
    let server = home.start().await;
    let id = server.ready_session(remote(&origin.url)).await;
    let record = server.run_command(&id, "exec prompt", &["quick"]).await;
    assert_eq!(record["status"], "done", "{record}");
    let command = record["id"].as_str().unwrap();

    let (status, body) = cancel(&server, command).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["status"], "done", "{body}");
    let (_, after) = get(&server, &format!("/v1/commands/{command}/status")).await;
    assert_eq!(after["status"], "done");
    assert_eq!(after["exit_code"], 0);
}

// ─── labels ──────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn real_network_invalid_labels_are_rejected_with_400() {
    let home = ApiHome::new();
    let server = home.start().await;
    let too_many: serde_json::Map<String, Value> =
        (0..17).map(|i| (format!("k{i}"), json!("v"))).collect();
    let bad = [
        json!({"": "v"}),
        json!({"a=b": "v"}),
        json!({"k": "line\nbreak"}),
        json!({"k": "x".repeat(257)}),
        json!({"x".repeat(257): "v"}),
        Value::Object(too_many),
        json!({"k": 1}),
    ];
    for labels in bad {
        let mut spec = remote(BAD_REPO);
        spec["labels"] = labels.clone();
        let (status, body) = post(&server, "/v1/sessions", &[], spec).await;
        assert_eq!(status, 400, "labels {labels} accepted: {body}");
    }
    // Empty values are valid.
    let mut spec = remote(BAD_REPO);
    spec["labels"] = json!({"k": ""});
    let (status, body) = post(&server, "/v1/sessions", &[], spec).await;
    assert_eq!(status, 202, "{body}");
    let (status, body) = get(&server, "/v1/sessions?label==bad").await;
    assert_eq!(status, 400, "{body}");
    let (status, body) = get(&server, "/v1/sessions?status=bogus").await;
    assert_eq!(status, 400, "{body}");
    let (status, body) = get(&server, "/v1/commands?status=bogus").await;
    assert_eq!(status, 400, "{body}");
    let (status, body) = get(&server, "/v1/commands?limit=abc").await;
    assert_eq!(status, 400, "{body}");
}

#[tokio::test(flavor = "multi_thread")]
async fn real_git_labels_are_stored_returned_on_every_read_and_filterable() {
    crate::real_git_skip!();
    let Some(origin) = Origin::start() else {
        return;
    };
    let home = ApiHome::new();
    let server = home.start().await;

    let mut labelled = remote(&origin.url);
    labelled["labels"] = json!({"team": "infra", "run": "7"});
    let a = server.ready_session(labelled).await;
    let mut other = remote(&origin.url);
    other["labels"] = json!({"team": "web"});
    let b = server.ready_session(other).await;
    let plain = server.ready_session(remote(&origin.url)).await;

    // Session reads all carry labels; unlabelled sessions get `{}`.
    let session = server.session(&a).await;
    assert_eq!(session["labels"], json!({"team": "infra", "run": "7"}));
    let (_, status) = get(&server, &format!("/v1/sessions/{a}/status")).await;
    assert_eq!(status["labels"], json!({"team": "infra", "run": "7"}));
    assert_eq!(server.session(&plain).await["labels"], json!({}));

    let ids = |body: &Value, key: &str| -> Vec<String> {
        let mut ids: Vec<String> = body[key]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["id"].as_str().unwrap().to_string())
            .collect();
        ids.sort();
        ids
    };
    let sorted = |mut v: Vec<String>| {
        v.sort();
        v
    };
    let (_, exact) = get(&server, "/v1/sessions?label=team=infra").await;
    assert_eq!(ids(&exact, "sessions"), vec![a.clone()]);
    let (_, presence) = get(&server, "/v1/sessions?label=team").await;
    assert_eq!(
        ids(&presence, "sessions"),
        sorted(vec![a.clone(), b.clone()])
    );
    let (_, anded) = get(&server, "/v1/sessions?label=team=infra&label=run=7").await;
    assert_eq!(ids(&anded, "sessions"), vec![a.clone()]);
    let (_, none) = get(&server, "/v1/sessions?label=team=infra&label=run=8").await;
    assert!(none["sessions"].as_array().unwrap().is_empty());
    let (_, active) = get(&server, "/v1/sessions?status=active&label=team=web").await;
    assert_eq!(ids(&active, "sessions"), vec![b.clone()]);

    // Command labels.
    let (status, body) = post(
        &server,
        "/v1/commands",
        &[("x-awman-session", &a)],
        json!({"subcommand": "exec prompt", "args": ["hi"], "labels": {"job": "one"}}),
    )
    .await;
    assert_eq!(status, 202, "{body}");
    let first = body["command_id"].as_str().unwrap().to_string();
    let done = wait_command(&server, &first, &["done", "error"]).await;
    assert_eq!(
        done["labels"],
        json!({"job": "one"}),
        "terminal read keeps labels"
    );
    let second = submit(&server, &a, "exec prompt", &["unlabelled"]).await;
    let second_rec = wait_command(&server, &second, &["done", "error"]).await;
    assert_eq!(second_rec["labels"], json!({}));
    let (_, queue) = get(&server, &format!("/v1/sessions/{a}/queue")).await;
    let completed = queue["recent_completed"].as_array().unwrap();
    assert!(
        completed
            .iter()
            .any(|c| c["command_id"] == first && c["labels"] == json!({"job": "one"})),
        "{queue}"
    );

    let (_, listed) = get(&server, "/v1/commands?label=job=one").await;
    assert_eq!(ids(&listed, "commands"), vec![first.clone()]);
    let (_, listed) = get(
        &server,
        &format!("/v1/commands?label=job&status=done&session_id={a}"),
    )
    .await;
    assert_eq!(ids(&listed, "commands"), vec![first.clone()]);
    let (_, listed) = get(&server, "/v1/commands?label=job=one&status=error").await;
    assert!(listed["commands"].as_array().unwrap().is_empty());
    let (_, listed) = get(&server, &format!("/v1/commands?session_id={a}&limit=1")).await;
    assert_eq!(listed["commands"].as_array().unwrap().len(), 1);
    assert_eq!(listed["commands"][0]["id"], json!(second), "newest first");

    // Invalid command labels are rejected and nothing is queued.
    let (status, body) = post(
        &server,
        "/v1/commands",
        &[("x-awman-session", &a)],
        json!({"subcommand": "exec prompt", "args": ["hi"], "labels": {"a=b": "v"}}),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    let (_, all) = get(&server, &format!("/v1/commands?session_id={a}")).await;
    assert_eq!(all["commands"].as_array().unwrap().len(), 2);
}

// ─── idempotency ─────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn real_network_session_idempotency_key_replays_and_creates_no_duplicate() {
    let home = ApiHome::new();
    let server = home.start().await;
    let key = [("Idempotency-Key", "sess-key-1")];
    let spec = remote(BAD_REPO);

    let first = send(
        &server,
        reqwest::Method::POST,
        "/v1/sessions",
        &key,
        Some(&spec),
    )
    .await;
    assert!(first.headers().get("idempotent-replayed").is_none());
    let (status, original) = json_of(first).await;
    assert_eq!(status, 202, "{original}");

    let replay = send(
        &server,
        reqwest::Method::POST,
        "/v1/sessions",
        &key,
        Some(&spec),
    )
    .await;
    assert_eq!(replay.headers()["idempotent-replayed"], "true");
    let (status, again) = json_of(replay).await;
    assert_eq!(status, 202);
    assert_eq!(again, original);

    let mut changed = spec.clone();
    changed["labels"] = json!({"x": "y"});
    let (status, body) = post(&server, "/v1/sessions", &key, changed).await;
    assert_eq!(status, 422, "{body}");

    let (_, sessions) = get(&server, "/v1/sessions").await;
    assert_eq!(sessions["sessions"].as_array().unwrap().len(), 1);

    // Invalid keys are rejected; a different key creates a new session.
    let (status, _) = post(
        &server,
        "/v1/sessions",
        &[("Idempotency-Key", "")],
        spec.clone(),
    )
    .await;
    assert_eq!(status, 400);
    let (status, fresh) = post(
        &server,
        "/v1/sessions",
        &[("Idempotency-Key", "sess-key-2")],
        spec,
    )
    .await;
    assert_eq!(status, 202);
    assert_ne!(fresh["session_id"], original["session_id"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn real_git_command_and_run_idempotency_keys_replay_and_create_no_duplicate() {
    crate::real_git_skip!();
    let Some(origin) = Origin::start() else {
        return;
    };
    let home = ApiHome::new();
    let server = home.start().await;
    let id = server.ready_session(remote(&origin.url)).await;

    let headers = [
        ("x-awman-session", id.as_str()),
        ("Idempotency-Key", "cmd-key"),
    ];
    let body = json!({"subcommand": "exec prompt", "args": ["once"]});
    let (status, original) = post(&server, "/v1/commands", &headers, body.clone()).await;
    assert_eq!(status, 202, "{original}");
    let replay = send(
        &server,
        reqwest::Method::POST,
        "/v1/commands",
        &headers,
        Some(&body),
    )
    .await;
    assert_eq!(replay.headers()["idempotent-replayed"], "true");
    let (status, again) = json_of(replay).await;
    assert_eq!(status, 202);
    assert_eq!(again, original);
    let (status, _) = post(
        &server,
        "/v1/commands",
        &headers,
        json!({"subcommand": "exec prompt", "args": ["different"]}),
    )
    .await;
    assert_eq!(status, 422);
    wait_command(
        &server,
        original["command_id"].as_str().unwrap(),
        &["done", "error"],
    )
    .await;
    let (_, listed) = get(&server, &format!("/v1/commands?session_id={id}")).await;
    assert_eq!(listed["commands"].as_array().unwrap().len(), 1);

    // /v1/runs
    let run = json!({
        "session": remote(&origin.url),
        "command": {"subcommand": "exec prompt", "args": ["run once"]},
    });
    let run_headers = [("Idempotency-Key", "run-key")];
    let (status, created) = post(&server, "/v1/runs", &run_headers, run.clone()).await;
    assert_eq!(status, 202, "{created}");
    let replay = send(
        &server,
        reqwest::Method::POST,
        "/v1/runs",
        &run_headers,
        Some(&run),
    )
    .await;
    assert_eq!(replay.headers()["idempotent-replayed"], "true");
    let (_, again) = json_of(replay).await;
    assert_eq!(again, created);
    wait_command(
        &server,
        created["command_id"].as_str().unwrap(),
        &["done", "error"],
    )
    .await;
    let (_, sessions) = get(&server, "/v1/sessions").await;
    assert_eq!(
        sessions["sessions"].as_array().unwrap().len(),
        2,
        "one session + one run session"
    );
    let (_, commands) = get(&server, "/v1/commands").await;
    assert_eq!(commands["commands"].as_array().unwrap().len(), 2);
}

// ─── POST /v1/runs ───────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn real_git_run_starts_command_after_setup_and_close_on_finish_closes_session() {
    crate::real_git_skip!();
    let Some(origin) = Origin::start() else {
        return;
    };
    let home = ApiHome::new();
    let server = home.start().await;

    let (status, created) = post(
        &server,
        "/v1/runs",
        &[],
        json!({
            "session": {"session_type": "remote", "repo_url": origin.url, "labels": {"who": "run"}},
            "command": {"subcommand": "exec prompt", "args": ["hello"], "labels": {"c": "1"}},
            "close_on_finish": true,
        }),
    )
    .await;
    assert_eq!(status, 202, "{created}");
    let session_id = created["session_id"].as_str().unwrap().to_string();
    let command_id = created["command_id"].as_str().unwrap().to_string();

    let record = wait_command(&server, &command_id, &["done", "error", "cancelled"]).await;
    assert_eq!(record["status"], "done", "{record}\n{}", server.log());
    assert_eq!(record["session_id"], json!(session_id));
    assert_eq!(record["labels"], json!({"c": "1"}));
    let closed = wait_session_closed(&server, &session_id).await;
    assert_eq!(closed["labels"], json!({"who": "run"}));

    // Without close_on_finish the session stays active.
    let (status, kept) = post(
        &server,
        "/v1/runs",
        &[],
        json!({
            "session": remote(&origin.url),
            "command": {"subcommand": "exec prompt", "args": ["hello"]},
        }),
    )
    .await;
    assert_eq!(status, 202, "{kept}");
    let record = wait_command(
        &server,
        kept["command_id"].as_str().unwrap(),
        &["done", "error"],
    )
    .await;
    assert_eq!(record["status"], "done", "{record}");
    tokio::time::sleep(Duration::from_millis(500)).await;
    let session = server.session(kept["session_id"].as_str().unwrap()).await;
    assert_ne!(session["status"], "closed", "{session}");
}

#[tokio::test(flavor = "multi_thread")]
async fn real_network_run_validation_errors_create_nothing() {
    let home = ApiHome::new();
    let server = home.start().await;
    let (status, body) = post(
        &server,
        "/v1/runs",
        &[],
        json!({"session": remote(BAD_REPO), "command": {"subcommand": "bogus nonsense", "args": []}}),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    let (status, body) = post(
        &server,
        "/v1/runs",
        &[],
        json!({
            "session": {"session_type": "remote", "repo_url": BAD_REPO, "labels": {"": "v"}},
            "command": {"subcommand": "exec prompt", "args": ["x"]},
        }),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    let (_, sessions) = get(&server, "/v1/sessions").await;
    assert!(sessions["sessions"].as_array().unwrap().is_empty());
    let (_, commands) = get(&server, "/v1/commands").await;
    assert!(commands["commands"].as_array().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn real_network_run_setup_failure_errors_the_command_and_close_on_finish_closes() {
    let home = ApiHome::new();
    let server = home.start().await;
    let (status, created) = post(
        &server,
        "/v1/runs",
        &[],
        json!({
            "session": remote(BAD_REPO),
            "command": {"subcommand": "exec prompt", "args": ["x"]},
            "close_on_finish": true,
        }),
    )
    .await;
    assert_eq!(status, 202, "{created}");
    let record = wait_command(
        &server,
        created["command_id"].as_str().unwrap(),
        &["error", "done"],
    )
    .await;
    assert_eq!(record["status"], "error", "{record}");
    assert_eq!(record["exit_code"], 1, "{record}");
    assert_eq!(record["result"]["error"], "setup_error", "{record}");
    assert_eq!(record["result"]["stage"], "setup", "{record}");
    wait_session_closed(&server, created["session_id"].as_str().unwrap()).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn real_network_cancel_during_run_setup_cancels_command_and_closes_session() {
    let home = ApiHome::new();
    let server = home.start().await;
    let (_listener, url) = hanging_origin();
    let (status, created) = post(
        &server,
        "/v1/runs",
        &[],
        json!({
            "session": remote(&url),
            "command": {"subcommand": "exec prompt", "args": ["x"]},
            "close_on_finish": false,
        }),
    )
    .await;
    assert_eq!(status, 202, "{created}");
    let command = created["command_id"].as_str().unwrap();
    let session = created["session_id"].as_str().unwrap();
    let (_, before) = get(&server, &format!("/v1/commands/{command}/status")).await;
    assert_eq!(before["status"], "queued", "{before}");

    let (status, body) = cancel(&server, command).await;
    assert!(status == 200 || status == 202, "{status} {body}");
    let record = wait_command(&server, command, &["cancelled", "error", "done"]).await;
    assert_eq!(record["status"], "cancelled", "{record}\n{}", server.log());
    assert_eq!(record["exit_code"], 130, "{record}");
    let closed = wait_session_closed(&server, session).await;
    assert!(
        !api_process::clone_path(&closed).exists(),
        "cancelled setup retained its clone"
    );
    let (_, setup) = get(&server, &format!("/v1/sessions/{session}/status")).await;
    assert_eq!(setup["status"], "failed", "{setup}");
    assert_eq!(setup["error"]["stage"], "cancelled", "{setup}");
}

#[tokio::test(flavor = "multi_thread")]
async fn real_git_restart_finishes_cancelled_cleanup_without_requeue_or_publication() {
    crate::real_git_skip!();
    let Some(origin) = Origin::start() else {
        return;
    };
    let home = ApiHome::new();
    home.set_agent(AgentMode::Slow);
    let sentinel = home.control().join("recovered-cleanup");
    let workflow = format!(
        r#"
title = "restart-cancel"
agent = "claude"
[[steps]]
name = "blocked"
prompt = "wait"
[[teardown]]
type = "run_shell"
command = "echo cleanup >> '{}'"
[[teardown]]
type = "push_branch"
[[teardown]]
type = "create_pull_request"
"#,
        sentinel.display()
    );
    origin.commit_on(
        "main",
        None,
        &[("workflow.toml", &workflow)],
        "cleanup fixture",
    );
    let mut server = home.start().await;
    let (status, created) = post(
        &server,
        "/v1/runs",
        &[],
        json!({
            "session": remote(&origin.url),
            "command": {"subcommand": "exec workflow", "args": ["workflow.toml"]},
            "close_on_finish": true,
        }),
    )
    .await;
    assert_eq!(status, 202, "{created}");
    let id = created["command_id"].as_str().unwrap();
    let session = created["session_id"].as_str().unwrap();
    wait_slow_agent(&home).await;
    let (_, details) = get(&server, &format!("/v1/sessions/{session}")).await;
    let clone = api_process::clone_path(&details);
    assert!(clone.exists());
    // Model the crash immediately after the cancel marker was committed,
    // before the owning worker received its in-memory cancellation token.
    let db = home.home().join(".local/share/awman/data/awman.db");
    let store =
        std::sync::Arc::new(awman::data::fs::api_db::SqliteSessionStore::open_at(&db).unwrap());
    let lifecycle = awman::data::api_lifecycle_store::CommandLifecycleStore::open(store).unwrap();
    lifecycle
        .request_cancel(id, &chrono::Utc::now().to_rfc3339())
        .unwrap();
    server.kill();
    drop(lifecycle);
    let server = home.start().await;
    wait_session_closed(&server, session).await;
    let (_, record) = get(&server, &format!("/v1/commands/{id}/status")).await;
    assert_eq!(record["status"], "cancelled", "{record}\n{}", server.log());
    assert_eq!(record["exit_code"], 130);
    assert_eq!(std::fs::read_to_string(&sentinel).unwrap(), "cleanup\n");
    assert!(
        !clone.exists(),
        "close_on_finish retained clone after cleanup"
    );
    let (_, state) = get(&server, &format!("/v1/workflows/{id}")).await;
    assert!(state.to_string().contains("Cancelled"), "{state}");
    assert!(!home.control().join("pr-head").exists());
    let launches = std::fs::read_to_string(home.control().join("docker.log")).unwrap();
    assert_eq!(
        launches
            .lines()
            .filter(|l| l.starts_with("docker run") && !l.contains("-d "))
            .count(),
        1,
        "agent was requeued: {launches}"
    );
    drop(server);
    let server = home.start().await;
    let (_, record) = get(&server, &format!("/v1/commands/{id}/status")).await;
    assert_eq!(record["status"], "cancelled");
    assert_eq!(
        std::fs::read_to_string(&sentinel).unwrap(),
        "cleanup\n",
        "recovery repeated completed cleanup"
    );
}
