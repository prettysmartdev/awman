//! Reconnect and dialog contracts against the production frame reducer.
//! Private component state; no host environment, daemon, or network is used.
use super::*;
use crate::data::{
    annotations::Annotations,
    dialog::DialogRequest,
    frontend_protocol::{FrontendKindWire, HelloEnv, FRONTEND_SUBPROTOCOL},
    instance_snapshot::{LabelColorWire, SessionView, StatusLine, TabLabelWire, YoloState},
    session::{SessionType, StaticGitRootResolver},
    workflow_state::CountdownKind,
};
use awman_test_support::{TestCase, TestSandbox};

fn env_for(root: &Path) -> EnvSnapshot {
    EnvSnapshot::from_full_env(
        [
            ("HOME".into(), root.join("home").display().to_string()),
            (
                "AWMAN_HOME".into(),
                root.join("home/.awman").display().to_string(),
            ),
            ("PATH".into(), String::new()),
        ]
        .into(),
    )
}
fn unit(id: &'static str) -> Unit {
    let sandbox = TestSandbox::for_test(TestCase::component(id));
    let root = sandbox.work().to_path_buf();
    std::fs::create_dir_all(root.join("home/.awman")).unwrap();
    std::fs::create_dir_all(root.join("repo")).unwrap();
    let (outbound, outbound_rx) = tokio::sync::mpsc::unbounded_channel();
    let shared = Arc::new(Shared {
        instance_id: InstanceId::new(),
        pid: 42,
        role: FrontendRole::Primary,
        address: root.join("no-daemon.sock"),
        catalogue: CommandCatalogue::get(),
        env: env_for(&root),
        hello: hello_for(&root, FrontendRole::Primary),
        state: Mutex::new(State::default()),
        outbound,
        stop: CancellationToken::new(),
        next_ticket: AtomicU64::new(1),
        reconnects: AtomicU64::new(0),
        squad: Arc::new(Mutex::new(SquadHealth::Unknown)),
        runtime: tokio::runtime::Handle::current(),
    });
    Unit {
        _sandbox: sandbox,
        root,
        backend: RemoteInstanceBackend { shared },
        outbound: outbound_rx,
    }
}
fn hello_for(root: &Path, role: FrontendRole) -> Hello {
    let home = root.join("home");
    Hello {
        protocol: FRONTEND_SUBPROTOCOL.into(),
        build: "test-build".into(),
        kind: FrontendKindWire::Tui,
        role,
        annotations: Annotations::try_from_pairs([("rex/session", "s1")]).unwrap(),
        cwd: root.join("repo"),
        argv: vec!["awman".into()],
        env: HelloEnv::from_pairs([
            ("HOME", home.display().to_string()),
            ("AWMAN_HOME", home.join(".awman").display().to_string()),
            ("PATH", "/usr/bin:/bin".to_string()),
        ]),
        term_size: TermSize {
            cols: 100,
            rows: 30,
        },
        stdin_is_tty: true,
        stdout_is_tty: true,
        view: None,
        resume: None,
        launch_context: None,
    }
}
fn session_at(root: &Path) -> Session {
    let cwd = root.join("repo");
    Session::open(
        cwd.clone(),
        &StaticGitRootResolver::new(cwd),
        SessionOpenOptions {
            env: Some(env_for(root)),
            ..Default::default()
        },
    )
    .unwrap()
}
fn session_spec(root: &Path) -> InitialTabSpec {
    InitialTabSpec::Session {
        session: session_at(root),
        fatal_startup_error: false,
        pty_size: (100, 30),
    }
}
struct Unit {
    _sandbox: TestSandbox,
    root: PathBuf,
    backend: RemoteInstanceBackend,
    outbound: UnboundedReceiver<Outbound>,
}
impl Unit {
    fn frame(&self, frame: EngineFrame) {
        let shared = &self.backend.shared;
        let mut state = shared.lock();
        handle_frame(shared, &mut state, 0, frame);
    }
    fn delta(&self, delta: InstanceDelta) {
        self.frame(EngineFrame::Delta(delta));
    }
    fn welcome(&self, snapshot: InstanceSnapshot) {
        let shared = &self.backend.shared;
        let mut state = shared.lock();
        resync(shared, &mut state, snapshot);
    }
    #[doc = " Frames the presenter side produced: queued ones and polled inputs."]
    fn sent(&mut self) -> Vec<FrontendFrame> {
        let mut frames = Vec::new();
        while let Ok(item) = self.outbound.try_recv() {
            frames.push(item);
        }
        frames.extend(collect_inputs(&self.backend.shared));
        frames
            .into_iter()
            .map(|item| match item {
                Outbound::Frame(frame) => frame,
                Outbound::Ui { request, .. } => FrontendFrame::UiRequest(request),
            })
            .collect()
    }
}
fn slot(step_name: &str, terminal: Option<TerminalId>) -> SlotSnapshot {
    SlotSnapshot {
        slot_id: SlotId(uuid::Uuid::new_v4()),
        step_name: step_name.into(),
        agent_display_name: "claude".into(),
        model: Some("opus".into()),
        container_name: String::new(),
        terminal_id: terminal,
        acp: None,
        started_at: chrono::Utc::now(),
        stats: None,
        program_status: None,
        stuck: false,
        countdown_kind: CountdownKind::default(),
        yolo: None,
        sandboxed: false,
        phase_step_title: None,
        dormant: false,
    }
}
fn tab_snapshot(root: &Path, tab_id: TabId, slots: Vec<SlotSnapshot>) -> TabSnapshot {
    let cwd = root.join("repo");
    TabSnapshot {
        tab_id,
        session: SessionView {
            session_id: SessionId::new(),
            session_type: SessionType::Local {
                workdir: cwd.clone(),
            },
            cwd: cwd.clone(),
            git_root: cwd,
            default_agent: None,
            labels: Default::default(),
        },
        kind: TabKindWire::Normal,
        label: TabLabelWire {
            text: "repo".into(),
            color: LabelColorWire::Running,
        },
        execution_phase: ExecutionPhase::Running {
            command: "ready".into(),
        },
        current_workflow: None,
        current_container: None,
        workflow: None,
        workflow_invocation_id: None,
        context_path: None,
        worktree_path: None,
        slots,
        focused_slot: None,
        yolo: None,
        yolo_mode: true,
        stuck: false,
        git_summary: None,
        status_log: vec![StatusLine {
            level: MessageLevel::Info,
            text: "checking".into(),
        }],
        agents_dashboard: None,
        last_container_summary: None,
        annotations: Annotations::new(),
    }
}
fn instance_snapshot(tabs: Vec<TabSnapshot>) -> InstanceSnapshot {
    InstanceSnapshot {
        instance_id: InstanceId::new(),
        created_at: chrono::Utc::now(),
        annotations: Annotations::new(),
        primary_frontend_kind: Some(FrontendKindWire::Tui),
        frontends: Vec::new(),
        tabs,
        active_tab_hint: None,
        squad_health: SquadHealthWire::Running,
        pending_dialogs: Vec::new(),
        running_jobs: 1,
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dynamic_sequential_replacement_reattaches_workflow_requests() {
    let mut unit = unit ("command::instance::remote::contracts::dynamic_sequential_replacement_reattaches_workflow_requests") ;
    let tab_id = TabId::new();
    unit.welcome(instance_snapshot(vec![tab_snapshot(
        &unit.root,
        tab_id.clone(),
        vec![slot("", None)],
    )]));
    let InitialTabOpened::Normal { tab } = unit.backend.open_initial(session_spec(&unit.root))
    else {
        panic!("expected the daemon tab");
    };
    let events = tab.feeds.container.events.clone();
    events.lock().unwrap().clear();
    let next = slot("leader", None);
    unit.delta(InstanceDelta::SlotLaunched { tab_id, slot: next });
    let queued: Vec<_> = events.lock().unwrap().drain(..).collect();
    let requests = queued.iter().find_map(|event| match event {
        ContainerSlotEvent::EngineAttached {
            requests: Some(requests),
            ..
        } => Some(requests),
        _ => None,
    });
    let requests = requests.expect("the replacement slot must attach Ctrl-W");
    requests
        .send(EngineRequest::OpenControlBoard {
            step_name: "leader".into(),
        })
        .unwrap();
    assert ! (unit . sent () . iter () . any (| frame | matches ! (frame , FrontendFrame :: WorkflowRequest { request : WorkflowRequestWire :: OpenControlBoard { step_name } , .. } if step_name == "leader")));
    tab.feeds.signals.touch_user_for_step(Some("build"));
    assert ! (unit . sent () . iter () . any (| frame | matches ! (frame , FrontendFrame :: Activity { step_name : Some (step_name) , .. } if step_name == "build")));
    tab.feeds.signals.touch_user_for_step(Some("test"));
    assert ! (unit . sent () . iter () . any (| frame | matches ! (frame , FrontendFrame :: Activity { step_name : Some (step_name) , .. } if step_name == "test")) , "changing slots must bypass the activity throttle");
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn attaching_mid_parallel_group_stashes_the_backbone_before_step_slots() {
    let unit = unit ("command::instance::remote::contracts::attaching_mid_parallel_group_stashes_the_backbone_before_step_slots") ;
    let tab_id = TabId::new();
    let mut backbone = slot("", None);
    backbone.dormant = true;
    let step = slot("build", None);
    unit.welcome(instance_snapshot(vec![tab_snapshot(
        &unit.root,
        tab_id,
        vec![backbone, step],
    )]));
    let InitialTabOpened::Normal { tab } = unit.backend.open_initial(session_spec(&unit.root))
    else {
        panic!("expected the daemon tab");
    };
    let events: Vec<_> = tab
        .feeds
        .container
        .events
        .lock()
        .unwrap()
        .drain(..)
        .collect();
    let group = events
        .iter()
        .position(|event| matches!(event, ContainerSlotEvent::GroupStarted))
        .expect("the snapshot must announce the parallel group");
    let step = events . iter () . position (| event | matches ! (event , ContainerSlotEvent :: Launched { step_name , .. } if step_name == "build")) . expect ("the parallel step must launch") ;
    assert!(group < step, "the TUI must stash the backbone first");
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reconnect_reconciles_parallel_transitions_and_cleared_reports() {
    use crate::data::program_status::{EffectiveStatus, ProgramState};
    let unit = unit ("command::instance::remote::contracts::reconnect_reconciles_parallel_transitions_and_cleared_reports") ;
    let tab_id = TabId::new();
    let backbone = slot("", None);
    unit.welcome(instance_snapshot(vec![tab_snapshot(
        &unit.root,
        tab_id.clone(),
        vec![backbone.clone()],
    )]));
    let InitialTabOpened::Normal { tab } = unit.backend.open_initial(session_spec(&unit.root))
    else {
        panic!("expected the daemon tab");
    };
    let events = tab.feeds.container.events.clone();
    events.lock().unwrap().clear();
    let mut dormant = backbone;
    dormant.dormant = true;
    let mut step = slot("build", None);
    step.program_status = Some(EffectiveStatus {
        state: ProgramState::Done,
        kind: None,
        progress: None,
        msg: None,
        app: None,
        id: None,
    });
    step.yolo = Some(YoloState {
        step_name: "build".into(),
        remaining_secs: 5,
        kind: CountdownKind::AgentDone,
    });
    let group = tab_snapshot(
        &unit.root,
        tab_id.clone(),
        vec![dormant.clone(), step.clone()],
    );
    unit.welcome(instance_snapshot(vec![group.clone()]));
    let queued: Vec<_> = events.lock().unwrap().drain(..).collect();
    let group_started = queued
        .iter()
        .position(|event| matches!(event, ContainerSlotEvent::GroupStarted))
        .unwrap();
    let launched = queued . iter () . position (| event | matches ! (event , ContainerSlotEvent :: Launched { step_name , .. } if step_name == "build")) . unwrap () ;
    assert!(group_started < launched);
    assert ! (queued . iter () . any (| event | matches ! (event , ContainerSlotEvent :: Reported { step_name , status : Some (_) } if step_name == "build")));
    assert ! (queued . iter () . any (| event | matches ! (event , ContainerSlotEvent :: YoloTick { step_name , remaining_secs : 5 } if step_name == "build")));
    let mut cleared = group;
    cleared.slots[1].program_status = None;
    cleared.slots[1].yolo = None;
    cleared.slots[1].container_name = "replacement-container".into();
    unit.welcome(instance_snapshot(vec![cleared]));
    let queued: Vec<_> = events.lock().unwrap().drain(..).collect();
    assert ! (queued . iter () . any (| event | matches ! (event , ContainerSlotEvent :: Reported { step_name , status : None } if step_name == "build")));
    assert ! (queued . iter () . any (| event | matches ! (event , ContainerSlotEvent :: YoloFinished { step_name } if step_name == "build")));
    assert ! (queued . iter () . any (| event | matches ! (event , ContainerSlotEvent :: ContainerName { step_name , container_name } if step_name == "build" && container_name == "replacement-container")));
    let replacement = slot("review", None);
    unit.welcome(instance_snapshot(vec![tab_snapshot(
        &unit.root,
        tab_id,
        vec![replacement],
    )]));
    let queued: Vec<_> = events.lock().unwrap().drain(..).collect();
    let group_finished = queued
        .iter()
        .position(|event| matches!(event, ContainerSlotEvent::GroupFinished))
        .unwrap();
    let launched = queued
        .iter()
        .position(|event| matches!(event, ContainerSlotEvent::SequentialLaunched { .. }))
        .unwrap();
    assert!(
        group_finished < launched,
        "restore the backbone before replacing it"
    );
    assert ! (queued . iter () . all (| event | ! matches ! (event , ContainerSlotEvent :: Launched { step_name , .. } if step_name == "review")) , "the next sequential step must not become a parallel slot");
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_closed_dialogs_late_response_cannot_answer_the_next_dialog() {
    let mut unit = unit ("command::instance::remote::contracts::a_closed_dialogs_late_response_cannot_answer_the_next_dialog") ;
    let tab_id = TabId::new();
    unit.welcome(instance_snapshot(vec![tab_snapshot(
        &unit.root,
        tab_id.clone(),
        vec![slot("", None)],
    )]));
    let InitialTabOpened::Normal { tab } = unit.backend.open_initial(session_spec(&unit.root))
    else {
        panic!("expected the daemon tab");
    };
    let StartupRun::Started(run) = tab.startup else {
        panic!("expected a bound run");
    };
    let first = DialogId::new();
    let next = DialogId::new();
    let request = || DialogRequest::YesNo {
        title: "Continue?".into(),
        body: "Proceed".into(),
    };
    unit.frame(EngineFrame::DialogRequest {
        dialog_id: first.clone(),
        tab_id: tab_id.clone(),
        request: request(),
    });
    let _ = run.feeds.dialog_request_rx.try_recv().unwrap();
    unit.frame(EngineFrame::DialogClosed {
        dialog_id: first,
        answered_by: AnsweredBy::Api,
    });
    unit.frame(EngineFrame::DialogRequest {
        dialog_id: next.clone(),
        tab_id,
        request: request(),
    });
    assert!(run.feeds.dialog_request_rx.try_recv().is_err());
    run.feeds
        .dialog_response_tx
        .send(DialogResponse::Yes)
        .unwrap();
    assert!(
        unit.sent()
            .iter()
            .all(|frame| !matches!(frame, FrontendFrame::DialogResponse { .. })),
        "the old answer must be discarded while the close is pending"
    );
    assert!(unit
        .backend
        .poll()
        .iter()
        .any(|event| matches!(event, BackendEvent::DialogClosed { .. })));
    let _ = run
        .feeds
        .dialog_request_rx
        .try_recv()
        .expect("the next dialog arrives after the close");
    run.feeds
        .dialog_response_tx
        .send(DialogResponse::No)
        .unwrap();
    assert ! (unit . sent () . iter () . any (| frame | matches ! (frame , FrontendFrame :: DialogResponse { dialog_id , response : DialogResponse :: No } if dialog_id == & next)));
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reconnect_acknowledges_a_submitted_workflow_from_its_snapshot_slot() {
    let unit = unit ("command::instance::remote::contracts::reconnect_acknowledges_a_submitted_workflow_from_its_snapshot_slot") ;
    let tab_id = TabId::new();
    unit.welcome(instance_snapshot(vec![tab_snapshot(
        &unit.root,
        tab_id.clone(),
        Vec::new(),
    )]));
    let InitialTabOpened::Normal { tab } = unit.backend.open_initial(session_spec(&unit.root))
    else {
        panic!("expected the daemon tab");
    };
    let (reply_tx, reply_rx) = std::sync::mpsc::channel();
    let (run, _, _) = new_run(tab.feeds.clone());
    unit.backend.shared.lock().tabs[0].pending = Some(PendingSubmit {
        run,
        saw_yolo_mode: false,
        expected_command: "exec workflow".into(),
        reply: reply_tx,
    });
    unit.welcome(instance_snapshot(vec![tab_snapshot(
        &unit.root,
        tab_id.clone(),
        vec![slot("", None)],
    )]));
    assert!(reply_rx.try_recv().is_err());
    let mut running = tab_snapshot(&unit.root, tab_id, vec![slot("build", None)]);
    running.execution_phase = ExecutionPhase::Running {
        command: "exec workflow".into(),
    };
    unit.welcome(instance_snapshot(vec![running]));
    let ack = reply_rx
        .try_recv()
        .expect("snapshot must accept the submit")
        .unwrap();
    assert_eq!(ack.display.agent_display_name, "claude");
    assert!(ack.display.unattended);
    assert!(unit.backend.shared.lock().tabs[0].run.is_some());
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_exit_frame_delivers_its_final_screen_before_closing_the_slot() {
    let unit = unit ("command::instance::remote::contracts::an_exit_frame_delivers_its_final_screen_before_closing_the_slot") ;
    let tab_id = TabId::new();
    let backbone = slot("", None);
    unit.welcome(instance_snapshot(vec![tab_snapshot(
        &unit.root,
        tab_id.clone(),
        vec![backbone.clone()],
    )]));
    let InitialTabOpened::Normal { tab } = unit.backend.open_initial(session_spec(&unit.root))
    else {
        panic!("expected the daemon tab");
    };
    let StartupRun::Started(mut run) = tab.startup else {
        panic!("expected a bound run");
    };
    let output = unit.backend.shared.lock().tabs[0]
        .run
        .as_ref()
        .unwrap()
        .slots
        .get(&backbone.slot_id)
        .unwrap()
        .output
        .clone();
    unit.delta(InstanceDelta::SlotExited {
        tab_id,
        slot_id: backbone.slot_id,
        exit_code: Some(0),
        final_screen: Some(b"final grid".to_vec()),
    });
    assert_eq!(run.slot_io.stdout_rx.try_recv().unwrap(), b"final grid");
    assert!(output.lock().unwrap().send_live(b"late frame".to_vec()));
    assert!(run.slot_io.stdout_rx.try_recv().is_err());
    assert!(tab
        .feeds
        .container
        .events
        .lock()
        .unwrap()
        .iter()
        .any(|event| matches!(
            event,
            ContainerSlotEvent::SequentialExited { exit_code: 0, .. }
        )));
}
