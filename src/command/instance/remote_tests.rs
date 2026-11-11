//! `RemoteInstanceBackend` tests (WI 0142 Part B §2, invariant I8).
//!
//! Two levels:
//! - **Mapping** (`mapping_*`): engine frames applied to a backend with no
//!   socket, asserting what lands on the presenter's channels and feeds and
//!   which frames the presenter's inputs produce.
//! - **Engine** (`engine_*`): an in-process daemon core — the real
//!   `EngineInstanceHost` over hermetic engines on a temp data dir — served on
//!   a private Unix socket by a minimal test adapter for
//!   `POST /v1/engine/instances`, the frontend WebSocket and
//!   `/v1/terminals/{id}`. (The production socket adapter lives in Layer 3,
//!   which Layer 2 tests cannot import.) Covers welcome → initial tab,
//!   reconnect inside the grace with nothing visible, engine loss → fatal
//!   outcome, detach, refused second primary and protocol violations.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::AtomicBool;

use axum::extract::ws::{Message as AxMessage, WebSocket, WebSocketUpgrade};
use axum::extract::{Path as AxPath, State as AxState};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::{SinkExt, StreamExt};

use super::*;
use crate::command::dispatch::parsed_input::ParsedCommandBoxInput;
use crate::command::dispatch::Engines;
use crate::command::instance::frontend::PlainPresentation;
use crate::command::instance::host::{
    EngineInstanceHost, HostParts, InstanceOpener, OpenInstanceRequest, OpenedInstance,
};
use crate::command::instance::{EngineInstance, HostedOptions, InstanceParts};
use crate::command::integrations::IntegrationRegistry;
use crate::data::annotations::Annotations;
use crate::data::dialog::DialogRequest;
use crate::data::frontend_protocol::{
    Codec, ErrorWire, FrontendKindWire, HelloEnv, OutputStream, FRONTEND_SUBPROTOCOL,
};
use crate::data::instance_snapshot::{
    LabelColorWire, SessionView, StatusLine, TabLabelWire, YoloState,
};
use crate::data::session::{SessionType, StaticGitRootResolver};
use crate::data::session_manager::SessionManager;
use crate::data::terminal::TERMINAL_SUBPROTOCOL;
use crate::data::workflow_state::CountdownKind;
use crate::engine::terminal_hub::TerminalHub;

// ─── Shared helpers ─────────────────────────────────────────────────────────

fn private_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::create_dir_all(dir.path().join("home/.awman")).unwrap();
    std::fs::create_dir_all(dir.path().join("repo")).unwrap();
    dir
}

fn env_for(root: &Path) -> EnvSnapshot {
    let home = root.join("home");
    EnvSnapshot::with_overrides([
        ("HOME", home.display().to_string()),
        ("AWMAN_HOME", home.join(".awman").display().to_string()),
        ("PATH", "/usr/bin:/bin".to_string()),
    ])
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

/// Wait (polling) until `check` holds, or fail after `limit`.
fn eventually(limit: Duration, mut check: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if check() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    check()
}

// ─── Mapping tests (no socket) ──────────────────────────────────────────────

struct Unit {
    _dir: tempfile::TempDir,
    root: PathBuf,
    backend: RemoteInstanceBackend,
    outbound: UnboundedReceiver<Outbound>,
}

fn unit() -> Unit {
    let dir = private_dir();
    let root = dir.path().to_path_buf();
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
        _dir: dir,
        root,
        backend: RemoteInstanceBackend { shared },
        outbound: outbound_rx,
    }
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

    /// Frames the presenter side produced: queued ones and polled inputs.
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
async fn mapping_initial_squad_tab_uses_the_engine_session() {
    let unit = unit();
    let tab_id = TabId::new();
    let mut squad = tab_snapshot(&unit.root, tab_id.clone(), Vec::new());
    squad.kind = TabKindWire::Squad;
    squad.execution_phase = ExecutionPhase::Idle;
    unit.welcome(instance_snapshot(vec![squad]));
    let fallback = session_at(&unit.root);
    let fallback_id = fallback.id();

    let InitialTabOpened::Squad { session, .. } = unit
        .backend
        .open_initial(InitialTabSpec::Squad { fallback })
    else {
        panic!("the daemon's initial squad tab must open as a squad view");
    };
    assert_ne!(session.id(), fallback_id);
    assert_eq!(unit.backend.tab_id(&session.id()), Some(tab_id));
}

/// The welcome snapshot's first tab is the initial tab, its running startup
/// command arrives as `StartupRun::Started`, and every later frame lands on
/// exactly the channel and feed the builtin backend would have written.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mapping_snapshot_and_frames_reach_the_presenter_channels() {
    let mut unit = unit();
    let tab_id = TabId::new();
    let backbone = slot("", None);
    unit.welcome(instance_snapshot(vec![tab_snapshot(
        &unit.root,
        tab_id.clone(),
        vec![backbone.clone()],
    )]));

    let InitialTabOpened::Normal { tab } = unit.backend.open_initial(session_spec(&unit.root))
    else {
        panic!("expected a normal initial tab");
    };
    let session_id = tab.session.id();
    assert_eq!(unit.backend.tab_id(&session_id), Some(tab_id.clone()));
    let StartupRun::Started(run) = tab.startup else {
        panic!("the engine's startup command is the tab's run");
    };
    assert_eq!(run.display.agent_display_name, "claude");
    assert!(run.display.unattended, "yolo_mode from the snapshot");
    let feeds = tab.feeds;
    assert_eq!(
        feeds.status_log.lock().unwrap().clone(),
        vec![StatusLine {
            level: MessageLevel::Info,
            text: "checking".into()
        }],
        "the snapshot's status log is the tab's"
    );
    assert!(matches!(
        *unit.backend.squad_health().lock().unwrap(),
        SquadHealth::Running
    ));
    // The backbone slot publishes the engine's workflow channels.
    let attached = feeds
        .container
        .events
        .lock()
        .unwrap()
        .drain(..)
        .collect::<Vec<_>>();
    let [ContainerSlotEvent::EngineAttached {
        requests: Some(requests),
        stuck: Some(_),
        ..
    }] = attached.as_slice()
    else {
        panic!("the backbone must attach workflow channels");
    };
    requests
        .send(EngineRequest::OpenControlBoard {
            step_name: "build".into(),
        })
        .unwrap();
    assert!(unit.sent().iter().any(|frame| matches!(
        frame,
        FrontendFrame::WorkflowRequest {
            request: WorkflowRequestWire::OpenControlBoard { step_name },
            ..
        } if step_name == "build"
    )));

    // Status lines and the session phase.
    unit.delta(InstanceDelta::StatusLine {
        tab_id: tab_id.clone(),
        line: StatusLine {
            level: MessageLevel::Warning,
            text: "almost".into(),
        },
    });
    assert_eq!(feeds.status_log.lock().unwrap().len(), 2);
    unit.delta(InstanceDelta::Phase {
        tab_id: tab_id.clone(),
        phase: ExecutionPhase::Done {
            command: "ready".into(),
            exit_code: 0,
        },
        current_workflow: None,
        current_container: None,
    });
    let mirrored = unit.backend.session_snapshot(&session_id).unwrap();
    assert_eq!(
        ExecutionPhase::of_session_state(mirrored.state()),
        ExecutionPhase::Done {
            command: "ready".into(),
            exit_code: 0
        }
    );

    // Workflow presentation and the sequential yolo countdown.
    let invocation = uuid::Uuid::new_v4();
    unit.delta(InstanceDelta::Workflow {
        tab_id: tab_id.clone(),
        view: Some(Default::default()),
        invocation_id: Some(invocation),
        context_path: Some(unit.root.join("ctx")),
        worktree_path: None,
    });
    let workflow = feeds.workflow.snapshot();
    assert_eq!(workflow.invocation_id, Some(invocation));
    assert_eq!(workflow.context_path, Some(unit.root.join("ctx")));
    let yolo = YoloState {
        step_name: "build".into(),
        remaining_secs: 9,
        kind: CountdownKind::default(),
    };
    unit.delta(InstanceDelta::Yolo {
        tab_id: tab_id.clone(),
        state: Some(yolo.clone()),
    });
    assert_eq!(feeds.yolo_state.lock().unwrap().clone(), Some(yolo));

    // A dialog: delivered on the tab's channel, answered with its engine id.
    let dialog_id = DialogId::new();
    unit.frame(EngineFrame::DialogRequest {
        dialog_id: dialog_id.clone(),
        tab_id: tab_id.clone(),
        request: DialogRequest::YesNo {
            title: "Continue?".into(),
            body: "really".into(),
        },
    });
    assert!(matches!(
        run.feeds.dialog_request_rx.try_recv().unwrap(),
        DialogRequest::YesNo { .. }
    ));
    run.feeds
        .dialog_response_tx
        .send(DialogResponse::Yes)
        .unwrap();
    let sent = unit.sent();
    assert!(
        sent.iter().any(|frame| matches!(
            frame,
            FrontendFrame::DialogResponse { dialog_id: id, response: DialogResponse::Yes } if id == &dialog_id
        )),
        "{sent:?}"
    );
    assert_eq!(unit.backend.latency_probe().dialog_samples, 1);

    // A dialog another frontend answered closes here.
    let other = DialogId::new();
    unit.frame(EngineFrame::DialogRequest {
        dialog_id: other.clone(),
        tab_id: tab_id.clone(),
        request: DialogRequest::YesNo {
            title: "t".into(),
            body: "b".into(),
        },
    });
    let _ = run.feeds.dialog_request_rx.try_recv();
    unit.frame(EngineFrame::DialogClosed {
        dialog_id: other.clone(),
        answered_by: AnsweredBy::Api,
    });
    let events = unit.backend.poll();
    assert!(events.iter().any(|event| matches!(
        event,
        BackendEvent::DialogClosed { dialog_id: Some(id), answered_by: AnsweredBy::Api, .. } if id == &other
    )));

    // A parallel group: step slots, their countdowns and their exits.
    unit.delta(InstanceDelta::GroupStarted {
        tab_id: tab_id.clone(),
    });
    let step = slot("test", None);
    unit.delta(InstanceDelta::SlotLaunched {
        tab_id: tab_id.clone(),
        slot: step.clone(),
    });
    unit.delta(InstanceDelta::SlotContainerName {
        tab_id: tab_id.clone(),
        slot_id: step.slot_id,
        container_name: "awman-test".into(),
    });
    unit.delta(InstanceDelta::SlotYolo {
        tab_id: tab_id.clone(),
        slot_id: step.slot_id,
        state: Some(YoloState {
            step_name: "test".into(),
            remaining_secs: 3,
            kind: CountdownKind::default(),
        }),
    });
    let events: Vec<ContainerSlotEvent> =
        feeds.container.events.lock().unwrap().drain(..).collect();
    let mut flag = None;
    let names: Vec<&str> = events
        .iter()
        .map(|event| match event {
            ContainerSlotEvent::GroupStarted => "group_started",
            ContainerSlotEvent::Launched { step_name, io, .. } => {
                assert_eq!(step_name, "test");
                assert!(io.is_some());
                "launched"
            }
            ContainerSlotEvent::ContainerName { container_name, .. } => {
                assert_eq!(container_name, "awman-test");
                "container_name"
            }
            ContainerSlotEvent::YoloStarted { cancel_flag, .. } => {
                flag = Some(cancel_flag.clone());
                "yolo_started"
            }
            ContainerSlotEvent::YoloTick { remaining_secs, .. } => {
                assert_eq!(*remaining_secs, 3);
                "yolo_tick"
            }
            _ => "other",
        })
        .collect();
    assert_eq!(
        names,
        [
            "group_started",
            "launched",
            "container_name",
            "yolo_started",
            "yolo_tick"
        ]
    );
    let error = crate::data::program_status::EffectiveStatus {
        state: crate::data::program_status::ProgramState::Error,
        kind: None,
        progress: None,
        msg: Some("tests failed".into()),
        app: None,
        id: None,
    };
    unit.frame(EngineFrame::ProgramStatus {
        tab_id: tab_id.clone(),
        slot_id: step.slot_id,
        status: Some(error.clone()),
    });
    assert!(feeds
        .container
        .events
        .lock()
        .unwrap()
        .drain(..)
        .any(|event| {
            matches!(event, ContainerSlotEvent::Reported { step_name, status: Some(got) }
            if step_name == "test" && got == error)
        }));
    // Esc on the step's countdown is that step's CancelCountdown.
    flag.unwrap().store(true, Ordering::SeqCst);
    assert!(unit.sent().iter().any(|frame| matches!(
        frame,
        FrontendFrame::CancelCountdown { step_name: Some(step), .. } if step == "test"
    )));
    unit.delta(InstanceDelta::SlotExited {
        tab_id: tab_id.clone(),
        slot_id: step.slot_id,
        exit_code: Some(0),
    });
    unit.delta(InstanceDelta::GroupFinished {
        tab_id: tab_id.clone(),
    });
    let events: Vec<ContainerSlotEvent> =
        feeds.container.events.lock().unwrap().drain(..).collect();
    assert!(matches!(
        events.as_slice(),
        [
            ContainerSlotEvent::Exited { .. },
            ContainerSlotEvent::GroupFinished
        ]
    ));

    // Ctrl-C (the command's cancel token) is one Cancel frame.
    feeds.command_cancel.cancel();
    let cancels = unit
        .sent()
        .into_iter()
        .filter(|frame| matches!(frame, FrontendFrame::Cancel { .. }))
        .count();
    assert_eq!(cancels, 1);
    assert!(unit
        .sent()
        .iter()
        .all(|frame| !matches!(frame, FrontendFrame::Cancel { .. })));

    // The result: the engine's error text, exactly.
    unit.frame(EngineFrame::CommandFinished {
        tab_id: tab_id.clone(),
        result: CommandResultWire {
            exit_code: 1,
            outcome: None,
            error: Some(ErrorWire {
                code: "internal".into(),
                message: "the agent failed".into(),
            }),
        },
    });
    let error = run.feeds.command_result_rx.try_recv().unwrap().unwrap_err();
    assert_eq!(error.to_string(), "the agent failed");

    // Window-management acks map back to their ticket.
    let ticket = unit.backend.ui_request(UiRequest::NextTab);
    let seq = 7;
    unit.backend.shared.lock().tickets.insert(seq, ticket);
    unit.frame(EngineFrame::UiAck {
        request_seq: seq,
        outcome: UiOutcome::claimed("rex"),
    });
    assert!(unit.backend.poll().iter().any(|event| matches!(
        event,
        BackendEvent::UiAck { ticket: t, outcome } if *t == ticket && outcome.claimed_by_host
    )));
}

/// `submit` sends argv the engine re-parses and is answered by the run's own
/// slot; a refusal (a finished frame before the run started) is `Err`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mapping_submit_is_acknowledged_by_the_runs_slot_or_refused() {
    let mut unit = unit();
    let tab_id = TabId::new();
    unit.welcome(instance_snapshot(vec![tab_snapshot(
        &unit.root,
        tab_id.clone(),
        Vec::new(),
    )]));
    let InitialTabOpened::Normal { tab } = unit.backend.open_initial(session_spec(&unit.root))
    else {
        panic!()
    };
    assert!(matches!(tab.startup, StartupRun::NotStarted));
    let session_id = tab.session.id();
    let backend = Arc::new(unit.backend);

    let parsed = crate::command::dispatch::parsed_input::parse(
        "exec prompt --yolo fix the build",
        CommandCatalogue::get(),
    )
    .unwrap();
    let submit = {
        let backend = Arc::clone(&backend);
        let feeds = tab.feeds.clone();
        std::thread::spawn(move || {
            backend.submit(SubmitRequest {
                lease_id: None,
                session_id,
                parsed,
                feeds,
                pty_size: (90, 20),
                squad_attach: None,
            })
        })
    };
    let frame = loop {
        if let Ok(Outbound::Frame(frame)) = unit.outbound.try_recv() {
            break frame;
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let FrontendFrame::Submit {
        tab_id: sent_tab,
        input: SubmitInput::Argv { args },
        pty_size,
    } = frame
    else {
        panic!("{frame:?}");
    };
    assert_eq!(sent_tab, tab_id);
    assert_eq!(pty_size, TermSize { cols: 90, rows: 20 });
    // The engine parses the argv back to the same invocation.
    let reparsed = CommandCatalogue::get()
        .parse_raw_args(&["exec", "prompt"], &args[2..])
        .unwrap();
    assert_eq!(
        reparsed.argument("prompt").as_deref(),
        Some("fix the build")
    );
    assert_eq!(reparsed.flag_bool("yolo"), Some(true));

    let shared = Arc::clone(&backend.shared);
    let apply = |delta| {
        let mut state = shared.lock();
        handle_frame(&shared, &mut state, 0, EngineFrame::Delta(delta));
    };
    apply(InstanceDelta::YoloMode {
        tab_id: tab_id.clone(),
        on: true,
    });
    apply(InstanceDelta::SlotLaunched {
        tab_id: tab_id.clone(),
        slot: slot("", None),
    });
    let submitted = submit.join().unwrap().unwrap();
    assert!(submitted.display.unattended);
    assert_eq!(submitted.display.agent_display_name, "claude");
    assert_eq!(backend.running_jobs(), 1);

    // A refused submission.
    let parsed =
        crate::command::dispatch::parsed_input::parse("ready", CommandCatalogue::get()).unwrap();
    let refused = {
        let backend = Arc::clone(&backend);
        let feeds = tab.feeds.clone();
        std::thread::spawn(move || {
            backend.submit(SubmitRequest {
                lease_id: None,
                session_id,
                parsed,
                feeds,
                pty_size: (90, 20),
                squad_attach: None,
            })
        })
    };
    while !matches!(
        unit.outbound.try_recv(),
        Ok(Outbound::Frame(FrontendFrame::Submit { .. }))
    ) {
        std::thread::sleep(Duration::from_millis(5));
    }
    {
        let mut state = shared.lock();
        handle_frame(
            &shared,
            &mut state,
            0,
            EngineFrame::CommandFinished {
                tab_id: tab_id.clone(),
                result: CommandResultWire {
                    exit_code: 2,
                    outcome: None,
                    error: Some(ErrorWire {
                        code: "unknown_flag".into(),
                        message: "unknown flag '--nope'".into(),
                    }),
                },
            },
        );
    }
    let error = refused.join().unwrap().err().unwrap();
    assert_eq!(error.to_string(), "unknown flag '--nope'");
    // The previous run is still the tab's.
    assert_eq!(backend.running_jobs(), 1);
}

/// After a gap, the snapshot catches the tab up: missed status lines, a
/// dialog raised meanwhile, a slot that ended and a run that finished.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mapping_resync_catches_up_without_visible_events() {
    let unit = unit();
    let tab_id = TabId::new();
    let backbone = slot("", None);
    unit.welcome(instance_snapshot(vec![tab_snapshot(
        &unit.root,
        tab_id.clone(),
        vec![backbone.clone()],
    )]));
    let InitialTabOpened::Normal { tab } = unit.backend.open_initial(session_spec(&unit.root))
    else {
        panic!()
    };
    let StartupRun::Started(run) = tab.startup else {
        panic!()
    };
    let _ = unit.backend.poll();

    let mut later = tab_snapshot(&unit.root, tab_id.clone(), Vec::new());
    later.execution_phase = ExecutionPhase::Done {
        command: "ready".into(),
        exit_code: 0,
    };
    later.status_log.push(StatusLine {
        level: MessageLevel::Info,
        text: "missed while away".into(),
    });
    let dialog_id = DialogId::new();
    let mut snapshot = instance_snapshot(vec![later]);
    snapshot
        .pending_dialogs
        .push(crate::data::instance_snapshot::PendingDialog {
            dialog_id: dialog_id.clone(),
            tab_id: tab_id.clone(),
            request: DialogRequest::YesNo {
                title: "t".into(),
                body: "b".into(),
            },
            raised_at: chrono::Utc::now(),
        });
    unit.welcome(snapshot.clone());
    // Applying the same snapshot twice delivers nothing twice.
    unit.welcome(snapshot);

    assert_eq!(
        tab.feeds.status_log.lock().unwrap().last().unwrap().text,
        "missed while away"
    );
    assert!(run.feeds.dialog_request_rx.try_recv().is_ok());
    assert!(run.feeds.dialog_request_rx.try_recv().is_err());
    assert!(run.feeds.command_result_rx.try_recv().unwrap().is_ok());
    assert!(unit.backend.poll().is_empty(), "a re-sync is invisible");
    assert!(unit.backend.health().is_ok());
}

/// A `Detach` the presenter did not ask for is fatal and reported once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mapping_detach_is_a_fatal_outcome() {
    let unit = unit();
    unit.welcome(instance_snapshot(Vec::new()));
    unit.frame(EngineFrame::Detach(DetachReasonWire::EngineDisabled));
    assert_eq!(
        unit.backend.health(),
        Err(BackendLoss::Detached(DetachReasonWire::EngineDisabled))
    );
    let events = unit.backend.poll();
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, BackendEvent::Detached(_)))
            .count(),
        1
    );
    assert!(unit
        .backend
        .open_tab(OpenTabRequest {
            dir: unit.root.clone(),
            options: SessionOpenOptions::default(),
            pty_size: (80, 24),
        })
        .is_err());
}

// ─── Engine tests (in-process daemon core over a Unix socket) ───────────────

/// The hermetic opener: `Engines::for_tests` on the temp data dir and a
/// first tab opened in its fatal-start form, so no command and no runtime run.
struct TestOpener {
    root: PathBuf,
}

impl InstanceOpener for TestOpener {
    fn open(&self, request: OpenInstanceRequest) -> Result<OpenedInstance, CommandError> {
        let cwd = request.env.cwd().to_path_buf();
        let session = Session::open(
            cwd.clone(),
            &StaticGitRootResolver::new(cwd),
            SessionOpenOptions {
                env: Some(request.env.clone()),
                ..Default::default()
            },
        )?;
        let env = request.env.clone();
        let instance = EngineInstance::new_hosted(
            InstanceParts {
                catalogue: request.catalogue,
                engines: Engines::for_tests(&self.root),
                session_manager: Arc::new(SessionManager::in_memory(request.env.clone())),
                runtime: tokio::runtime::Handle::current(),
                env: Arc::new(move || env.clone()),
                presentation: Arc::new(PlainPresentation),
            },
            HostedOptions {
                id: request.id,
                kind: crate::command::dispatch::catalogue::FrontendKind::ENGINE_LOCAL,
                hub: Some(request.hub),
                executor: request.executor,
            },
        );
        Ok(OpenedInstance {
            instance,
            initial: InitialTabSpec::Session {
                session,
                fatal_startup_error: true,
                pty_size: (request.term_size.cols, request.term_size.rows),
            },
            notices: Vec::new(),
        })
    }
}

/// Rewrites one outbound frame (protocol-violation tests).
type Tamper = fn(Seq, String) -> String;

#[derive(Clone)]
struct Adapter {
    host: Arc<EngineInstanceHost>,
    hub: Arc<TerminalHub>,
    /// Drops every open frontend socket (no `Detach`).
    kill: tokio::sync::broadcast::Sender<()>,
    accepting: Arc<AtomicBool>,
    /// Rewrites outbound frames (protocol-violation tests).
    tamper: Arc<Mutex<Option<Tamper>>>,
}

struct Daemon {
    _dir: tempfile::TempDir,
    root: PathBuf,
    socket: PathBuf,
    adapter: Adapter,
    server: tokio::task::JoinHandle<()>,
}

impl Daemon {
    async fn start() -> Self {
        let dir = private_dir();
        let root = dir.path().to_path_buf();
        let hub = Arc::new(TerminalHub::new(Default::default()));
        let host = EngineInstanceHost::new(HostParts {
            hub: Arc::clone(&hub),
            catalogue: CommandCatalogue::get(),
            registry: IntegrationRegistry::compiled_in(),
            opener: Arc::new(TestOpener { root: root.clone() }),
            executor: None,
            build: "test-build".into(),
        });
        let adapter = Adapter {
            host,
            hub,
            kill: tokio::sync::broadcast::channel(4).0,
            accepting: Arc::new(AtomicBool::new(true)),
            tamper: Arc::new(Mutex::new(None)),
        };
        let socket = root.join("daemon.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).unwrap();
        let router = Router::new()
            .route("/v1/engine/instances", post(create))
            .route("/v1/engine/instances/{id}/frontend", get(frontend))
            .route("/v1/terminals/{id}", get(terminal))
            .with_state(adapter.clone());
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        Self {
            _dir: dir,
            root,
            socket,
            adapter,
            server,
        }
    }

    fn open(&self, role: FrontendRole, instance: Option<InstanceId>) -> RemoteOpen {
        RemoteOpen {
            address: self.socket.clone(),
            pid: std::process::id(),
            hello: hello_for(&self.root, role),
            create: instance.is_none(),
            instance_id: instance,
            catalogue: CommandCatalogue::get(),
            env: env_for(&self.root),
        }
    }

    async fn primary(&self) -> Arc<RemoteInstanceBackend> {
        RemoteInstanceBackend::connect(self.open(FrontendRole::Primary, None))
            .await
            .unwrap()
    }

    /// Stop serving: drop the listener and every socket.
    fn stop(&self) {
        self.adapter.accepting.store(false, Ordering::SeqCst);
        self.server.abort();
        let _ = self.adapter.kill.send(());
    }
}

async fn create(
    AxState(adapter): AxState<Adapter>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let annotations: Annotations = body
        .get("annotations")
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok())
        .unwrap_or_default();
    match adapter.host.create_instance(annotations) {
        Ok(id) => (StatusCode::CREATED, Json(serde_json::json!({ "id": id }))).into_response(),
        Err(error) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": error.to_string(), "code": error.code()})),
        )
            .into_response(),
    }
}

async fn frontend(
    ws: WebSocketUpgrade,
    AxPath(id): AxPath<String>,
    AxState(adapter): AxState<Adapter>,
) -> Response {
    if !adapter.accepting.load(Ordering::SeqCst) {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let Ok(id) = id.parse::<InstanceId>() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    ws.protocols([FRONTEND_SUBPROTOCOL])
        .on_upgrade(move |socket| serve_frontend(socket, adapter, id))
}

async fn serve_frontend(mut socket: WebSocket, adapter: Adapter, id: InstanceId) {
    let mut inbound = Codec::new();
    let Some(Ok(AxMessage::Text(text))) = socket.recv().await else {
        return;
    };
    let Ok((_, FrontendFrame::Hello(hello))) = inbound.decode::<FrontendFrame>(&text) else {
        return;
    };
    let (tx, mut rx) = tokio::sync::mpsc::channel(4096);
    let session = match adapter.host.attach_frontend(id, hello, tx).await {
        Ok(session) => session,
        Err(refusal) => {
            let body = serde_json::json!({"error": refusal.to_string(), "code": refusal.code()});
            let _ = socket.send(AxMessage::Text(body.to_string().into())).await;
            let _ = socket.close().await;
            return;
        }
    };
    let mut outbound = Codec::new();
    let mut kill = adapter.kill.subscribe();
    loop {
        tokio::select! {
            _ = kill.recv() => break,
            message = socket.recv() => match message {
                Some(Ok(AxMessage::Text(text))) => match inbound.decode::<FrontendFrame>(&text) {
                    Ok((seq, frame)) => {
                        if session.handle(seq, frame).await.is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                },
                Some(Ok(AxMessage::Close(_))) | None | Some(Err(_)) => break,
                Some(Ok(_)) => {}
            },
            frame = rx.recv() => {
                let Some(frame) = frame else { break };
                let mut text = outbound.encode(&frame).unwrap();
                let tamper = *adapter.tamper.lock().unwrap();
                if let Some(tamper) = tamper {
                    text = tamper(outbound.last_sent(), text);
                }
                if socket.send(AxMessage::Text(text.into())).await.is_err() {
                    break;
                }
            }
        }
    }
    session.disconnected();
}

async fn terminal(
    ws: WebSocketUpgrade,
    AxPath(id): AxPath<String>,
    AxState(adapter): AxState<Adapter>,
) -> Response {
    let Ok(id) = id.parse::<TerminalId>() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    ws.protocols([TERMINAL_SUBPROTOCOL])
        .on_upgrade(move |socket| serve_terminal(socket, adapter.hub, id))
}

async fn serve_terminal(socket: WebSocket, hub: Arc<TerminalHub>, id: TerminalId) {
    let Ok(mut subscription) = hub.subscribe(&id) else {
        return;
    };
    let (mut sink, mut stream) = socket.split();
    let mut grant = None;
    loop {
        tokio::select! {
            frame = subscription.next() => {
                let Some(frame) = frame else { break };
                let exit = matches!(frame, TerminalFrame::Exit { .. });
                let text = serde_json::to_string(&frame).unwrap();
                if sink.send(AxMessage::Text(text.into())).await.is_err() || exit {
                    break;
                }
            }
            message = stream.next() => match message {
                Some(Ok(AxMessage::Text(text))) => {
                    match serde_json::from_str::<TerminalClientFrame>(&text) {
                        Ok(TerminalClientFrame::TakeControl) => grant = hub.take_control(&id).ok(),
                        Ok(TerminalClientFrame::Input { bytes }) => {
                            if let Some(grant) = &grant {
                                let _ = grant.send_input(bytes);
                            }
                        }
                        Ok(_) => {}
                        Err(_) => break,
                    }
                }
                Some(Ok(_)) => {}
                _ => break,
            }
        }
    }
}

/// Welcome → the engine's first tab is the presenter's initial tab, the
/// merged annotations reach the instance, and the backend reports itself as
/// the daemon's.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn engine_primary_connects_and_the_snapshot_is_the_initial_tab() {
    let daemon = Daemon::start().await;
    let backend = daemon.primary().await;
    let id = backend.instance_id();
    assert!(matches!(backend.kind(), BackendKind::Daemon { instance_id, .. } if instance_id == id));
    assert_eq!(backend.role(), FrontendRole::Primary);
    let detail = daemon.adapter.host.get(&id).unwrap();
    assert_eq!(detail.frontends.len(), 1);
    assert_eq!(detail.frontends[0].frontend_id, backend.frontend_id());
    assert_eq!(
        detail.summary.annotations.get("rex/session"),
        Some("s1"),
        "the frontend's merged annotations reach the instance"
    );

    let root = daemon.root.clone();
    let backend_for_open = Arc::clone(&backend);
    let opened =
        tokio::task::spawn_blocking(move || backend_for_open.open_initial(session_spec(&root)))
            .await
            .unwrap();
    let InitialTabOpened::Normal { tab } = opened else {
        panic!("expected the engine's first tab");
    };
    let engine_tab = daemon.adapter.host.tabs(&id).unwrap()[0].tab_id.clone();
    assert_eq!(backend.tab_id(&tab.session.id()), Some(engine_tab));
    assert!(backend.session_snapshot(&tab.session.id()).is_some());
    assert!(backend.health().is_ok());
    assert!(backend
        .poll()
        .iter()
        .all(|event| !matches!(event, BackendEvent::Detached(_))));
}

/// A dropped socket is re-established inside the grace with the same
/// frontend id; nothing reaches the presenter and frames flow afterwards.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn engine_reconnect_inside_the_grace_is_invisible() {
    let daemon = Daemon::start().await;
    let backend = daemon.primary().await;
    let id = backend.instance_id();
    let frontend_id = backend.frontend_id();
    let _ = backend.poll();

    let _ = daemon.adapter.kill.send(());
    let reconnected = {
        let backend = Arc::clone(&backend);
        tokio::task::spawn_blocking(move || {
            eventually(Duration::from_secs(3), || backend.reconnects() == 1)
        })
        .await
        .unwrap()
    };
    assert!(reconnected, "the backend reconnected");
    assert_eq!(backend.frontend_id(), frontend_id, "resumed, not replaced");
    assert!(backend.health().is_ok());
    let detail = daemon.adapter.host.get(&id).unwrap();
    assert_eq!(detail.frontends.len(), 1);
    assert!(!detail.frontends[0].reconnecting);

    // Frames flow on the new socket.
    let ticket = backend.ui_request(UiRequest::NextTab);
    let acked = {
        let backend = Arc::clone(&backend);
        tokio::task::spawn_blocking(move || {
            let mut events = Vec::new();
            let ok = eventually(Duration::from_secs(3), || {
                events.extend(backend.poll());
                events.iter().any(
                    |event| matches!(event, BackendEvent::UiAck { ticket: t, .. } if *t == ticket),
                )
            });
            (ok, events)
        })
        .await
        .unwrap()
    };
    assert!(acked.0, "the ui request was answered after the reconnect");
    assert!(
        acked.1.iter().all(|event| !matches!(
            event,
            BackendEvent::Detached(_)
                | BackendEvent::Notice { .. }
                | BackendEvent::TabClosedByEngine { .. }
        )),
        "nothing visible: {:?}",
        acked.1
    );
}

/// The engine gone for longer than the grace is a typed fatal outcome; calls
/// that need the engine fail instead of hanging.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn engine_loss_after_the_grace_is_fatal() {
    let daemon = Daemon::start().await;
    let backend = daemon.primary().await;
    let started = Instant::now();
    daemon.stop();
    let lost = {
        let backend = Arc::clone(&backend);
        tokio::task::spawn_blocking(move || {
            eventually(Duration::from_secs(5), || backend.health().is_err())
        })
        .await
        .unwrap()
    };
    assert!(lost, "the loss is reported");
    assert!(
        started.elapsed() >= Duration::from_millis(1500),
        "the grace was waited out first ({:?})",
        started.elapsed()
    );
    assert!(matches!(backend.health(), Err(BackendLoss::Transport(_))));
    let root = daemon.root.clone();
    let backend_for_open = Arc::clone(&backend);
    let result = tokio::task::spawn_blocking(move || {
        backend_for_open.open_tab(OpenTabRequest {
            dir: root.join("repo"),
            options: SessionOpenOptions::default(),
            pty_size: (80, 24),
        })
    })
    .await
    .unwrap();
    assert!(result.is_err());
}

/// The engine ending the instance is `Detached`; a presenter's own quit is
/// the normal end and leaves `health` clean.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn engine_detach_is_fatal_but_a_quit_is_not() {
    let daemon = Daemon::start().await;
    let backend = daemon.primary().await;
    daemon.adapter.host.delete(&backend.instance_id()).unwrap();
    let detached = {
        let backend = Arc::clone(&backend);
        tokio::task::spawn_blocking(move || {
            eventually(Duration::from_secs(3), || backend.health().is_err())
        })
        .await
        .unwrap()
    };
    assert!(detached);
    assert_eq!(
        backend.health(),
        Err(BackendLoss::Detached(DetachReasonWire::Quit))
    );
    assert!(backend
        .poll()
        .iter()
        .any(|event| matches!(event, BackendEvent::Detached(DetachReasonWire::Quit))));

    let other = daemon.primary().await;
    let id = other.instance_id();
    let plan = {
        let other = Arc::clone(&other);
        tokio::task::spawn_blocking(move || other.quit())
            .await
            .unwrap()
    };
    assert_eq!(plan.exit_code, 0);
    assert!(
        other.health().is_ok(),
        "a presenter's own quit is not a loss"
    );
    assert!(daemon.adapter.host.get(&id).is_none(), "the instance ended");
}

/// A second primary for an instance is refused with `primary_exists`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn engine_refuses_a_second_primary() {
    let daemon = Daemon::start().await;
    let backend = daemon.primary().await;
    let error = RemoteInstanceBackend::connect(
        daemon.open(FrontendRole::Primary, Some(backend.instance_id())),
    )
    .await
    .unwrap_err();
    assert_eq!(error.code(), Some("primary_exists"), "{error}");
    assert!(backend.health().is_ok(), "the first primary is untouched");
    // Unknown instances are refused too.
    let error =
        RemoteInstanceBackend::connect(daemon.open(FrontendRole::Primary, Some(InstanceId::new())))
            .await
            .unwrap_err();
    assert_eq!(error.code(), Some("not_found"), "{error}");
    let command: CommandError = error.into();
    assert!(command.to_string().contains("not found"));
}

fn skip_a_seq(seq: Seq, text: String) -> String {
    if seq < 3 {
        return text;
    }
    text.replacen(
        &format!("\"seq\":{seq}"),
        &format!("\"seq\":{}", seq + 5),
        1,
    )
}

fn oversize(seq: Seq, text: String) -> String {
    if seq < 3 {
        return text;
    }
    let padding = "x".repeat(MAX_FRAME_BYTES_FOR_TESTS);
    text.replacen('{', &format!("{{\"pad\":\"{padding}\","), 1)
}

const MAX_FRAME_BYTES_FOR_TESTS: usize = crate::data::frontend_protocol::MAX_FRAME_BYTES + 1;

/// An out-of-order or oversized frame is fatal at once (invariant I8): no
/// reconnect, no panic.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn engine_protocol_violations_are_fatal_without_a_retry() {
    for tamper in [skip_a_seq as fn(Seq, String) -> String, oversize] {
        let daemon = Daemon::start().await;
        let backend = daemon.primary().await;
        *daemon.adapter.tamper.lock().unwrap() = Some(tamper);
        // Any engine frame after the handshake now breaks the protocol.
        let _ = backend.ui_request(UiRequest::NextTab);
        let failed = {
            let backend = Arc::clone(&backend);
            tokio::task::spawn_blocking(move || {
                eventually(Duration::from_secs(3), || backend.health().is_err())
            })
            .await
            .unwrap()
        };
        assert!(failed);
        assert!(
            matches!(
                backend.health(),
                Err(BackendLoss::Detached(
                    DetachReasonWire::ProtocolError { .. }
                ))
            ),
            "{:?}",
            backend.health()
        );
        assert_eq!(backend.reconnects(), 0, "a protocol error is never retried");
    }
}

/// A slot's hub terminal reaches the presenter's slot I/O and keystrokes go
/// back to the agent, through 0141's terminal client.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn engine_slot_terminals_are_subscribed_through_the_hub() {
    use crate::data::daemon_state::FeatureName;
    use crate::data::terminal::{TerminalOwner, TerminalOwnerInfo};
    use crate::engine::terminal_hub::{TerminalInput, TerminalRegistration};

    let daemon = Daemon::start().await;
    let (stdin, mut stdin_rx) = crate::engine::terminal_hub::input_channel();
    let handle = daemon.adapter.hub.register(
        TerminalRegistration {
            owner: TerminalOwnerInfo {
                owner: TerminalOwner::Tab(TabId::new()),
                feature: FeatureName::Engine,
                source: None,
                attempt: 1,
                allow_remote_input: false,
            },
            agent: "claude".into(),
            model: None,
            cols: 40,
            rows: 10,
        },
        TerminalInput {
            stdin,
            resize: None,
        },
    );
    handle.push_output(b"hello from the agent");
    let (io, pipes) = slot_channels();
    let ContainerSlotIo {
        mut stdout_rx,
        stdin_tx,
        resize_tx: _resize,
    } = io;
    let cancel = CancellationToken::new();
    tokio::spawn(subscribe_terminal(
        daemon.socket.clone(),
        handle.id().clone(),
        pipes,
        cancel.clone(),
    ));
    let mut seen = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(3);
    while !String::from_utf8_lossy(&seen).contains("hello from the agent") {
        let bytes = tokio::time::timeout_at(deadline.into(), stdout_rx.recv())
            .await
            .expect("output arrives")
            .expect("stream open");
        seen.extend(bytes);
    }
    // Input waits for the grant the subscription takes on connect.
    let mut delivered = None;
    for _ in 0..100 {
        stdin_tx.send(b"y\r".to_vec()).unwrap();
        if let Ok(Some(bytes)) =
            tokio::time::timeout(Duration::from_millis(30), stdin_rx.recv()).await
        {
            delivered = Some(bytes);
            break;
        }
    }
    assert_eq!(delivered.as_deref(), Some(&b"y\r"[..]));
    handle.exit(0);
    cancel.cancel();
}

/// `TerminalRelease { terminal_id: Some }`: the presenter's bridge ends reach
/// the host process's hub terminal (output out, keystrokes in after the grant)
/// and dropping them ends the subscription.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn engine_host_terminals_open_through_the_backend() {
    use crate::data::daemon_state::FeatureName;
    use crate::data::terminal::{TerminalOwner, TerminalOwnerInfo};
    use crate::engine::terminal_hub::{TerminalInput, TerminalRegistration};

    let daemon = Daemon::start().await;
    let backend = daemon.primary().await;
    let (stdin, mut stdin_rx) = crate::engine::terminal_hub::input_channel();
    let handle = daemon.adapter.hub.register(
        TerminalRegistration {
            owner: TerminalOwnerInfo {
                owner: TerminalOwner::Tab(TabId::new()),
                feature: FeatureName::Engine,
                source: None,
                attempt: 1,
                allow_remote_input: false,
            },
            agent: "gh auth login".into(),
            model: None,
            cols: 40,
            rows: 10,
        },
        TerminalInput {
            stdin,
            resize: None,
        },
    );
    handle.push_output(b"? Paste your token:");
    let ContainerSlotIo {
        mut stdout_rx,
        stdin_tx,
        resize_tx: _resize,
    } = backend.open_host_terminal(handle.id()).unwrap();
    let mut seen = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(3);
    while !String::from_utf8_lossy(&seen).contains("Paste your token") {
        let bytes = tokio::time::timeout_at(deadline.into(), stdout_rx.recv())
            .await
            .expect("output arrives")
            .expect("stream open");
        seen.extend(bytes);
    }
    let mut delivered = None;
    for _ in 0..100 {
        stdin_tx.send(b"tok\r".to_vec()).unwrap();
        if let Ok(Some(bytes)) =
            tokio::time::timeout(Duration::from_millis(30), stdin_rx.recv()).await
        {
            delivered = Some(bytes);
            break;
        }
    }
    assert_eq!(delivered.as_deref(), Some(&b"tok\r"[..]));
    drop(stdin_tx);
    handle.exit(0);
    assert!(
        tokio::time::timeout(Duration::from_secs(3), async {
            while stdout_rx.recv().await.is_some() {}
        })
        .await
        .is_ok(),
        "the subscription ends with the terminal"
    );
}

#[test]
fn p95_and_invocations_are_computed_like_the_builtin_session() {
    assert_eq!(p95(&[]), None);
    let samples: Vec<Duration> = (1..=20).map(Duration::from_millis).collect();
    assert_eq!(p95(&samples), Some(Duration::from_millis(19)));
    let running = invocation_for(
        &ExecutionPhase::Running {
            command: "chat".into(),
        },
        None,
    )
    .unwrap();
    assert_eq!(running.status, InvocationStatus::Running);
    let done = invocation_for(
        &ExecutionPhase::Done {
            command: "chat".into(),
            exit_code: 3,
        },
        Some(running.clone()),
    )
    .unwrap();
    assert_eq!(done.id, running.id, "the same invocation finishes");
    assert_eq!(done.exit_code, Some(3));
    assert!(invocation_for(&ExecutionPhase::Idle, Some(done)).is_none());
    let _ = OutputStream::Stdout;
    let _: Option<ParsedCommandBoxInput> = None;
}

/// The engine's program status for the backbone reaches the presenter on the
/// backbone's stuck channel (as the builtin backend reports it), and engine
/// stats arrive as a `SlotStats` event naming the presenter's own slot.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn program_status_and_stats_reach_the_presenter_slot() {
    use crate::data::instance_snapshot::AgentStatsWire;
    use crate::data::program_status::{EffectiveStatus, ProgramState};
    use crate::engine::agent_runtime::execution::StuckEvent;

    let unit = unit();
    let tab_id = TabId::new();
    let backbone = slot("", None);
    unit.welcome(instance_snapshot(vec![tab_snapshot(
        &unit.root,
        tab_id.clone(),
        vec![backbone.clone()],
    )]));
    let InitialTabOpened::Normal { tab } = unit.backend.open_initial(session_spec(&unit.root))
    else {
        panic!("expected a normal initial tab");
    };
    let feeds = tab.feeds;
    let mut stuck_rx = feeds
        .container
        .events
        .lock()
        .unwrap()
        .drain(..)
        .find_map(|event| match event {
            ContainerSlotEvent::EngineAttached { stuck, .. } => stuck,
            _ => None,
        })
        .expect("the backbone publishes a stuck channel");

    let status = EffectiveStatus {
        state: ProgramState::Blocked,
        kind: None,
        progress: None,
        msg: Some("needs approval".into()),
        app: None,
        id: None,
    };
    // The attach replay restores the snapshot's (unstuck) state first.
    while stuck_rx.try_recv().is_ok() {}
    unit.frame(EngineFrame::ProgramStatus {
        tab_id: tab_id.clone(),
        slot_id: backbone.slot_id,
        status: Some(status.clone()),
    });
    match stuck_rx.try_recv() {
        Ok(StuckEvent::Reported(Some(got))) => assert_eq!(got, status),
        other => panic!("expected the report on the stuck channel, got {other:?}"),
    }

    unit.delta(InstanceDelta::SlotStats {
        tab_id: tab_id.clone(),
        slot_id: backbone.slot_id,
        stats: AgentStatsWire {
            name: "awman-c".into(),
            cpu_percent: 12.5,
            memory_mb: 256.0,
        },
    });
    let launch = feeds.container.launch_id();
    let events = unit.backend.poll();
    assert!(
        events.iter().any(|event| matches!(
            event,
            BackendEvent::SlotStats { tab_id: t, slot: PresenterSlot::Launch(id), stats }
                if t == &tab_id && *id == launch && stats.memory_mb == 256.0
        )),
        "stats must name the presenter's backbone launch: {events:?}"
    );
}

/// A presenter attaching mid-run shows the engine's current workflow, stuck
/// state, agent status and dashboard at once, not after each next change.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn attaching_restores_live_tab_state_from_the_snapshot() {
    use crate::data::instance_snapshot::{AgentsDashboardWire, WorkflowViewState};
    use crate::data::program_status::{EffectiveStatus, ProgramState};
    use crate::engine::agent_runtime::execution::StuckEvent;

    let unit = unit();
    let tab_id = TabId::new();
    let status = EffectiveStatus {
        state: ProgramState::Working,
        kind: None,
        progress: None,
        msg: Some("compiling".into()),
        app: None,
        id: None,
    };
    let mut backbone = slot("", None);
    backbone.program_status = Some(status.clone());
    let mut tab = tab_snapshot(&unit.root, tab_id.clone(), vec![backbone]);
    let invocation = uuid::Uuid::new_v4();
    tab.workflow = Some(WorkflowViewState::default());
    tab.workflow_invocation_id = Some(invocation);
    tab.context_path = Some(unit.root.join("ctx.md"));
    tab.stuck = true;
    tab.agents_dashboard = Some(AgentsDashboardWire {
        overview: serde_json::json!({"not": "an overview"}),
        watching: true,
    });
    unit.welcome(instance_snapshot(vec![tab]));

    let InitialTabOpened::Normal { tab } = unit.backend.open_initial(session_spec(&unit.root))
    else {
        panic!("expected a normal initial tab");
    };
    let feeds = tab.feeds;
    let presented = feeds.workflow.snapshot();
    assert_eq!(presented.invocation_id, Some(invocation));
    assert_eq!(presented.context_path, Some(unit.root.join("ctx.md")));

    let mut stuck_rx = feeds
        .container
        .events
        .lock()
        .unwrap()
        .drain(..)
        .find_map(|event| match event {
            ContainerSlotEvent::EngineAttached { stuck, .. } => stuck,
            _ => None,
        })
        .expect("the backbone publishes a stuck channel");
    let mut seen = Vec::new();
    while let Ok(event) = stuck_rx.try_recv() {
        seen.push(event);
    }
    assert!(
        seen.iter().any(|e| matches!(e, StuckEvent::Stuck)),
        "{seen:?}"
    );
    assert!(
        seen.iter()
            .any(|e| matches!(e, StuckEvent::Reported(Some(s)) if s == &status)),
        "{seen:?}"
    );
}
