//! Live API server smoke test.
//!
//! Boots the real Axum router (via `routes::build_router`) on an ephemeral
//! loopback port, hits each documented endpoint with `reqwest`, and tears down
//! cleanly. Tests are gated by whether we can bind a TCP port; on hosts that
//! deny loopback binding we skip rather than hard-fail.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use crate::spawn_router;
use awman::command::dispatch::Engines;
use awman::data::fs::api_db::SqliteSessionStore;
use awman::data::fs::api_paths::ApiPaths;
use awman::data::fs::auth_paths::AuthPathResolver;
use awman::data::WorkflowStateStore;
use awman::engine::agent::AgentEngine;
use awman::engine::auth::AuthEngine;
use awman::engine::container::ContainerRuntime;
use awman::engine::git::GitEngine;
use awman::engine::overlay::OverlayEngine;
use awman::frontend::api::routes::AppState;

fn make_app_state(root: &std::path::Path) -> Arc<AppState> {
    let paths = ApiPaths::from_root(root);
    paths.ensure_root().expect("ensure_root");
    let store = SqliteSessionStore::open(paths.root()).expect("open sqlite");

    let auth_paths = AuthPathResolver::at_home(root);
    let runtime = Arc::new(ContainerRuntime::docker());
    let git_engine = Arc::new(GitEngine::new());
    let overlay_engine = Arc::new(OverlayEngine::with_auth_resolver(auth_paths.clone()));
    let agent_engine = Arc::new(AgentEngine::new(overlay_engine.clone(), runtime.clone()));
    let auth_engine = Arc::new(AuthEngine::with_paths(auth_paths, paths.clone()));
    let workflow_state_store = Arc::new(WorkflowStateStore::at_git_root(paths.root()));

    let engines = Engines {
        runtime: runtime.clone(),
        container_runtime: Some(runtime),
        sandbox_runtime: None,
        git_engine,
        overlay_engine,
        auth_engine,
        agent_engine,
        workflow_state_store,
        credential_monitor: None,
        global_config: std::sync::Arc::new(Default::default()),
    };

    let store = Arc::new(store);

    let lifecycle_store = awman::data::CommandLifecycleStore::open(Arc::clone(&store)).unwrap();

    let setup_cancels = Arc::new(awman::command::commands::api_server::TokenRegistry::new());

    let canceller = awman::command::commands::api_server::CommandCanceller::new(
        Arc::clone(&store),
        lifecycle_store.clone(),
        Arc::new(awman::command::commands::api_server::TokenRegistry::new()),
        Arc::clone(&setup_cancels),
    );

    Arc::new(AppState {
        live_config: std::sync::RwLock::new(None),
        settings_tx: None,
        lifecycle_store,
        setup_cancels,
        canceller,

        store,
        paths,
        workdirs: vec![],
        workdir_roots: Vec::new(),
        started_at: Instant::now(),
        task_handles: tokio::sync::Mutex::new(Vec::new()),
        engines,
        sessions: Arc::new(awman::data::session_manager::SessionManager::in_memory(
            awman::data::config::env::EnvSnapshot::empty(),
        )),
        event_buses: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        setup_buses: tokio::sync::Mutex::new(HashMap::new()),
    })
}

#[tokio::test]
async fn real_network_api_status_endpoint_returns_ok() {
    let tmp = tempfile::tempdir().unwrap();
    let state = make_app_state(tmp.path());
    let Some((addr, server)) = spawn_router(state).await else {
        eprintln!("SKIP: cannot bind 127.0.0.1");
        return;
    };

    let url = format!("http://{addr}/v1/status");
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .unwrap();
    let resp = client
        .get(&url)
        .send()
        .await
        .expect("status endpoint must respond");

    assert_eq!(resp.status(), 200, "expected 200, got {}", resp.status());
    let body: serde_json::Value = resp.json().await.expect("json body");
    assert_eq!(body["status"], "ok");
    assert!(body["uptime_ms"].is_number());
    assert!(body["pid"].is_number());

    server.abort();
}

#[tokio::test]
async fn real_network_api_unknown_route_returns_404() {
    let tmp = tempfile::tempdir().unwrap();
    let state = make_app_state(tmp.path());
    let Some((addr, server)) = spawn_router(state).await else {
        eprintln!("SKIP: cannot bind 127.0.0.1");
        return;
    };

    let url = format!("http://{addr}/v1/this-does-not-exist");
    let resp = reqwest::get(&url).await.expect("response");
    assert_eq!(resp.status(), 404);

    server.abort();
}

#[tokio::test]
async fn real_network_api_workdirs_endpoint_returns_200() {
    let tmp = tempfile::tempdir().unwrap();
    let state = make_app_state(tmp.path());
    let Some((addr, server)) = spawn_router(state).await else {
        eprintln!("SKIP: cannot bind 127.0.0.1");
        return;
    };

    let url = format!("http://{addr}/v1/worker/workdirs");
    let resp = reqwest::get(&url).await.expect("workdirs response");
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.expect("workdirs body");
    assert!(body["items"].is_array());
    assert!(body["items"].is_array());

    server.abort();
}

#[tokio::test]
async fn real_network_api_auth_required_when_enabled() {
    let tmp = tempfile::tempdir().unwrap();
    let api = Arc::new(awman::command::daemon_api::DaemonApi::for_worker_tests(
        make_app_state(tmp.path()),
    ));
    let Some((addr, server)) = spawn_network(api).await else {
        eprintln!("SKIP: cannot bind 127.0.0.1");
        return;
    };

    let url = format!("http://{addr}/v1/status");
    let resp = reqwest::get(&url).await.expect("response");
    assert_eq!(
        resp.status(),
        401,
        "auth-enabled mode must reject requests without an Authorization header"
    );

    server.abort();
}

#[tokio::test]
async fn real_network_api_auth_accepts_valid_key() {
    let tmp = tempfile::tempdir().unwrap();

    let api = Arc::new(awman::command::daemon_api::DaemonApi::for_worker_tests(
        make_app_state(tmp.path()),
    ));
    let (_, key) = api.runtime.keys().create("test").unwrap();
    let key = key.as_str();
    let Some((addr, server)) = spawn_network(api).await else {
        eprintln!("SKIP: cannot bind 127.0.0.1");
        return;
    };

    let url = format!("http://{addr}/v1/status");
    let resp = reqwest::Client::new()
        .get(&url)
        .header("Authorization", format!("Bearer {key}"))
        .send()
        .await
        .expect("response");
    assert_eq!(resp.status(), 200, "valid bearer key must be accepted");

    server.abort();
}

async fn spawn_network(
    api: Arc<awman::command::daemon_api::DaemonApi>,
) -> Option<(std::net::SocketAddr, tokio::task::JoinHandle<()>)> {
    let app = awman::frontend::api::routes::build_router_for_listener(
        api,
        awman::frontend::api::routes::ListenerPolicy::Network {
            name: "default".into(),
        },
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.ok()?;
    let addr = listener.local_addr().unwrap();
    Some((
        addr,
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        }),
    ))
}
