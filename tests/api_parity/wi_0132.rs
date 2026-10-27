//! WI 0132 API workflow-state contract regression tests.

use std::sync::Arc;
use std::time::Instant;

use awman::data::fs::api_db::{NewSessionRow, SqliteSessionStore};
use awman::data::fs::api_paths::ApiPaths;
use awman::data::fs::auth_paths::AuthPathResolver;
use awman::data::session::SessionKind;
use awman::data::session_setup_event::SessionSetupStatus;
use awman::data::workflow_definition::WorkflowStep;
use awman::data::workflow_state::{PhaseStepStatus, StepState, WorkflowState};
use awman::data::workflow_state_store::WorkflowStateStore;
use awman::engine::container::ContainerRuntime;
use awman::engine::git::GitEngine;
use awman::engine::overlay::OverlayEngine;
use awman::frontend::api::routes::{build_router, AppState, AuthMode};

fn test_step(name: &str) -> WorkflowStep {
    WorkflowStep {
        name: name.into(),
        depends_on: vec![],
        prompt_template: "work".into(),
        agent: None,
        model: None,
        overlays: None,
        abort_on_failure: false,
    }
}

fn sample_state() -> WorkflowState {
    WorkflowState::new(
        "wi0132".into(),
        &[test_step("agent")],
        "test-hash".into(),
        None,
    )
}

fn app_state(root: &std::path::Path) -> Arc<AppState> {
    let paths = ApiPaths::from_root(root);
    paths.ensure_root().unwrap();
    let store = SqliteSessionStore::open(paths.root()).unwrap();
    let auth_paths = AuthPathResolver::at_home(root);
    let runtime = Arc::new(ContainerRuntime::docker());
    let git_engine = Arc::new(GitEngine::new());
    let overlay_engine = Arc::new(OverlayEngine::with_auth_resolver(auth_paths.clone()));
    let agent_engine = Arc::new(awman::engine::agent::AgentEngine::new(
        overlay_engine.clone(),
        runtime.clone(),
    ));
    let auth_engine = Arc::new(awman::engine::auth::AuthEngine::with_paths(
        auth_paths,
        paths.clone(),
    ));
    let engines = awman::command::dispatch::Engines {
        runtime: runtime.clone(),
        container_runtime: Some(runtime),
        sandbox_runtime: None,
        git_engine,
        overlay_engine,
        auth_engine,
        agent_engine,
        workflow_state_store: Arc::new(WorkflowStateStore::at_git_root(paths.root())),
        credential_monitor: None,
        global_config: Arc::new(Default::default()),
    };
    Arc::new(AppState {
        store: Arc::new(store),
        paths,
        workdirs: vec![],
        started_at: Instant::now(),
        task_handles: tokio::sync::Mutex::new(Vec::new()),
        auth_mode: AuthMode::Disabled,
        engines,
        sessions: Arc::new(awman::data::session_manager::SessionManager::in_memory(
            awman::data::config::env::EnvSnapshot::empty(),
        )),
        event_buses: Arc::new(tokio::sync::Mutex::new(Default::default())),
        setup_buses: tokio::sync::Mutex::new(Default::default()),
    })
}

async fn serve_router(state: Arc<AppState>) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    let app = build_router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (addr, task)
}

#[test]
fn command_workflow_state_write_is_atomic_and_leaves_no_temp_file() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = ApiPaths::from_root(tmp.path());
    let command_dir = paths.command_dir("s", "c");
    std::fs::create_dir_all(&command_dir).unwrap();
    let mut state = sample_state();
    state.workflow_hash = "a".repeat(512 * 1024);
    paths
        .write_command_workflow_state("s", "c", &state)
        .unwrap();

    // An open reader must retain the old complete document after replacement.
    // A truncate-and-write implementation fails this deterministically.
    use std::io::Read;
    let mut old_file = std::fs::File::open(paths.command_workflow_state_path("s", "c")).unwrap();
    let old_state = state.clone();
    state.workflow_hash = "b".repeat(512 * 1024);
    paths
        .write_command_workflow_state("s", "c", &state)
        .unwrap();
    let mut old_bytes = String::new();
    old_file.read_to_string(&mut old_bytes).unwrap();
    assert_eq!(
        serde_json::from_str::<WorkflowState>(&old_bytes).unwrap(),
        old_state
    );

    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let reader_done = done.clone();
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let reader_barrier = barrier.clone();
    let path = paths.command_workflow_state_path("s", "c");
    let reader = std::thread::spawn(move || {
        let bytes = std::fs::read(&path).unwrap();
        let parsed: WorkflowState =
            serde_json::from_slice(&bytes).expect("reader saw a partial initial JSON file");
        assert_eq!(parsed.workflow_hash.len(), 512 * 1024);
        let mut reads = 1;
        reader_barrier.wait();
        while !reader_done.load(std::sync::atomic::Ordering::Acquire) {
            let bytes = std::fs::read(&path).unwrap();
            let parsed: WorkflowState =
                serde_json::from_slice(&bytes).expect("reader saw a partial JSON file");
            assert_eq!(parsed.workflow_hash.len(), 512 * 1024);
            reads += 1;
        }
        reads
    });
    barrier.wait();
    for ch in ['b', 'c', 'd', 'e'] {
        state.workflow_hash = std::iter::repeat_n(ch, 512 * 1024).collect();
        paths
            .write_command_workflow_state("s", "c", &state)
            .unwrap();
    }
    done.store(true, std::sync::atomic::Ordering::Release);
    assert!(reader.join().unwrap() > 0);
    assert!(paths.command_workflow_state_path("s", "c").exists());
    assert!(!command_dir.join("workflow.state.json.tmp").exists());
}

#[test]
fn step_and_phase_status_wire_shapes_are_externally_tagged() {
    for (status, expected) in [
        (StepState::Pending, serde_json::json!("Pending")),
        (StepState::Cancelled, serde_json::json!("Cancelled")),
        (StepState::Skipped, serde_json::json!("Skipped")),
        (
            StepState::Running {
                container_id: Some("id".into()),
            },
            serde_json::json!({"Running":{"container_id":"id"}}),
        ),
        (
            StepState::Failed {
                exit_code: 1,
                error_message: Some("bad".into()),
            },
            serde_json::json!({"Failed":{"exit_code":1,"error_message":"bad"}}),
        ),
    ] {
        assert_eq!(serde_json::to_value(status).unwrap(), expected);
    }
    for (status, expected) in [
        (PhaseStepStatus::Running, serde_json::json!("Running")),
        (PhaseStepStatus::Succeeded, serde_json::json!("Succeeded")),
        (
            PhaseStepStatus::Remediating { attempt: 1, of: 2 },
            serde_json::json!({"Remediating":{"attempt":1,"of":2}}),
        ),
    ] {
        assert_eq!(serde_json::to_value(status).unwrap(), expected);
    }
    assert_eq!(
        serde_json::to_value(StepState::Succeeded).unwrap(),
        serde_json::json!("Succeeded")
    );
    assert_eq!(
        serde_json::to_value(StepState::Running { container_id: None }).unwrap(),
        serde_json::json!({"Running":{"container_id":null}})
    );
    assert_eq!(
        serde_json::to_value(PhaseStepStatus::Pending).unwrap(),
        serde_json::json!("Pending")
    );
    assert_eq!(
        serde_json::to_value(PhaseStepStatus::Failed {
            error: "bad".into()
        })
        .unwrap(),
        serde_json::json!({"Failed":{"error":"bad"}})
    );
}

#[tokio::test]
async fn status_advertises_workflow_state_feature_and_queued_exec_says_not_started() {
    let tmp = tempfile::tempdir().unwrap();
    let app = app_state(tmp.path());
    app.store
        .insert_session_full(NewSessionRow {
            id: "session",
            workdir: "/work",
            created_at: &chrono::Utc::now().to_rfc3339(),
            setup_status: SessionSetupStatus::Ready,
            kind: SessionKind::Local,
            cloned_path: None,
        })
        .unwrap();
    app.store
        .enqueue_command("queued-exec", "session", "exec workflow", "[]", "/l")
        .unwrap();
    app.store
        .enqueue_command("queued-prompt", "session", "exec prompt", "[]", "/l")
        .unwrap();
    app.store
        .enqueue_command("alias", "session", "exec wf", "[]", "/l")
        .unwrap();
    for status in ["done", "error", "aborted", "cancelled"] {
        app.store
            .enqueue_command(status, "session", "exec workflow", "[]", "/l")
            .unwrap();
        app.store
            .update_command_finished(status, status, Some(1), "now")
            .unwrap();
    }
    let (addr, server) = serve_router(app).await;
    let client = reqwest::Client::new();
    for id in ["alias", "done", "error", "aborted", "cancelled"] {
        let response = client
            .get(format!("http://{addr}/v1/workflows/{id}"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 404);
        assert_eq!(
            response.json::<serde_json::Value>().await.unwrap()["error"],
            if id == "alias" {
                "workflow not started"
            } else {
                "workflow never started"
            }
        );
    }
    let status: serde_json::Value = client
        .get(format!("http://{addr}/v1/status"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(status["features"]
        .as_array()
        .unwrap()
        .iter()
        .any(|f| f == "workflow_state"));
    let response = client
        .get(format!("http://{addr}/v1/workflows/queued-exec"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 404);
    assert_eq!(
        response.json::<serde_json::Value>().await.unwrap()["error"],
        "workflow not started"
    );

    let unknown = client
        .get(format!("http://{addr}/v1/workflows/unknown-id"))
        .send()
        .await
        .unwrap();
    assert_eq!(unknown.status(), 404);
    assert_eq!(
        unknown.json::<serde_json::Value>().await.unwrap()["error"],
        "command not found"
    );
    let other = client
        .get(format!("http://{addr}/v1/workflows/queued-prompt"))
        .send()
        .await
        .unwrap();
    assert_eq!(other.status(), 404);
    assert_eq!(
        other.json::<serde_json::Value>().await.unwrap()["error"],
        "no workflow for this command"
    );
    server.abort();
}

#[tokio::test]
async fn remote_workflow_poller_reads_real_router_and_maps_phase_columns() {
    use awman::command::commands::remote_client::{RemoteApiWorkflowSource, RemoteWorkflowPoller};
    use awman::engine::remote::client::RemoteClient;
    use awman::frontend::tui::tabs::WorkflowStepKind;
    use awman::frontend::tui::workflow_view::workflow_state_to_view_state;
    use tokio_util::sync::CancellationToken;

    let tmp = tempfile::tempdir().unwrap();
    let app = app_state(tmp.path());
    let worktree = tmp.path().join("worktree");
    std::fs::create_dir_all(&worktree).unwrap();
    app.store
        .insert_session_full(NewSessionRow {
            id: "poll-session",
            workdir: worktree.to_str().unwrap(),
            created_at: &chrono::Utc::now().to_rfc3339(),
            setup_status: SessionSetupStatus::Ready,
            kind: SessionKind::Local,
            cloned_path: None,
        })
        .unwrap();
    app.store
        .enqueue_command("poll-command", "poll-session", "exec workflow", "[]", "/l")
        .unwrap();
    let mut state = sample_state();
    state.setup_step_states = vec![awman::data::workflow_state::PhaseStepState {
        description: "prepare".into(),
        status: PhaseStepStatus::Succeeded,
    }];
    state.teardown_step_states = vec![awman::data::workflow_state::PhaseStepState {
        description: "cleanup".into(),
        status: PhaseStepStatus::Pending,
    }];
    app.paths.prepare_session_dirs("poll-session").unwrap();
    std::fs::create_dir_all(app.paths.command_dir("poll-session", "poll-command")).unwrap();
    app.paths
        .write_command_workflow_state("poll-session", "poll-command", &state)
        .unwrap();
    std::fs::remove_dir_all(&worktree).unwrap();
    let (addr, server) = serve_router(app).await;
    let client = Arc::new(RemoteClient::new(&format!("http://{addr}"), None).unwrap());
    let source = Arc::new(RemoteApiWorkflowSource::new(client, "poll-command"));
    let published = Arc::new(std::sync::Mutex::new(None));
    let published_cb = published.clone();
    let cancel = CancellationToken::new();
    let cancel_cb = cancel.clone();
    let task = RemoteWorkflowPoller::new(
        source,
        Box::new(move |snapshot| {
            *published_cb.lock().unwrap() = Some(workflow_state_to_view_state(snapshot));
            cancel_cb.cancel();
        }),
    )
    .start(cancel);
    tokio::time::timeout(std::time::Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap();
    let view = published
        .lock()
        .unwrap()
        .clone()
        .expect("poller published the router snapshot");
    assert_eq!(
        view.steps.iter().map(|step| step.kind).collect::<Vec<_>>(),
        [
            WorkflowStepKind::Setup,
            WorkflowStepKind::Agent,
            WorkflowStepKind::Teardown,
        ]
    );
    server.abort();
}
