//! WI 0101 — the squad daemon's HTTP surface.
//!
//! Boots the real daemon via the injectable Layer 2 bootstrap
//! (`SquadDaemonHandles::bootstrap`) plus `frontend::squad::serve`, on an
//! ephemeral loopback port, drives it with `reqwest`, and tears down by
//! aborting the task — mirroring `tests/api_parity/live_server.rs`'s
//! established pattern for this codebase's other HTTP daemon.
//!
//! The bootstrap reads `AWMAN_CONFIG_HOME` (and friends) from the real process
//! environment (`Env::from_process()` is hardcoded inside it), so every test
//! here scopes that env var for its duration under a shared lock — the same
//! technique `tests/data_layer/rename_0077.rs` and
//! `tests/overlays_integration.rs` already use for env-mutating tests.

use std::sync::Arc;
use std::time::Duration;

use awman::command::commands::squad::commands::SquadServeConfig;
use awman::command::commands::squad::daemon_runtime::SquadDaemonHandles;
use awman::command::dispatch::Engines;
use awman::data::fs::daemon_process::{DaemonProcess, SQUAD_PLIST_LABEL, SQUAD_UNIT_NAME};
use awman::data::fs::{ApiPaths, AuthPathResolver, SquadPaths};
use awman::data::EngineWorkflowStateStore;
use awman::engine::agent::AgentEngine;
use awman::engine::auth::AuthEngine;
use awman::engine::container::ContainerRuntime;
use awman::engine::git::GitEngine;
use awman::engine::overlay::OverlayEngine;
use awman::engine::squad::{EvaluationOutcome, EvaluationRequest, TaskEvaluator};
use tokio::sync::Mutex;

static ENV_LOCK: Mutex<()> = Mutex::const_new(());

struct NeverTriggeredEvaluator;
#[async_trait::async_trait]
impl TaskEvaluator for NeverTriggeredEvaluator {
    async fn evaluate(&self, _request: EvaluationRequest) -> EvaluationOutcome {
        EvaluationOutcome::NotTriggered
    }
}

fn engines_with(container_runtime: Arc<ContainerRuntime>, root: &std::path::Path) -> Engines {
    let api_paths = ApiPaths::from_root(root);
    let auth_paths = AuthPathResolver::at_home(root);
    let overlay_engine = Arc::new(OverlayEngine::with_auth_resolver(auth_paths.clone()));
    Engines {
        runtime: container_runtime.clone(),
        container_runtime: Some(container_runtime.clone()),
        sandbox_runtime: None,
        git_engine: Arc::new(GitEngine::new()),
        overlay_engine: overlay_engine.clone(),
        auth_engine: Arc::new(AuthEngine::with_paths(auth_paths, api_paths.clone())),
        agent_engine: Arc::new(AgentEngine::new(overlay_engine, container_runtime)),
        workflow_state_store: Arc::new(EngineWorkflowStateStore::at_git_root(api_paths.root())),
    }
}

/// Sets `AWMAN_CONFIG_HOME` to `root`, starts the daemon in a background
/// task, and polls for its published metadata. Returns the task handle and
/// the resolved `http://127.0.0.1:<port>` base URL. Restores the previous
/// env var value once metadata has been read (the daemon itself already
/// captured its own `EnvSnapshot` by then).
async fn start_daemon(
    root: &std::path::Path,
    container_runtime: Arc<ContainerRuntime>,
) -> (tokio::task::JoinHandle<()>, String) {
    start_daemon_with(root, container_runtime, Arc::new(NeverTriggeredEvaluator)).await
}

/// The same bootstrap with a caller-supplied evaluator, for the tests that
/// need an evaluation to actually be in flight.
async fn start_daemon_with(
    root: &std::path::Path,
    container_runtime: Arc<ContainerRuntime>,
    evaluator: Arc<dyn TaskEvaluator>,
) -> (tokio::task::JoinHandle<()>, String) {
    // A test daemon must never reach the developer's real OS keychain. The
    // stored item is a single fixed `awman-squad`/`daemon-env` pair — it is not
    // keyed by storage root — so an isolated `AWMAN_CONFIG_HOME` does *not*
    // isolate it: a daemon booted here with the default `envPersistence:
    // "keychain"` would overwrite the item a real daemon on this machine is
    // using, with this test's fixture values. Default every test daemon to the
    // explicit opt-out, and let a test that wants a different setting write its
    // own config first.
    if !root.join("config.json").exists() {
        write_squad_config(root, "none");
    }

    let engines = engines_with(container_runtime, root);
    let config = SquadServeConfig {
        port: 0,
        dangerously_skip_auth: true,
    };

    let previous = std::env::var("AWMAN_CONFIG_HOME").ok();
    std::env::set_var("AWMAN_CONFIG_HOME", root);

    // WI 0113 F-02: Layer 2 bootstraps the daemon, Layer 3 serves it. This is
    // the same seam `serve_with` was, split across the layer boundary.
    let handle = tokio::spawn(async move {
        let handles = SquadDaemonHandles::bootstrap(config, engines, evaluator)
            .await
            .expect("squad daemon bootstrap");
        let _ = awman::frontend::squad::serve(handles).await;
    });

    let daemon = DaemonProcess::new(
        SquadPaths::from_root(root.join("squad")).daemon(),
        SQUAD_UNIT_NAME,
        SQUAD_PLIST_LABEL,
    );
    let mut waited = Duration::ZERO;
    let meta = loop {
        if let Ok(Some(meta)) = daemon.read_meta() {
            break meta;
        }
        if waited >= Duration::from_secs(10) {
            panic!("squad daemon never published its server metadata");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        waited += Duration::from_millis(50);
    };

    match previous {
        Some(v) => std::env::set_var("AWMAN_CONFIG_HOME", v),
        None => std::env::remove_var("AWMAN_CONFIG_HOME"),
    }

    (
        handle,
        format!("{}://{}:{}", meta.scheme, meta.bind_ip, meta.port),
    )
}

// ─── Startup succeeds under both Docker and Apple, no tier branching ────────

#[tokio::test]
async fn daemon_startup_succeeds_under_docker_and_apple_with_no_tier_branching() {
    let _env_guard = ENV_LOCK.lock().await;

    for runtime in [
        Arc::new(ContainerRuntime::docker()),
        Arc::new(ContainerRuntime::apple()),
    ] {
        let name = runtime.runtime_name();
        let tmp = tempfile::tempdir().unwrap();
        let (handle, base) = start_daemon(tmp.path(), runtime).await;

        let resp = reqwest::get(format!("{base}/v1/status"))
            .await
            .unwrap_or_else(|e| panic!("{name}: status request must succeed: {e}"));
        assert_eq!(
            resp.status(),
            200,
            "{name}: /v1/status must succeed identically regardless of container tier"
        );
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["running"], true);

        handle.abort();
    }
}

// ─── POST /v1/commands rejections ────────────────────────────────────────────

#[tokio::test]
async fn post_commands_rejects_non_squad_subcommand() {
    let _env_guard = ENV_LOCK.lock().await;
    let tmp = tempfile::tempdir().unwrap();
    let (handle, base) = start_daemon(tmp.path(), Arc::new(ContainerRuntime::docker())).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/commands"))
        .json(&serde_json::json!({"subcommand": "exec workflow", "args": []}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(
        body["error"].as_str().unwrap().contains("squad subtree"),
        "error must explain the squad-only subtree restriction: {body}"
    );

    handle.abort();
}

#[tokio::test]
async fn post_commands_rejects_unknown_flag_with_catalogue_error_shape() {
    let _env_guard = ENV_LOCK.lock().await;
    let tmp = tempfile::tempdir().unwrap();
    let (handle, base) = start_daemon(tmp.path(), Arc::new(ContainerRuntime::docker())).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/commands"))
        .json(&serde_json::json!({
            "subcommand": "squad add",
            "args": ["--this-flag-does-not-exist", "value"],
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(
        body.get("error")
            .and_then(|v| v.as_str())
            .is_some_and(|s| !s.is_empty()),
        "unknown-flag rejection must use the shared {{\"error\": \"...\"}} envelope: {body}"
    );

    handle.abort();
}

#[tokio::test]
async fn attach_is_refused_by_the_catalogue_even_though_it_exists_for_cli_tui() {
    let _env_guard = ENV_LOCK.lock().await;
    let tmp = tempfile::tempdir().unwrap();
    let (handle, base) = start_daemon(tmp.path(), Arc::new(ContainerRuntime::docker())).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/commands"))
        .json(&serde_json::json!({"subcommand": "squad attach", "args": ["some-task"]}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        400,
        "`squad attach` is api_allowed: false in the catalogue and must be refused"
    );

    handle.abort();
}

// ─── Loopback-only bind ──────────────────────────────────────────────────────

/// This host's own non-loopback IPv4 address, discovered without sending a
/// packet: connecting a UDP socket only sets the kernel's route, and
/// `local_addr` then reports the interface it chose. Returns `None` on a host
/// with no outbound route (a fully isolated container), where the property
/// below cannot be observed.
fn non_loopback_local_ip() -> Option<std::net::IpAddr> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("203.0.113.1:9").ok()?;
    let ip = socket.local_addr().ok()?.ip();
    (!ip.is_loopback() && !ip.is_unspecified()).then_some(ip)
}

#[tokio::test]
async fn daemon_binds_loopback_only() {
    let _env_guard = ENV_LOCK.lock().await;
    let tmp = tempfile::tempdir().unwrap();
    let (handle, base) = start_daemon(tmp.path(), Arc::new(ContainerRuntime::docker())).await;

    assert!(
        base.starts_with("http://127.0.0.1:"),
        "squad always binds 127.0.0.1 regardless of any configuration input: {base}"
    );

    // The published metadata is the daemon's own record of what it bound;
    // corroborate it actually answers there.
    let resp = reqwest::get(format!("{base}/v1/status")).await.unwrap();
    assert_eq!(resp.status(), 200);

    let port: u16 = base
        .rsplit(':')
        .next()
        .and_then(|p| p.parse().ok())
        .expect("the published endpoint must carry a port");

    // The same port on this host's *external* interface must not accept a
    // connection at all — the socket is bound to 127.0.0.1, not 0.0.0.0.
    match non_loopback_local_ip() {
        Some(ip) => {
            let addr = std::net::SocketAddr::new(ip, port);
            let refused = tokio::task::spawn_blocking(move || {
                std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_secs(2))
            })
            .await
            .unwrap();
            assert!(
                refused.is_err(),
                "the daemon must not answer on the non-loopback interface {ip}:{port}"
            );
        }
        None => eprintln!("SKIP (partial): host has no non-loopback IPv4 interface to probe"),
    }

    handle.abort();
}

// ─── The live workflow-state route ──────────────────────────────────────────

/// An evaluator that stands in for a triggered task whose generated
/// workflow is *currently executing*: it persists a real `WorkflowState`
/// through the engine's own `WorkflowStateStore`, reports the resulting path
/// through the `RunProgress` seam (exactly as `LocalTaskEvaluator` does
/// before it hands off to `ExecWorkflowCommand`), then parks so the run row
/// stays `running` while the test queries the route.
struct WorkflowInFlightEvaluator {
    state_dir: std::path::PathBuf,
    started: tokio::sync::mpsc::UnboundedSender<()>,
    release: Arc<tokio::sync::Notify>,
}

#[async_trait::async_trait]
impl TaskEvaluator for WorkflowInFlightEvaluator {
    async fn evaluate(&self, request: EvaluationRequest) -> EvaluationOutcome {
        let steps = vec![awman::data::workflow_definition::WorkflowStep {
            name: "analyze".to_string(),
            depends_on: Vec::new(),
            prompt_template: "analyze the issue".to_string(),
            agent: Some("claude".to_string()),
            model: None,
            overlays: None,
            abort_on_failure: false,
        }];
        let state = awman::data::workflow_state::WorkflowState::new(
            "squad-generated".to_string(),
            &steps,
            "deadbeef".to_string(),
            None,
        );
        let store = EngineWorkflowStateStore::at_git_root(&self.state_dir);
        let state_path = store.save(&state).expect("persisting workflow state");

        let workflow_path = request.task_dir.join("workflow.toml");
        request
            .progress
            .workflow_started(&request.run_id, &workflow_path, &state_path);

        let _ = self.started.send(());
        // Park while the run row is still `running`, which is the only window
        // in which the route is supposed to answer.
        self.release.notified().await;

        EvaluationOutcome::WorkflowExecuted {
            workflow_path,
            workflow_state_path: Some(state_path),
            exit_code: Some(0),
        }
    }
}

#[tokio::test]
async fn the_workflow_route_serves_the_live_state_verbatim_while_a_run_is_in_flight() {
    let _env_guard = ENV_LOCK.lock().await;
    let tmp = tempfile::tempdir().unwrap();

    // A task that is immediately due, seeded straight into the shared
    // database the daemon is about to open.
    let db = tmp.path().join("data").join("awman.db");
    std::fs::create_dir_all(db.parent().unwrap()).unwrap();
    let store = awman::data::fs::TaskStore::open(&db).unwrap();
    store.migrate().unwrap();
    let now = chrono::Utc::now();
    store
        .create(&awman::data::fs::Task {
            id: uuid::Uuid::new_v4().to_string(),
            name: "issue-triage".into(),
            description: "watch new issues".into(),
            repo_scope: tmp.path().to_path_buf(),
            mount_scope: awman::data::fs::MountScope::GitRoot,
            overlays: Vec::new(),
            interval_secs: 60,
            status: awman::data::fs::TaskStatus::Active,
            agent: None,
            model: None,
            backoff_until: None,
            created_at: now,
            updated_at: now,
            last_run_at: None,
            trigger_requested_at: None,
            last_run_status: None,
            unmet_env: Vec::new(),
        })
        .unwrap();
    drop(store);

    let (started_tx, mut started_rx) = tokio::sync::mpsc::unbounded_channel();
    let release = Arc::new(tokio::sync::Notify::new());
    let evaluator = Arc::new(WorkflowInFlightEvaluator {
        state_dir: tmp.path().join("state"),
        started: started_tx,
        release: release.clone(),
    });

    let (handle, base) =
        start_daemon_with(tmp.path(), Arc::new(ContainerRuntime::docker()), evaluator).await;

    // Wait for the scheduler's first tick to reach the evaluator.
    tokio::time::timeout(Duration::from_secs(10), started_rx.recv())
        .await
        .expect("the scheduler must dispatch the due task on its first tick")
        .expect("evaluator must signal that the workflow started");

    let resp = reqwest::get(format!("{base}/v1/tasks/issue-triage/workflow"))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        200,
        "the route must serve the live workflow state while the run is in flight"
    );
    let body: serde_json::Value = resp.json().await.unwrap();

    // The contract is the *verbatim* `WorkflowState`, no projection or DTO —
    // a client must be able to deserialize it straight back into the type.
    let round_tripped: awman::data::workflow_state::WorkflowState =
        serde_json::from_value(body.clone())
            .expect("the route must return a WorkflowState verbatim, not a projection");
    assert_eq!(round_tripped.workflow_name, "squad-generated");
    assert_eq!(round_tripped.workflow_hash, "deadbeef");
    assert!(round_tripped.step_states.contains_key("analyze"));

    release.notify_waiters();
    handle.abort();
}

// ═══════════════════════════════════════════════════════════════════════════
// WI 0116 §4 — the daemon-env protocol over HTTP.
//
// Everything below drives the *shipped wire format* (`API-protocol.md` §1)
// against a real daemon booted through the same seam the tests above use. The
// governing property of the whole surface is negative — **no response body and
// no log line ever carries a payload value** — so the assertions are made on
// the serialised JSON text, never on the typed struct, and every value used
// here is a distinctive sentinel that a scan can look for.
// ═══════════════════════════════════════════════════════════════════════════

/// Sentinels chosen so a substring scan over a whole response body (or the
/// whole captured daemon log) cannot match by accident.
const SECRET: &str = "wi0116-secret-must-never-appear";
const ROTATED_SECRET: &str = "wi0116-rotated-must-never-appear";
const STRAY_SECRET: &str = "wi0116-stray-must-never-appear";

/// Everything this test binary's daemons wrote to `tracing`, for the
/// "a rejected request logs no values either" assertion. Installed once,
/// globally, because the daemon under test runs inside this same process and a
/// thread-local subscriber would not see the axum handler's task.
static CAPTURED_LOG: std::sync::Mutex<Vec<u8>> = std::sync::Mutex::new(Vec::new());

#[derive(Clone, Default)]
struct LogSink;

impl std::io::Write for LogSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        CAPTURED_LOG.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogSink {
    type Writer = LogSink;
    fn make_writer(&'a self) -> Self::Writer {
        LogSink
    }
}

/// Install the capturing subscriber for the whole test binary. Idempotent: a
/// second call is a no-op, and a failure (another subscriber already global)
/// leaves the log assertions vacuous rather than failing the run.
fn capture_daemon_logs() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let subscriber = tracing_subscriber::fmt()
            .with_writer(LogSink)
            .with_ansi(false)
            .with_max_level(tracing::Level::TRACE)
            .finish();
        let _ = tracing::subscriber::set_global_default(subscriber);
    });
}

fn captured_log() -> String {
    String::from_utf8_lossy(&CAPTURED_LOG.lock().unwrap()).into_owned()
}

/// Assert that `text` carries none of this module's sentinels. Used on every
/// response body and on the daemon log.
fn assert_carries_no_value(text: &str, what: &str) {
    for secret in [SECRET, ROTATED_SECRET, STRAY_SECRET] {
        assert!(
            !text.contains(secret),
            "{what} must never carry a payload value, but it contains {secret:?}: {text}"
        );
    }
}

/// A squad task seeded straight into the daemon's database before it boots, so
/// its `env()` names are in `required_env` from the daemon's first tick.
fn seed_task(root: &std::path::Path, name: &str, overlays: &[&str]) {
    let db = root.join("data").join("awman.db");
    std::fs::create_dir_all(db.parent().unwrap()).unwrap();
    let store = awman::data::fs::TaskStore::open(&db).unwrap();
    store.migrate().unwrap();
    let now = chrono::Utc::now();
    store
        .create(&awman::data::fs::Task {
            id: uuid::Uuid::new_v4().to_string(),
            name: name.into(),
            description: "seeded for the env protocol tests".into(),
            repo_scope: root.to_path_buf(),
            mount_scope: awman::data::fs::MountScope::GitRoot,
            overlays: overlays.iter().map(|s| s.to_string()).collect(),
            interval_secs: 3600,
            status: awman::data::fs::TaskStatus::Active,
            agent: None,
            model: None,
            backoff_until: None,
            created_at: now,
            updated_at: now,
            last_run_at: None,
            trigger_requested_at: None,
            last_run_status: None,
            unmet_env: Vec::new(),
        })
        .unwrap();
}

/// Start the daemon with bearer auth **on**, returning the plaintext key.
/// `AuthMode::resolve_for_daemon` demands a hash already on disk, so one is
/// written before the bootstrap — exactly what `squad start` does for itself.
async fn start_daemon_with_auth(
    root: &std::path::Path,
) -> (tokio::task::JoinHandle<()>, String, String) {
    let key = awman::engine::auth::ApiKey::from_string(
        "d0d0cafe00000000000000000000000000000000000000000000000000000000",
    );
    let auth = awman::engine::auth::AuthEngine::with_paths(
        AuthPathResolver::at_home(root),
        ApiPaths::from_root(root),
    );
    let hash = auth.hash_api_key(&key);
    let squad_paths = SquadPaths::from_root(root.join("squad"));
    std::fs::create_dir_all(squad_paths.daemon().root()).ok();
    squad_paths
        .daemon()
        .write_key_hash(hash.as_str())
        .expect("the daemon's key hash must be writable");

    let engines = engines_with(Arc::new(ContainerRuntime::docker()), root);
    let config = SquadServeConfig {
        port: 0,
        dangerously_skip_auth: false,
    };
    let previous = std::env::var("AWMAN_CONFIG_HOME").ok();
    std::env::set_var("AWMAN_CONFIG_HOME", root);
    let handle = tokio::spawn(async move {
        let handles =
            SquadDaemonHandles::bootstrap(config, engines, Arc::new(NeverTriggeredEvaluator))
                .await
                .expect("squad daemon bootstrap");
        let _ = awman::frontend::squad::serve(handles).await;
    });
    let base = wait_for_endpoint(root).await;
    match previous {
        Some(v) => std::env::set_var("AWMAN_CONFIG_HOME", v),
        None => std::env::remove_var("AWMAN_CONFIG_HOME"),
    }
    (handle, base, key.as_str().to_string())
}

/// Poll the daemon's endpoint sidecar until it publishes one.
async fn wait_for_endpoint(root: &std::path::Path) -> String {
    let daemon = DaemonProcess::new(
        SquadPaths::from_root(root.join("squad")).daemon(),
        SQUAD_UNIT_NAME,
        SQUAD_PLIST_LABEL,
    );
    let mut waited = Duration::ZERO;
    loop {
        if let Ok(Some(meta)) = daemon.read_meta() {
            return format!("{}://{}:{}", meta.scheme, meta.bind_ip, meta.port);
        }
        if waited >= Duration::from_secs(10) {
            panic!("squad daemon never published its server metadata");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        waited += Duration::from_millis(50);
    }
}

/// One request, returning the status and the **raw body text** — the shape
/// every "no value in the body" assertion needs.
async fn request(
    method: reqwest::Method,
    url: &str,
    key: Option<&str>,
    body: Option<serde_json::Value>,
) -> (u16, String) {
    let mut builder = reqwest::Client::new().request(method, url);
    if let Some(key) = key {
        builder = builder.header("authorization", format!("Bearer {key}"));
    }
    if let Some(body) = body {
        builder = builder.json(&body);
    }
    let response = builder.send().await.expect("request must reach the daemon");
    let status = response.status().as_u16();
    (status, response.text().await.unwrap_or_default())
}

async fn env_get(base: &str, key: Option<&str>) -> serde_json::Value {
    let (status, text) = request(
        reqwest::Method::GET,
        &format!("{base}/v1/daemon/env"),
        key,
        None,
    )
    .await;
    assert_eq!(status, 200, "GET /v1/daemon/env: {text}");
    assert_carries_no_value(&text, "GET /v1/daemon/env");
    serde_json::from_str(&text).expect("coverage must be JSON")
}

async fn env_push(
    base: &str,
    key: Option<&str>,
    body: serde_json::Value,
) -> (u16, String, serde_json::Value) {
    let (status, text) = request(
        reqwest::Method::POST,
        &format!("{base}/v1/daemon/env"),
        key,
        Some(body),
    )
    .await;
    let json = serde_json::from_str(&text).unwrap_or(serde_json::Value::Null);
    (status, text, json)
}

/// The coverage entry for `name`, or `None` when the daemon no longer requires
/// it at all.
fn entry<'a>(coverage: &'a serde_json::Value, name: &str) -> Option<&'a serde_json::Value> {
    coverage["required"]
        .as_array()
        .expect("`required` is an array")
        .iter()
        .find(|entry| entry["name"] == name)
}

// ─── §4: auth, the accepted/ignored split, and the negative property ────────

/// The route sits behind the same `auth_middleware` as every other squad
/// route: a push with no bearer key is refused before it reaches the handler,
/// so a value cannot be planted in a daemon by anything that merely found the
/// port.
#[tokio::test]
async fn daemon_env_push_requires_the_bearer_key() {
    let _env_guard = ENV_LOCK.lock().await;
    capture_daemon_logs();
    let tmp = tempfile::tempdir().unwrap();
    seed_task(tmp.path(), "nightly", &["env(AUTH_TOKEN)"]);
    let (handle, base, key) = start_daemon_with_auth(tmp.path()).await;

    let (status, text, _) = env_push(
        &base,
        None,
        serde_json::json!({"vars": {"AUTH_TOKEN": SECRET}}),
    )
    .await;
    assert_eq!(
        status, 401,
        "an unauthenticated push must be refused: {text}"
    );
    assert_carries_no_value(&text, "the 401 body");

    let (status, text, _) = env_push(
        &base,
        Some("not-the-key"),
        serde_json::json!({"vars": {"AUTH_TOKEN": SECRET}}),
    )
    .await;
    assert_eq!(status, 401, "a wrong key must be refused: {text}");

    // GET and DELETE are behind the same middleware.
    for method in [reqwest::Method::GET, reqwest::Method::DELETE] {
        let (status, _) =
            request(method.clone(), &format!("{base}/v1/daemon/env"), None, None).await;
        assert_eq!(status, 401, "{method} /v1/daemon/env must require the key");
    }

    // The same request with the key succeeds, so the 401s above are auth and
    // not a broken route.
    let (status, text, body) = env_push(
        &base,
        Some(&key),
        serde_json::json!({"vars": {"AUTH_TOKEN": SECRET}}),
    )
    .await;
    assert_eq!(status, 200, "an authenticated push must succeed: {text}");
    assert_eq!(body["accepted"], serde_json::json!(["AUTH_TOKEN"]));

    handle.abort();
}

/// A push reports the names it took and the names it refused — and nothing
/// else. `STRAY` is outside `required_env`, so it is `ignored` rather than
/// stored: a daemon never accumulates values it has no declared use for.
#[tokio::test]
async fn daemon_env_push_reports_accepted_names_only_and_its_body_carries_no_value() {
    let _env_guard = ENV_LOCK.lock().await;
    capture_daemon_logs();
    let tmp = tempfile::tempdir().unwrap();
    seed_task(tmp.path(), "nightly", &["env(ACCEPTED_TOKEN)"]);
    let (handle, base) = start_daemon(tmp.path(), Arc::new(ContainerRuntime::docker())).await;

    let (status, text, body) = env_push(
        &base,
        None,
        serde_json::json!({"vars": {"ACCEPTED_TOKEN": SECRET, "STRAY": STRAY_SECRET}}),
    )
    .await;
    assert_eq!(status, 200, "{text}");
    assert_eq!(body["accepted"], serde_json::json!(["ACCEPTED_TOKEN"]));
    assert_eq!(
        body["ignored"],
        serde_json::json!(["STRAY"]),
        "a name outside required_env is reported, never stored"
    );
    // The whole point of the route: assert on the serialised JSON, because a
    // typed struct with no value field proves nothing about what the
    // *serializer* emitted.
    assert_carries_no_value(&text, "the POST /v1/daemon/env body");

    // The embedded coverage reports the name and a digest, and the digest is
    // not the value: 16 hex chars, whatever the value's length.
    let digest = body["coverage"]["required"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["name"] == "ACCEPTED_TOKEN")
        .and_then(|e| e["digest"].as_str())
        .expect("an accepted name reports a digest");
    assert_eq!(digest.len(), 16, "the digest is 16 hex chars: {digest}");
    assert!(digest.chars().all(|c| c.is_ascii_hexdigit()));

    handle.abort();
}

/// A body that will not deserialise is answered with a **fixed** string. axum's
/// own `JsonDataError` text quotes the offending input, which for this one
/// route is the payload — so neither the body nor a serde message may reach the
/// response *or the log*.
#[tokio::test]
async fn a_malformed_daemon_env_push_echoes_neither_the_body_nor_a_serde_message() {
    let _env_guard = ENV_LOCK.lock().await;
    capture_daemon_logs();
    let tmp = tempfile::tempdir().unwrap();
    seed_task(tmp.path(), "nightly", &["env(MALFORMED_TOKEN)"]);
    let (handle, base) = start_daemon(tmp.path(), Arc::new(ContainerRuntime::docker())).await;

    // A non-string value: serde rejects it, and the default rejection text
    // would quote `{"vars":{"MALFORMED_TOKEN":"<the value>"}}`.
    let (status, text, body) = env_push(
        &base,
        None,
        serde_json::json!({"vars": {"MALFORMED_TOKEN": {"nested": SECRET}}}),
    )
    .await;
    assert_eq!(status, 400, "{text}");
    assert_eq!(body["error"], "malformed daemon env request");
    assert_carries_no_value(&text, "the 400 body");
    assert!(
        !text.contains("MALFORMED_TOKEN"),
        "not even the name may be echoed back from a rejected body: {text}"
    );

    // …and the daemon's own log — which is what `awman squad logs` prints —
    // carries no part of it either.
    let log = captured_log();
    assert_carries_no_value(&log, "the daemon log");
    assert!(
        log.contains("rejected a malformed daemon env push"),
        "the rejection is still recorded, as a fixed line with no fields: {log}"
    );

    handle.abort();
}

/// **The regression this work item's merge semantics exist to prevent.**
///
/// Running any squad command from a shell that does not export the tokens
/// reports every one of them `absent`. If that push were a whole-map replace —
/// or if `absent` meant "delete" — every scheduled task would be silently
/// disarmed by someone opening a terminal. It must be a strict no-op: the
/// overlay, the digest, `last_provided_at` and `unmet_since` all survive
/// untouched.
///
/// A whole-map-replace implementation fails this test on the very first
/// assertion after the all-absent push.
#[tokio::test]
async fn an_all_absent_push_leaves_every_existing_value_in_place() {
    let _env_guard = ENV_LOCK.lock().await;
    capture_daemon_logs();
    let tmp = tempfile::tempdir().unwrap();
    seed_task(
        tmp.path(),
        "nightly",
        &["env(ABSENT_ONE)", "env(ABSENT_TWO)"],
    );
    let (handle, base) = start_daemon(tmp.path(), Arc::new(ContainerRuntime::docker())).await;

    // An armed daemon: two names, both provided.
    let (status, _, _) = env_push(
        &base,
        None,
        serde_json::json!({"vars": {"ABSENT_ONE": SECRET, "ABSENT_TWO": SECRET}}),
    )
    .await;
    assert_eq!(status, 200);
    let armed = env_get(&base, None).await;
    let deploy_before = entry(&armed, "ABSENT_ONE").unwrap().clone();
    let npm_before = entry(&armed, "ABSENT_TWO").unwrap().clone();
    assert!(deploy_before["digest"].is_string(), "{armed}");

    // The under-equipped shell: it has neither, and says so.
    let (status, text, body) = env_push(
        &base,
        None,
        serde_json::json!({"vars": {}, "absent": ["ABSENT_ONE", "ABSENT_TWO"]}),
    )
    .await;
    assert_eq!(status, 200, "{text}");
    assert_eq!(
        body["accepted"],
        serde_json::json!([]),
        "an all-absent push accepts nothing"
    );
    assert_eq!(body["ignored"], serde_json::json!([]));

    let after = env_get(&base, None).await;
    assert_eq!(
        entry(&after, "ABSENT_ONE"),
        Some(&deploy_before),
        "an `absent` report must not disturb a held value — digest, source, \
         last_provided_at and unmet_since all unchanged"
    );
    assert_eq!(entry(&after, "ABSENT_TWO"), Some(&npm_before));
    assert_eq!(
        after["required"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["source"] == "pushed")
            .count(),
        2,
        "both names are still held by the daemon: {after}"
    );

    handle.abort();
}

/// Rotation works, an `absent` never removes, and the daemon's own answer says
/// which of the two happened.
#[tokio::test]
async fn a_provided_value_replaces_an_existing_one_and_an_absent_never_removes_it() {
    let _env_guard = ENV_LOCK.lock().await;
    capture_daemon_logs();
    let tmp = tempfile::tempdir().unwrap();
    seed_task(tmp.path(), "nightly", &["env(ROTATE_TOKEN)"]);
    let (handle, base) = start_daemon(tmp.path(), Arc::new(ContainerRuntime::docker())).await;

    env_push(
        &base,
        None,
        serde_json::json!({"vars": {"ROTATE_TOKEN": SECRET}}),
    )
    .await;
    let first = env_get(&base, None).await;
    let first_digest = entry(&first, "ROTATE_TOKEN").unwrap()["digest"]
        .as_str()
        .unwrap()
        .to_string();

    // Rotation: a different value for a name the daemon already holds.
    let (_, text, body) = env_push(
        &base,
        None,
        serde_json::json!({"vars": {"ROTATE_TOKEN": ROTATED_SECRET}}),
    )
    .await;
    assert_eq!(body["accepted"], serde_json::json!(["ROTATE_TOKEN"]));
    assert_carries_no_value(&text, "the rotation response");
    let rotated = env_get(&base, None).await;
    let rotated_digest = entry(&rotated, "ROTATE_TOKEN").unwrap()["digest"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(
        rotated_digest, first_digest,
        "a rotated value must replace what the daemon held"
    );

    // An `absent` for a name the daemon holds is not a deletion.
    env_push(
        &base,
        None,
        serde_json::json!({"vars": {}, "absent": ["ROTATE_TOKEN"]}),
    )
    .await;
    let after_absent = env_get(&base, None).await;
    assert_eq!(
        entry(&after_absent, "ROTATE_TOKEN").unwrap()["digest"]
            .as_str()
            .unwrap(),
        rotated_digest,
        "an `absent` report never wins over a provision"
    );

    handle.abort();
}

/// The **only** removal: the last task naming a variable goes away, so nothing
/// declares it any more. It leaves `required_env`, and its value leaves with
/// it — re-declaring the name reports it uncovered rather than silently
/// re-attaching the old value.
#[tokio::test]
async fn a_name_leaving_required_env_takes_its_value_with_it() {
    let _env_guard = ENV_LOCK.lock().await;
    capture_daemon_logs();
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    seed_task(tmp.path(), "nightly", &["env(GC_TOKEN)"]);
    let (handle, base) = start_daemon(tmp.path(), Arc::new(ContainerRuntime::docker())).await;

    env_push(
        &base,
        None,
        serde_json::json!({"vars": {"GC_TOKEN": SECRET}}),
    )
    .await;
    assert!(entry(&env_get(&base, None).await, "GC_TOKEN").is_some());

    // Drop the only task that declares it.
    let (status, _) = request(
        reqwest::Method::POST,
        &format!("{base}/v1/commands"),
        None,
        Some(serde_json::json!({"subcommand": "squad remove", "args": ["nightly"]})),
    )
    .await;
    assert_eq!(status, 200);

    let after = env_get(&base, None).await;
    assert!(
        entry(&after, "GC_TOKEN").is_none(),
        "a name nothing declares any more leaves required_env: {after}"
    );

    // Re-declaring it must find nothing held: the value was garbage-collected
    // with the name, not merely hidden from the report.
    let (status, text) = request(
        reqwest::Method::POST,
        &format!("{base}/v1/commands"),
        None,
        Some(serde_json::json!({
            "subcommand": "squad add",
            "args": [
                "--name", "nightly-again",
                "--description", "re-declares the same variable",
                "--repo", repo.to_str().unwrap(),
                "--interval", "3600",
                "--mount-scope", "gitroot",
                "--overlay", "env(GC_TOKEN)",
            ],
        })),
    )
    .await;
    assert_eq!(status, 200, "{text}");
    let readded = env_get(&base, None).await;
    let entry = entry(&readded, "GC_TOKEN").expect("the name is required again");
    assert!(
        entry["digest"].is_null() && entry["source"].is_null(),
        "the value left with the name and did not come back: {entry}"
    );

    handle.abort();
}

/// An exported-but-empty variable is not a provision. The rule matches the
/// set-and-non-empty test `src/engine/container/options.rs` already applies to
/// a declared `env()`, so `FOO=` behaves identically on both sides of the
/// socket — and it may not overwrite a value the daemon already holds.
#[tokio::test]
async fn an_empty_string_value_is_treated_exactly_as_absent() {
    let _env_guard = ENV_LOCK.lock().await;
    capture_daemon_logs();
    let tmp = tempfile::tempdir().unwrap();
    seed_task(tmp.path(), "nightly", &["env(EMPTY_TOKEN)"]);
    let (handle, base) = start_daemon(tmp.path(), Arc::new(ContainerRuntime::docker())).await;

    // On an empty daemon: neither accepted nor ignored, and nothing is held.
    let (status, text, body) = env_push(
        &base,
        None,
        serde_json::json!({"vars": {"EMPTY_TOKEN": ""}}),
    )
    .await;
    assert_eq!(status, 200, "{text}");
    assert_eq!(body["accepted"], serde_json::json!([]));
    assert_eq!(
        body["ignored"],
        serde_json::json!([]),
        "an empty value is absent, not an unrequired name"
    );
    let empty = env_get(&base, None).await;
    assert!(entry(&empty, "EMPTY_TOKEN").unwrap()["digest"].is_null());

    // On an armed daemon: the held value survives.
    env_push(
        &base,
        None,
        serde_json::json!({"vars": {"EMPTY_TOKEN": SECRET}}),
    )
    .await;
    let armed = entry(&env_get(&base, None).await, "EMPTY_TOKEN")
        .unwrap()
        .clone();
    env_push(
        &base,
        None,
        serde_json::json!({"vars": {"EMPTY_TOKEN": ""}}),
    )
    .await;
    assert_eq!(
        entry(&env_get(&base, None).await, "EMPTY_TOKEN"),
        Some(&armed),
        "an empty value must not disarm a name the daemon holds"
    );

    handle.abort();
}

/// The negative property across the *whole* surface a push touches, asserted on
/// raw response text: coverage, push, clear, status, and the command envelope
/// that returns tasks.
#[tokio::test]
async fn no_endpoint_the_env_protocol_touches_returns_a_payload_value() {
    let _env_guard = ENV_LOCK.lock().await;
    capture_daemon_logs();
    let tmp = tempfile::tempdir().unwrap();
    seed_task(tmp.path(), "nightly", &["env(SCAN_TOKEN)"]);
    let (handle, base) = start_daemon(tmp.path(), Arc::new(ContainerRuntime::docker())).await;

    env_push(
        &base,
        None,
        serde_json::json!({"vars": {"SCAN_TOKEN": SECRET, "STRAY": STRAY_SECRET}}),
    )
    .await;

    let mut bodies: Vec<(String, String)> = Vec::new();
    for (method, path) in [
        (reqwest::Method::GET, "/v1/daemon/env"),
        (reqwest::Method::GET, "/v1/status"),
        (reqwest::Method::DELETE, "/v1/daemon/env"),
    ] {
        let (status, text) = request(method.clone(), &format!("{base}{path}"), None, None).await;
        assert_eq!(status, 200, "{method} {path}: {text}");
        bodies.push((format!("{method} {path}"), text));
    }
    for subcommand in ["squad list", "squad status"] {
        let (_, text) = request(
            reqwest::Method::POST,
            &format!("{base}/v1/commands"),
            None,
            Some(serde_json::json!({"subcommand": subcommand, "args": []})),
        )
        .await;
        bodies.push((subcommand.to_string(), text));
    }
    let (_, text) = request(
        reqwest::Method::POST,
        &format!("{base}/v1/commands"),
        None,
        Some(serde_json::json!({"subcommand": "squad show", "args": ["nightly"]})),
    )
    .await;
    bodies.push(("squad show".into(), text));

    for (what, text) in &bodies {
        assert_carries_no_value(text, what);
    }
    // And the daemon's log, which `awman squad logs` prints verbatim.
    assert_carries_no_value(&captured_log(), "the daemon log");

    handle.abort();
}

/// `DELETE` removes what is *stored*, and says so honestly: `false` when there
/// is no keychain behind this daemon. It must never disarm a daemon that is
/// running fine, so the in-memory overlay is untouched.
#[tokio::test]
async fn clearing_the_store_reports_honestly_and_leaves_the_running_overlay_intact() {
    let _env_guard = ENV_LOCK.lock().await;
    capture_daemon_logs();
    let tmp = tempfile::tempdir().unwrap();
    write_squad_config(tmp.path(), "none");
    seed_task(tmp.path(), "nightly", &["env(CLEAR_TOKEN)"]);
    let (handle, base) = start_daemon(tmp.path(), Arc::new(ContainerRuntime::docker())).await;

    env_push(
        &base,
        None,
        serde_json::json!({"vars": {"CLEAR_TOKEN": SECRET}}),
    )
    .await;
    let armed = entry(&env_get(&base, None).await, "CLEAR_TOKEN")
        .unwrap()
        .clone();

    let (status, text) = request(
        reqwest::Method::DELETE,
        &format!("{base}/v1/daemon/env"),
        None,
        None,
    )
    .await;
    assert_eq!(status, 200, "{text}");
    let body: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(
        body["cleared"], false,
        "there is no keychain item to remove on a daemon that persists nothing"
    );

    assert_eq!(
        entry(&env_get(&base, None).await, "CLEAR_TOKEN"),
        Some(&armed),
        "clearing the store must not disarm the running daemon"
    );

    handle.abort();
}

/// Write a global config carrying `squad.envPersistence`.
fn write_squad_config(root: &std::path::Path, persistence: &str) {
    let path = root.join("config.json");
    std::fs::create_dir_all(root).unwrap();
    std::fs::write(
        &path,
        serde_json::to_string_pretty(&serde_json::json!({
            "squad": { "envPersistence": persistence }
        }))
        .unwrap(),
    )
    .unwrap();
}

/// §5a on the wire: `GET /v1/status` reports the daemon's persistence state as
/// a plain string. `"none"` is the explicit opt-out, and it is what a daemon
/// configured that way must say — never a guess and never an empty string.
#[tokio::test]
async fn status_reports_env_persistence_none_for_the_explicit_opt_out() {
    let _env_guard = ENV_LOCK.lock().await;
    capture_daemon_logs();
    let tmp = tempfile::tempdir().unwrap();
    write_squad_config(tmp.path(), "none");
    seed_task(tmp.path(), "nightly", &["env(STATUS_TOKEN)"]);
    let (handle, base) = start_daemon(tmp.path(), Arc::new(ContainerRuntime::docker())).await;

    let (status, text) = request(
        reqwest::Method::GET,
        &format!("{base}/v1/status"),
        None,
        None,
    )
    .await;
    assert_eq!(status, 200, "{text}");
    let body: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(body["env_persistence"], "none");
    assert_eq!(
        body["unmet_env"],
        serde_json::json!(["STATUS_TOKEN"]),
        "the daemon-wide unmet list is required, non-optional and uncovered: {body}"
    );
    // GITHUB_TOKEN is required but optional, so it is never in `unmet_env`.
    assert!(!body["unmet_env"]
        .as_array()
        .unwrap()
        .contains(&serde_json::json!("GITHUB_TOKEN")));

    // The same daemon's coverage carries the same string.
    let coverage = env_get(&base, None).await;
    assert_eq!(coverage["persistence"], "none");
    // …and the same name carries the `unmet_since` clock `awman squad env`'s
    // SINCE column renders: the count in the status line and the "since when"
    // in the table are two views of one fact, stamped when the name first
    // became required while uncovered.
    let unmet = entry(&coverage, "STATUS_TOKEN").expect("the name is required");
    assert!(
        unmet["unmet_since"].is_string(),
        "an uncovered required name carries an unmet clock: {unmet}"
    );
    assert_eq!(unmet["source"], serde_json::Value::Null);
    assert_eq!(unmet["required_by"], serde_json::json!(["nightly"]));

    handle.abort();
}

/// The three `env_persistence` strings, at the seam `GET /v1/status`
/// serialises. Driven through `LocalTaskGateway::status` — the function the
/// route calls — because the `keychain` and `unavailable(...)` states depend on
/// what the host machine actually has, and a wire test could only pin whichever
/// one this CI box happens to be in.
#[tokio::test]
async fn the_status_route_reports_all_three_env_persistence_forms() {
    use awman::command::commands::squad::gateway::{LocalTaskGateway, TaskGateway};
    use awman::data::fs::daemon_env::{EnvPersistence, NoStore};
    use awman::engine::squad::env_state::{DaemonEnvState, Salt};

    let _env_guard = ENV_LOCK.lock().await;
    let tmp = tempfile::tempdir().unwrap();
    let store = Arc::new(awman::data::fs::TaskStore::open(&tmp.path().join("squad.db")).unwrap());
    store.migrate().unwrap();

    for (persistence, expected) in [
        (EnvPersistence::Keychain, "keychain"),
        (EnvPersistence::None, "none"),
        (
            EnvPersistence::Unavailable("secret-tool not found".into()),
            "unavailable(secret-tool not found)",
        ),
    ] {
        let env_state = Arc::new(DaemonEnvState::new(
            Box::new(NoStore),
            persistence,
            Salt::random(),
        ));
        let gateway = LocalTaskGateway::new(
            store.clone(),
            engines_with(Arc::new(ContainerRuntime::docker()), tmp.path()),
            Arc::new(std::sync::Mutex::new(
                awman::engine::squad::SchedulerStatus::default(),
            )),
            SquadPaths::from_root(tmp.path().join("squad")),
            env_state,
        );
        let status = gateway.status().await.expect("status must succeed");
        assert_eq!(
            status.env_persistence, expected,
            "the string `GET /v1/status` serialises is the daemon's own Display"
        );
        // And it survives serialisation verbatim — the frontend renders it as
        // written, `<reason>` included.
        let json = serde_json::to_value(&status).unwrap();
        assert_eq!(json["env_persistence"], expected);
    }
}

/// Remediation of review-security F3 / review-adversarial F4.
///
/// `DaemonEnvState`'s store calls are synchronous and capped at five seconds.
/// Issuing one directly from an `async fn` parks a runtime worker for the whole
/// cap whenever the keychain is slow, locked, or has no D-Bus behind it — the
/// exact states the design anticipates — and an authenticated client can drive
/// it once per push. The gateway therefore hands them to `spawn_blocking`, as
/// the daemon bootstrap already does.
///
/// A single-threaded runtime makes the difference observable: if the keychain
/// write ran inline, the one worker would be parked for the whole write and the
/// concurrently spawned task below could not tick at all.
#[tokio::test(flavor = "current_thread")]
async fn a_slow_keychain_write_does_not_park_the_runtime_during_a_push() {
    use awman::command::commands::squad::gateway::{EnvPush, LocalTaskGateway, TaskGateway};
    use awman::data::config::env::DaemonEnvMap;
    use awman::data::error::DataError;
    use awman::data::fs::daemon_env::{DaemonEnvStore, EnvPersistence};
    use awman::engine::squad::env_state::{DaemonEnvState, Salt};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Stands in for a keychain that answers slowly — well short of the five
    /// second cap, so the test stays fast, but long enough that a parked worker
    /// is unambiguous.
    struct SlowStore;
    impl DaemonEnvStore for SlowStore {
        fn backend_name(&self) -> &'static str {
            "keychain"
        }
        fn store(&self, _vars: &DaemonEnvMap) -> Result<(), DataError> {
            std::thread::sleep(std::time::Duration::from_millis(400));
            Ok(())
        }
        fn load(&self) -> Result<Option<DaemonEnvMap>, DataError> {
            Ok(None)
        }
        fn clear(&self) -> Result<(), DataError> {
            std::thread::sleep(std::time::Duration::from_millis(400));
            Ok(())
        }
    }

    let _env_guard = ENV_LOCK.lock().await;
    let tmp = tempfile::tempdir().unwrap();
    let store = Arc::new(awman::data::fs::TaskStore::open(&tmp.path().join("squad.db")).unwrap());
    store.migrate().unwrap();

    let env_state = Arc::new(DaemonEnvState::new(
        Box::new(SlowStore),
        EnvPersistence::Keychain,
        Salt::random(),
    ));
    let gateway = LocalTaskGateway::new(
        store.clone(),
        engines_with(Arc::new(ContainerRuntime::docker()), tmp.path()),
        Arc::new(std::sync::Mutex::new(
            awman::engine::squad::SchedulerStatus::default(),
        )),
        SquadPaths::from_root(tmp.path().join("squad")),
        env_state,
    );

    // A host-side name is always in `required_env`, so the push is accepted and
    // reaches the store without needing a task on disk.
    let ticks = Arc::new(AtomicUsize::new(0));
    let ticker = tokio::spawn({
        let ticks = Arc::clone(&ticks);
        async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                ticks.fetch_add(1, Ordering::SeqCst);
            }
        }
    });

    let response = gateway
        .push_env(EnvPush {
            vars: DaemonEnvMap::from_pairs([("GITHUB_TOKEN", "ghp-not-a-real-token")]),
            absent: Vec::new(),
        })
        .await
        .expect("a push must succeed even when the store is slow");
    assert_eq!(response.accepted, vec!["GITHUB_TOKEN".to_string()]);

    let during_push = ticks.load(Ordering::SeqCst);
    assert!(
        during_push > 5,
        "the runtime was parked for the keychain write: only {during_push} ticks \
         elapsed during a 400ms store call on a single-threaded runtime"
    );

    // `DELETE /v1/daemon/env` never degrades the daemon, so it is the one call a
    // client can re-drive indefinitely. It must be off the runtime too.
    let before_clear = ticks.load(Ordering::SeqCst);
    gateway
        .clear_env_store()
        .await
        .expect("a clear must succeed even when the store is slow");
    assert!(
        ticks.load(Ordering::SeqCst) - before_clear > 5,
        "the runtime was parked for the keychain clear"
    );

    ticker.abort();
    // Leave the process-global overlay as this test found it.
    awman::data::config::env::set_daemon_overlay(DaemonEnvMap::new());
}
