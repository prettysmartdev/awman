//! WI 0101 — `awman agents status` renders the `squad:<task>` source marker.
//!
//! The primary test drives the real `AgentsStatusCommand` end-to-end against a fake
//! `AgentRuntimeEngine` (no live Docker/Apple daemon needed) whose
//! `list_running_all()` returns one squad-launched container and one plain
//! session container, proving the marker is additive (the session row renders
//! exactly as it always has) and derived purely from the container **name**
//! (`AgentHandle` has no label field to read back).
//!
//! Docker- and Apple-gated tiers then plant a real squad-named container and
//! a real session-named container on each live backend and assert the same
//! marker survives a round trip through that backend's own
//! `list_running_all()`, skipping cleanly when the backend is unavailable.

use std::sync::Arc;

#[path = "helpers/mod.rs"]
mod helpers;

use awman::command::commands::agents::{
    AgentSource, AgentState, AgentsStatusCommand, AgentsStatusFlags, AgentsStatusOutcome,
};
use awman::command::commands::Command;
use awman::command::dispatch::Engines;
use awman::data::fs::{ApiPaths, AuthPathResolver};
use awman::data::session::{AgentHandle, Session};
use awman::data::WorkflowStateStore;
use awman::engine::agent::AgentEngine;
use awman::engine::agent_runtime::execution::AgentInstance;
use awman::engine::agent_runtime::{
    AgentRuntimeEngine, AgentStats, Capabilities, ResolvedAgentOptions,
};
use awman::engine::auth::AuthEngine;
use awman::engine::container::naming::generate_squad_container_name;
use awman::engine::container::ContainerRuntime;
use awman::engine::error::EngineError;
use awman::engine::git::GitEngine;
use awman::engine::overlay::OverlayEngine;

// ─── A fake runtime returning canned handles, no label concept ───────────────

struct FakeRuntime {
    handles: Vec<AgentHandle>,
}

impl AgentRuntimeEngine for FakeRuntime {
    fn ready_agent(
        &self,
        _agent: &str,
        _opts: awman::engine::agent_runtime::ReadyAgentOptions,
        _sink: &mut dyn awman::data::message::UserMessageSink,
    ) -> Result<(), awman::engine::error::EngineError> {
        Ok(())
    }
    fn image_exists(&self, _tag: &str) -> Result<bool, awman::engine::error::EngineError> {
        Ok(true)
    }
    fn image_home_dir(
        &self,
        _tag: &str,
    ) -> Result<Option<String>, awman::engine::error::EngineError> {
        Ok(None)
    }
    fn build_image(
        &self,
        _request: &awman::engine::container::BuildRequest,
        _on_line: &mut dyn FnMut(&str),
    ) -> Result<(), awman::engine::error::EngineError> {
        Ok(())
    }
    fn runtime_name(&self) -> &'static str {
        "fake"
    }
    fn display_name(&self) -> &'static str {
        "Fake"
    }
    fn capabilities(&self) -> &Capabilities {
        static CAPS: Capabilities = Capabilities {
            arbitrary_host_mounts: true,
            kit_declarative: false,
            has_image_store: false,
            image_acquisition: awman::engine::agent_runtime::capabilities::ImageAcquisition::Kit,
            per_launch_vm_settings: false,
            reports_resource_stats: true,
        };
        &CAPS
    }
    fn is_available(&self) -> bool {
        true
    }
    fn build(&self, _options: ResolvedAgentOptions) -> Result<Box<dyn AgentInstance>, EngineError> {
        unimplemented!("not exercised by the status test")
    }
    fn list_running(&self, _session: &Session) -> Result<Vec<AgentHandle>, EngineError> {
        Ok(self.handles.clone())
    }
    fn list_running_all(&self) -> Result<Vec<AgentHandle>, EngineError> {
        Ok(self.handles.clone())
    }
    fn stats(&self, _handle: &AgentHandle) -> Result<AgentStats, EngineError> {
        // `AgentsStatusCommand` treats a stats failure as best-effort (`.ok()`), so
        // returning an error here (rather than panicking) exercises exactly
        // the path a real backend takes for a container it can't inspect.
        Err(EngineError::NotImplemented(
            "stats not exercised in this test",
        ))
    }
    fn stop(&self, _handle: &AgentHandle) -> Result<(), EngineError> {
        unimplemented!("not exercised by the status test")
    }
    fn attach(&self, _handle: &AgentHandle) -> Result<Box<dyn AgentInstance>, EngineError> {
        unimplemented!("not exercised by the status test")
    }
    fn list_running_with_name_prefix(&self, prefix: &str) -> Result<Vec<AgentHandle>, EngineError> {
        Ok(self
            .handles
            .iter()
            .filter(|h| h.name.starts_with(prefix))
            .cloned()
            .collect())
    }
}

fn handle(name: &str) -> AgentHandle {
    AgentHandle {
        id: format!("id-{name}"),
        image_tag: "awman/dev:latest".into(),
        name: name.to_string(),
        started_at: chrono::Utc::now(),
    }
}

fn engines_with(runtime: Arc<dyn AgentRuntimeEngine>) -> Engines {
    let tmp = tempfile::tempdir().unwrap();
    let api_paths = ApiPaths::from_root(tmp.path());
    let auth_paths = AuthPathResolver::at_home(tmp.path());
    let overlay_engine = Arc::new(OverlayEngine::with_auth_resolver(auth_paths.clone()));
    let container_runtime = Arc::new(ContainerRuntime::docker());
    Engines {
        runtime,
        container_runtime: Some(container_runtime.clone()),
        sandbox_runtime: None,
        git_engine: Arc::new(GitEngine::new()),
        overlay_engine: overlay_engine.clone(),
        auth_engine: Arc::new(AuthEngine::with_paths(auth_paths, api_paths.clone())),
        agent_engine: Arc::new(AgentEngine::new(overlay_engine, container_runtime)),
        workflow_state_store: Arc::new(WorkflowStateStore::at_git_root(api_paths.root())),
        credential_monitor: None,
        global_config: std::sync::Arc::new(Default::default()),
    }
}

// ─── Run the snapshot through the real command ────────────────────────────────

async fn run_status(runtime: Arc<dyn AgentRuntimeEngine>) -> AgentsStatusOutcome {
    let matches = awman::command::dispatch::catalogue::CommandCatalogue::get()
        .build_clap_command()
        .try_get_matches_from(["awman", "agents", "status"])
        .unwrap();
    let cmd = AgentsStatusCommand::new(AgentsStatusFlags::default(), runtime);
    cmd.run_with_frontend(Box::new(awman::frontend::cli::CliFrontend::new(matches)))
        .await
        .expect("agents status command must succeed")
}

#[tokio::test]
async fn status_marks_squad_container_and_leaves_session_container_unmarked() {
    let squad_name = generate_squad_container_name("issue-triage");
    let runtime = Arc::new(FakeRuntime {
        handles: vec![handle(&squad_name), handle("awman-1234-5678")],
    });

    let outcome = run_status(runtime).await;
    assert_eq!(outcome.overview.agents.len(), 2);

    let squad_row = outcome
        .overview
        .agents
        .iter()
        .find(|c| c.name == squad_name)
        .expect("squad container row must be present");
    assert_eq!(
        squad_row.source,
        AgentSource::Squad("issue-triage".to_string())
    );
    assert_eq!(squad_row.source_cell(), "squad:issue-triage");
    assert_eq!(squad_row.state, AgentState::Running);

    let session_row = outcome
        .overview
        .agents
        .iter()
        .find(|c| c.name == "awman-1234-5678")
        .expect("session container row must be present");
    assert_eq!(
        session_row.source,
        AgentSource::Session,
        "a plain session container must render exactly as today — unmarked as squad"
    );
    assert_eq!(session_row.source_cell(), "session");
}

#[tokio::test]
async fn status_renders_squad_marker_in_the_dashboard_text() {
    // Exercise the real TUI frontend and dashboard renderer, since status
    // rendering now belongs to frontends rather than a Layer 2 default.
    use awman::command::dispatch::catalogue::CommandCatalogue;
    use awman::data::config::env::EnvSnapshot;
    use awman::data::session_manager::SessionManager;
    use awman::engine::agent_runtime::frontend::AgentIo;
    use awman::frontend::tui::app::App;
    use awman::frontend::tui::command_frontend::{DialogChannels, TuiCommandFrontend};
    use awman::frontend::tui::tabs::Tab;

    let squad_name = generate_squad_container_name("nightly-audit");
    let runtime = Arc::new(FakeRuntime {
        handles: vec![handle(&squad_name)],
    });
    let tmp = tempfile::tempdir().unwrap();
    let session = helpers::session_at(tmp.path());
    let tab = Tab::new(session);
    let shared = tab.shared();
    let (dialog_tx, _dialog_rx) = std::sync::mpsc::channel();
    let (_response_tx, response_rx) = std::sync::mpsc::channel();
    let (stdout, _stdout_rx) = tokio::sync::mpsc::unbounded_channel();
    let (stderr, _stderr_rx) = tokio::sync::mpsc::unbounded_channel();
    let (stdin_tx, stdin_rx) = tokio::sync::mpsc::unbounded_channel();
    let catalogue = CommandCatalogue::get();
    let parsed = awman::command::dispatch::parsed_input::parse("agents status", catalogue).unwrap();
    let frontend = TuiCommandFrontend::new(
        parsed,
        DialogChannels::new(dialog_tx, response_rx),
        AgentIo {
            stdout,
            stderr,
            stdin_tx,
            stdin_rx,
            resize: None,
            initial_size: None,
        },
        shared,
    );
    let cmd = AgentsStatusCommand::new(AgentsStatusFlags::default(), runtime.clone());
    cmd.run_with_frontend(Box::new(frontend)).await.unwrap();
    let mut app = App::new(
        catalogue,
        engines_with(runtime),
        Arc::new(SessionManager::in_memory(EnvSnapshot::empty())),
        tab,
        tokio::runtime::Handle::current(),
    );
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(160, 40)).unwrap();
    terminal
        .draw(|frame| awman::frontend::tui::render::render_frame(&mut app, frame))
        .unwrap();
    let buffer = terminal.backend().buffer();
    let combined = buffer
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(
        combined.contains("squad:nightly-audit"),
        "dashboard text must contain the squad marker: {combined}"
    );
}

// ─── Live-backend tiers ──────────────────────────────────────────────────────
//
// The fake-runtime tests above prove the classification and rendering. These
// prove the same marker survives a round trip through a *real* backend's
// `list_running_all()`: each tier plants one squad-named container and one
// plain session-named container with a raw CLI call, then asserts `awman
// agents status` marks exactly the first. Skips cleanly when the backend or the
// image is unavailable.

use std::process::{Command as OsCommand, Stdio};

fn unique_suffix() -> String {
    format!(
        "{:x}",
        (std::process::id() as u128) << 32
            | u128::from(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .subsec_nanos()
            )
    )
}

fn cli_ok(cli: &str, args: &[&str]) -> bool {
    OsCommand::new(cli)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn remove_container(cli: &str, name: &str) {
    let _ = cli_ok(cli, &["rm", "-f", name]);
}

/// Plant one squad container and one session container on a live backend and
/// assert `awman agents status` marks exactly the squad one.
async fn assert_status_marks_squad_on(cli: &str, runtime: Arc<ContainerRuntime>) {
    let suffix = unique_suffix();
    let slug = format!("audit-{suffix}");
    let squad_name = generate_squad_container_name(&slug);
    let session_name = format!("awman-{suffix}-0");

    for name in [&squad_name, &session_name] {
        if !cli_ok(
            cli,
            &["run", "-d", "--name", name, "alpine:latest", "sleep", "120"],
        ) {
            remove_container(cli, &squad_name);
            remove_container(cli, &session_name);
            eprintln!("SKIP: `{cli} run` failed for the {name} fixture container");
            return;
        }
    }

    let outcome = run_status(runtime).await;
    remove_container(cli, &squad_name);
    remove_container(cli, &session_name);

    let squad_row = outcome
        .overview
        .agents
        .iter()
        .find(|c| c.name == squad_name)
        .unwrap_or_else(|| {
            panic!("`awman agents status` must list the squad container {squad_name}")
        });
    assert_eq!(
        squad_row.source,
        AgentSource::Squad(slug.clone()),
        "a squad-launched container must render as squad:{slug}"
    );
    assert_eq!(squad_row.source_cell(), format!("squad:{slug}"));

    let session_row = outcome
        .overview
        .agents
        .iter()
        .find(|c| c.name == session_name)
        .unwrap_or_else(|| {
            panic!("`awman agents status` must list the session container {session_name}")
        });
    assert_eq!(
        session_row.source,
        AgentSource::Session,
        "a user session's container must render exactly as it does today"
    );
    assert_eq!(session_row.source_cell(), "session");
}

// ─── Docker tier (skip cleanly if unavailable) ──────────────────────────────

#[tokio::test]
async fn docker_status_marks_squad_containers_when_backend_is_available() {
    let runtime = Arc::new(ContainerRuntime::docker());
    if !runtime.is_available() {
        eprintln!("SKIP: Docker daemon not available");
        return;
    }
    if !cli_ok("docker", &["pull", "alpine:latest"]) {
        eprintln!("SKIP: docker pull alpine:latest failed (no network?)");
        return;
    }
    assert_status_marks_squad_on("docker", runtime).await;
}

// ─── Apple tier (skip cleanly if unavailable) ───────────────────────────────

#[tokio::test]
async fn apple_status_marks_squad_containers_when_backend_is_available() {
    let runtime = Arc::new(ContainerRuntime::apple());
    if !runtime.is_available() {
        eprintln!("SKIP: Apple Containers not available");
        return;
    }
    if !cli_ok("container", &["pull", "alpine:latest"]) {
        eprintln!("SKIP: `container pull alpine:latest` failed");
        return;
    }
    assert_status_marks_squad_on("container", runtime).await;
}
