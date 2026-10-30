//! WI 0138 HTTP API consistency regressions.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Instant;

use awman::command::commands::api_server::{CommandCanceller, TokenRegistry};
use awman::command::dispatch::Engines;
use awman::data::fs::api_db::{NewSessionRow, SqliteSessionStore};
use awman::data::fs::api_paths::ApiPaths;
use awman::data::fs::auth_paths::AuthPathResolver;
use awman::data::session::SessionKind;
use awman::data::session_setup_event::SessionSetupStatus;
use awman::data::WorkflowStateStore;
use awman::engine::agent::AgentEngine;
use awman::engine::auth::AuthEngine;
use awman::engine::container::ContainerRuntime;
use awman::engine::git::GitEngine;
use awman::engine::overlay::OverlayEngine;
use awman::frontend::api::error::ApiError;
use awman::frontend::api::routes::{build_router, AppState, AuthMode, API_FEATURES};
use axum::http::StatusCode;
use axum::response::IntoResponse;

const SESSION: &str = "wi-0138-session";

fn state(root: &std::path::Path) -> Arc<AppState> {
    let paths = ApiPaths::from_root(root);
    paths.ensure_root().unwrap();
    let store = Arc::new(SqliteSessionStore::open(paths.root()).unwrap());
    let auth_paths = AuthPathResolver::at_home(root);
    let runtime = Arc::new(ContainerRuntime::docker());
    let overlay = Arc::new(OverlayEngine::with_auth_resolver(auth_paths.clone()));
    let engines = Engines {
        runtime: runtime.clone(),
        container_runtime: Some(runtime.clone()),
        sandbox_runtime: None,
        git_engine: Arc::new(GitEngine::new()),
        overlay_engine: overlay.clone(),
        auth_engine: Arc::new(AuthEngine::with_paths(auth_paths, paths.clone())),
        agent_engine: Arc::new(AgentEngine::new(overlay, runtime)),
        config_notices: Default::default(),
        workflow_state_store: Arc::new(WorkflowStateStore::at_git_root(paths.root())),
        credential_monitor: None,
        global_config: Arc::new(Default::default()),
    };
    let lifecycle_store = awman::data::CommandLifecycleStore::open(Arc::clone(&store)).unwrap();
    let setup_cancels = Arc::new(TokenRegistry::new());
    let canceller = CommandCanceller::new(
        Arc::clone(&store),
        lifecycle_store.clone(),
        Arc::new(TokenRegistry::new()),
        Arc::clone(&setup_cancels),
    );
    store
        .insert_session_full(NewSessionRow {
            id: SESSION,
            workdir: "/work",
            created_at: &chrono::Utc::now().to_rfc3339(),
            setup_status: SessionSetupStatus::Ready,
            kind: SessionKind::Local,
            cloned_path: None,
        })
        .unwrap();
    Arc::new(AppState {
        lifecycle_store,
        setup_cancels,
        canceller,
        store,
        paths,
        workdirs: vec![],
        workdir_roots: vec![],
        started_at: Instant::now(),
        task_handles: tokio::sync::Mutex::new(Vec::new()),
        auth_mode: AuthMode::Disabled,
        engines,
        sessions: Arc::new(awman::data::session_manager::SessionManager::in_memory(
            awman::data::config::env::EnvSnapshot::empty(),
        )),
        event_buses: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        setup_buses: tokio::sync::Mutex::new(HashMap::new()),
    })
}

fn enqueue(app: &AppState, id: &str) {
    app.paths.prepare_command_dir(SESSION, id).unwrap();
    app.store
        .enqueue_command(
            id,
            SESSION,
            "exec prompt",
            "[]",
            &app.paths.command_log_path(SESSION, id).to_string_lossy(),
        )
        .unwrap();
}

async fn server(app: Arc<AppState>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(
            listener,
            build_router(app).into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });
    format!("http://{addr}")
}

async fn server_with_peer(app: Arc<AppState>, peer: SocketAddr) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = build_router(app).layer(axum::middleware::from_fn(move |mut req: axum::http::Request<axum::body::Body>, next: axum::middleware::Next| async move {
        req.extensions_mut().insert(axum::extract::ConnectInfo(peer));
        next.run(req).await
    }));
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    format!("http://{addr}")
}

async fn body(response: reqwest::Response) -> (StatusCode, serde_json::Value) {
    let status = response.status();
    let value = response.json().await.unwrap_or(serde_json::Value::Null);
    (status, value)
}

#[tokio::test]
async fn every_route_error_has_code_and_authoritative_code_status_table_is_exercised() {
    // Representative errors exercise the shared route envelope; ApiError::ALL is the
    // authoritative code/status table and is iterated so additions require coverage.
    let tmp = tempfile::tempdir().unwrap();
    let base = server(state(tmp.path())).await;
    let client = reqwest::Client::new();
    for path in [
        "/v1/sessions/missing",
        "/v1/commands/missing",
        "/v1/workflows/missing",
    ] {
        let response = client.get(format!("{base}{path}")).send().await.unwrap();
        let (status, json) = body(response).await;
        assert!(
            status.is_client_error() || status.is_server_error(),
            "{path}: {status}"
        );
        assert!(json["code"].is_string(), "{path}: {json}");
    }
    let expected = [
        ("unauthorized", 401),
        ("forbidden_workdir", 403),
        ("invalid_request", 400),
        ("forbidden", 403),
        ("not_found", 404),
        ("method_not_allowed", 405),
        ("output_not_found", 404),
        ("request_too_large", 413),
        ("output_too_large", 413),
        ("unsupported_media_type", 415),
        ("unprocessable_request", 422),
        ("unknown_subcommand", 400),
        ("unknown_flag", 400),
        ("session_not_found", 404),
        ("session_closing", 409),
        ("session_not_ready", 409),
        ("command_not_found", 404),
        ("no_workflow", 404),
        ("workflow_not_started", 404),
        ("workflow_never_started", 404),
        ("idempotency_conflict", 422),
        ("idempotency_in_flight", 409),
        ("internal", 500),
        ("unavailable", 503),
        ("worktree_exists", 409),
        ("merge_conflict", 409),
        ("merge_target_dirty", 409),
        ("workflow_prerequisites_unresolved", 409),
        ("unknown_agent", 404),
        ("agent_untrusted", 403),
    ]
    .into_iter()
    .collect::<std::collections::HashSet<_>>();
    let actual = ApiError::ALL
        .iter()
        .map(|(code, status)| (*code, status.as_u16()))
        .collect();
    assert_eq!(expected, actual);
    let mut pairs = std::collections::HashSet::new();
    for (code, status) in ApiError::ALL {
        assert!(
            pairs.insert((*code, status.as_u16())),
            "duplicate table pair {code} {status}"
        );
        let response = ApiError {
            code,
            status: *status,
            message: "table probe".into(),
        }
        .into_response();
        assert_eq!(response.status(), *status);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["code"], *code);
    }
}

#[tokio::test]
async fn legacy_error_messages_and_command_resource_aliases_are_stable() {
    let tmp = tempfile::tempdir().unwrap();
    let app = state(tmp.path());
    enqueue(&app, "alias-command");
    app.paths
        .prepare_command_dir(SESSION, "queued-workflow")
        .unwrap();
    app.store
        .enqueue_command(
            "queued-workflow",
            SESSION,
            "exec workflow",
            "[]",
            "/queued-workflow.log",
        )
        .unwrap();
    app.paths
        .prepare_command_dir(SESSION, "finished-workflow")
        .unwrap();
    app.store
        .enqueue_command(
            "finished-workflow",
            SESSION,
            "exec workflow",
            "[]",
            "/finished-workflow.log",
        )
        .unwrap();
    app.store
        .complete_command("finished-workflow", "done", Some(0), None)
        .unwrap();
    let base = server(app.clone()).await;
    let client = reqwest::Client::new();

    for (path, expected) in [
        ("/v1/workflows/no-such", "command not found"),
        ("/v1/commands/no-such/workflow", "command not found"),
        ("/v1/commands/no-such/status", "Command 'no-such' not found"),
        ("/v1/commands/no-such", "Command 'no-such' not found"),
    ] {
        let (status, json) = body(client.get(format!("{base}{path}")).send().await.unwrap()).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(json["error"], expected);
        assert_eq!(json["code"], "command_not_found");
    }

    for (id, expected_error, expected_code) in [
        (
            "alias-command",
            "no workflow for this command",
            "no_workflow",
        ),
        (
            "queued-workflow",
            "workflow not started",
            "workflow_not_started",
        ),
        (
            "finished-workflow",
            "workflow never started",
            "workflow_never_started",
        ),
    ] {
        let (status, json) = body(
            client
                .get(format!("{base}/v1/workflows/{id}"))
                .send()
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(json["error"], expected_error);
        assert_eq!(json["code"], expected_code);
    }

    for (left, right) in [
        (
            "/v1/commands/alias-command",
            "/v1/commands/alias-command/status",
        ),
        (
            "/v1/commands/alias-command/workflow",
            "/v1/workflows/alias-command",
        ),
    ] {
        let a = client.get(format!("{base}{left}")).send().await.unwrap();
        let b = client.get(format!("{base}{right}")).send().await.unwrap();
        assert_eq!(a.status(), b.status());
        assert_eq!(
            a.bytes().await.unwrap(),
            b.bytes().await.unwrap(),
            "{left} vs {right}"
        );
    }
}

#[tokio::test]
async fn session_setup_status_filters_errors_and_closed_stage_are_returned() {
    let tmp = tempfile::tempdir().unwrap();
    let app = state(tmp.path());
    app.store
        .insert_session("ready-session", "/ready", "2026-01-01")
        .unwrap();
    app.store.update_setup_status(SESSION, "failed").unwrap();
    let setup_dir = app.paths.session_dir(SESSION);
    std::fs::create_dir_all(&setup_dir).unwrap();
    std::fs::write(setup_dir.join("setup_state.json"), r#"{"status":"failed","current_stage":"Failed: git clone failed","current_ready_phase":null,"ready_step_statuses":[],"ready_summary":null,"error":{"stage":"clone","message":"git clone failed"}}"#).unwrap();
    let base = server(app.clone()).await;
    let client = reqwest::Client::new();

    let (_, list) = body(
        client
            .get(format!("{base}/v1/sessions"))
            .send()
            .await
            .unwrap(),
    )
    .await;
    let item = list["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == SESSION)
        .unwrap();
    assert_eq!(item["setup_status"], "failed");
    assert_eq!(item["setup_error"]["stage"], "clone");
    assert_eq!(item["setup_error"]["message"], "git clone failed");
    let (_, detail) = body(
        client
            .get(format!("{base}/v1/sessions/{SESSION}"))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(detail["setup_status"], "failed");
    assert_eq!(detail["setup_error"]["stage"], "clone");
    let (_, filtered) = body(
        client
            .get(format!("{base}/v1/sessions?setup_status=failed"))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(filtered["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(filtered["sessions"][0]["id"], SESSION);
    let (_, ready) = body(
        client
            .get(format!("{base}/v1/sessions?setup_status=ready"))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(ready["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(ready["sessions"][0]["id"], "ready-session");
    let (status, invalid) = body(
        client
            .get(format!("{base}/v1/sessions?setup_status=wat"))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(invalid["code"], "invalid_request");

    app.store
        .close_session(SESSION, &chrono::Utc::now().to_rfc3339())
        .unwrap();
    let (_, closed) = body(
        client
            .get(format!("{base}/v1/sessions/{SESSION}"))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(closed["status"], "closed");
    assert_eq!(closed["setup_status"], "failed");
    let deleted = client
        .delete(format!("{base}/v1/sessions/{SESSION}"))
        .send()
        .await
        .unwrap();
    assert_eq!(deleted.status(), StatusCode::OK);
    let value: serde_json::Value = deleted.json().await.unwrap();
    assert_eq!(value["setup_status"], "failed");
}

#[tokio::test]
async fn command_body_and_header_session_forms_and_idempotent_replays_work() {
    let tmp = tempfile::tempdir().unwrap();
    let app = state(tmp.path());
    app.store
        .insert_session("other-session", "/other", "2026-01-01T00:00:00Z")
        .unwrap();
    let base = server(app.clone()).await;
    let client = reqwest::Client::new();
    let url = format!("{base}/v1/commands");
    let command = |session: Option<&str>| {
        let mut value = serde_json::json!({"subcommand":"exec prompt","args":["hello"]});
        if let Some(session) = session {
            value["session_id"] = session.into();
        }
        value
    };
    for (body_session, header_session, key) in [
        (Some(SESSION), None, "body"),
        (None, Some(SESSION), "header"),
        (Some(SESSION), Some(SESSION), "both"),
    ] {
        let mut request = client
            .post(&url)
            .json(&command(body_session))
            .header("idempotency-key", key);
        if let Some(sid) = header_session {
            request = request.header("x-awman-session", sid);
        }
        let response = request.send().await.unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let first: serde_json::Value = response.json().await.unwrap();
        let command_id = first["command_id"]
            .as_str()
            .expect("command id in admission response");
        assert_eq!(
            app.store
                .get_command(command_id)
                .unwrap()
                .unwrap()
                .session_id,
            SESSION
        );
        let mut replay = client
            .post(&url)
            .json(&command(body_session))
            .header("idempotency-key", key);
        if let Some(sid) = header_session {
            replay = replay.header("x-awman-session", sid);
        }
        let response = replay.send().await.unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        assert_eq!(
            response.headers().get("idempotent-replayed").unwrap(),
            "true"
        );
        let again: serde_json::Value = response.json().await.unwrap();
        assert_eq!(first, again);
    }
    let conflicting = client
        .post(&url)
        .json(&command(Some(SESSION)))
        .header("x-awman-session", "other-session")
        .send()
        .await
        .unwrap();
    let (status, error) = body(conflicting).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(error["code"], "invalid_request");
    assert_eq!(
        error["error"],
        "session_id in the body and the x-awman-session header name different sessions"
    );

    let response = client
        .post(&url)
        .json(&command(None))
        .header("x-awman-session", SESSION)
        .header("idempotency-key", "canonical")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let replay = client
        .post(&url)
        .json(&command(Some(SESSION)))
        .header("idempotency-key", "canonical")
        .send()
        .await
        .unwrap();
    assert_eq!(replay.status(), StatusCode::ACCEPTED);
    assert_eq!(replay.headers().get("idempotent-replayed").unwrap(), "true");
    // Body-original replayed with the header form.
    let replay = client
        .post(&url)
        .json(&command(None))
        .header("x-awman-session", SESSION)
        .header("idempotency-key", "body")
        .send()
        .await
        .unwrap();
    assert_eq!(replay.status(), StatusCode::ACCEPTED);
    assert_eq!(replay.headers()["idempotent-replayed"], "true");
    let (status, error) = body(
        client
            .post(&url)
            .json(&command(None))
            .header("x-awman-session", "other-session")
            .header("idempotency-key", "body")
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(error["code"], "idempotency_conflict");
    // Selection validation must precede replay and hash mismatch handling.
    for header in [
        axum::http::HeaderValue::from_static("other-session"),
        axum::http::HeaderValue::from_bytes(&[0xff]).unwrap(),
    ] {
        let response = client
            .post(&url)
            .json(&command(Some(SESSION)))
            .header("x-awman-session", header)
            .header("idempotency-key", "body")
            .send()
            .await
            .unwrap();
        assert!(response.headers().get("idempotent-replayed").is_none());
        let (status, error) = body(response).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(error["code"], "invalid_request");
    }
    app.store.with_conn(|conn| {
        conn.execute("INSERT INTO idempotency_keys(scope,key,request_hash,created_at,owner_instance,owner_request)
            SELECT scope,'inflight',request_hash,created_at,owner_instance,owner_request
            FROM idempotency_keys WHERE key='body'", [])?;
        Ok(())
    }).unwrap();
    let response = client
        .post(&url)
        .json(&command(Some(SESSION)))
        .header("idempotency-key", "inflight")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(response.headers()["retry-after"], "1");
    assert_eq!(response.headers()["content-type"], "application/json");
    let (_, error) = body(response).await;
    assert_eq!(error["code"], "idempotency_in_flight");
    // Seed the exact pre-upgrade hash and response, without using canonicalization.
    use sha2::{Digest, Sha256};
    let old_hash: String = Sha256::digest(format!("{}\n{SESSION}", command(None)))
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    app.store
        .with_conn(|conn| {
            conn.execute(
                "INSERT INTO idempotency_keys(scope,key,request_hash,status,body,created_at)
            VALUES (?1,?2,?3,202,?4,?5)",
                rusqlite::params![
                    "/v1/commands|anonymous",
                    "pre-upgrade",
                    old_hash,
                    r#"{"command_id":"old-command"}"#,
                    chrono::Utc::now().to_rfc3339()
                ],
            )?;
            Ok(())
        })
        .unwrap();
    let response = client
        .post(&url)
        .json(&command(Some(SESSION)))
        .header("idempotency-key", "pre-upgrade")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    assert_eq!(response.headers()["idempotent-replayed"], "true");
    assert_eq!(
        response.text().await.unwrap(),
        r#"{"command_id":"old-command"}"#
    );
}

#[tokio::test]
async fn pending_alias_unknown_flags_and_exec_parent_refusal_are_pinned() {
    let tmp = tempfile::tempdir().unwrap();
    let app = state(tmp.path());
    enqueue(&app, "legacy-command");
    let base = server(app).await;
    let client = reqwest::Client::new();
    let (_, filtered) = body(
        client
            .get(format!("{base}/v1/commands?status=pending"))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert!(
        filtered["commands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["id"] == "legacy-command" && c["status"] == "queued"),
        "{filtered}"
    );
    let (_, all) = body(
        client
            .get(format!("{base}/v1/commands"))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert!(!all.to_string().contains("pending"));

    let unknown_flag = client
        .post(format!("{base}/v1/commands"))
        .header("x-awman-session", SESSION)
        .json(&serde_json::json!({"subcommand":"exec workflow", "args":["--wi-0138-nope"]}))
        .send()
        .await
        .unwrap();
    let (status, error) = body(unknown_flag).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(error["code"], "unknown_flag");

    let parent = client
        .post(format!("{base}/v1/commands"))
        .header("x-awman-session", SESSION)
        .json(&serde_json::json!({"subcommand":"exec", "args":["prompt", "hello"]}))
        .send()
        .await
        .unwrap();
    let (status, error) = body(parent).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(error["code"], "invalid_request");
    assert_eq!(error["blocked_command"], "exec");
}

#[tokio::test]
async fn log_path_is_loopback_only_and_status_features_are_unique() {
    let tmp = tempfile::tempdir().unwrap();
    let app = state(tmp.path());
    enqueue(&app, "log-path");
    let loopback_base = server_with_peer(
        app.clone(),
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 1234),
    )
    .await;
    let direct_base = server(app.clone()).await;
    let upstream = direct_base.strip_prefix("http://").unwrap().to_string();
    let proxy = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = proxy.local_addr().unwrap();
    let proxy_task = tokio::spawn(async move {
        let (mut client, _) = proxy.accept().await.unwrap();
        let mut upstream = tokio::net::TcpStream::connect(upstream).await.unwrap();
        let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
    });
    let remote_base = server_with_peer(app, "192.0.2.1:1234".parse().unwrap()).await;
    let client = reqwest::Client::new();
    let loopback = client
        .get(format!("{loopback_base}/v1/commands/log-path"))
        .send()
        .await
        .unwrap();
    let value: serde_json::Value = loopback.json().await.unwrap();
    assert!(value["log_path"].is_string());
    let remote = client
        .get(format!("{remote_base}/v1/commands/log-path"))
        .send()
        .await
        .unwrap();
    let value: serde_json::Value = remote.json().await.unwrap();
    assert!(value.get("log_path").is_none());
    // A local proxy is a loopback peer, even if forwarding headers name a remote client.
    let proxy_response: serde_json::Value = client
        .get(format!("http://{proxy_addr}/v1/commands/log-path"))
        .header("x-forwarded-for", "192.0.2.1")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(proxy_response["log_path"].is_string());
    proxy_task.abort();
    let features = API_FEATURES
        .iter()
        .copied()
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(features.len(), API_FEATURES.len());
    for feature in [
        "error_codes",
        "command_record",
        "session_setup_status",
        "command_session_body",
    ] {
        assert!(features.contains(feature), "missing feature {feature}");
    }
}

#[tokio::test]
async fn closing_acknowledgements_retain_readiness() {
    let tmp = tempfile::tempdir().unwrap();
    let app = state(tmp.path());
    enqueue(&app, "draining-command");
    app.store.claim_next_command("worker").unwrap().unwrap();
    let base = server(app).await;
    let client = reqwest::Client::new();
    for expected in [StatusCode::ACCEPTED, StatusCode::OK] {
        let (status, json) = body(
            client
                .delete(format!("{base}/v1/sessions/{SESSION}"))
                .send()
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(status, expected);
        assert_eq!(json["status"], "closing");
        assert_eq!(json["setup_status"], "ready");
        assert_eq!(json["running_command_id"], "draining-command");
    }
}

#[tokio::test]
async fn framework_and_admission_errors_have_codes_over_http() {
    let tmp = tempfile::tempdir().unwrap();
    let app = state(tmp.path());
    enqueue(&app, "outputs-command");
    let base = server(app.clone()).await;
    let client = reqwest::Client::new();
    let submit = || {
        client.post(format!("{base}/v1/commands")).json(
            &serde_json::json!({"session_id":SESSION,"subcommand":"exec prompt","args":["hello"]}),
        )
    };
    let cases = [
        (client.get(format!("{base}/v1/nope")), 404, "not_found"),
        (client.delete(format!("{base}/v1/commands")), 405, "method_not_allowed"),
        (client.post(format!("{base}/v1/commands")).header("content-type", "application/json").body("{bad"), 400, "invalid_request"),
        (client.post(format!("{base}/v1/commands")).body("{}"), 415, "unsupported_media_type"),
        (client.post(format!("{base}/v1/commands")).header("content-type", "application/json").body(" ".repeat(3 * 1024 * 1024)), 413, "request_too_large"),
        (client.get(format!("{base}/v1/commands/outputs-command/outputs/missing.txt")), 404, "output_not_found"),
        (client.get(format!("{base}/v1/commands/outputs-command/logs?after=bad")), 400, "invalid_request"),
        (client.post(format!("{base}/v1/sessions")).json(&serde_json::json!({"workdir": tmp.path()})), 403, "forbidden_workdir"),
        (client.post(format!("{base}/v1/runs")).json(&serde_json::json!({"session":{"workdir":tmp.path()},"command":{"session_id":SESSION,"subcommand":"exec prompt","args":["hello"]}})), 400, "invalid_request"),
    ];
    for (request, expected_status, code) in cases {
        let response = request.send().await.unwrap();
        assert_eq!(response.status().as_u16(), expected_status, "{code}");
        assert_eq!(response.headers()["content-type"], "application/json");
        if expected_status == 405 {
            assert!(response.headers().contains_key("allow"));
        }
        let (_, json) = body(response).await;
        assert_eq!(json["code"], code, "{json}");
        assert!(!json["error"].as_str().unwrap().is_empty());
    }
    for setup in ["initializing", "failed"] {
        app.store.update_setup_status(SESSION, setup).unwrap();
        let (status, json) = body(submit().send().await.unwrap()).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(json["code"], "session_not_ready");
    }
    app.store.update_session_status(SESSION, "closing").unwrap();
    let (status, json) = body(submit().send().await.unwrap()).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(json["code"], "session_closing");

    let auth_tmp = tempfile::tempdir().unwrap();
    let mut authenticated = state(auth_tmp.path());
    Arc::get_mut(&mut authenticated).unwrap().auth_mode = AuthMode::Enabled {
        key_hash: "unused-test-hash".into(),
    };
    let auth_base = server(authenticated).await;
    let response = client
        .get(format!("{auth_base}/v1/status"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.headers()["content-type"], "application/json");
    let (status, json) = body(response).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(json["code"], "unauthorized");
}
