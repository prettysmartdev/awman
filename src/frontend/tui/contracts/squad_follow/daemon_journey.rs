//! Real Squad key dispatch -> Dispatch -> AttachCommand -> terminal sessions
//! and WebSocket client -> production TUI bridge/emulator. Only the task
//! gateway and the terminal producer are doubles; no host agent is invoked.
use super::*;
use crate::command::commands::remote_client::{SquadTaskWorkflowSource, WorkflowStateSource};
use crate::command::commands::squad::attach::{
    pin_current_run, AttachStartupPolicy, FollowEvent, RunFollower, TaskAttachEnd,
};
use crate::command::commands::squad::gateway::{CreateTask, DaemonStatus, UpdateTask};
use crate::command::terminals::{AccessPolicy, OwnerEventLog, OwnerResolver, TerminalOperations};
use crate::data::daemon_state::FeatureName;
use crate::data::execution_event::{EventPayload, EventSource, EventSourcePhase};
use crate::data::fs::{Run, RunStatus, Task, TaskStatus};
use crate::data::ids::{RunId, TaskId, TerminalId};
use crate::data::terminal::{
    TerminalClientFrame, TerminalFrame, TerminalOwner, TerminalOwnerInfo, ViewerIdentity,
    TERMINAL_SUBPROTOCOL,
};
use crate::data::workflow_state::{PhaseStepState, PhaseStepStatus, WorkflowPhase, WorkflowState};
use crate::engine::remote::terminal_client::{TerminalClientOptions, TerminalStream};
use crate::engine::terminal_hub::{
    input_channel, SubscribeOptions, TerminalHandle, TerminalHub, TerminalInput,
    TerminalRegistration,
};
use async_trait::async_trait;
use axum::extract::{
    ws::{Message, WebSocketUpgrade},
    Path, Query, State,
};
use axum::response::Response;
use futures_util::SinkExt;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Duration;

struct Events;
impl OwnerEventLog for Events {
    fn append(&self, _: &TerminalOwner, _: EventPayload) {}
}
impl OwnerResolver for Events {
    fn active_run(&self, _: &TaskId) -> Option<RunId> {
        None
    }
}

struct Daemon {
    ops: Arc<TerminalOperations>,
    clients: AtomicUsize,
    frames: Mutex<Vec<(TerminalId, TerminalClientFrame)>>,
    streams: Mutex<CancellationToken>,
}

async fn terminal(
    State(daemon): State<Arc<Daemon>>,
    Path(id): Path<String>,
    Query(query): Query<HashMap<String, String>>,
    ws: WebSocketUpgrade,
) -> Response {
    let id = TerminalId::parse(&id).unwrap();
    let mut viewer = ViewerIdentity::local();
    viewer.read_only = query.get("read_only").is_some_and(|v| v == "true");
    let mut session = daemon
        .ops
        .open_with(
            &id,
            viewer,
            SubscribeOptions {
                history: query.get("history").is_some_and(|v| v == "true"),
            },
        )
        .unwrap();
    let stop = daemon.streams.lock().unwrap().clone();
    ws.protocols([TERMINAL_SUBPROTOCOL]).on_upgrade(move |mut socket| async move {
        daemon.clients.fetch_add(1, Ordering::SeqCst);
        loop {
            tokio::select! {
                biased;
                _ = stop.cancelled() => break,
                incoming = socket.recv() => {
                    let Some(Ok(Message::Text(text))) = incoming else { break; };
                    let frame: TerminalClientFrame = serde_json::from_str(&text).unwrap();
                    daemon.frames.lock().unwrap().push((id.clone(), frame.clone()));
                    match session.handle_client_frame(frame) {
                        Ok(response) => {
                            for frame in response.frames {
                                if socket.send(Message::Text(serde_json::to_string(&frame).unwrap().into())).await.is_err() { break; }
                            }
                            if response.detached { break; }
                        }
                        Err(_) => panic!("the client sent a frame refused by terminal policy"),
                    }
                }
                frame = session.next_frame() => {
                    let Some(frame) = frame else { break; };
                    let exited = matches!(frame, TerminalFrame::Exit { .. });
                    if socket.send(Message::Text(serde_json::to_string(&frame).unwrap().into())).await.is_err() || exited { break; }
                }
            }
        }
        session.detach(crate::data::terminal::DetachReason::User);
        let _ = socket.close().await;
        daemon.clients.fetch_sub(1, Ordering::SeqCst);
    })
}

struct GatewayState {
    run: Run,
    workflow: Option<WorkflowState>,
    failed: bool,
    replace_during_terminals: bool,
    replace_during_workflow: bool,
    opens: Vec<(TerminalId, TerminalClientOptions)>,
}
struct Gateway {
    task: Task,
    state: Mutex<GatewayState>,
    hub: Arc<TerminalHub>,
    endpoint: PathBuf,
}
impl Gateway {
    fn check(&self) -> Result<(), CommandError> {
        if self.state.lock().unwrap().failed {
            Err(CommandError::DaemonNotRunning)
        } else {
            Ok(())
        }
    }
    fn run_id(&self) -> RunId {
        self.state.lock().unwrap().run.id.clone()
    }
    fn register(
        &self,
        step: Option<&str>,
    ) -> (TerminalHandle, tokio::sync::mpsc::Receiver<Vec<u8>>) {
        let (stdin, rx) = input_channel();
        let mut source = EventSource::phase(if step.is_none() {
            EventSourcePhase::Leader
        } else {
            EventSourcePhase::Main
        });
        source.step_name = step.map(str::to_owned);
        source.container = Some(step.unwrap_or("leader").into());
        let handle = self.hub.register(
            TerminalRegistration {
                automation: None,
                owner: TerminalOwnerInfo {
                    owner: TerminalOwner::Run(self.run_id()),
                    feature: FeatureName::Squad,
                    source: Some(source),
                    attempt: 1,
                    allow_remote_input: false,
                },
                agent: "fixture".into(),
                model: None,
                cols: 80,
                rows: 6,
            },
            TerminalInput {
                stdin,
                resize: None,
            },
        );
        (handle, rx)
    }
}
#[async_trait]
impl TaskGateway for Gateway {
    async fn get(&self, _: &str) -> Result<Task, CommandError> {
        self.check()?;
        Ok(self.task.clone())
    }
    async fn list(&self) -> Result<Vec<Task>, CommandError> {
        self.check()?;
        Ok(vec![self.task.clone()])
    }
    async fn runs(&self, _: &str, _: usize) -> Result<Vec<Run>, CommandError> {
        self.check()?;
        Ok(vec![self.state.lock().unwrap().run.clone()])
    }
    async fn workflow_state(&self, _: &str) -> Result<Option<WorkflowState>, CommandError> {
        self.check()?;
        let mut state = self.state.lock().unwrap();
        let workflow = state.workflow.clone();
        if state.replace_during_workflow {
            state.run.id = RunId::new();
        }
        Ok(workflow)
    }
    async fn run_terminals(
        &self,
        run: &RunId,
    ) -> Result<Vec<crate::data::terminal::TerminalRecord>, CommandError> {
        self.check()?;
        let records = self.hub.list(Some(&TerminalOwner::Run(run.clone())));
        let mut state = self.state.lock().unwrap();
        if state.replace_during_terminals {
            state.run.id = RunId::new();
        }
        Ok(records)
    }
    async fn open_terminal(
        &self,
        id: &TerminalId,
        options: &TerminalClientOptions,
        _: &EnvSnapshot,
    ) -> Result<TerminalStream, CommandError> {
        self.check()?;
        self.state
            .lock()
            .unwrap()
            .opens
            .push((id.clone(), options.clone()));
        Ok(TerminalStream::connect_local(&self.endpoint, id, options).await?)
    }
    async fn create(&self, _: CreateTask) -> Result<Task, CommandError> {
        unreachable!()
    }
    async fn update(&self, _: &str, _: UpdateTask) -> Result<Task, CommandError> {
        unreachable!()
    }
    async fn set_status(&self, _: &str, _: TaskStatus) -> Result<(), CommandError> {
        unreachable!()
    }
    async fn trigger(&self, _: &str) -> Result<(), CommandError> {
        unreachable!()
    }
    async fn cancel(&self, _: &str) -> Result<(), CommandError> {
        panic!("detach must never cancel the daemon run")
    }
    async fn delete(&self, _: &str) -> Result<(), CommandError> {
        unreachable!()
    }
    async fn status(&self) -> Result<DaemonStatus, CommandError> {
        unreachable!()
    }
}

struct Fixture {
    sandbox: TestSandbox,
    gateway: Arc<Gateway>,
    daemon: Arc<Daemon>,
    stop: CancellationToken,
    server: tokio::task::JoinHandle<()>,
}
impl Fixture {
    async fn new(test: &'static str) -> Self {
        Self::with_hub(test, Default::default()).await
    }
    /// hygiene-allow(H4): the production TerminalStream client requires a private Unix socket. It is below sandbox.sock_dir(); finish cancels and joins its server, waits for all owned viewers and removes the socket directory.
    async fn with_hub(
        test: &'static str,
        hub_config: crate::engine::terminal_hub::TerminalHubConfig,
    ) -> Self {
        use std::os::unix::fs::PermissionsExt;
        let sandbox = TestSandbox::for_test(TestCase::component(test));
        let task: Task = serde_json::from_value(serde_json::json!({
            "id": TaskId::new(), "name": "selected-task", "description": "fixture",
            "repo_scope": sandbox.work(), "mount_scope": "directory", "overlays": [],
            "interval_secs": 60, "status": "active", "created_at": chrono::Utc::now(), "updated_at": chrono::Utc::now()
        })).unwrap();
        let run: Run = serde_json::from_value(serde_json::json!({
            "id": RunId::new(), "task_id": task.id, "status": "running", "started_at": chrono::Utc::now()
        })).unwrap();
        let dir = sandbox.sock_dir().join("f");
        crate::data::fs::private_files::create_private_dir(&dir).unwrap();
        let endpoint = dir.join("terminal.sock");
        let listener = tokio::net::UnixListener::bind(&endpoint).unwrap();
        std::fs::set_permissions(&endpoint, std::fs::Permissions::from_mode(0o600)).unwrap();
        let hub = Arc::new(TerminalHub::new(hub_config));
        let ops = Arc::new(TerminalOperations::new(
            hub.clone(),
            Arc::new(AccessPolicy::default),
            Arc::new(Events),
            Arc::new(Events),
        ));
        let daemon = Arc::new(Daemon {
            ops,
            clients: AtomicUsize::new(0),
            frames: Mutex::new(vec![]),
            streams: Mutex::new(CancellationToken::new()),
        });
        let router = axum::Router::new()
            .route("/v1/terminals/{id}", axum::routing::get(terminal))
            .with_state(daemon.clone());
        let stop = CancellationToken::new();
        let stopped = stop.clone();
        let server = tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(stopped.cancelled_owned())
                .await
                .unwrap();
        });
        let gateway = Arc::new(Gateway {
            task,
            state: Mutex::new(GatewayState {
                run,
                workflow: None,
                failed: false,
                replace_during_terminals: false,
                replace_during_workflow: false,
                opens: vec![],
            }),
            hub,
            endpoint,
        });
        Self {
            sandbox,
            gateway,
            daemon,
            stop,
            server,
        }
    }
    fn app(&self) -> App {
        let app = squad_app(&self.sandbox);
        app.backend.set_squad_gateway(self.gateway.clone());
        app.active_tab()
            .squad
            .as_ref()
            .unwrap()
            .snapshot
            .lock()
            .unwrap()
            .tasks
            .push(self.gateway.task.clone());
        app
    }
    async fn finish(self) {
        self.daemon.streams.lock().unwrap().cancel();
        self.stop.cancel();
        tokio::time::timeout(Duration::from_secs(5), self.server)
            .await
            .unwrap()
            .unwrap();
        awman_test_support::wait_for(
            "owned terminal viewers to close",
            Duration::from_secs(5),
            || (self.daemon.clients.load(Ordering::SeqCst) == 0).then_some(()),
        );
        std::fs::remove_file(&self.gateway.endpoint).unwrap();
        std::fs::remove_dir(self.gateway.endpoint.parent().unwrap()).unwrap();
        self.sandbox.finish();
    }
}

fn wait(app: &mut App, label: &str, predicate: impl Fn(&App) -> bool) {
    awman_test_support::wait_for(label, Duration::from_secs(8), || {
        app.tick_all_tabs();
        predicate(app).then_some(())
    });
}
fn status(app: &App) -> String {
    app.active_tab()
        .shared
        .attach_status
        .lock()
        .unwrap()
        .as_ref()
        .map(|m| m.text.clone())
        .unwrap_or_default()
}
fn workflow(phase: WorkflowPhase) -> WorkflowState {
    let steps: Vec<crate::data::workflow_definition::WorkflowStep> =
        serde_json::from_value(serde_json::json!([
            {"name":"build","agent":"fixture","prompt_template":"fixture"},
            {"name":"review","agent":"fixture","prompt_template":"fixture","depends_on":[]}
        ]))
        .unwrap();
    let mut state = WorkflowState::new("pinned-workflow".into(), &steps, "fixture".into(), None);
    state.current_phase = phase;
    state.setup_step_states.push(PhaseStepState {
        description: "prepare workspace".into(),
        status: PhaseStepStatus::Running,
    });
    state.teardown_step_states.push(PhaseStepState {
        description: "publish result".into(),
        status: PhaseStepStatus::Pending,
    });
    state
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn squad_dispatch_follows_one_run_through_handoff_detach_reconnect_and_replacement() {
    use crate::command::instance::feeds::ContainerSlotEvent;
    let f = Fixture::new("frontend::tui::contracts::squad_follow::daemon_journey::squad_dispatch_follows_one_run_through_handoff_detach_reconnect_and_replacement").await;
    let (leader, mut leader_input) = f.gateway.register(None);
    let history = (0..80)
        .map(|n| format!("leader history {n}\r\n"))
        .collect::<String>();
    leader.push_output(history.as_bytes());
    let mut app = f.app();
    press(&mut app, KeyCode::Char('a'), KeyModifiers::NONE);
    wait(
        &mut app,
        "leader history in the production Squad slot",
        |app| {
            app.active_tab().container_slots.iter().any(|slot| {
                slot.step_name.starts_with("leader:")
                    && slot
                        .vt100_parser
                        .screen()
                        .contents()
                        .contains("leader history 79")
            })
        },
    );
    assert!(!app.squad_grid_active());
    let screen = app
        .active_tab_mut()
        .focused_slot_mut()
        .unwrap()
        .vt100_parser
        .screen_mut();
    screen.set_scrollback(10_000);
    assert!(
        screen.scrollback() > 40,
        "late attach did not replay scrollback"
    );
    assert!(screen.contents().contains("leader history 0"));
    screen.set_scrollback(0);
    let first_generation = app
        .active_tab()
        .squad
        .as_ref()
        .unwrap()
        .attach_view
        .as_ref()
        .unwrap()
        .generation;
    assert!(f.daemon.ops.control_holder(leader.id()).is_some());
    // The leader is not the viewer lifetime. No workflow exists yet.
    leader.exit(0);
    wait(
        &mut app,
        "explicit missing workflow state after leader exit",
        |app| {
            app.active_tab().container_slots.is_empty() && status(app).contains("No workflow state")
        },
    );
    assert!(!app.squad_grid_active());
    assert!(draw(&mut app, 160, 40).contains("No workflow state for this run yet"));
    f.gateway.state.lock().unwrap().workflow = Some(workflow(WorkflowPhase::Setup));
    wait(&mut app, "setup overview without a terminal", |app| {
        app.active_tab().shared.workflow.snapshot().view.is_some()
            && status(app).contains("Waiting for the next")
    });
    assert!(!app.squad_grid_active());
    assert!(draw(&mut app, 160, 40).contains("prepare workspace"));
    let (setup, _setup_input) = f.gateway.register(Some("setup"));
    setup.push_output(b"setup output\r\n");
    wait(&mut app, "setup terminal", |app| {
        app.active_tab()
            .container_slots
            .iter()
            .any(|slot| slot.step_name.starts_with("setup:"))
    });
    setup.exit(0);
    f.gateway
        .state
        .lock()
        .unwrap()
        .workflow
        .as_mut()
        .unwrap()
        .current_phase = WorkflowPhase::Main;
    let (build, mut build_input) = f.gateway.register(Some("build"));
    let (review, _review_input) = f.gateway.register(Some("review"));
    build.push_output(b"build before attach\r\n");
    review.push_output(b"review before attach\r\n");
    wait(&mut app, "both parallel slots with replay", |app| {
        app.active_tab().container_slots.len() == 2
            && app.active_tab().container_slots.iter().all(|slot| {
                slot.vt100_parser
                    .screen()
                    .contents()
                    .contains("before attach")
            })
    });
    assert!(f.daemon.ops.control_holder(build.id()).is_some());
    assert!(f.daemon.ops.control_holder(review.id()).is_some());
    let old_feeds = app.active_tab().shared.clone();
    let old_cancel = old_feeds.command_cancel.clone();
    let root_poller = app.active_tab().squad.as_ref().unwrap().cancel.clone();
    app.active_tab_mut().workflow_overview_scroll_offset = 3;
    app.active_tab_mut().workflow_overview_hscroll_offset = 8;
    app.active_tab_mut().container_scroll_offset = 2;
    press(&mut app, KeyCode::Char('\\'), KeyModifiers::CONTROL);
    assert!(old_cancel.is_cancelled());
    assert!(app.squad_grid_active());
    assert_eq!(app.active_tab().squad.as_ref().unwrap().selected, 0);
    assert!(!root_poller.is_cancelled());
    assert_eq!(
        app.active_tab().container_window_state,
        tabs::ContainerWindowState::Hidden
    );
    assert!(app.active_tab().last_container_summary.is_none());
    assert!(app.active_tab().shared.workflow.snapshot().view.is_none());
    assert_eq!(app.active_tab().workflow_overview_scroll_offset, 0);
    assert_eq!(app.active_tab().workflow_overview_hscroll_offset, 0);
    assert_eq!(app.active_tab().container_scroll_offset, 0);
    assert!(leader_input.try_recv().is_err());
    assert!(
        build_input.try_recv().is_err(),
        "Ctrl-\\ reached the producer"
    );
    assert_eq!(
        f.gateway.state.lock().unwrap().run.status,
        RunStatus::Running
    );
    assert_eq!(
        f.gateway.hub.get(build.id()).unwrap().state,
        crate::data::terminal::TerminalState::Open
    );
    old_feeds.container.push(ContainerSlotEvent::Launched {
        step_name: "late".into(),
        agent: "fixture".into(),
        model: None,
        io: None,
    });
    old_feeds
        .workflow
        .update_view(|view| *view = Some(Default::default()));
    app.active_tab_mut().drain_container_slot_events();
    assert!(app.squad_grid_active());
    // A reattach has a fresh generation and replays output written while away.
    build.push_output(b"while detached\r\n");
    press(&mut app, KeyCode::Char('a'), KeyModifiers::NONE);
    let session = app
        .active_tab()
        .squad
        .as_ref()
        .unwrap()
        .attach_session
        .clone();
    let second_generation = session.current_generation().unwrap();
    assert_ne!(first_generation, second_generation);
    session.end_if_current(first_generation);
    assert!(session.is_current(second_generation));
    assert!(!app.active_tab().shared.command_cancel.is_cancelled());
    wait(&mut app, "fresh replay after reattach", |app| {
        app.active_tab().container_slots.len() == 2
            && app.active_tab().container_slots.iter().any(|slot| {
                slot.vt100_parser
                    .screen()
                    .contents()
                    .contains("while detached")
            })
    });
    // Lose both metadata and the socket. The last overview and screens stay.
    f.gateway.state.lock().unwrap().failed = true;
    f.daemon.streams.lock().unwrap().cancel();
    wait(&mut app, "explicit frozen daemon state", |app| {
        status(app).contains("daemon not reachable")
    });
    assert_eq!(app.active_tab().container_slots.len(), 2);
    assert!(app.active_tab().shared.workflow.snapshot().view.is_some());
    assert!(draw(&mut app, 160, 40).contains("view frozen"));
    // Recovery creates fresh emulators and requests bounded history again.
    *f.daemon.streams.lock().unwrap() = CancellationToken::new();
    f.gateway.state.lock().unwrap().failed = false;
    build.push_output(b"after daemon recovery\r\n");
    wait(&mut app, "recovered terminal and overview", |app| {
        status(app).contains("Following this run")
            && app.active_tab().container_slots.iter().any(|slot| {
                slot.vt100_parser
                    .screen()
                    .contents()
                    .contains("after daemon recovery")
            })
    });
    build.exit(0);
    review.exit(0);
    f.gateway
        .state
        .lock()
        .unwrap()
        .workflow
        .as_mut()
        .unwrap()
        .current_phase = WorkflowPhase::Teardown;
    let (teardown, _teardown_input) = f.gateway.register(Some("teardown"));
    teardown.push_output(b"teardown output\r\n");
    wait(&mut app, "teardown terminal in the same viewer", |app| {
        app.active_tab().container_slots.len() == 1
            && app.active_tab().container_slots[0]
                .step_name
                .starts_with("teardown:")
    });
    // A replacement run has a tempting terminal. The old viewer must end.
    let opens_before = f.gateway.state.lock().unwrap().opens.len();
    f.gateway.state.lock().unwrap().run.id = RunId::new();
    let (replacement, _replacement_input) = f.gateway.register(Some("replacement"));
    replacement.push_output(b"must not appear\r\n");
    wait(&mut app, "run replacement returns to the grid", |app| {
        app.squad_grid_active()
    });
    assert!(app.active_tab().shared.workflow.snapshot().view.is_none());
    assert_eq!(f.gateway.state.lock().unwrap().opens.len(), opens_before);
    assert!(status(&app).contains("finished or was replaced"));
    assert_eq!(
        f.gateway.hub.get(replacement.id()).unwrap().state,
        crate::data::terminal::TerminalState::Open
    );
    drop(app);
    f.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_only_squad_switches_and_reattaches_without_taking_control() {
    let f = Fixture::new("frontend::tui::contracts::squad_follow::daemon_journey::read_only_squad_switches_and_reattaches_without_taking_control").await;
    let (leader, mut leader_input) = f.gateway.register(None);
    leader.push_output(b"leader\r\n");
    let mut app = f.app();
    // Same production squad attach Dispatch path with the explicit viewer flag.
    app.spawn_command(
        Dispatch::<TuiCommandFrontend>::parse_command_box_input(
            "squad attach selected-task --read-only",
        )
        .unwrap(),
    );
    wait(&mut app, "read-only leader slot", |app| {
        app.active_tab().container_slots.len() == 1
            && app.active_tab().container_slots[0]
                .step_name
                .starts_with("leader:")
    });
    leader.exit(0);
    let (build, mut build_input) = f.gateway.register(Some("build"));
    let (review, mut review_input) = f.gateway.register(Some("review"));
    build.push_output(b"build\r\n");
    review.push_output(b"review\r\n");
    wait(&mut app, "parallel read-only slots", |app| {
        app.active_tab().container_slots.len() == 2
    });
    app.active_tab_mut().container_window_state = tabs::ContainerWindowState::Maximized;
    app.active_tab_mut().execution_phase = tabs::ExecutionPhase::Running {
        command: "squad attach".into(),
    };
    press(&mut app, KeyCode::Char('x'), KeyModifiers::NONE);
    let first = app.active_tab().focused_slot_idx;
    press(&mut app, KeyCode::Char('s'), KeyModifiers::CONTROL);
    assert_ne!(app.active_tab().focused_slot_idx, first);
    press(&mut app, KeyCode::Char('y'), KeyModifiers::NONE);
    press(&mut app, KeyCode::Char('4'), KeyModifiers::CONTROL);
    assert!(app.squad_grid_active());
    app.spawn_command(
        Dispatch::<TuiCommandFrontend>::parse_command_box_input(
            "squad attach selected-task --read-only",
        )
        .unwrap(),
    );
    wait(&mut app, "reattached read-only slots", |app| {
        app.active_tab().container_slots.len() == 2
            && app
                .active_tab()
                .container_slots
                .iter()
                .all(|slot| !slot.step_name.is_empty())
    });
    press(&mut app, KeyCode::Char('\\'), KeyModifiers::CONTROL);
    assert!(app.squad_grid_active());
    awman_test_support::wait_for("read-only socket detach", Duration::from_secs(5), || {
        (f.daemon.clients.load(Ordering::SeqCst) == 0).then_some(())
    });
    assert!(f
        .gateway
        .state
        .lock()
        .unwrap()
        .opens
        .iter()
        .all(|(_, options)| options.read_only && options.history));
    assert!(
        f.daemon
            .frames
            .lock()
            .unwrap()
            .iter()
            .all(|(_, frame)| matches!(frame, TerminalClientFrame::Detached { .. })),
        "read-only slot switch requested control/input/resize"
    );
    assert!(leader_input.try_recv().is_err());
    assert!(build_input.try_recv().is_err());
    assert!(review_input.try_recv().is_err());
    assert!(f.daemon.ops.control_holder(build.id()).is_none());
    assert!(f.daemon.ops.control_holder(review.id()).is_none());
    drop(app);
    f.finish().await;
}

#[tokio::test]
async fn run_and_workflow_snapshots_crossing_replacement_are_refused() {
    let f = Fixture::new("frontend::tui::contracts::squad_follow::daemon_journey::run_and_workflow_snapshots_crossing_replacement_are_refused").await;
    let pin = pin_current_run(f.gateway.as_ref(), "selected-task")
        .await
        .unwrap();
    let original = pin.run_id.clone();
    let (_leader, _input) = f.gateway.register(None);
    let policy = AttachStartupPolicy {
        window: Duration::from_millis(1),
        check_interval: Duration::ZERO,
    };
    let mut follower = RunFollower::new(f.gateway.clone(), pin, policy, None, None);
    f.gateway.state.lock().unwrap().replace_during_terminals = true;
    assert!(matches!(
        follower.next(&CancellationToken::new()).await.unwrap(),
        FollowEvent::RunEnded(TaskAttachEnd::RunCompleted)
    ));
    {
        let mut state = f.gateway.state.lock().unwrap();
        state.run.id = original.clone();
        state.workflow = Some(workflow(WorkflowPhase::Main));
        state.replace_during_workflow = true;
    }
    let source = SquadTaskWorkflowSource::pinned(f.gateway.clone(), "selected-task", original);
    assert!(source.fetch_workflow_state().await.unwrap().is_none());
    f.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn squad_detach_while_waiting_never_opens_a_late_terminal() {
    let f = Fixture::new("frontend::tui::contracts::squad_follow::daemon_journey::squad_detach_while_waiting_never_opens_a_late_terminal").await;
    let mut app = f.app();
    press(&mut app, KeyCode::Char('a'), KeyModifiers::NONE);
    wait(&mut app, "startup wait from production dispatch", |app| {
        app.active_tab().container_slots.is_empty()
            && status(app).contains("Waiting for agent terminal")
    });
    assert!(!app.squad_grid_active());
    let old = app.active_tab().shared.clone();
    press(&mut app, KeyCode::Char('\\'), KeyModifiers::CONTROL);
    let (late, _input) = f.gateway.register(None);
    late.push_output(b"late leader\r\n");
    // Reattach waits for the cancelled local command to release its session.
    // Its old end_attach/drop lease must not cancel the new generation.
    press(&mut app, KeyCode::Char('a'), KeyModifiers::NONE);
    old.workflow
        .update_view(|view| *view = Some(Default::default()));
    wait(&mut app, "new viewer survives older completion", |app| {
        app.active_tab().container_slots.iter().any(|slot| {
            slot.step_name.starts_with("leader:")
                && slot
                    .vt100_parser
                    .screen()
                    .contents()
                    .contains("late leader")
        })
    });
    assert!(!app.active_tab().shared.command_cancel.is_cancelled());
    assert!(app.active_tab().shared.workflow.snapshot().view.is_none());
    press(&mut app, KeyCode::Char('\\'), KeyModifiers::CONTROL);
    awman_test_support::wait_for("cancelled viewer cleanup", Duration::from_secs(5), || {
        (f.daemon.clients.load(Ordering::SeqCst) == 0).then_some(())
    });
    old.container.push(
        crate::command::instance::feeds::ContainerSlotEvent::Launched {
            step_name: "late".into(),
            agent: "fixture".into(),
            model: None,
            io: None,
        },
    );
    app.tick_all_tabs();
    assert!(app.squad_grid_active());
    assert!(app.active_tab().shared.workflow.snapshot().view.is_none());
    assert_eq!(
        f.gateway.hub.get(late.id()).unwrap().state,
        crate::data::terminal::TerminalState::Open
    );
    drop(app);
    f.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lost_daemon_backend_keeps_squad_detachable_and_normal_tabs_fatal() {
    let f = Fixture::new("frontend::tui::contracts::squad_follow::daemon_journey::lost_daemon_backend_keeps_squad_detachable_and_normal_tabs_fatal").await;
    let (leader, _input) = f.gateway.register(None);
    leader.push_output(b"last screen before loss\r\n");
    let mut app = f.app();
    let lost = Arc::new(std::sync::atomic::AtomicBool::new(false));
    app.backend = Arc::new(LeaseLessBackend(app.backend.clone(), lost.clone()));
    press(&mut app, KeyCode::Char('a'), KeyModifiers::NONE);
    wait(&mut app, "viewer before complete backend loss", |app| {
        app.active_tab().container_slots.iter().any(|slot| {
            slot.step_name.starts_with("leader:")
                && slot
                    .vt100_parser
                    .screen()
                    .contents()
                    .contains("last screen before loss")
        })
    });
    lost.store(true, Ordering::SeqCst);
    app.tick_all_tabs();
    assert!(app.active_dialog.is_none());
    // The loss is recorded at once (quitting from the Squad tab reports it)
    // even though its dialog is deferred while the Squad tab presents it.
    assert!(app.remote_view.fatal_exit_code().is_some());
    assert_eq!(app.active_tab().container_slots.len(), 1);
    assert!(draw(&mut app, 160, 40).contains("view frozen"));
    press(&mut app, KeyCode::Char('\\'), KeyModifiers::CONTROL);
    app.tick_all_tabs();
    assert!(app.squad_grid_active());
    assert!(app.active_dialog.is_none());
    // The detach confirmation survives later ticks: the loss is presented
    // once, not rewritten every tick.
    app.tick_all_tabs();
    app.tick_all_tabs();
    assert!(
        app.status_bar.text.contains("Detached from"),
        "{}",
        app.status_bar.text
    );
    assert!(app.active_tab().container_slots.is_empty());
    assert!(app.active_tab().shared.workflow.snapshot().view.is_none());
    assert_eq!(
        f.gateway.hub.get(leader.id()).unwrap().state,
        crate::data::terminal::TerminalState::Open
    );
    // The existing engine-loss policy for a normal tab is unchanged.
    app.active_tab = 0;
    app.tick_all_tabs();
    assert!(matches!(app.active_dialog, Some(Dialog::FatalError { .. })));
    assert!(app.remote_view.fatal_exit_code().is_some());
    drop(app);
    f.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
/// hygiene-allow(H3): proving the queue stays bounded while the producer keeps writing needs real elapsed time after the budget is reached; there is no event to wait for when nothing more may happen.
async fn an_undrained_slot_bounds_its_queue_and_still_detaches() {
    use crate::command::instance::feeds::SLOT_OUTPUT_BUDGET;
    let f = Fixture::new("frontend::tui::contracts::squad_follow::daemon_journey::an_undrained_slot_bounds_its_queue_and_still_detaches").await;
    let (leader, _input) = f.gateway.register(None);
    leader.push_output(b"before flood\r\n");
    let mut app = f.app();
    press(&mut app, KeyCode::Char('a'), KeyModifiers::NONE);
    wait(&mut app, "leader slot is live", |app| {
        app.active_tab().container_slots.iter().any(|slot| {
            slot.step_name.starts_with("leader:")
                && slot
                    .vt100_parser
                    .screen()
                    .contents()
                    .contains("before flood")
        })
    });
    // The presenter stops draining (no ticks) while the producer writes many
    // times the budget.
    let line = format!("{}\r\n", "x".repeat(1022));
    let chunk = line.repeat(16);
    for _ in 0..(8 * SLOT_OUTPUT_BUDGET / chunk.len()) {
        leader.push_output(chunk.as_bytes());
    }
    let queued = || {
        app.active_tab()
            .container_slots
            .iter()
            .find(|slot| slot.step_name.starts_with("leader:"))
            .and_then(|slot| slot.container_stdout_rx.as_ref())
            .map_or(0, |rx| rx.queued())
    };
    awman_test_support::wait_for(
        "the bridge fills its budget",
        Duration::from_secs(8),
        || (queued() >= SLOT_OUTPUT_BUDGET).then_some(()),
    );
    std::thread::sleep(Duration::from_millis(300));
    let bound = SLOT_OUTPUT_BUDGET + 64 * 1024;
    assert!(queued() <= bound, "queued {} > {bound}", queued());
    press(&mut app, KeyCode::Char('\\'), KeyModifiers::CONTROL);
    wait(&mut app, "detach while the slot was stalled", |app| {
        app.squad_grid_active()
    });
    awman_test_support::wait_for("stalled viewer cleanup", Duration::from_secs(5), || {
        (f.daemon.clients.load(Ordering::SeqCst) == 0).then_some(())
    });
    drop(app);
    f.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
/// hygiene-allow(H3): proving the queue stays bounded while the producer keeps writing needs real elapsed time after the budget is reached; there is no event to wait for when nothing more may happen.
async fn the_remote_presenter_slot_reports_lag_once_and_bounds_its_queue() {
    use crate::command::instance::feeds::SLOT_OUTPUT_BUDGET;
    let f = Fixture::with_hub(
        "frontend::tui::contracts::squad_follow::daemon_journey::the_remote_presenter_slot_reports_lag_once_and_bounds_its_queue",
        crate::engine::terminal_hub::TerminalHubConfig {
            subscriber_capacity: 4,
            ..Default::default()
        },
    )
    .await;
    let (producer, _input) = f.gateway.register(None);
    let stop = CancellationToken::new();
    let (mut io, status_log) = crate::command::instance::remote::spawn_slot_subscription(
        f.gateway.endpoint.clone(),
        producer.id().clone(),
        stop.clone(),
    );
    producer.push_output(b"first\r\n");
    awman_test_support::wait_for("the slot is live", Duration::from_secs(5), || {
        io.stdout_rx.try_recv().ok().map(|_| ())
    });
    // The presenter stalls while the producer writes many times the budget.
    let chunk = format!("{}\r\n", "z".repeat(16_000));
    for _ in 0..(6 * SLOT_OUTPUT_BUDGET / chunk.len()) {
        producer.push_output(chunk.as_bytes());
    }
    awman_test_support::wait_for("the slot fills its budget", Duration::from_secs(8), || {
        (io.stdout_rx.queued() >= SLOT_OUTPUT_BUDGET).then_some(())
    });
    std::thread::sleep(Duration::from_millis(300));
    assert!(io.stdout_rx.queued() <= SLOT_OUTPUT_BUDGET + 64 * 1024);
    // The presenter drains again: the hub's lag recovery reaches the slot and
    // the user is told once.
    let lagged = || {
        status_log
            .lock()
            .unwrap()
            .iter()
            .filter(|line| line.text == crate::command::terminals::LAGGED_MESSAGE)
            .count()
    };
    awman_test_support::wait_for("the lag notice", Duration::from_secs(8), || {
        while io.stdout_rx.try_recv().is_ok() {}
        (lagged() > 0).then_some(())
    });
    producer.push_output(b"after recovery\r\n");
    awman_test_support::wait_for("live output after recovery", Duration::from_secs(5), || {
        let mut seen = false;
        while let Ok(bytes) = io.stdout_rx.try_recv() {
            seen |= String::from_utf8_lossy(&bytes).contains("after recovery");
        }
        seen.then_some(())
    });
    assert_eq!(lagged(), 1);
    stop.cancel();
    awman_test_support::wait_for("slot viewer cleanup", Duration::from_secs(5), || {
        (f.daemon.clients.load(Ordering::SeqCst) == 0).then_some(())
    });
    f.finish().await;
}
