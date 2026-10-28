use super::super::*;

use std::collections::HashMap;
use std::sync::Arc;

use crate::command::commands::api_server::event_bus::EventBus;
use crate::command::dispatch::Engines;
use crate::data::fs::api_command_log::CommandStateWriter;
use crate::data::fs::api_db::{NewSessionRow, SqliteSessionStore};
use crate::data::fs::api_paths::ApiPaths;
use crate::data::session::SessionKind;
use crate::data::session_setup_event::SessionSetupStatus;
use crate::data::workflow_definition::{SetupStep, TeardownStep, Workflow, WorkflowStep};
use crate::data::workflow_state::{PhaseKind, PhaseStepStatus, WorkflowState};
use crate::engine::agent_runtime::background::{AgentExec, ExecOutput};
use crate::engine::agent_runtime::execution::{AgentExecution, AgentExitInfo};
use crate::engine::workflow::actions::WorkflowOutcome;
use crate::engine::workflow::factory::{AgentExecutionFactory, WorkflowRuntimeContext};
use crate::engine::workflow::{WorkflowEngine, WorkflowEngineDeps, WorkflowSpec};
use crate::frontend::api::routes::{build_router, AppState, AuthMode};

fn app_state(root: &std::path::Path) -> Arc<AppState> {
    let paths = ApiPaths::from_root(root);
    paths.ensure_root().unwrap();
    let store = SqliteSessionStore::open(paths.root()).unwrap();
    let auth_paths = crate::data::fs::auth_paths::AuthPathResolver::at_home(root);
    let runtime = Arc::new(crate::engine::container::ContainerRuntime::docker());
    let git_engine = Arc::new(crate::engine::git::GitEngine::new());
    let overlay_engine = Arc::new(crate::engine::overlay::OverlayEngine::with_auth_resolver(
        auth_paths.clone(),
    ));
    let agent_engine = Arc::new(crate::engine::agent::AgentEngine::new(
        overlay_engine.clone(),
        runtime.clone(),
    ));
    let auth_engine = Arc::new(crate::engine::auth::AuthEngine::with_paths(
        auth_paths,
        paths.clone(),
    ));
    let engines = Engines {
        runtime: runtime.clone(),
        container_runtime: Some(runtime),
        sandbox_runtime: None,
        git_engine,
        overlay_engine,
        auth_engine,
        agent_engine,
        workflow_state_store: Arc::new(crate::data::WorkflowStateStore::at_git_root(paths.root())),
        credential_monitor: None,
        global_config: Arc::new(Default::default()),
    };
    Arc::new(AppState {
        store: Arc::new(store),
        paths,
        workdirs: vec![],
        workdir_roots: Vec::new(),
        started_at: std::time::Instant::now(),
        task_handles: tokio::sync::Mutex::new(Vec::new()),
        auth_mode: AuthMode::Disabled,
        engines,
        sessions: Arc::new(crate::data::session_manager::SessionManager::in_memory(
            crate::data::config::env::EnvSnapshot::empty(),
        )),
        event_buses: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        setup_buses: tokio::sync::Mutex::new(HashMap::new()),
    })
}

async fn serve_router(state: Arc<AppState>) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    let app = build_router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (addr, handle)
}

struct InstantFactory;

impl AgentExecutionFactory for InstantFactory {
    fn execution_for_step(
        &self,
        _: &WorkflowStep,
        _: &crate::data::session::Session,
        _: &WorkflowRuntimeContext,
    ) -> Result<AgentExecution, crate::engine::error::EngineError> {
        let now = chrono::Utc::now();
        Ok(AgentExecution::finished(
            crate::data::session::AgentHandle {
                id: "agent-id".into(),
                image_tag: "test".into(),
                name: "fake".into(),
                started_at: now,
            },
            AgentExitInfo {
                exit_code: 0,
                signal: None,
                started_at: now,
                ended_at: now,
            },
        ))
    }

    fn inject_prompt(
        &self,
        _: &AgentExecution,
        _: &str,
    ) -> Result<Option<()>, crate::engine::error::EngineError> {
        Ok(None)
    }
}

struct Shell;

impl AgentExec for Shell {
    fn exec(
        &self,
        _: &str,
        _: Option<&HashMap<String, String>>,
    ) -> Result<ExecOutput, crate::engine::error::EngineError> {
        Ok(ExecOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
        })
    }
}

fn step(name: &str) -> WorkflowStep {
    WorkflowStep {
        name: name.into(),
        depends_on: vec![],
        prompt_template: "do work".into(),
        agent: None,
        model: None,
        overlays: None,
        abort_on_failure: false,
    }
}

#[tokio::test]
async fn api_frontend_mirrors_live_engine_state_and_phase_events() {
    let tmp = tempfile::tempdir().unwrap();
    let worktree = tmp.path().join("worktree");
    std::fs::create_dir_all(&worktree).unwrap();
    let app = app_state(&tmp.path().join("api"));
    app.store
        .insert_session_full(NewSessionRow {
            id: "session",
            workdir: worktree.to_str().unwrap(),
            created_at: &chrono::Utc::now().to_rfc3339(),
            setup_status: SessionSetupStatus::Ready,
            kind: SessionKind::Local,
            cloned_path: None,
        })
        .unwrap();
    app.store
        .enqueue_command("command", "session", "exec workflow", "[]", "/log")
        .unwrap();
    let (addr, server) = serve_router(app.clone()).await;
    let endpoint = format!("http://{addr}/v1/workflows/command");
    let client = reqwest::Client::new();
    let api_paths = app.paths.clone();
    let command_dir = api_paths.command_dir("session", "command");
    std::fs::create_dir_all(&command_dir).unwrap();

    let bus = EventBus::new(128);
    let mut events = bus.subscribe();
    let frontend =
        ApiDispatchFrontend::new("exec workflow", &["workflow.toml".into()], bus.sender())
            .with_state_writer(CommandStateWriter::new(
                api_paths.clone(),
                "session",
                "command",
            ));

    let workflow = Workflow {
        title: Some("live-state".into()),
        agent: Some("claude".into()),
        model: None,
        overlays: None,
        steps: vec![step("agent")],
        setup: vec![],
        teardown: vec![],
        teardown_on_failure: false,
    };
    let session = crate::data::session::Session::for_tests(&worktree);
    let mut engine = WorkflowEngine::new(
        &session,
        WorkflowSpec::new(workflow),
        WorkflowEngineDeps {
            frontend: Box::new(frontend),
            agent_factory: Box::new(InstantFactory),
        },
    )
    .unwrap();
    engine.set_yolo(true);

    let setup = vec![SetupStep::RunShell {
        command: "setup".into(),
        env: None,
    }];
    let teardown = vec![TeardownStep::RunShell {
        command: "teardown".into(),
        env: None,
    }];
    engine.declare_phase_steps(&setup, &teardown).unwrap();
    let read_state = || -> WorkflowState {
        serde_json::from_str(
            &api_paths
                .read_command_workflow_state_raw("session", "command")
                .unwrap()
                .unwrap(),
        )
        .unwrap()
    };
    let initial: WorkflowState = client
        .get(&endpoint)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        initial.current_phase,
        crate::data::workflow_state::WorkflowPhase::Setup
    );
    assert_eq!(
        initial.step_states["agent"],
        crate::data::workflow_state::StepState::Pending
    );
    assert_eq!(
        initial.setup_step_states[0].status,
        PhaseStepStatus::Pending
    );
    assert_eq!(
        initial.teardown_step_states[0].status,
        PhaseStepStatus::Pending
    );

    engine
        .run_phase(PhaseKind::Setup, &setup, &[], &[], |_| Ok(Box::new(Shell)))
        .unwrap();
    let after_setup: WorkflowState = client
        .get(&endpoint)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        after_setup.setup_step_states[0].status,
        PhaseStepStatus::Succeeded
    );
    assert_eq!(
        engine.run_to_completion().await.unwrap(),
        WorkflowOutcome::Completed
    );
    engine
        .run_phase(PhaseKind::Teardown, &teardown, &[], &[], |_| {
            Ok(Box::new(Shell))
        })
        .unwrap();
    engine.mark_done().unwrap();

    let phase_events: Vec<_> = std::iter::from_fn(|| events.try_recv().ok())
        .filter_map(|event| match event.payload {
            crate::data::execution_event::EventPayload::WorkflowPhaseTransition {
                phase,
                step_desc,
                status,
            } if phase == "setup" || phase == "teardown" => Some((phase, step_desc, status)),
            _ => None,
        })
        .collect();
    assert_eq!(
        phase_events
            .iter()
            .map(|(phase, _, status)| (phase.as_str(), *status))
            .collect::<Vec<_>>(),
        [
            (
                "setup",
                crate::data::execution_event::PhaseStatusKind::Running
            ),
            (
                "setup",
                crate::data::execution_event::PhaseStatusKind::Succeeded
            ),
            (
                "teardown",
                crate::data::execution_event::PhaseStatusKind::Running
            ),
            (
                "teardown",
                crate::data::execution_event::PhaseStatusKind::Succeeded
            ),
        ]
    );
    let final_state = read_state();
    assert!(final_state.setup_completed && final_state.teardown_completed);
    assert!(final_state
        .setup_step_states
        .iter()
        .chain(&final_state.teardown_step_states)
        .all(|step| step.status == PhaseStepStatus::Succeeded));
    for (_, desc, _) in &phase_events {
        assert!(final_state
            .setup_step_states
            .iter()
            .chain(&final_state.teardown_step_states)
            .any(|step| &step.description == desc));
    }

    drop(engine);
    std::fs::remove_dir_all(&worktree).unwrap();
    assert_eq!(read_state(), final_state);
    let after_remove = client.get(&endpoint).send().await.unwrap();
    assert_eq!(after_remove.status(), 200);
    let served: WorkflowState = after_remove.json().await.unwrap();
    assert_eq!(served, final_state);
    server.abort();
}
