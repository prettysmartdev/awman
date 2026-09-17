//! WI 0102 — CLI-side gateway, frontend-parity, and live-daemon tests.
//!
//! The unit/integration half injects a recording gateway into `Dispatch`, so
//! it can prove the CLI and TUI both construct the same Layer-2 squad command
//! and that the frontend itself never opens a task store. The subprocess
//! half drives the real CLI against a live squad daemon and checks its JSON
//! envelopes against the daemon's wire responses.

use std::path::Path;
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::Mutex as AsyncMutex;

use awman::command::commands::squad::commands::SquadServeConfig;
use awman::command::commands::squad::daemon_runtime::SquadDaemonHandles;
use awman::command::commands::squad::gateway::{CreateTask, DaemonStatus, TaskGateway, UpdateTask};
use awman::command::dispatch::catalogue::CommandCatalogue;
use awman::command::dispatch::parsed_input::parse as parse_command_box;
use awman::command::dispatch::{BuiltCommand, Dispatch, Engines};
use awman::command::error::CommandError;
use awman::command::CommandOutcome;
use awman::data::config::env::Env;
use awman::data::fs::daemon_guard::{DaemonGuard, DaemonKind};
use awman::data::fs::daemon_process::{DaemonProcess, ServerMeta};
use awman::data::fs::task_store::{MountScope, Run, RunStatus, Task, TaskStatus};
use awman::data::fs::{ApiPaths, AuthPathResolver, SquadPaths};
use awman::data::session::Session;
use awman::data::EngineWorkflowStateStore;
use awman::engine::agent::AgentEngine;
use awman::engine::agent_runtime::frontend::AgentIo;
use awman::engine::auth::AuthEngine;
use awman::engine::container::ContainerRuntime;
use awman::engine::git::GitEngine;
use awman::engine::overlay::OverlayEngine;
use awman::frontend::cli::{command_path_from_matches, CliFrontend};
use awman::frontend::tui::command_frontend::TuiCommandFrontend;
use awman::frontend::tui::tabs::{
    SharedActiveWorktreePath, SharedContainerExitCode, SharedContainerName,
    SharedContainerSlotEvents, SharedEngineTx, SharedPtyResetFlag, SharedResizeTx,
    SharedStatusDashboard, SharedStdinTx, SharedStuckSender, SharedTuiContext,
    SharedWorkflowViewState, SharedYoloCancelFlag, SharedYoloState,
};
use awman::frontend::tui::user_message::SharedStatusLog;

#[path = "helpers/mod.rs"]
mod helpers;

static ENV_LOCK: AsyncMutex<()> = AsyncMutex::const_new(());

// ─── Recording gateway ───────────────────────────────────────────────────────

#[derive(Clone, Default)]
struct RecordingGateway {
    calls: Arc<Mutex<Vec<String>>>,
    /// Every `CreateTask` that reached the gateway, so a test can assert what
    /// Dispatch actually resolved (interval, workspace, overlays) and not just
    /// that a call happened.
    created: Arc<Mutex<Vec<CreateTask>>>,
    /// Every `UpdateTask` that reached the gateway (WI 0110), so a test can
    /// assert what Dispatch resolved — clears in particular, which a call
    /// name alone cannot distinguish from an unmentioned field.
    updated: Arc<Mutex<Vec<UpdateTask>>>,
}

impl RecordingGateway {
    fn calls(&self) -> Vec<String> {
        self.calls.lock().expect("recording gateway mutex").clone()
    }

    fn created(&self) -> Vec<CreateTask> {
        self.created
            .lock()
            .expect("recording gateway mutex")
            .clone()
    }

    fn updated(&self) -> Vec<UpdateTask> {
        self.updated
            .lock()
            .expect("recording gateway mutex")
            .clone()
    }

    fn push(&self, call: impl Into<String>) {
        self.calls
            .lock()
            .expect("recording gateway mutex")
            .push(call.into());
    }
}

fn task(name: &str) -> Task {
    let now = chrono::Utc::now();
    Task {
        id: format!("task-{name}"),
        name: name.to_string(),
        description: "recorded task".into(),
        repo_scope: Path::new("/recorded/repo").to_path_buf(),
        mount_scope: MountScope::GitRoot,
        overlays: Vec::new(),
        interval_secs: 60,
        status: TaskStatus::Active,
        agent: None,
        model: None,
        backoff_until: None,
        created_at: now,
        updated_at: now,
        last_run_at: None,
        trigger_requested_at: None,
        last_run_status: None,
        unmet_env: Vec::new(),
    }
}

#[async_trait]
impl TaskGateway for RecordingGateway {
    async fn create(&self, request: CreateTask) -> Result<Task, CommandError> {
        self.push(format!("create:{}", request.name));
        self.created
            .lock()
            .expect("recording gateway mutex")
            .push(request.clone());
        Ok(task(&request.name))
    }

    async fn update(&self, name: &str, req: UpdateTask) -> Result<Task, CommandError> {
        self.push(format!("update:{name}"));
        self.updated
            .lock()
            .expect("recording gateway mutex")
            .push(req);
        Ok(task(name))
    }

    async fn list(&self) -> Result<Vec<Task>, CommandError> {
        self.push("list");
        Ok(vec![task("recorded")])
    }

    async fn get(&self, name: &str) -> Result<Task, CommandError> {
        self.push(format!("get:{name}"));
        Ok(task(name))
    }

    async fn runs(&self, name: &str, limit: usize) -> Result<Vec<Run>, CommandError> {
        self.push(format!("runs:{name}:{limit}"));
        Ok(vec![Run {
            id: "run-recorded".into(),
            task_id: format!("task-{name}"),
            status: RunStatus::NotTriggered,
            workflow_path: None,
            workflow_state_path: None,
            session_id: None,
            started_at: chrono::Utc::now(),
            finished_at: None,
            error: None,
            reason: None,
            unmet_env: Vec::new(),
        }])
    }

    async fn set_status(&self, name: &str, status: TaskStatus) -> Result<(), CommandError> {
        self.push(format!(
            "set_status:{name}:{}",
            match status {
                TaskStatus::Active => "active",
                TaskStatus::Paused => "paused",
            }
        ));
        Ok(())
    }

    async fn trigger(&self, name: &str) -> Result<(), CommandError> {
        self.push(format!("trigger:{name}"));
        Ok(())
    }

    async fn cancel(&self, name: &str) -> Result<(), CommandError> {
        self.push(format!("cancel:{name}"));
        Ok(())
    }

    async fn delete(&self, name: &str) -> Result<(), CommandError> {
        self.push(format!("delete:{name}"));
        Ok(())
    }

    async fn status(&self) -> Result<DaemonStatus, CommandError> {
        self.push("status");
        Ok(DaemonStatus {
            running: true,
            pid: Some(42),
            bound_addr: Some("http://127.0.0.1:1234".into()),
            task_count: 1,
            active_count: 1,
            last_tick: None,
            in_flight: 0,
            env_persistence: "none".into(),
            unmet_env: Vec::new(),
        })
    }
}

fn engines_at(root: &Path) -> Engines {
    let api_paths = ApiPaths::from_root(root);
    let auth_paths = AuthPathResolver::at_home(root);
    let runtime = Arc::new(ContainerRuntime::docker());
    let overlay_engine = Arc::new(OverlayEngine::with_auth_resolver(auth_paths.clone()));
    Engines {
        runtime: runtime.clone(),
        container_runtime: Some(runtime.clone()),
        sandbox_runtime: None,
        git_engine: Arc::new(GitEngine::new()),
        overlay_engine: overlay_engine.clone(),
        auth_engine: Arc::new(AuthEngine::with_paths(auth_paths, api_paths.clone())),
        agent_engine: Arc::new(AgentEngine::new(overlay_engine, runtime)),
        workflow_state_store: Arc::new(EngineWorkflowStateStore::at_git_root(api_paths.root())),
    }
}

fn cli_matches(raw: &str) -> clap::ArgMatches {
    let mut argv = vec!["awman".to_string()];
    argv.extend(shell_words::split(raw).expect("test argv must tokenize"));
    CommandCatalogue::get()
        .build_clap_command()
        .try_get_matches_from(argv)
        .unwrap_or_else(|error| panic!("CLI test argv rejected: {raw}: {error}"))
}

fn tui_frontend(raw: &str) -> TuiCommandFrontend {
    let parsed = parse_command_box(raw, CommandCatalogue::get()).expect("TUI test argv must parse");
    let (dialog_tx, _dialog_request_rx) = std::sync::mpsc::channel();
    let (_dialog_response_tx, dialog_rx) = std::sync::mpsc::channel();
    let (stdout_tx, _stdout_rx) = tokio::sync::mpsc::unbounded_channel();
    let (stderr_tx, _stderr_rx) = tokio::sync::mpsc::unbounded_channel();
    let (stdin_tx, stdin_rx) = tokio::sync::mpsc::unbounded_channel();

    TuiCommandFrontend::new(
        parsed,
        Arc::new(Mutex::new(Vec::new())) as SharedStatusLog,
        dialog_tx,
        dialog_rx,
        AgentIo {
            stdout: stdout_tx,
            stderr: stderr_tx,
            stdin_tx,
            stdin_rx,
            resize: None,
            initial_size: None,
        },
        Arc::new(Mutex::new(None)) as SharedWorkflowViewState,
        Arc::new(Mutex::new(None)) as SharedYoloState,
        Arc::new(std::sync::atomic::AtomicBool::new(false)) as SharedYoloCancelFlag,
        Arc::new(std::sync::atomic::AtomicBool::new(false)) as SharedPtyResetFlag,
        Arc::new(Mutex::new(None)) as SharedContainerName,
        Arc::new(Mutex::new(None)) as SharedContainerExitCode,
        Arc::new(Mutex::new(None)) as SharedStdinTx,
        Arc::new(Mutex::new(None)) as SharedResizeTx,
        Arc::new(Mutex::new(None)) as SharedEngineTx,
        Arc::new(Mutex::new(None)) as SharedStuckSender,
        Arc::new(Mutex::new(None)) as SharedActiveWorktreePath,
        Arc::new(Mutex::new(None)) as SharedStatusDashboard,
        Arc::new(Mutex::new(Default::default())) as SharedTuiContext,
        Arc::new(Mutex::new(std::collections::VecDeque::new())) as SharedContainerSlotEvents,
    )
}

async fn run_cli(
    raw: &str,
    session: &Session,
    engines: Engines,
    gateway: Arc<RecordingGateway>,
) -> Result<CommandOutcome, CommandError> {
    let matches = cli_matches(raw);
    let path = command_path_from_matches(&matches);
    let path_refs: Vec<&str> = path.iter().map(String::as_str).collect();
    let frontend = CliFrontend::new(matches);
    let dispatch = Dispatch::new(
        frontend,
        Arc::new(tokio::sync::RwLock::new(session.clone())),
        engines,
    )
    .with_squad_gateway(gateway);
    let built = dispatch
        .build_command(&path_refs)
        .unwrap_or_else(|error| panic!("CLI failed to build {raw}: {error}"));
    assert!(
        matches!(built, BuiltCommand::Squad(_)),
        "{raw} must resolve to SquadCommand"
    );
    dispatch.run_command(&path_refs).await
}

async fn run_tui(
    raw: &str,
    session: &Session,
    engines: Engines,
    gateway: Arc<RecordingGateway>,
) -> Result<CommandOutcome, CommandError> {
    let parsed = parse_command_box(raw, CommandCatalogue::get()).expect("TUI argv must parse");
    let path = parsed.path.clone();
    let path_refs: Vec<&str> = path.iter().map(String::as_str).collect();
    let frontend = tui_frontend(raw);
    let dispatch = Dispatch::new(
        frontend,
        Arc::new(tokio::sync::RwLock::new(session.clone())),
        engines,
    )
    .with_squad_gateway(gateway);
    let built = dispatch
        .build_command(&path_refs)
        .unwrap_or_else(|error| panic!("TUI failed to build {raw}: {error}"));
    assert!(
        matches!(built, BuiltCommand::Squad(_)),
        "{raw} must resolve to SquadCommand"
    );
    dispatch.run_command(&path_refs).await
}

// ─── Dispatch / frontend integration ────────────────────────────────────────

#[tokio::test]
async fn cli_crud_uses_exactly_one_gateway_call_and_no_frontend_validation() {
    let env = helpers::IsolatedEnv::new();
    let session = env.open_session();
    let cases = [
        (
            "squad add --name 'not a slug' --description desc --repo /path/that/does/not/exist --interval 60",
            vec!["create:not a slug".to_string()],
        ),
        ("squad list", vec!["list".to_string()]),
        // `show` is the contract-pinned exception: Layer 2 combines the
        // task and its history with one `get` plus one `runs` call.
        (
            "squad show recorded",
            vec!["get:recorded".to_string(), "runs:recorded:20".to_string()],
        ),
        // WI 0110: a scripted edit is one `update` call, exactly like the
        // other CRUD verbs, with no frontend-side validation of its own.
        (
            "squad edit recorded --description updated --interval 10m",
            vec!["update:recorded".to_string()],
        ),
        ("squad remove recorded", vec!["delete:recorded".to_string()]),
        (
            "squad pause recorded",
            vec!["set_status:recorded:paused".to_string()],
        ),
        (
            "squad resume recorded",
            vec!["set_status:recorded:active".to_string()],
        ),
        // `squad trigger` is its own gateway verb, not a `set_status` or an
        // `update`: it changes no stored schedule, so nothing about the task's
        // configuration is rewritten to make it run.
        (
            "squad trigger recorded",
            vec!["trigger:recorded".to_string()],
        ),
        ("squad cancel recorded", vec!["cancel:recorded".to_string()]),
    ];

    for (raw, expected) in cases {
        let gateway = Arc::new(RecordingGateway::default());
        run_cli(
            raw,
            &session,
            engines_at(env.home_dir.path()),
            gateway.clone(),
        )
        .await
        .unwrap_or_else(|error| panic!("{raw} must reach its gateway: {error}"));
        assert_eq!(gateway.calls(), expected, "gateway sequence for {raw}");
    }
}

/// WI 0106 Part 2.5: `6h` is a *default*, not a floor or a ceiling. The value
/// that reaches Layer 1 — and is therefore what gets persisted — must be
/// 21_600 seconds with no flag, and exactly what was asked for with one.
#[tokio::test]
async fn squad_add_resolves_six_hours_by_default_and_honours_an_explicit_interval() {
    let env = helpers::IsolatedEnv::new();
    let session = env.open_session();

    for (raw, expected_secs) in [
        (
            "squad add --name defaulted --description watch-for-issues",
            6 * 60 * 60,
        ),
        (
            "squad add --name explicit --description watch-for-issues --interval 5m",
            300,
        ),
    ] {
        let gateway = Arc::new(RecordingGateway::default());
        run_cli(
            raw,
            &session,
            engines_at(env.home_dir.path()),
            gateway.clone(),
        )
        .await
        .unwrap_or_else(|error| panic!("{raw} must reach its gateway: {error}"));
        let created = gateway.created();
        assert_eq!(created.len(), 1, "{raw} must create exactly one task");
        assert_eq!(
            created[0].interval_secs, expected_secs,
            "{raw} must persist {expected_secs}s"
        );
    }
}

/// With neither `--workspace` nor `--repo`, scripted creation binds the task to
/// its durable per-task workspace — the same answer the interview's default
/// choice gives.
#[tokio::test]
async fn squad_add_without_a_workspace_flag_asks_for_the_durable_task_workspace() {
    let env = helpers::IsolatedEnv::new();
    let session = env.open_session();
    let gateway = Arc::new(RecordingGateway::default());
    run_cli(
        "squad add --name defaulted --description watch-for-issues",
        &session,
        engines_at(env.home_dir.path()),
        gateway.clone(),
    )
    .await
    .expect("scripted add must reach its gateway");
    assert_eq!(
        gateway.created()[0].workspace,
        awman::data::fs::task_store::TaskWorkspace::Default
    );
}

#[tokio::test]
async fn cli_and_tui_same_argv_build_squad_and_issue_the_same_gateway_sequence() {
    let env = helpers::IsolatedEnv::new();
    let session = env.open_session();
    for raw in [
        "squad add --name parity --description desc --repo /not/validated --interval 60",
        "squad list",
        "squad show parity",
        "squad remove parity",
        "squad pause parity",
        "squad resume parity",
    ] {
        let cli_gateway = Arc::new(RecordingGateway::default());
        run_cli(
            raw,
            &session,
            engines_at(env.home_dir.path()),
            cli_gateway.clone(),
        )
        .await
        .unwrap_or_else(|error| panic!("CLI {raw} failed: {error}"));

        let tui_gateway = Arc::new(RecordingGateway::default());
        run_tui(
            raw,
            &session,
            engines_at(env.home_dir.path()),
            tui_gateway.clone(),
        )
        .await
        .unwrap_or_else(|error| panic!("TUI {raw} failed: {error}"));

        assert_eq!(
            cli_gateway.calls(),
            tui_gateway.calls(),
            "Layer-2 call sequence for {raw}"
        );
    }
}

#[tokio::test]
async fn cli_dispatch_succeeds_with_an_empty_home_and_never_reads_awman_db() {
    let _env_guard = ENV_LOCK.lock().await;
    let home = tempfile::tempdir().expect("temporary home");
    let previous_home = std::env::var("HOME").ok();
    let previous_config = std::env::var("AWMAN_CONFIG_HOME").ok();
    let previous_squad = std::env::var("AWMAN_SQUAD_ROOT").ok();
    std::env::set_var("HOME", home.path());
    std::env::set_var("AWMAN_CONFIG_HOME", home.path().join("config"));
    std::env::set_var("AWMAN_SQUAD_ROOT", home.path().join("squad"));

    let env = helpers::IsolatedEnv::new();
    let session = env.open_session();
    let gateway = Arc::new(RecordingGateway::default());
    let result = run_cli(
        "squad list",
        &session,
        engines_at(env.home_dir.path()),
        gateway.clone(),
    )
    .await;

    match previous_home {
        Some(value) => std::env::set_var("HOME", value),
        None => std::env::remove_var("HOME"),
    }
    match previous_config {
        Some(value) => std::env::set_var("AWMAN_CONFIG_HOME", value),
        None => std::env::remove_var("AWMAN_CONFIG_HOME"),
    }
    match previous_squad {
        Some(value) => std::env::set_var("AWMAN_SQUAD_ROOT", value),
        None => std::env::remove_var("AWMAN_SQUAD_ROOT"),
    }

    result.expect("CLI dispatch must use the injected gateway without a database");
    assert_eq!(gateway.calls(), vec!["list".to_string()]);
    assert!(
        !home.path().join(".awman/data/awman.db").exists(),
        "CLI must not create or read ~/.awman/data/awman.db"
    );
}

/// `squad status` overlays live scheduler counts from the injected gateway with
/// exactly one `status()` call (§9.4). A stopped daemon (no gateway injected)
/// falls back to the pidfile-only answer and makes no call — that is what
/// `GatewayNeed::IfRunning` buys.
#[tokio::test]
async fn cli_status_uses_exactly_one_gateway_call() {
    let env = helpers::IsolatedEnv::new();
    let session = env.open_session();
    let gateway = Arc::new(RecordingGateway::default());
    run_cli(
        "squad status --json",
        &session,
        engines_at(env.home_dir.path()),
        gateway.clone(),
    )
    .await
    .expect("status command must succeed");
    assert_eq!(gateway.calls(), vec!["status".to_string()]);
}

/// A `GatewayNeed::Running` command against a daemon this shell holds no key
/// for is refused by `Dispatch` before the request, with the typed answer —
/// not left to surface as a bare `HTTP 401` from a request that was never
/// going to be accepted (WI 0113 F-04).
///
/// The daemon is *represented*, not started: a pidfile naming this live test
/// process plus an endpoint sidecar is exactly what `SquadSupervisor` reads to
/// decide a daemon is already running, so the supervisor takes its
/// already-running branch and never spawns anything.
#[cfg(unix)]
#[tokio::test]
async fn a_running_need_with_no_key_in_this_shell_is_refused_before_the_request() {
    let _env_guard = ENV_LOCK.lock().await;
    let home = tempfile::tempdir().expect("temporary home");
    let squad_root = home.path().join("squad");
    std::fs::create_dir_all(&squad_root).expect("squad root");

    let restore = ScopedProcessEnv::set(&[
        ("HOME", home.path().to_str().unwrap()),
        (
            "AWMAN_CONFIG_HOME",
            home.path().join("config").to_str().unwrap(),
        ),
        ("AWMAN_API_ROOT", home.path().join("api").to_str().unwrap()),
        ("AWMAN_SQUAD_ROOT", squad_root.to_str().unwrap()),
        // An empty key is treated as unset, which is the state under test.
        ("AWMAN_SQUAD_KEY", ""),
    ]);

    // A live process whose name contains "awman" — `check_already_running`
    // rejects a pidfile naming anything else, and the test binary is not it.
    let holder = FakeAwmanProcess::spawn();
    let paths = SquadPaths::from_root(&squad_root);
    let process = DaemonProcess::new(paths.daemon(), "awman-squad-test", "io.awman.squad.test");
    process.force_write_pidfile(holder.id()).expect("pidfile");
    process
        .write_meta(&ServerMeta {
            port: 1,
            bind_ip: "127.0.0.1".to_string(),
            scheme: "http".to_string(),
            auth_disabled: false,
        })
        .expect("endpoint sidecar");
    // A hash on disk with no plaintext anywhere is precisely `Missing`: the
    // daemon will check a key this process cannot produce.
    paths
        .daemon()
        .write_key_hash("0000000000000000000000000000000000000000000000000000000000000000")
        .expect("key hash");

    let env = helpers::IsolatedEnv::new();
    let session = env.open_session();
    let matches = cli_matches("squad list");
    let path = command_path_from_matches(&matches);
    let path_refs: Vec<&str> = path.iter().map(String::as_str).collect();
    let dispatch = Dispatch::new(
        CliFrontend::new(matches),
        Arc::new(tokio::sync::RwLock::new(session.clone())),
        engines_at(env.home_dir.path()),
    );
    let error = dispatch
        .run_command(&path_refs)
        .await
        .expect_err("a daemon this shell holds no key for must be refused");

    drop(restore);
    drop(holder);

    assert!(
        matches!(error, CommandError::SquadKeyMissing),
        "the refusal must be typed, not a stringly-typed `Other`: {error:?}"
    );
}

/// Scope the real process environment for one test and restore it on drop.
/// `Dispatch` resolves the squad gateway through `Env::from_process()`, so a
/// test that exercises that path has to move the process environment.
struct ScopedProcessEnv(Vec<(&'static str, Option<String>)>);

impl ScopedProcessEnv {
    fn set(vars: &[(&'static str, &str)]) -> Self {
        Self(
            vars.iter()
                .map(|(key, value)| {
                    let previous = std::env::var(key).ok();
                    std::env::set_var(key, value);
                    (*key, previous)
                })
                .collect(),
        )
    }
}

impl Drop for ScopedProcessEnv {
    fn drop(&mut self) {
        for (key, previous) in &self.0 {
            match previous {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}

// ─── Live CLI / daemon JSON integration ─────────────────────────────────────

struct NeverTriggeredEvaluator;

#[async_trait]
impl awman::engine::squad::TaskEvaluator for NeverTriggeredEvaluator {
    async fn evaluate(
        &self,
        _request: awman::engine::squad::EvaluationRequest,
    ) -> awman::engine::squad::EvaluationOutcome {
        awman::engine::squad::EvaluationOutcome::NotTriggered { reason: None }
    }
}

fn init_repo(path: &Path) {
    std::fs::create_dir_all(path).expect("repo directory");
    let status = Command::new("git")
        .args(["init", "-q"])
        .current_dir(path)
        .status()
        .expect("git must run");
    assert!(status.success(), "git init must succeed");
}

#[cfg(unix)]
struct FakeAwmanProcess {
    _dir: tempfile::TempDir,
    child: std::process::Child,
}

#[cfg(unix)]
impl FakeAwmanProcess {
    fn spawn() -> Self {
        let dir = tempfile::tempdir().expect("fake awman process directory");
        let executable = dir.path().join("awman-squad-test-holder");
        std::fs::copy("/bin/sleep", &executable).expect("copy sleep holder");
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(&executable)
            .expect("holder metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&executable, permissions).expect("holder permissions");
        // Under `cargo test`'s default concurrent-test-function execution,
        // another test can fork while this thread still holds the holder's
        // write descriptor open; the kernel then reports the just-written
        // executable as busy (`ETXTBSY`) until that fork execs. Retry briefly
        // rather than require `--test-threads=1`, as the other squad fixtures do.
        let mut attempt = 0;
        let child = loop {
            match Command::new(&executable).arg("60").spawn() {
                Ok(child) => break child,
                Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy && attempt < 20 => {
                    attempt += 1;
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(e) => panic!("awman-named holder must start: {e}"),
            }
        };

        // `spawn` returns once the child *exists*, not once it has finished
        // `execve`. Until that exec lands, the child's command name is still
        // the test-function name it inherited, which `pid_is_awman` correctly
        // rejects; a supervisor checked inside that window would treat the
        // pidfile as stale and try to auto-start a daemon. Wait for the
        // identity this fixture exists to present.
        let mut child = child;
        let pid = child.id();
        for _ in 0..500 {
            if awman::data::fs::daemon_process::pid_is_awman(pid) {
                return Self { _dir: dir, child };
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let _ = child.kill();
        let _ = child.wait();
        panic!("awman-named holder (PID {pid}) never presented an awman command name");
    }

    fn id(&self) -> u32 {
        self.child.id()
    }
}

#[cfg(unix)]
impl Drop for FakeAwmanProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(unix)]
async fn start_daemon(root: &Path) -> (tokio::task::JoinHandle<()>, String, FakeAwmanProcess) {
    let engines = engines_at(root);
    let previous_config = std::env::var("AWMAN_CONFIG_HOME").ok();
    let previous_squad = std::env::var("AWMAN_SQUAD_ROOT").ok();
    std::env::set_var("AWMAN_CONFIG_HOME", root);
    std::env::set_var("AWMAN_SQUAD_ROOT", root.join("squad"));
    let daemon_guard = DaemonGuard::for_daemon(DaemonKind::Squad, &Env::from_process())
        .expect("test daemon guard");
    let holder = FakeAwmanProcess::spawn();
    daemon_guard
        .acquire(holder.id())
        .expect("test daemon guard must claim squad");

    // WI 0113 F-02: Layer 2 bootstraps the daemon, Layer 3 serves it.
    let handle = tokio::spawn(async move {
        let handles = SquadDaemonHandles::bootstrap(
            SquadServeConfig {
                port: 0,
                dangerously_skip_auth: true,
            },
            engines,
            Arc::new(NeverTriggeredEvaluator),
        )
        .await
        .expect("squad daemon bootstrap");
        let _ = awman::frontend::squad::serve(handles).await;
        let _ = daemon_guard.release();
    });

    let daemon = awman::data::fs::daemon_process::DaemonProcess::new(
        SquadPaths::from_root(root.join("squad")).daemon(),
        awman::data::fs::daemon_process::SQUAD_UNIT_NAME,
        awman::data::fs::daemon_process::SQUAD_PLIST_LABEL,
    );
    let mut waited = Duration::ZERO;
    let meta = loop {
        if let Ok(Some(meta)) = daemon.read_meta() {
            break meta;
        }
        assert!(
            waited < Duration::from_secs(10),
            "squad daemon never published metadata"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
        waited += Duration::from_millis(50);
    };

    match previous_config {
        Some(value) => std::env::set_var("AWMAN_CONFIG_HOME", value),
        None => std::env::remove_var("AWMAN_CONFIG_HOME"),
    }
    match previous_squad {
        Some(value) => std::env::set_var("AWMAN_SQUAD_ROOT", value),
        None => std::env::remove_var("AWMAN_SQUAD_ROOT"),
    }

    (
        handle,
        format!("{}://{}:{}", meta.scheme, meta.bind_ip, meta.port),
        holder,
    )
}

/// A stable, private copy of the built `awman` binary that the subprocess tests
/// exec instead of `CARGO_BIN_EXE_awman` (`target/debug/awman`) directly.
///
/// Cargo publishes that top-level path with an unlink-then-relink sequence, so a
/// spawn racing a concurrent (re)build can momentarily see ENOENT on a binary
/// that assuredly exists — the transient `Os { code: 2, NotFound }` that fails
/// these tests. Exec'ing a copy under our own tempdir removes the race entirely:
/// the shared source is read once per test-binary process (cached below), and
/// every subsequent launch is deterministic. No assertion on CLI behaviour
/// changes — it is still the freshly built binary that runs.
///
/// The `root` argument is retained for call-site compatibility; the copy itself
/// lives in a process-global location so the racy top-level path is touched at
/// most once, no matter how many tests run.
fn awman_under_test(_root: &Path) -> std::path::PathBuf {
    use std::sync::OnceLock;
    static SHARED: OnceLock<std::path::PathBuf> = OnceLock::new();
    SHARED.get_or_init(build_shared_awman_copy).clone()
}

/// Resolve the freshly built `awman` binary and copy it to a stable, private
/// path for the lifetime of this test process.
fn build_shared_awman_copy() -> std::path::PathBuf {
    // Persist for the whole process: `into_path` leaks the tempdir (fine for a
    // test binary — the OS reclaims it on exit) so the copy outlives any single
    // test's `tempfile::tempdir`.
    let dir = tempfile::Builder::new()
        .prefix("awman-under-test-")
        .tempdir()
        .expect("temp dir for awman copy")
        .keep();
    let dest = dir.join("awman");

    // The freshly built binary is guaranteed to exist for a legitimate `cargo
    // test`, but the source can be momentarily unresolvable, and for longer
    // than a single relink: cargo republishes the top-level path with an
    // unlink-then-relink, and a *concurrent* `cargo` invocation sharing this
    // target dir (e.g. a CI job that overlaps a build with the test run) both
    // widens that window and churns the `deps/` artifacts the fallback probes,
    // so every resolution path can be transiently unavailable at once. Retry
    // resolve-then-copy against a generous wall-clock deadline rather than
    // giving up after a fixed, too-short burst — the old fixed-attempt loops
    // could panic while the binary was merely mid-rebuild. In the common case
    // the source resolves on the first iteration and this returns immediately.
    let top_level = std::path::PathBuf::from(env!("CARGO_BIN_EXE_awman"));
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut backoff = Duration::from_millis(25);
    let mut last_copy_err: Option<String> = None;
    loop {
        if let Some(src) = resolve_awman_source() {
            match std::fs::copy(&src, &dest) {
                Ok(_) => break,
                Err(err) => {
                    last_copy_err = Some(format!("last copy from {}: {err:?}", src.display()))
                }
            }
        }
        if Instant::now() >= deadline {
            panic!(
                "awman binary never became usable within 120s; top-level {} try_exists={:?}; {}",
                top_level.display(),
                top_level.try_exists(),
                last_copy_err
                    .as_deref()
                    .unwrap_or("top-level absent and no real-CLI deps/awman-<hash> found"),
            );
        }
        std::thread::sleep(backoff);
        backoff = (backoff * 2).min(Duration::from_millis(500));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755))
            .expect("mark awman copy executable");
    }
    dest
}

/// Best-effort *single* resolution of the freshly built `awman` binary. Returns
/// `None` when no source is currently available; the caller retries against a
/// deadline (see `build_shared_awman_copy`).
///
/// The primary source is `CARGO_BIN_EXE_awman` (`target/debug/awman`). While
/// that convenience path is momentarily missing — cargo publishes it with an
/// unlink-then-relink, which a concurrent (re)build can widen well beyond a
/// single relink — fall back to a per-hash binary under `target/debug/deps/`.
///
/// The fallback must be careful: `deps/` also holds the bin crate's own *libtest
/// harness* executables, which are likewise named `awman-<hash>` but only accept
/// test-runner flags (running one with `squad add …` yields "Unrecognized
/// option"). We cannot tell them apart by hard-link count — in this build cargo
/// publishes the top-level path as a plain copy (link count 1), so *no* deps
/// binary is ever hard-linked to it — so we positively identify the real CLI by
/// probing each candidate with `--version` and keeping the newest that answers
/// like the CLI.
fn resolve_awman_source() -> Option<std::path::PathBuf> {
    let top_level = std::path::PathBuf::from(env!("CARGO_BIN_EXE_awman"));
    // `try_exists` distinguishes a genuine absence (`Ok(false)`) from a
    // transient stat error (`Err(_)`, e.g. under fd pressure). Only a confirmed
    // absence sends us to the `deps/` fallback; on an ambiguous error prefer the
    // real path and let the copy surface any genuine failure.
    if !matches!(top_level.try_exists(), Ok(false)) {
        return Some(top_level);
    }
    newest_real_cli_deps_binary(&top_level)
}

/// The newest `target/debug/deps/awman-<hash>` executable that probes as the real
/// `awman` CLI (not one of the bin crate's libtest harnesses of the same name).
#[cfg(unix)]
fn newest_real_cli_deps_binary(top_level: &Path) -> Option<std::path::PathBuf> {
    use std::time::SystemTime;

    let deps = top_level.parent()?.join("deps");
    // Collect candidates newest-first, tolerating transient per-entry errors from
    // a concurrent build (skip the entry rather than abandoning the whole scan).
    let mut candidates: Vec<(SystemTime, std::path::PathBuf)> = Vec::new();
    for entry in std::fs::read_dir(&deps).ok()?.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        // Match the bin (`awman-<hash>`), never the `.d` depfiles or unrelated
        // test binaries (`squad_cli_gateway-<hash>`, ...).
        if !name.starts_with("awman-") || name.contains('.') {
            continue;
        }
        let meta = match entry.metadata() {
            Ok(meta) if meta.is_file() => meta,
            _ => continue,
        };
        let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        candidates.push((mtime, entry.path()));
    }
    candidates.sort_by(|a, b| b.0.cmp(&a.0));
    candidates
        .into_iter()
        .map(|(_, path)| path)
        .find(|path| probes_as_awman_cli(path))
}

#[cfg(not(unix))]
fn newest_real_cli_deps_binary(_top_level: &Path) -> Option<std::path::PathBuf> {
    None
}

/// True if `path` responds to `--version` like the real `awman` CLI. A libtest
/// harness rejects the flag (nonzero exit) and never prints an `awman <version>`
/// banner, so this cleanly excludes the same-named test executables in `deps/`.
fn probes_as_awman_cli(path: &Path) -> bool {
    match Command::new(path).arg("--version").output() {
        Ok(out) => out.status.success() && out.stdout.starts_with(b"awman "),
        Err(_) => false,
    }
}

fn run_binary(root: &Path, cwd: &Path, args: &[&str]) -> Output {
    let home = root.join("home");
    std::fs::create_dir_all(&home).expect("test home");
    Command::new(awman_under_test(root))
        .args(args)
        .current_dir(cwd)
        .env("HOME", &home)
        .env("AWMAN_CONFIG_HOME", root)
        .env("AWMAN_SQUAD_ROOT", root.join("squad"))
        .output()
        .expect("awman subprocess must start")
}

fn stdout_json(output: &Output) -> Value {
    assert!(
        output.status.success(),
        "awman failed: status={:?} stdout={} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "awman --json emitted invalid JSON: {error}; stdout={:?}",
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

async fn daemon_command(base: &str, subcommand: &str, args: &[&str]) -> Value {
    reqwest::Client::new()
        .post(format!("{base}/v1/commands"))
        .json(&serde_json::json!({ "subcommand": subcommand, "args": args }))
        .send()
        .await
        .expect("daemon command request")
        .json()
        .await
        .expect("daemon command JSON")
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_crud_round_trip_and_json_payloads_match_the_live_daemon() {
    if !helpers::git_available() {
        eprintln!("SKIP: git not available");
        return;
    }
    let _env_guard = ENV_LOCK.lock().await;
    let root = tempfile::tempdir().expect("temporary squad root");
    let repo = root.path().join("repo");
    init_repo(&repo);
    let (daemon, base, _holder) = start_daemon(root.path()).await;

    let repo_arg = repo.to_str().expect("repo path");
    let add = run_binary(
        root.path(),
        &repo,
        &[
            "squad",
            "add",
            "--name",
            "cli-roundtrip",
            "--description",
            "from cli",
            "--repo",
            repo_arg,
            "--interval",
            "60",
        ],
    );
    assert!(
        add.status.success(),
        "squad add failed: {}",
        String::from_utf8_lossy(&add.stderr)
    );

    let list_json = stdout_json(&run_binary(
        root.path(),
        &repo,
        &["squad", "list", "--json"],
    ));
    let list_payload = list_json
        .get("payload")
        .expect("CLI list JSON must contain a payload");
    assert_eq!(
        list_payload,
        &daemon_command(&base, "squad list", &[]).await
    );

    let show_json = stdout_json(&run_binary(
        root.path(),
        &repo,
        &["squad", "show", "cli-roundtrip", "--json"],
    ));
    let show_payload = show_json
        .get("payload")
        .expect("CLI show JSON must contain a payload");
    assert_eq!(
        show_payload,
        &daemon_command(&base, "squad show", &["cli-roundtrip"]).await
    );

    let pause = run_binary(root.path(), &repo, &["squad", "pause", "cli-roundtrip"]);
    assert!(pause.status.success(), "squad pause failed");
    let paused = stdout_json(&run_binary(
        root.path(),
        &repo,
        &["squad", "show", "cli-roundtrip", "--json"],
    ));
    assert_eq!(paused["payload"]["task"]["status"], "paused");

    let resume = run_binary(root.path(), &repo, &["squad", "resume", "cli-roundtrip"]);
    assert!(resume.status.success(), "squad resume failed");
    let resumed = stdout_json(&run_binary(
        root.path(),
        &repo,
        &["squad", "show", "cli-roundtrip", "--json"],
    ));
    assert_eq!(resumed["payload"]["task"]["status"], "active");

    let status_json = stdout_json(&run_binary(
        root.path(),
        &repo,
        &["squad", "status", "--json"],
    ));
    let status_payload = status_json
        .get("payload")
        .expect("CLI status JSON must contain a payload");
    let daemon_status = reqwest::get(format!("{base}/v1/status"))
        .await
        .expect("daemon status request")
        .json::<Value>()
        .await
        .expect("daemon status JSON");
    let mut cli_keys: Vec<_> = status_payload
        .as_object()
        .expect("CLI status payload must be an object")
        .keys()
        .collect();
    let mut daemon_keys: Vec<_> = daemon_status
        .as_object()
        .expect("daemon status response must be an object")
        .keys()
        .collect();
    cli_keys.sort_unstable();
    daemon_keys.sort_unstable();
    assert_eq!(
        cli_keys, daemon_keys,
        "CLI status JSON must preserve the daemon response shape"
    );

    let remove = run_binary(
        root.path(),
        &repo,
        &["squad", "remove", "cli-roundtrip", "--yes"],
    );
    assert!(remove.status.success(), "squad remove failed");
    let final_list = stdout_json(&run_binary(
        root.path(),
        &repo,
        &["squad", "list", "--json"],
    ));
    assert_eq!(final_list["payload"], serde_json::json!([]));

    daemon.abort();
}

#[test]
fn json_cli_failure_is_a_structured_error_with_nonzero_exit() {
    if !helpers::git_available() {
        eprintln!("SKIP: git not available");
        return;
    }
    let root = tempfile::tempdir().expect("temporary root");
    let repo = root.path().join("repo");
    init_repo(&repo);
    let invalid_squad_root = root.path().join("squad-file");
    std::fs::write(&invalid_squad_root, "not a directory").expect("invalid squad root fixture");

    let home = root.path().join("home");
    std::fs::create_dir_all(&home).expect("test home");
    let output = Command::new(awman_under_test(root.path()))
        .args(["squad", "list", "--json"])
        .current_dir(&repo)
        .env("HOME", &home)
        .env("AWMAN_CONFIG_HOME", root.path().join("config"))
        .env("AWMAN_SQUAD_ROOT", &invalid_squad_root)
        .output()
        .expect("awman subprocess must start");

    assert!(
        !output.status.success(),
        "unreachable daemon must return non-zero"
    );
    let body: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "JSON failure must be valid JSON: {error}; stdout={:?}, stderr={:?}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    });
    assert!(body["error"]
        .as_str()
        .is_some_and(|message| !message.is_empty()));
}

/// WI 0110: `squad edit` with no field flags would only bump `updated_at`, so
/// Layer 2 refuses it outright rather than reporting a successful no-op. The
/// gateway must never be reached.
#[tokio::test]
async fn squad_edit_with_no_fields_is_refused_before_the_gateway() {
    let env = helpers::IsolatedEnv::new();
    let session = env.open_session();
    let gateway = Arc::new(RecordingGateway::default());

    let error = run_cli(
        "squad edit recorded",
        &session,
        engines_at(env.home_dir.path()),
        gateway.clone(),
    )
    .await
    .expect_err("an edit that changes nothing must be refused");

    assert!(
        error.to_string().contains("nothing to change"),
        "the refusal should say what is missing: {error}"
    );
    assert!(
        gateway.calls().is_empty(),
        "no gateway call may be made for an empty edit: {:?}",
        gateway.calls()
    );
}

/// The clear flags are how an edit says "fall back to the squad default", and
/// they must survive the trip through Dispatch as `Some(None)` rather than
/// being indistinguishable from an unmentioned field.
#[tokio::test]
async fn squad_edit_clear_flags_reach_the_gateway_as_explicit_clears() {
    let env = helpers::IsolatedEnv::new();
    let session = env.open_session();
    let gateway = Arc::new(RecordingGateway::default());

    run_cli(
        "squad edit recorded --clear-agent --clear-model --clear-overlays --clear-agent-models",
        &session,
        engines_at(env.home_dir.path()),
        gateway.clone(),
    )
    .await
    .expect("clear-only edits are real edits");

    let update = gateway
        .updated()
        .pop()
        .expect("the edit must reach the gateway");
    assert_eq!(update.agent, Some(None));
    assert_eq!(update.model, Some(None));
    assert_eq!(update.overlays, Some(Vec::new()));
    assert_eq!(update.agents_to_models, Some(Default::default()));
}

/// `--agent-models` is parsed at the dispatch boundary, so Layer 2 and the
/// daemon only ever see the assembled pool.
#[tokio::test]
async fn squad_edit_agent_models_specs_reach_the_gateway_as_a_pool() {
    let env = helpers::IsolatedEnv::new();
    let session = env.open_session();
    let gateway = Arc::new(RecordingGateway::default());

    run_cli(
        "squad edit recorded --agent-models claude=opus,sonnet --agent-models codex=gpt-5",
        &session,
        engines_at(env.home_dir.path()),
        gateway.clone(),
    )
    .await
    .expect("an agent-models edit must reach the gateway");

    let pool = gateway
        .updated()
        .pop()
        .expect("the edit must reach the gateway")
        .agents_to_models
        .expect("the pool must be carried");
    assert_eq!(
        pool.get("claude").map(Vec::as_slice),
        Some(["opus", "sonnet"].map(String::from).as_slice())
    );
    assert_eq!(
        pool.get("codex").map(Vec::as_slice),
        Some(["gpt-5"].map(String::from).as_slice())
    );
}
