//! E2E: the real `curl` executable drives the squad feature of the unified
//! daemon (`/v1/squad/*`) with no `awman` frontend involved, plus the upgrade
//! path from a pre-0139 API-mode database and the `env()` overlay regression.
//!
//! Each test boots `awman daemon start` as a process with only the squad
//! feature enabled; requests reach its owner-only local socket through the
//! loopback proxy `RootDaemon` provides. Tests that need a container launch
//! gate on a real Docker daemon and are skipped by `make test-fast`.

use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use awman::data::fs::api_db::SqliteSessionStore;
use awman::engine::container::ContainerRuntime;
use serde_json::Value;
use tokio::sync::Mutex;

#[path = "helpers/api_process.rs"]
mod api_process;
use api_process::RootDaemon;

static ENV_LOCK: Mutex<()> = Mutex::const_new(());

fn tool_available(bin: &str, arg: &str) -> bool {
    awman::engine::host_cli::command(&awman::data::config::env::Env::from_process(), bin)
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

async fn curl_resource(method: &str, url: &str, body: Option<Value>) -> (u16, Value) {
    let mut args = vec![
        "-X".into(),
        method.into(),
        "-H".into(),
        "Content-Type: application/json".into(),
    ];
    if let Some(body) = body {
        args.extend(["-d".into(), body.to_string()]);
    }
    args.push(url.into());
    curl(args).await
}

/// Boot a squad-only daemon under `root` (its `AWMAN_HOME`).
async fn start_daemon(root: &std::path::Path) -> (RootDaemon, String) {
    std::fs::create_dir_all(root).unwrap();
    std::fs::write(
        root.join("config.json"),
        r#"{"daemon":{"features":{"squad":{"enabled":true,"envPersistence":"none"}}}}"#,
    )
    .unwrap();
    let daemon = RootDaemon::start(root).await;
    let base = daemon.base();
    (daemon, base)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn curl_drives_the_full_task_lifecycle_with_no_awman_frontend() {
    if !tool_available("git", "--version") || !tool_available("curl", "--version") {
        return;
    }
    let _env_guard = ENV_LOCK.lock().await;
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    init_repo(&repo);
    let (handle, base) = start_daemon(tmp.path()).await;
    let (code, status) = curl_get(format!("{base}/v1/status")).await;
    assert_eq!(code, 200);
    assert_eq!(status["features"]["squad"]["state"], "on");
    let (_, empty) = curl_get(format!("{base}/v1/squad/tasks")).await;
    assert!(empty["items"].as_array().unwrap().is_empty());
    let(code,created)=curl_resource("POST",&format!("{base}/v1/squad/tasks"),Some(serde_json::json!({"name":"issue-triage","description":"watch new issues","workspace":{"kind":"custom","path":repo},"mount_scope":"gitroot","interval_ms":60000}))).await;
    assert_eq!(code, 201, "{created}");
    assert_eq!(created["name"], "issue-triage");
    assert_eq!(created["interval_ms"], 60000);
    let id = created["id"].as_str().unwrap();
    assert!(id.starts_with("task_"));
    let task_url = format!("{base}/v1/squad/tasks/{id}");
    let (code, list) = curl_get(format!("{base}/v1/squad/tasks")).await;
    assert_eq!(code, 200);
    assert_eq!(list["items"].as_array().unwrap().len(), 1);
    assert!(list.get("next_cursor").is_some());
    let (code, task) = curl_get(task_url.clone()).await;
    assert_eq!(code, 200);
    assert_eq!(task["name"], "issue-triage");
    let (code, runs) = curl_get(format!("{task_url}/runs")).await;
    assert_eq!(code, 200);
    assert!(runs["items"].is_array());
    for (paused, state) in [(true, "paused"), (false, "active")] {
        let (code, task) = curl_resource(
            "PATCH",
            &task_url,
            Some(serde_json::json!({"paused":paused})),
        )
        .await;
        assert_eq!(code, 200, "{task}");
        assert_eq!(task["status"], state);
    }
    let (code, body) = curl_get(format!(
        "{base}/v1/squad/tasks/task_00000000000000000000000000000000/workflow"
    ))
    .await;
    assert_eq!(code, 404);
    assert_eq!(body["code"], "task_not_found");
    let (code, _) = curl_resource("DELETE", &task_url, None).await;
    assert_eq!(code, 204);
    let (_, list) = curl_get(format!("{base}/v1/squad/tasks")).await;
    assert!(list["items"].as_array().unwrap().is_empty());
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
            "ses_11e2f5784f73bd56693657eb4763c31a",
            "/pre/upgrade/repo",
            "2026-08-01T00:00:00Z",
        )
        .unwrap();
    legacy
        .insert_command(
            "cmd_0883cc1b67be96c13068ef76216dedff",
            "ses_11e2f5784f73bd56693657eb4763c31a",
            "status",
            "[\"--json\"]",
            "/pre/upgrade/output.log",
        )
        .unwrap();
    let expected_session = legacy
        .get_session("ses_11e2f5784f73bd56693657eb4763c31a")
        .unwrap();
    let expected_command = legacy
        .get_command("cmd_0883cc1b67be96c13068ef76216dedff")
        .unwrap();
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
        migrated
            .get_session("ses_11e2f5784f73bd56693657eb4763c31a")
            .unwrap(),
        expected_session,
        "an upgrade must not lose a pre-existing session row"
    );
    assert_eq!(
        migrated
            .get_command("cmd_0883cc1b67be96c13068ef76216dedff")
            .unwrap(),
        expected_command,
        "an upgrade must not lose a pre-existing command row"
    );
    drop(migrated);

    // ── and the same file now carries squad's tables, driven over HTTP ──
    let (code,created)=curl_resource("POST",&format!("{base}/v1/squad/tasks"),Some(serde_json::json!({"name":"post-upgrade","description":"created after the upgrade","workspace":{"kind":"custom","path":repo},"mount_scope":"gitroot","interval_ms":300000}))).await;
    assert_eq!(
        code, 201,
        "squad add must succeed on a migrated database: {created}"
    );
    let (_, list) = curl_get(format!("{base}/v1/squad/tasks")).await;
    assert_eq!(list["items"].as_array().unwrap().len(), 1);

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

use awman::command::commands::daemon::client::DaemonClient;
use awman::command::commands::squad::client::DaemonSquadClient;
use awman::command::commands::squad::env_sync;
use awman::command::commands::squad::gateway::TaskGateway;
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
/// 3. The variable is removed from the client process environment. The daemon
///    API confirms that its separate process still holds the value.
/// 4. The test mirrors that value into its own daemon overlay, then resolves
///    the task's declared overlay through the production container builder.
///    The launched container must print the value.
///
/// **This test must fail against `main`, and it must fail against any tree
/// whose `-e` gate still reads `std::env::var`** — the container is launched
/// with no `-e NAME` at all in that case and `printenv` finds nothing. It is
/// The API assertion checks the remote daemon's state, while the container
/// assertion checks the host-side environment passthrough path.
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
    if !awman::engine::host_cli::command(&awman::data::config::env::Env::from_process(), "docker")
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
    let (code,created)=curl_resource("POST",&format!("{base}/v1/squad/tasks"),Some(serde_json::json!({"name":"env-e2e","description":"declares an env overlay","workspace":{"kind":"custom","path":repo},"mount_scope":"gitroot","interval_ms":3600000,"overlays":[format!("env({E2E_ENV_NAME})")]}))).await;
    assert_eq!(code, 201, "squad add must succeed: {created}");
    assert_eq!(
        created["unmet_env"],
        serde_json::json!([E2E_ENV_NAME]),
        "the daemon has no value yet, and says so — and creates the task anyway"
    );

    // ── 2. the launching shell has it exported, and syncs ───────────────────
    std::env::set_var(E2E_ENV_NAME, E2E_ENV_VALUE);
    let env = awman::data::config::env::EnvSnapshot::with_overrides([(
        "AWMAN_HOME",
        tmp.path().to_str().unwrap(),
    )]);
    let gateway = DaemonSquadClient::new(DaemonClient::connect(&env).unwrap());
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
    // The daemon is a separate process. Check its coverage through the API,
    // then mirror that value in this process to exercise the same container
    // option builder that runs inside the daemon.
    let task = gateway.get("env-e2e").await.expect("the task");
    assert!(
        task.unmet_env.is_empty(),
        "the daemon must hold the synced value: {:?}",
        task.unmet_env
    );
    struct RestoreOverlay(awman::data::config::env::DaemonEnvMap);
    impl Drop for RestoreOverlay {
        fn drop(&mut self) {
            awman::data::config::env::set_daemon_overlay(std::mem::take(&mut self.0));
        }
    }
    let _restore = RestoreOverlay(awman::data::config::env::daemon_overlay_snapshot());
    awman::data::config::env::set_daemon_overlay(
        awman::data::config::env::DaemonEnvMap::from_pairs([(E2E_ENV_NAME, E2E_ENV_VALUE)]),
    );
    assert_eq!(
        awman::data::config::env::host_var(E2E_ENV_NAME).as_deref(),
        Some(E2E_ENV_VALUE)
    );

    // ── 4. launch a container the way a task's overlays say to ──────────────
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
    let _ =
        awman::engine::host_cli::command(&awman::data::config::env::Env::from_process(), "docker")
            .args(["rm", "-f", &name])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();

    let printed = captured.text();
    assert_eq!(exit.exit_code, 0, "the container ran: {printed}");
    assert!(
        printed.contains(E2E_ENV_VALUE),
        "THE WI 0116 REGRESSION: a squad task's `env({E2E_ENV_NAME})` value did not \
         reach its container. The value is in the local overlay (asserted \
         above) but the container was launched without it — the `-e` gate in \
         `src/engine/container/docker.rs` still reads `std::env::var`, which \
         cannot see the overlay, and nothing injects the resolved value into \
         the spawned CLI child's environment. Container output was: {printed:?}"
    );

    daemon.abort();
}
