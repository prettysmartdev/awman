//! WI 0101 — E2E: the real `curl` executable drives the full task
//! lifecycle against a running squad daemon with **no `awman` frontend
//! involved** — proving the daemon is complete on its own, exactly the
//! exercise documented in the `daemon-frontend` handoff
//! (`part4-daemon-summary.md`'s curl walkthrough).
//!
//! Every request below is issued by the `curl` binary, not by an in-process
//! HTTP client, so none of awman's own client stack (`HttpCore`,
//! `RemoteTaskGateway`, the CLI) is on the path — only the daemon's
//! loopback socket is.
//!
//! Boots the real daemon via `SquadDaemonHandles::bootstrap` +
//! `frontend::squad::serve` (the same injectable seam `tests/squad_daemon_http.rs` uses) and drives it purely
//! over HTTP: add → list → show → pause → resume → workflow (404, none
//! running yet) → remove.

use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use awman::command::commands::squad::commands::SquadServeConfig;
use awman::command::commands::squad::daemon_runtime::SquadDaemonHandles;
use awman::command::dispatch::Engines;
use awman::data::fs::api_db::SqliteSessionStore;
use awman::data::fs::daemon_process::{DaemonProcess, SQUAD_PLIST_LABEL, SQUAD_UNIT_NAME};
use awman::data::fs::{ApiPaths, AuthPathResolver, SquadPaths};
use awman::data::WorkflowStateStore;
use awman::engine::agent::AgentEngine;
use awman::engine::auth::AuthEngine;
use awman::engine::container::ContainerRuntime;
use awman::engine::git::GitEngine;
use awman::engine::overlay::OverlayEngine;
use awman::engine::squad::{EvaluationOutcome, EvaluationRequest, TaskEvaluator};
use serde_json::Value;
use tokio::sync::Mutex;

static ENV_LOCK: Mutex<()> = Mutex::const_new(());

struct NeverTriggeredEvaluator;
#[async_trait::async_trait]
impl TaskEvaluator for NeverTriggeredEvaluator {
    async fn evaluate(&self, _request: EvaluationRequest) -> EvaluationOutcome {
        EvaluationOutcome::NotTriggered { reason: None }
    }
}

fn tool_available(bin: &str, arg: &str) -> bool {
    Command::new(awman::engine::host_cli::program(bin))
        .arg(arg)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn init_repo(path: &std::path::Path) {
    std::fs::create_dir_all(path).unwrap();
    let status = Command::new("git")
        .args(["init", "-q"])
        .current_dir(path)
        .status()
        .unwrap();
    assert!(status.success(), "git init must succeed");
}

// ─── the curl driver ────────────────────────────────────────────────────────

/// Run one `curl` invocation and return `(status_code, parsed_json_body)`.
/// `-w '\n%{http_code}'` appends the status on its own final line, so the body
/// and the code come back from a single process without a temp file.
fn curl_blocking(args: &[String]) -> (u16, Value) {
    let output = Command::new("curl")
        .args(["-sS", "-w", "\n%{http_code}"])
        .args(args)
        .output()
        .expect("curl must be executable");
    assert!(
        output.status.success(),
        "curl exited {:?}: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    let (body, code) = text
        .rsplit_once('\n')
        .expect("curl -w must append the status code on its own line");
    let status: u16 = code
        .trim()
        .parse()
        .unwrap_or_else(|_| panic!("unparsable curl status {code:?}"));
    let value = if body.trim().is_empty() {
        Value::Null
    } else {
        serde_json::from_str(body)
            .unwrap_or_else(|e| panic!("daemon returned non-JSON body {body:?}: {e}"))
    };
    (status, value)
}

/// `curl` blocks its thread, so every call is pushed onto the blocking pool
/// rather than parking a worker the daemon is also running on.
async fn curl(args: Vec<String>) -> (u16, Value) {
    tokio::task::spawn_blocking(move || curl_blocking(&args))
        .await
        .expect("curl task must not panic")
}

async fn curl_get(url: String) -> (u16, Value) {
    curl(vec![url]).await
}

async fn curl_command(base: &str, subcommand: &str, args: &[&str]) -> (u16, Value) {
    let payload = serde_json::json!({ "subcommand": subcommand, "args": args }).to_string();
    curl(vec![
        "-X".into(),
        "POST".into(),
        "-H".into(),
        "Content-Type: application/json".into(),
        "-d".into(),
        payload,
        format!("{base}/v1/commands"),
    ])
    .await
}

async fn start_daemon(root: &std::path::Path) -> (tokio::task::JoinHandle<()>, String) {
    let api_paths = ApiPaths::from_root(root);
    let auth_paths = AuthPathResolver::at_home(root);
    let overlay_engine = Arc::new(OverlayEngine::with_auth_resolver(auth_paths.clone()));
    let container_runtime = Arc::new(ContainerRuntime::docker());
    let engines = Engines {
        runtime: container_runtime.clone(),
        container_runtime: Some(container_runtime.clone()),
        sandbox_runtime: None,
        git_engine: Arc::new(GitEngine::new()),
        overlay_engine: overlay_engine.clone(),
        auth_engine: Arc::new(AuthEngine::with_paths(auth_paths, api_paths.clone())),
        agent_engine: Arc::new(AgentEngine::new(overlay_engine, container_runtime)),
        workflow_state_store: Arc::new(WorkflowStateStore::at_git_root(api_paths.root())),
        credential_monitor: None,
        global_config: std::sync::Arc::new(Default::default()),
    };
    let config = SquadServeConfig {
        port: 0,
        dangerously_skip_auth: true,
    };

    // The stored keychain item is a single fixed `awman-squad`/`daemon-env`
    // pair, not keyed by storage root, so an isolated `AWMAN_CONFIG_HOME` does
    // not isolate it. Opting out with `envPersistence: "none"` is no answer
    // either: opting out *clears* the item. The daemon runs in this process,
    // so isolate the whole process (in-memory keychain, no launchd/systemd),
    // and leave it isolated — a later persist must not reach the real one.
    std::fs::create_dir_all(root).unwrap();
    std::env::set_var("AWMAN_TEST_ISOLATION", "1");

    let previous = std::env::var("AWMAN_CONFIG_HOME").ok();
    std::env::set_var("AWMAN_CONFIG_HOME", root);

    // WI 0113 F-02: Layer 2 bootstraps the daemon, Layer 3 serves it. This is
    // the same seam `serve_with` was, split across the layer boundary.
    let handle = tokio::spawn(async move {
        let handles =
            SquadDaemonHandles::bootstrap(config, engines, Arc::new(NeverTriggeredEvaluator))
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn curl_drives_the_full_task_lifecycle_with_no_awman_frontend() {
    if !tool_available("git", "--version") {
        eprintln!("SKIP: git not available");
        return;
    }
    if !tool_available("curl", "--version") {
        eprintln!("SKIP: curl not available");
        return;
    }

    let _env_guard = ENV_LOCK.lock().await;
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    init_repo(&repo);
    let (handle, base) = start_daemon(tmp.path()).await;

    // status — daemon is up, no tasks yet.
    let (code, status) = curl_get(format!("{base}/v1/status")).await;
    assert_eq!(code, 200);
    assert_eq!(status["running"], true);
    assert_eq!(status["task_count"], 0);

    // add
    let repo_str = repo.to_str().unwrap();
    let (code, created) = curl_command(
        &base,
        "squad add",
        &[
            "--name",
            "issue-triage",
            "--description",
            "watch new issues",
            "--repo",
            repo_str,
            "--interval",
            "60",
            "--mount-scope",
            "gitroot",
        ],
    )
    .await;
    assert_eq!(code, 200, "squad add must succeed: {created}");
    assert_eq!(created["name"], "issue-triage");
    assert_eq!(created["status"], "active");

    // list
    let (code, list) = curl_command(&base, "squad list", &[]).await;
    assert_eq!(code, 200);
    assert_eq!(list.as_array().unwrap().len(), 1);

    // show — one response shape carrying the task *and* its run history,
    // which is what both gateway implementations deserialize.
    let (code, shown) = curl_command(&base, "squad show", &["issue-triage"]).await;
    assert_eq!(code, 200);
    assert_eq!(shown["task"]["name"], "issue-triage");
    assert_eq!(
        shown["runs"].as_array().map(Vec::len),
        Some(0),
        "a never-evaluated task has an empty (not absent) run history: {shown}"
    );

    // pause
    let (code, _) = curl_command(&base, "squad pause", &["issue-triage"]).await;
    assert_eq!(code, 200);
    let (_, shown) = curl_command(&base, "squad show", &["issue-triage"]).await;
    assert_eq!(shown["task"]["status"], "paused");

    // resume
    let (code, _) = curl_command(&base, "squad resume", &["issue-triage"]).await;
    assert_eq!(code, 200);
    let (_, shown) = curl_command(&base, "squad show", &["issue-triage"]).await;
    assert_eq!(shown["task"]["status"], "active");

    // workflow — 404 until a running evaluation persists a workflow state path.
    let (code, _) = curl_get(format!("{base}/v1/tasks/issue-triage/workflow")).await;
    assert_eq!(code, 404);

    // an unknown task is a 404, not a 500 or an empty 200.
    let (code, _) = curl_get(format!("{base}/v1/tasks/no-such-task/workflow")).await;
    assert_eq!(code, 404);

    // status reflects the one active task.
    let (_, status) = curl_get(format!("{base}/v1/status")).await;
    assert_eq!(status["task_count"], 1);
    assert_eq!(status["active_count"], 1);

    // remove
    let (code, _) = curl_command(&base, "squad remove", &["issue-triage"]).await;
    assert_eq!(code, 200);
    let (_, list) = curl_command(&base, "squad list", &[]).await;
    assert_eq!(list.as_array().unwrap().len(), 0);

    handle.abort();
}

/// WI 0101 — E2E: a **pre-migration install** (a legacy `<api_root>/awman.db`
/// holding real session and command rows) starts the squad daemon, gains the
/// squad tables, and retains every prior API-mode row.
///
/// This is the upgrade path an existing user actually takes: they have been
/// running `awman api`, they upgrade, and the first thing that touches the
/// database is the squad daemon. Nothing may be lost, and the legacy original
/// must survive under its `.pre-migration` name for `awman clean` to remove.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pre_migration_install_starts_squad_gains_its_tables_and_keeps_api_rows() {
    if !tool_available("git", "--version") || !tool_available("curl", "--version") {
        eprintln!("SKIP: git and curl are both required");
        return;
    }

    let _env_guard = ENV_LOCK.lock().await;
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    init_repo(&repo);

    // ── the pre-upgrade world: an API-mode database at the legacy location ──
    let legacy_root = tmp.path().join("api");
    std::fs::create_dir_all(&legacy_root).unwrap();
    let legacy = SqliteSessionStore::open(&legacy_root).unwrap();
    legacy
        .insert_session(
            "pre-upgrade-session",
            "/pre/upgrade/repo",
            "2026-08-01T00:00:00Z",
        )
        .unwrap();
    legacy
        .insert_command(
            "pre-upgrade-command",
            "pre-upgrade-session",
            "status",
            "[\"--json\"]",
            "/pre/upgrade/output.log",
        )
        .unwrap();
    let expected_session = legacy.get_session("pre-upgrade-session").unwrap();
    let expected_command = legacy.get_command("pre-upgrade-command").unwrap();
    drop(legacy);
    assert!(legacy_root.join("awman.db").exists());

    // ── upgrade: the squad daemon is the first process to open the database ──
    let (handle, base) = start_daemon(tmp.path()).await;

    // The database moved, and the original is retained under its backup name
    // (which `awman clean` is the one thing allowed to remove).
    let data_db = tmp.path().join("data").join("awman.db");
    assert!(
        data_db.exists(),
        "the database must relocate to <data_home>/data"
    );
    assert!(
        !legacy_root.join("awman.db").exists(),
        "the legacy original must be renamed aside, not left in place"
    );
    assert!(
        legacy_root.join("awman.db.pre-migration").exists(),
        "the one-release safety net must be retained"
    );

    // ── the prior API-mode rows are intact at the new location ──
    let migrated = SqliteSessionStore::open_at(&data_db).unwrap();
    assert_eq!(
        migrated.get_session("pre-upgrade-session").unwrap(),
        expected_session,
        "an upgrade must not lose a pre-existing session row"
    );
    assert_eq!(
        migrated.get_command("pre-upgrade-command").unwrap(),
        expected_command,
        "an upgrade must not lose a pre-existing command row"
    );
    drop(migrated);

    // ── and the same file now carries squad's tables, driven over HTTP ──
    let (code, created) = curl_command(
        &base,
        "squad add",
        &[
            "--name",
            "post-upgrade",
            "--description",
            "created after the upgrade",
            "--repo",
            repo.to_str().unwrap(),
            "--interval",
            "300",
            "--mount-scope",
            "gitroot",
        ],
    )
    .await;
    assert_eq!(
        code, 200,
        "squad add must succeed on a migrated database: {created}"
    );
    let (_, list) = curl_command(&base, "squad list", &[]).await;
    assert_eq!(list.as_array().unwrap().len(), 1);

    // The squad rows landed in the shared database, not a second file.
    let conn = rusqlite::Connection::open(&data_db).unwrap();
    let tasks: i64 = conn
        .query_row("SELECT COUNT(*) FROM squad_tasks", [], |row| row.get(0))
        .expect("the shared database must carry squad_tasks after startup");
    assert_eq!(tasks, 1);
    let sessions: i64 = conn
        .query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0))
        .expect("the shared database must still carry the API-mode tables");
    assert_eq!(
        sessions, 1,
        "both daemons' tables live in one database file"
    );

    handle.abort();
}

// ═══════════════════════════════════════════════════════════════════════════
// WI 0116 — the regression the whole work item exists for.
// ═══════════════════════════════════════════════════════════════════════════

use std::sync::Mutex as StdMutex;

use awman::command::commands::squad::env_sync;
use awman::command::commands::squad::gateway::{RemoteTaskGateway, TaskGateway};
use awman::command::commands::HttpCore;
use awman::data::fs::daemon_env::env_overlay_names;
use awman::data::message::{UserMessage, UserMessageSink};
use awman::engine::agent_runtime::frontend::{AgentFrontend, AgentIo, AgentProgress, AgentStatus};
use awman::engine::container::options::ResolvedContainerOptions;
use awman::engine::container::options::{
    ContainerName, ContainerOption, Entrypoint, EnvVar, ImageRef,
};

/// The variable the E2E task declares. Distinctive enough that a scan of the
/// container's output cannot match by accident.
const E2E_ENV_NAME: &str = "AWMAN_SQUAD_E2E_TOKEN";
const E2E_ENV_VALUE: &str = "wi0116-value-that-must-reach-the-container";

/// Captures the container's stdout. The non-interactive (piped, no PTY) spawn
/// path streams it here, which is how the test reads what `printenv` printed
/// *inside* the container.
#[derive(Clone, Default)]
struct CapturedStdout(Arc<StdMutex<Vec<u8>>>);

impl CapturedStdout {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

struct CapturingFrontend {
    captured: CapturedStdout,
}

impl UserMessageSink for CapturingFrontend {
    fn write_message(&mut self, _msg: UserMessage) {}
    fn replay_queued(&mut self) {}
}

impl AgentFrontend for CapturingFrontend {
    fn report_status(&mut self, _status: AgentStatus) {}
    fn report_progress(&mut self, _progress: AgentProgress) {}

    fn take_io(&mut self) -> AgentIo {
        let (stdout, mut stdout_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let (stderr, mut stderr_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let (stdin_tx, stdin_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();

        let buffer = self.captured.0.clone();
        tokio::spawn(async move {
            while let Some(chunk) = stdout_rx.recv().await {
                buffer.lock().unwrap().extend_from_slice(&chunk);
            }
        });
        tokio::spawn(async move { while stderr_rx.recv().await.is_some() {} });

        AgentIo {
            stdout,
            stderr,
            stdin_tx,
            stdin_rx,
            // Non-interactive: piped stdio, no PTY.
            resize: None,
            initial_size: None,
        }
    }

    fn grace_timeout(&self) -> Duration {
        Duration::from_secs(15)
    }
    fn stuck_timeout(&self) -> Duration {
        Duration::from_secs(30)
    }
}

/// **E2E: a squad task's `env(VAR)` value reaches its container.**
///
/// This is the regression the whole of WI 0116 exists for, and the shape of the
/// test is the shape of the bug:
///
/// 1. A squad task declaring `env(AWMAN_SQUAD_E2E_TOKEN)` is created against a
///    live daemon.
/// 2. A shell that **has** the variable exported runs the ordinary client sync
///    (`env_sync::sync_env`, the exact call `SquadGatewayResolver` makes before
///    every command). The daemon takes the value into its overlay.
/// 3. The variable is then removed from the process environment, so the value
///    lives **only** in the daemon's in-memory overlay — precisely the state a
///    real background daemon is in, where `std::env::var` cannot see it and
///    `data::config::env::host_var` can.
/// 4. The task's declared overlay is resolved into container options through
///    the production path and a container is launched. It must print the value.
///
/// **This test must fail against `main`, and it must fail against any tree
/// whose `-e` gate still reads `std::env::var`** — the container is launched
/// with no `-e NAME` at all in that case and `printenv` finds nothing. It is
/// the one assertion that distinguishes "the daemon knows the value" (which
/// every other test in this work item covers) from "the container gets it"
/// (which is what a user asked for).
///
/// Gated on Docker exactly like the rest of this tree's live-backend tests, and
/// named with `docker` so `make test-fast`'s `--skip docker` filter excludes it;
/// `make test-full` includes it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn docker_a_squad_task_env_overlay_value_reaches_its_container() {
    if !tool_available("docker", "info") {
        eprintln!(
            "SKIP: Docker daemon not available — \
             run `make test-full` on a host with Docker to include this test"
        );
        return;
    }
    if !tool_available("git", "--version") {
        eprintln!("SKIP: git not available");
        return;
    }
    if !Command::new(awman::engine::host_cli::program("docker"))
        .args(["pull", "alpine:latest"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
    {
        eprintln!("SKIP: docker pull alpine:latest failed (no network?)");
        return;
    }

    let _env_guard = ENV_LOCK.lock().await;
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    init_repo(&repo);
    let (daemon, base) = start_daemon(tmp.path()).await;

    // ── 1. a task that declares the variable ────────────────────────────────
    let (code, created) = curl_command(
        &base,
        "squad add",
        &[
            "--name",
            "env-e2e",
            "--description",
            "declares an env() overlay",
            "--repo",
            repo.to_str().unwrap(),
            "--interval",
            "3600",
            "--mount-scope",
            "gitroot",
            "--overlay",
            &format!("env({E2E_ENV_NAME})"),
        ],
    )
    .await;
    assert_eq!(code, 200, "squad add must succeed: {created}");
    assert_eq!(
        created["unmet_env"],
        serde_json::json!([E2E_ENV_NAME]),
        "the daemon has no value yet, and says so — and creates the task anyway"
    );

    // ── 2. the launching shell has it exported, and syncs ───────────────────
    std::env::set_var(E2E_ENV_NAME, E2E_ENV_VALUE);
    let gateway = RemoteTaskGateway::new(HttpCore::new(&base, "v1", None).unwrap());
    let report = env_sync::sync_env(&gateway, false).await;
    assert_eq!(
        report.pushed,
        vec![E2E_ENV_NAME.to_string()],
        "the ordinary client sync must arm the daemon: {report:?}"
    );

    // ── 3. the shell goes away; only the daemon's overlay holds it ──────────
    std::env::remove_var(E2E_ENV_NAME);
    assert!(
        std::env::var(E2E_ENV_NAME).is_err(),
        "the discriminator: the process environment no longer has it"
    );
    assert_eq!(
        awman::data::config::env::host_var(E2E_ENV_NAME).as_deref(),
        Some(E2E_ENV_VALUE),
        "…but the daemon overlay does, which is exactly the state a background \
         daemon's container launch runs in"
    );

    // ── 4. launch a container the way a task's overlays say to ──────────────
    let task = gateway.get("env-e2e").await.expect("the task");
    let declared = env_overlay_names(&task.overlays);
    assert_eq!(
        declared,
        vec![E2E_ENV_NAME.to_string()],
        "the container options are built from the task's own declaration"
    );

    let name = format!("awman-wi0116-e2e-{}", uuid::Uuid::new_v4());
    let options = ResolvedContainerOptions::resolve([
        ContainerOption::Image(ImageRef::new("alpine:latest")),
        ContainerOption::Name(ContainerName::new(name.clone())),
        ContainerOption::EnvPassthrough(EnvVar(declared[0].clone())),
        ContainerOption::Interactive(false),
        ContainerOption::Entrypoint(Entrypoint::new([
            "sh",
            "-c",
            &format!("printenv {E2E_ENV_NAME} || echo AWMAN_E2E_MISSING"),
        ])),
    ])
    .expect("the option set must resolve");

    let captured = CapturedStdout::default();
    let runtime = ContainerRuntime::docker();
    let instance = runtime.build(options).expect("build must succeed");
    let mut execution = instance
        .run_with_frontend(Box::new(CapturingFrontend {
            captured: captured.clone(),
        }))
        .expect("the container must start");
    let exit = tokio::time::timeout(Duration::from_secs(90), execution.wait())
        .await
        .expect("the container must finish within 90s")
        .expect("waiting on the container must succeed");

    // Best-effort cleanup; `--rm` normally handles it.
    let _ = Command::new(awman::engine::host_cli::program("docker"))
        .args(["rm", "-f", &name])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();

    let printed = captured.text();
    assert_eq!(exit.exit_code, 0, "the container ran: {printed}");
    assert!(
        printed.contains(E2E_ENV_VALUE),
        "THE WI 0116 REGRESSION: a squad task's `env({E2E_ENV_NAME})` value did not \
         reach its container. The value is in the daemon's overlay (asserted \
         above) but the container was launched without it — the `-e` gate in \
         `src/engine/container/docker.rs` still reads `std::env::var`, which \
         cannot see the overlay, and nothing injects the resolved value into \
         the spawned CLI child's environment. Container output was: {printed:?}"
    );

    daemon.abort();
}
