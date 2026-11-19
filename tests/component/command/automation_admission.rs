//! WI 0070 automation journeys through the production execution controller,
//! terminal operations, OSC/native observation decoders and durable store.
//! Controllable fakes only: the "agent" is the test feeding PTY bytes and
//! lifecycle records; the writer is an in-memory channel. Real deadlines.
use awman::command::automation::admission::{codes, AdmissionConfig, TurnPhase};
use awman::command::automation::controller::{
    AutomationHost, ControllerLaunch, ExecutionController, ExitFacts, OwnedExecution,
};
use awman::command::terminals::{
    AccessPolicy, OwnerEventLog, OwnerResolver, TerminalAutomation, TerminalOperations,
    TerminalSession,
};
use awman::data::automation::{
    AutomationAdapterKind, AutomationMode, AutomationState, CleanupStatus, OutputStatus,
    StatusSource, StatusSupport, TurnStatus,
};
use awman::data::automation_records::*;
use awman::data::automation_store::AutomationStore;
use awman::data::config::agents::AutomationTimeouts;
use awman::data::config::units::DurationStr;
use awman::data::daemon_state::FeatureName;
use awman::data::execution_event::EventPayload;
use awman::data::fs::api_db::SqliteSessionStore;
use awman::data::ids::{CommandId, ExecutionId, TerminalId, WorkerSessionId};
use awman::data::settings_profile::SettingsProfileName;
use awman::data::terminal::{
    TerminalClientFrame, TerminalFrame, TerminalOwner, TerminalOwnerInfo, ViewerIdentity,
};
use awman::engine::agent::automation::observer::{new_nonce, ChannelDecoder};
use awman::engine::agent::automation::osc7501::OscLifecycle;
use awman::engine::agent::automation::{adapter, prompt_digest};
use awman::engine::agent_runtime::program_status::Osc7501Scanner;
use awman::engine::terminal_hub::{TerminalHub, TerminalInput, TerminalRegistration};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

struct NoResolver;
impl OwnerResolver for NoResolver {
    fn active_run(&self, _: &awman::data::ids::TaskId) -> Option<awman::data::ids::RunId> {
        None
    }
}
struct NoLog;
impl OwnerEventLog for NoLog {
    fn append(&self, _: &TerminalOwner, _: EventPayload) {}
}

fn fixture_snapshot() -> AutomationSnapshot {
    let fx: serde_json::Value = serde_json::from_str(include_str!(
        "../../../web/test/lib/fixtures/terminal-frames.json"
    ))
    .unwrap();
    serde_json::from_value(fx["automation_examples"]["snapshot"].clone()).unwrap()
}

fn timeouts(startup_secs: u64) -> AutomationTimeouts {
    AutomationTimeouts {
        startup: DurationStr::from_secs(startup_secs),
        cancel: DurationStr::from_secs(2),
        output_drain: DurationStr::from_secs(1),
    }
}

struct Options {
    mode: AutomationMode,
    native: bool,
    osc: bool,
    initial: Option<&'static str>,
    startup_secs: u64,
    terminal: bool,
}
impl Default for Options {
    fn default() -> Self {
        Self {
            mode: AutomationMode::Auto,
            native: true,
            osc: true,
            initial: None,
            startup_secs: 20,
            terminal: true,
        }
    }
}

/// One owned execution with its store, hub terminal and writer.
struct Rig {
    /// This rig's own controller: the process-wide one keeps retired
    /// terminals (and their store) for replay, past the sandbox.
    controller: Arc<ExecutionController>,
    store: AutomationStore,
    command: CommandId,
    hub: Arc<TerminalHub>,
    ops: Arc<TerminalOperations>,
    terminal: Option<TerminalId>,
    owned: OwnedExecution,
    writes: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
    scanner: Osc7501Scanner,
    osc: OscLifecycle,
    channel: ChannelDecoder,
    nonce: String,
    sequence: u64,
    events: Arc<Mutex<Vec<EventPayload>>>,
    // Last: fields drop in order, and Windows cannot remove the tempdir
    // while the store above still holds its database open.
    _sandbox: awman_test_support::TestSandbox,
}

impl Rig {
    fn new(name: &'static str, options: Options) -> Self {
        let sandbox = crate::support::sandbox(name);
        let db = Arc::new(SqliteSessionStore::open(&sandbox.root().join("api")).unwrap());
        let session = WorkerSessionId::new();
        let command = CommandId::new();
        db.insert_session(
            session.as_str(),
            "/fixture",
            &chrono::Utc::now().to_rfc3339(),
        )
        .unwrap();
        db.insert_running_lease_command(command.as_str(), session.as_str(), "chat", "[]", false)
            .unwrap();
        let store = AutomationStore::open(db).unwrap();
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        let execution = ExecutionId::new();
        let controller = Arc::new(ExecutionController::default());
        let owned = controller
            .start(ControllerLaunch {
                host: AutomationHost {
                    store: store.clone(),
                    command_id: command.clone(),
                    attempt: 1,
                    owner_key: "uid:test:fixture".into(),
                    events: Some(Arc::new(move |p| sink.lock().unwrap().push(p))),
                    finished: None,
                },
                execution_id: execution.clone(),
                snapshot: fixture_snapshot(),
                config: AdmissionConfig {
                    minimum: options.mode.minimum_status_support(),
                    osc_enabled: options.osc,
                    native_channel: options.native,
                    delivery: true,
                },
                adapter: adapter(AutomationAdapterKind::Codex),
                timeouts: timeouts(options.startup_secs),
                requested_native: None,
                profile_binding: ExecutionProfileBinding {
                    repository_key: "123456789abcdef0".into(),
                    root_sha256: "123456789abcdef0".repeat(4),
                    provider: fixture_snapshot().provider,
                    name: SettingsProfileName::parse("fixture").unwrap(),
                },
                runtime: "fixture".into(),
                initial_task: options.initial.map(str::to_owned),
            })
            .unwrap();
        let (writer, writes) = tokio::sync::mpsc::unbounded_channel();
        owned.bind_writer(writer);
        let hub = Arc::new(TerminalHub::new(Default::default()));
        let ops = Arc::new(TerminalOperations::new(
            hub.clone(),
            Arc::new(AccessPolicy::default),
            Arc::new(NoLog),
            Arc::new(NoResolver),
        ));
        let _ = ops.install_automation(controller.clone());
        let terminal = options.terminal.then(|| {
            let (stdin, _input) = awman::engine::terminal_hub::input_channel();
            std::mem::forget(_input);
            let handle = hub.register(
                TerminalRegistration {
                    automation: Some(owned.snapshot()),
                    owner: TerminalOwnerInfo {
                        owner: TerminalOwner::Command(command.clone()),
                        feature: FeatureName::Worker,
                        source: None,
                        attempt: 1,
                        allow_remote_input: false,
                    },
                    agent: "codex".into(),
                    model: None,
                    cols: 80,
                    rows: 24,
                },
                TerminalInput {
                    stdin,
                    resize: None,
                },
            );
            owned.bind_terminal(handle.id(), &hub);
            handle.id().clone()
        });
        owned.launching();
        owned.running();
        let nonce = new_nonce();
        Self {
            controller,
            _sandbox: sandbox,
            store,
            command,
            hub,
            ops,
            terminal,
            channel: ChannelDecoder::new(execution.clone(), nonce.clone(), 1),
            nonce,
            sequence: 0,
            osc: OscLifecycle::new(execution, options.osc),
            owned,
            writes,
            scanner: Osc7501Scanner::new(),
            events,
        }
    }

    /// Agent PTY output: decoded only for OSC 7501, otherwise inert bytes.
    fn pty(&mut self, bytes: &[u8]) {
        let observations: Vec<_> = self
            .scanner
            .feed(bytes)
            .events
            .iter()
            .flat_map(|e| self.osc.on_scan_event(e))
            .collect();
        self.owned.observe(observations);
    }

    fn osc_state(&mut self, body: &str) {
        let bytes = format!("\x1b]7501;{body}\x07");
        self.pty(bytes.as_bytes());
    }

    /// One guest lifecycle record through the production channel decoder.
    fn native(&mut self, kind: &str, extra: serde_json::Value) {
        self.sequence += 1;
        let mut record = serde_json::json!({
            "schema": 1,
            "execution_id": self.owned.execution_id().as_str(),
            "nonce": self.nonce,
            "sequence": self.sequence,
            "native_session_id": "thread-1",
            "native_turn_id": null,
            "root": true,
            "kind": kind,
            "payload": {},
        });
        if let serde_json::Value::Object(extra) = extra {
            for (k, v) in extra {
                record[k] = v;
            }
        }
        let mut line = serde_json::to_vec(&record).unwrap();
        line.push(b'\n');
        let observations = self.channel.feed(&line, Instant::now());
        self.owned.observe(observations);
    }

    fn state(&self) -> AutomationSnapshot {
        self.owned.snapshot()
    }

    fn session(&self) -> TerminalSession {
        self.ops
            .open(self.terminal.as_ref().unwrap(), ViewerIdentity::local())
            .unwrap()
    }

    fn submit(
        &self,
        session: &mut TerminalSession,
        request: uuid::Uuid,
        revision: u64,
        prompt: &str,
    ) -> Vec<TerminalFrame> {
        let frames = session
            .handle_client_frame(TerminalClientFrame::SubmitTurn {
                request: SubmitTurnRequest {
                    request_id: request,
                    execution_id: self.owned.execution_id().clone(),
                    expected_revision: revision,
                    prompt: prompt.into(),
                },
            })
            .unwrap()
            .frames;
        self.settle();
        frames
    }

    /// Wait until the writer pump has decided the pending guarded write
    /// (written, or revoked with zero bytes). The pump runs on another
    /// runtime worker and decides under the execution lock.
    fn settle(&self) {
        awman_test_support::wait::wait_for(
            "the writer pump decision",
            Duration::from_secs(5),
            || (self.owned.active_turn_phase() != Some(TurnPhase::Reserved)).then_some(()),
        );
    }

    fn written(&mut self) -> Vec<Vec<u8>> {
        self.settle();
        let mut out = Vec::new();
        while let Ok(bytes) = self.writes.try_recv() {
            out.push(bytes);
        }
        out
    }

    fn turns(&self) -> Vec<TurnRecord> {
        self.store
            .list_turns(&self.command, None, Some(200))
            .unwrap()
            .items
    }

    fn finish(&self) -> awman::command::automation::controller::FinishedExecution {
        self.owned.finish(ExitFacts {
            exit_code: Some(0),
            wait_error: None,
            output_status: OutputStatus::Complete,
            cleanup_status: CleanupStatus::Complete,
        })
    }
}

fn encoded(prompt: &str) -> Vec<u8> {
    adapter(AutomationAdapterKind::Codex)
        .encode_prompt(prompt)
        .unwrap()
        .into_bytes()
}

fn refusal(frames: &[TerminalFrame]) -> Option<&str> {
    match frames {
        [TerminalFrame::OperationError { code, .. }] => Some(code.as_str()),
        _ => None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_guarded_turns_with_dedup_conflicts_staleness_and_no_retry() {
    let mut rig = Rig::new(
        "command::automation_admission::two_guarded_turns_with_dedup_conflicts_staleness_and_no_retry",
        Options::default(),
    );
    let mut session = rig.session();
    session
        .handle_client_frame(TerminalClientFrame::TakeControl)
        .unwrap();
    // Startup without any lifecycle record: unknown/starting, nothing admitted.
    assert_eq!(rig.state().state, AutomationState::Starting);
    let r = rig.submit(
        &mut session,
        uuid::Uuid::new_v4(),
        rig.state().revision,
        "early",
    );
    assert_eq!(refusal(&r), Some(codes::STATUS_SUPPORT_INSUFFICIENT));
    rig.osc_state("state=idle");
    let ready = rig.state();
    assert_eq!(ready.state, AutomationState::Ready);
    assert_eq!(ready.status_support, StatusSupport::BestEffort);
    assert_eq!(ready.status_source, StatusSource::Osc7501);

    // Stale revision is refused with zero writes.
    let r = rig.submit(&mut session, uuid::Uuid::new_v4(), ready.revision + 9, "x");
    assert_eq!(refusal(&r), Some(codes::STALE_REVISION));
    assert!(rig.written().is_empty());

    let first = uuid::Uuid::new_v4();
    let frames = rig.submit(&mut session, first, ready.revision, "Review the diff");
    assert!(
        frames.is_empty(),
        "acceptance is published, not replied: {frames:?}"
    );
    assert_eq!(rig.written(), vec![encoded("Review the diff")]);
    assert_eq!(rig.state().state, AutomationState::Submitting);
    // Identical retry replays the acceptance; never another write.
    let replay = rig.submit(&mut session, first, ready.revision, "Review the diff");
    assert!(
        matches!(&replay[..], [TerminalFrame::TurnAccepted { request_id, .. }] if *request_id == first)
    );
    // Same ID, different content.
    let conflict = rig.submit(&mut session, first, ready.revision, "Something else");
    assert_eq!(refusal(&conflict), Some(codes::IDEMPOTENCY_CONFLICT));
    // A new request while busy is refused immediately, not queued.
    let busy = rig.submit(
        &mut session,
        uuid::Uuid::new_v4(),
        rig.state().revision,
        "more",
    );
    assert_eq!(refusal(&busy), Some(codes::AGENT_NOT_READY));
    // Raw input is refused while submitting/working.
    assert!(session
        .handle_client_frame(TerminalClientFrame::Input {
            bytes: b"x".to_vec()
        })
        .is_err());
    assert!(rig.written().is_empty());

    rig.osc_state("state=working");
    assert_eq!(rig.state().state, AutomationState::Working);
    rig.osc_state("state=done");
    let turns = rig.turns();
    let result = turns[0].result.as_ref().expect("settled");
    assert_eq!(result.status, TurnStatus::CompletedUnverified);
    assert!(!result.completion_verified);
    assert_eq!(result.final_message, None);
    assert_ne!(
        rig.state().state,
        AutomationState::Ready,
        "done is not readiness"
    );
    // A duplicate after the result replays acceptance and the result.
    let replay = rig.submit(&mut session, first, ready.revision, "Review the diff");
    assert!(matches!(
        &replay[..],
        [
            TerminalFrame::TurnAccepted { .. },
            TerminalFrame::TurnResult { .. }
        ]
    ));
    assert!(rig.written().is_empty());

    // Second turn in the same execution after a fresh idle.
    rig.osc_state("state=idle");
    let second = uuid::Uuid::new_v4();
    rig.submit(&mut session, second, rig.state().revision, "Now fix it");
    assert_eq!(rig.written(), vec![encoded("Now fix it")]);
    rig.osc_state("state=working");
    rig.osc_state("state=done");
    let turns = rig.turns();
    assert_eq!(turns.len(), 2);
    assert_eq!(turns[1].ordinal, 2);
    assert!(turns.iter().all(|t| t.result.is_some()));
    let events = rig.events.lock().unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, EventPayload::AgentTurnCompleted { .. }))
            .count(),
        2
    );
    assert!(events
        .iter()
        .any(|e| matches!(e, EventPayload::AgentTurnStarted { .. })));
    drop(events);
    let finished = rig.finish();
    assert!(finished.failure.is_none());
    assert!(!finished.automation.completion_verified);
    assert_eq!(
        finished.automation.status_support,
        StatusSupport::BestEffort
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn initial_task_is_delivered_exactly_once_after_readiness_and_holds_bootstrap_control() {
    let mut rig = Rig::new(
        "command::automation_admission::initial_task_is_delivered_exactly_once_after_readiness_and_holds_bootstrap_control",
        Options {
            initial: Some("Review the diff"),
            ..Default::default()
        },
    );
    // Runtime chatter and a convincing prompt screen admit nothing.
    rig.pty(b"\x1b[32m> Ready for input\x1b[0m\r\n$ ");
    assert!(rig.written().is_empty());
    // External control is held by the command's bootstrap grant.
    let mut session = rig.session();
    assert!(session
        .handle_client_frame(TerminalClientFrame::TakeControl)
        .is_err());
    rig.osc_state("state=idle");
    assert_eq!(rig.written(), vec![encoded("Review the diff")]);
    // Repeated readiness never resends.
    rig.osc_state("state=idle");
    rig.osc_state("state=working");
    assert!(rig.written().is_empty());
    assert_eq!(rig.turns().len(), 1);
    // Acknowledgement released the bootstrap grant.
    assert!(session
        .handle_client_frame(TerminalClientFrame::TakeControl)
        .is_ok());
    rig.osc_state("state=done");
    rig.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idle_chat_creates_no_turn_and_closes_with_one_final_publication() {
    let mut rig = Rig::new(
        "command::automation_admission::idle_chat_creates_no_turn_and_closes_with_one_final_publication",
        Options::default(),
    );
    let mut viewer = rig.hub.subscribe(rig.terminal.as_ref().unwrap()).unwrap();
    rig.osc_state("state=idle");
    assert_eq!(rig.state().state, AutomationState::Ready);
    assert!(rig.turns().is_empty());
    assert!(rig.written().is_empty());
    let mut session = rig.session();
    session
        .handle_client_frame(TerminalClientFrame::TakeControl)
        .unwrap();
    // A control change invalidates readiness; close needs fresh idle too.
    rig.osc_state("state=idle");
    let revision = rig.state().revision;
    let close = uuid::Uuid::new_v4();
    let frames = session
        .handle_client_frame(TerminalClientFrame::CloseExecution {
            request: CloseExecutionRequest {
                request_id: close,
                execution_id: rig.owned.execution_id().clone(),
                expected_revision: revision,
            },
        })
        .unwrap()
        .frames;
    assert!(frames.is_empty());
    assert_eq!(rig.state().state, AutomationState::Stopping);
    assert!(rig.owned.stop_requested().is_cancelled());
    drop(session);
    let finished = rig.finish();
    assert!(finished.failure.is_none());
    // The daemon stores the command result, then the controller publishes
    // execution_closed and the one logical exit.
    let result = awman::data::fs::api_db::CommandResult {
        exit_code: Some(0),
        automation: Some(finished.automation.clone()),
        ..Default::default()
    };
    let completion = awman::data::command_status::CommandCompletion {
        attempt: 1,
        status: awman::data::command_status::CommandStatus::Done,
        exit_code: 0,
        result_json: serde_json::to_string(&result).unwrap(),
        finished_at: chrono::Utc::now().to_rfc3339(),
    };
    assert!(rig
        .store
        .finalize_command(&rig.command, &completion)
        .unwrap());
    rig.controller.command_finalized(&rig.command, &result);
    let mut saw_closed = false;
    loop {
        match tokio::time::timeout(Duration::from_secs(5), viewer.next())
            .await
            .unwrap()
        {
            Some(TerminalFrame::ExecutionClosed { request_id, .. }) => {
                assert_eq!(request_id, close);
                saw_closed = true;
            }
            Some(TerminalFrame::Exit { .. }) => break,
            Some(TerminalFrame::TurnAccepted { .. } | TerminalFrame::TurnResult { .. }) => {
                panic!("no turn exists")
            }
            Some(_) => {}
            None => break,
        }
    }
    assert!(saw_closed, "execution_closed precedes the exit");
    assert_eq!(
        rig.store
            .read_execution(rig.owned.execution_id())
            .unwrap()
            .launch_phase,
        LaunchPhase::Ended
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn require_full_refuses_best_effort_with_zero_writes_and_expires_at_the_startup_deadline() {
    let mut rig = Rig::new(
        "command::automation_admission::require_full_refuses_best_effort_with_zero_writes_and_expires_at_the_startup_deadline",
        Options {
            mode: AutomationMode::RequireFull,
            initial: Some("strict task"),
            startup_secs: 1,
            ..Default::default()
        },
    );
    rig.osc_state("state=idle");
    // Observed best effort stays visible, but cannot deliver the task.
    assert_eq!(rig.state().status_support, StatusSupport::BestEffort);
    let started = Instant::now();
    // Output and chatter do not reset the absolute deadline.
    for _ in 0..5 {
        rig.pty(b"chatter\r\n");
    }
    tokio::time::timeout(
        Duration::from_secs(5),
        rig.owned.stop_requested().cancelled(),
    )
    .await
    .expect("startup deadline stops the execution");
    assert!(started.elapsed() >= Duration::from_millis(500));
    assert_eq!(
        rig.owned.stop_failure().map(|f| f.code),
        Some(codes::STARTUP_TIMEOUT.to_string())
    );
    assert!(rig.written().is_empty(), "zero prompt writes");
    let finished = rig.finish();
    assert_eq!(finished.failure.unwrap().code, codes::STARTUP_TIMEOUT);
    assert!(rig.turns().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_native_lifecycle_two_verified_turns_through_the_guest_channel() {
    let mut rig = Rig::new(
        "command::automation_admission::full_native_lifecycle_two_verified_turns_through_the_guest_channel",
        Options {
            mode: AutomationMode::RequireFull,
            ..Default::default()
        },
    );
    let mut session = rig.session();
    session
        .handle_client_frame(TerminalClientFrame::TakeControl)
        .unwrap();
    // Early session hook is activity only.
    rig.native("session_start", serde_json::json!({}));
    assert_eq!(rig.state().state, AutomationState::Starting);
    assert_eq!(
        rig.state().native_session_id.as_ref().map(|s| s.as_str()),
        Some("thread-1")
    );
    rig.native(
        "facility",
        serde_json::json!({"payload": {"full_lifecycle": true}}),
    );
    rig.native("ready", serde_json::json!({}));
    assert_eq!(rig.state().status_support, StatusSupport::Full);
    assert_eq!(rig.state().state, AutomationState::Ready);
    for (prompt, answer) in [("first task", "done one"), ("second task", "")] {
        rig.submit(
            &mut session,
            uuid::Uuid::new_v4(),
            rig.state().revision,
            prompt,
        );
        assert_eq!(rig.written(), vec![encoded(prompt)]);
        // An unrelated echo (other digest) does not acknowledge.
        rig.native(
            "prompt_accepted",
            serde_json::json!({"native_turn_id": "t-0", "payload": {"prompt_sha256": prompt_digest("other")}}),
        );
        assert_eq!(rig.state().state, AutomationState::Submitting);
        rig.native(
            "prompt_accepted",
            serde_json::json!({"native_turn_id": "t-1", "payload": {"prompt_sha256": prompt_digest(prompt)}}),
        );
        assert_eq!(rig.state().state, AutomationState::Working);
        // Generic stop / continuation hooks never settle a turn.
        rig.native("stop", serde_json::json!({}));
        rig.native("continuation", serde_json::json!({}));
        assert_eq!(rig.state().state, AutomationState::Working);
        // OSC cannot overrule the native working state.
        rig.osc_state("state=done");
        assert_eq!(rig.state().state, AutomationState::Working);
        rig.native(
            "settled",
            serde_json::json!({"native_turn_id": "t-1", "payload": {"outcome": "success", "final_message": answer}}),
        );
        rig.native("ready", serde_json::json!({}));
    }
    let turns = rig.turns();
    assert_eq!(turns.len(), 2);
    for t in &turns {
        let r = t.result.as_ref().unwrap();
        assert_eq!(r.status, TurnStatus::Completed);
        assert!(r.completion_verified);
        assert_eq!(r.status_support, StatusSupport::Full);
    }
    let finished = rig.finish();
    assert!(finished.automation.completion_verified);
    assert_eq!(finished.final_message.as_deref(), Some(""));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_loss_falls_back_to_osc_and_strict_floor_survives_source_loss_race() {
    let mut rig = Rig::new(
        "command::automation_admission::native_loss_falls_back_to_osc_and_strict_floor_survives_source_loss_race",
        Options::default(),
    );
    let mut session = rig.session();
    session
        .handle_client_frame(TerminalClientFrame::TakeControl)
        .unwrap();
    rig.osc_state("state=idle");
    rig.native(
        "facility",
        serde_json::json!({"payload": {"full_lifecycle": true}}),
    );
    rig.native("ready", serde_json::json!({}));
    assert_eq!(rig.state().status_support, StatusSupport::Full);
    let revision = rig.state().revision;
    // A gap loses the native source: revision bumps, readiness invalidated.
    rig.sequence += 1;
    rig.native("ready", serde_json::json!({}));
    assert_eq!(rig.state().status_support, StatusSupport::BestEffort);
    assert!(rig.state().revision > revision);
    assert_ne!(rig.state().state, AutomationState::Ready);
    let r = rig.submit(&mut session, uuid::Uuid::new_v4(), revision, "stale");
    assert_eq!(refusal(&r), Some(codes::STALE_REVISION));
    // A fresh root idle re-admits at best effort.
    rig.osc_state("state=idle");
    assert_eq!(rig.state().state, AutomationState::Ready);
    rig.submit(
        &mut session,
        uuid::Uuid::new_v4(),
        rig.state().revision,
        "after fallback",
    );
    assert_eq!(rig.written().len(), 1);
    // Post-delivery source loss: no replay, honest delivery_unknown.
    rig.pty(b"\x1bc");
    rig.owned.observe(rig.osc.on_stream_end());
    rig.owned.cancel();
    assert!(rig.written().is_empty());
    let finished = rig.finish();
    assert_eq!(
        rig.turns()[0].result.as_ref().unwrap().status,
        TurnStatus::DeliveryUnknown
    );
    assert!(finished.cancelled);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn identical_lifecycle_records_give_identical_decisions_whatever_the_screen_shows() {
    let mut decisions = Vec::new();
    for (i, decoration) in [
        b"plain prompt > ".as_slice(),
        b"\x1b[38;5;196m\xe2\x9d\xaf ready? \x1b[0m\x1b[10;40H".as_slice(),
        b"\x1b[2J\x1b[H(theme dark) Waiting for your input...".as_slice(),
    ]
    .into_iter()
    .enumerate()
    {
        let names = [
            "command::automation_admission::identical_lifecycle_records_give_identical_decisions_whatever_the_screen_shows::0",
            "command::automation_admission::identical_lifecycle_records_give_identical_decisions_whatever_the_screen_shows::1",
            "command::automation_admission::identical_lifecycle_records_give_identical_decisions_whatever_the_screen_shows::2",
        ];
        let mut rig = Rig::new(names[i], Options::default());
        let mut seen = Vec::new();
        rig.pty(decoration);
        seen.push(rig.state().state);
        let mut bytes = decoration.to_vec();
        bytes.extend_from_slice(b"\x1b]7501;state=idle\x07");
        bytes.extend_from_slice(decoration);
        rig.pty(&bytes);
        seen.push(rig.state().state);
        rig.osc_state("state=working");
        seen.push(rig.state().state);
        decisions.push((seen, rig.written().len()));
        rig.finish();
    }
    assert!(decisions.windows(2).all(|w| w[0] == w[1]), "{decisions:?}");
    assert_eq!(decisions[0].0[0], AutomationState::Starting);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn crash_after_delivery_recovers_without_replay_and_another_execution_is_untouched() {
    let mut crashed = Rig::new(
        "command::automation_admission::crash_after_delivery_recovers_without_replay_and_another_execution_is_untouched",
        Options::default(),
    );
    let mut other = Rig::new(
        "command::automation_admission::crash_after_delivery_recovers_without_replay_and_another_execution_is_untouched::other",
        Options::default(),
    );
    other.osc_state("state=idle");
    let other_revision = other.state().revision;
    let mut session = crashed.session();
    session
        .handle_client_frame(TerminalClientFrame::TakeControl)
        .unwrap();
    crashed.osc_state("state=idle");
    crashed.submit(
        &mut session,
        uuid::Uuid::new_v4(),
        crashed.state().revision,
        "task",
    );
    assert_eq!(crashed.written().len(), 1);
    // While its owner is alive (its liveness lease is held), recovery
    // never touches the execution.
    assert!(ExecutionController::recover(&crashed.store)
        .unwrap()
        .is_empty());
    assert_eq!(
        crashed
            .store
            .read_execution(crashed.owned.execution_id())
            .unwrap()
            .launch_phase,
        LaunchPhase::Running
    );
    // The owner dies: a new process recovers from the durable store alone.
    crashed.owned.clone().abandon();
    let recovered = ExecutionController::recover(&crashed.store).unwrap();
    assert!(recovered.contains(&crashed.command));
    let record = crashed
        .store
        .read_execution(crashed.owned.execution_id())
        .unwrap();
    assert_eq!(record.launch_phase, LaunchPhase::Ended);
    assert_eq!(record.end_failure.unwrap().code, codes::EXECUTION_LOST);
    let turn = &crashed.turns()[0];
    assert_eq!(
        turn.result.as_ref().unwrap().status,
        TurnStatus::DeliveryUnknown
    );
    assert!(crashed.written().is_empty(), "no replay");
    // Recovery is idempotent (the ended execution's command is reported for
    // finalization again, nothing is redone) and the other execution is
    // untouched.
    assert_eq!(
        ExecutionController::recover(&crashed.store).unwrap(),
        vec![crashed.command.clone()]
    );
    assert_eq!(crashed.turns().len(), 1);
    assert_eq!(other.state().revision, other_revision);
    assert_eq!(other.state().state, AutomationState::Ready);
    other.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unauthorized_viewers_cannot_submit_and_manual_input_follows_the_state_table() {
    let mut rig = Rig::new(
        "command::automation_admission::unauthorized_viewers_cannot_submit_and_manual_input_follows_the_state_table",
        Options::default(),
    );
    rig.osc_state("state=idle");
    let mut network = ViewerIdentity::local();
    network.origin = awman::data::daemon_state::RequestOrigin::Browser;
    let mut browser = rig
        .ops
        .open(rig.terminal.as_ref().unwrap(), network)
        .unwrap();
    let r = rig.submit(
        &mut browser,
        uuid::Uuid::new_v4(),
        rig.state().revision,
        "x",
    );
    assert_eq!(refusal(&r), Some(codes::INPUT_NOT_ALLOWED));
    drop(browser);
    let mut session = rig.session();
    // Without the grant: input_not_allowed.
    let r = rig.submit(
        &mut session,
        uuid::Uuid::new_v4(),
        rig.state().revision,
        "x",
    );
    assert_eq!(refusal(&r), Some(codes::INPUT_NOT_ALLOWED));
    session
        .handle_client_frame(TerminalClientFrame::TakeControl)
        .unwrap();
    // Control change invalidated readiness: a fresh idle is needed.
    assert_ne!(rig.state().state, AutomationState::Ready);
    rig.osc_state("state=idle");
    // Manual input in ready is admitted and invalidates readiness.
    session
        .handle_client_frame(TerminalClientFrame::Input {
            bytes: b"y".to_vec(),
        })
        .unwrap();
    assert_eq!(rig.state().state, AutomationState::Unknown);
    // Blocked: manual input allowed (e.g. answering a login prompt).
    rig.osc_state("state=blocked:kind=auth");
    assert_eq!(rig.state().state, AutomationState::BlockedAuth);
    session
        .handle_client_frame(TerminalClientFrame::Input {
            bytes: b"z".to_vec(),
        })
        .unwrap();
    assert!(
        rig.written().is_empty(),
        "manual bytes never pass the guarded writer"
    );
    assert!(rig.turns().is_empty());
    rig.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_adapter_uses_the_same_observers_and_generic_osc_never_satisfies_require_full() {
    for kind in AutomationAdapterKind::ALL {
        let a = adapter(kind);
        let native =
            a.native_lifecycle() == awman::engine::agent::automation::NativeLifecycle::GuestChannel;
        assert_eq!(native, kind.is_provider_adapter(), "{kind:?}");
        // Task-only delivery for every adapter: no command shortcut passes.
        assert!(a.encode_prompt("/help").is_err(), "{kind:?}");
        // Delivery is established per adapter: a first-party entrypoint
        // pastes whole tasks; an unknown OSC program has no safe delivery.
        let encoded = a.encode_prompt("Explain --model");
        if kind.is_provider_adapter() {
            assert!(encoded.is_ok(), "{kind:?}");
        } else {
            assert!(
                matches!(
                    encoded,
                    Err(awman::engine::agent::automation::PromptRefusal::Unavailable(_))
                ),
                "{kind:?}"
            );
        }
    }
    // The generic adapter has no native channel: OSC reaches best effort only.
    let mut rig = Rig::new(
        "command::automation_admission::every_adapter_uses_the_same_observers_and_generic_osc_never_satisfies_require_full",
        Options {
            mode: AutomationMode::RequireFull,
            native: false,
            startup_secs: 1,
            ..Default::default()
        },
    );
    rig.native("ready", serde_json::json!({}));
    rig.osc_state("state=idle");
    assert_eq!(rig.state().status_support, StatusSupport::BestEffort);
    assert_eq!(rig.state().native_session_id, None);
    tokio::time::timeout(
        Duration::from_secs(5),
        rig.owned.stop_requested().cancelled(),
    )
    .await
    .unwrap();
    assert_eq!(
        rig.owned.stop_failure().unwrap().code,
        codes::STARTUP_TIMEOUT
    );
    rig.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disabled_osc_reporting_without_native_records_stays_unknown_and_admits_nothing() {
    let mut rig = Rig::new(
        "command::automation_admission::disabled_osc_reporting_without_native_records_stays_unknown_and_admits_nothing",
        Options {
            osc: false,
            initial: Some("task"),
            startup_secs: 1,
            ..Default::default()
        },
    );
    rig.osc_state("state=idle");
    assert_eq!(rig.state().status_support, StatusSupport::None);
    assert_eq!(rig.state().status_source, StatusSource::None);
    tokio::time::timeout(
        Duration::from_secs(5),
        rig.owned.stop_requested().cancelled(),
    )
    .await
    .unwrap();
    assert!(rig.written().is_empty());
    rig.finish();
}

#[test]
fn capability_declarations_are_unknown_not_unsupported_and_never_launch() {
    let sandbox = crate::support::sandbox(
        "command::automation_admission::capability_declarations_are_unknown_not_unsupported_and_never_launch",
    );
    let session = super::settings_profiles::session(&sandbox);
    let caps = awman::command::automation::capabilities::agent_capabilities(&session, "docker");
    for agent in [
        "claude",
        "codex",
        "opencode",
        "opencode-v2",
        "cursor",
        "copilot",
        "cline",
        "antigravity",
    ] {
        let c = caps
            .iter()
            .find(|c| c.agent == agent)
            .unwrap_or_else(|| panic!("{agent} declared"));
        assert_eq!(
            c.evidence,
            awman::data::automation::CapabilityEvidence::Declaration
        );
        assert_eq!(
            c.terminal_turns.supported, None,
            "{agent}: unknown until observed"
        );
        assert_eq!(c.terminal_turns.status_support, None);
        assert_eq!(c.native_resume.interactive, Some(true), "{agent}");
        assert!(c.settings_profiles, "{agent}");
        assert!(!c.credential_updates.routes.is_empty());
        for route in &c.credential_updates.routes {
            assert_eq!(
                route.live_update,
                if route.delivery == awman::data::credential_update::CredentialDelivery::Env {
                    Some(false)
                } else {
                    None
                }
            );
            assert_eq!(route.restart_supported, None);
        }
    }
    assert!(caps.iter().all(|c| c.agent != "gemini"));
}

/// Automation findings 19, 1 and 18 through the real terminal subscription:
/// the durable acceptance precedes the submitting state; a launch secret in
/// a native answer is redacted in the stored result and the published
/// frame; and after the execution ended a retried request still replays its
/// acceptance and result (a new one is stale, never "not automation").
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn acceptance_precedes_state_results_are_redacted_and_replay_outlives_the_execution() {
    let mut rig = Rig::new(
        "command::automation_admission::acceptance_precedes_state_results_are_redacted_and_replay_outlives_the_execution",
        Options::default(),
    );
    let secret = "sk-fixture-credential-41f2";
    let redaction = awman::engine::auth::redaction::RedactionSet::new();
    redaction.add(awman::data::secret::SecretString::new(secret));
    rig.owned.set_redaction(redaction);
    let mut session = rig.session();
    session
        .handle_client_frame(TerminalClientFrame::TakeControl)
        .unwrap();
    rig.native(
        "facility",
        serde_json::json!({"payload": {"full_lifecycle": true}}),
    );
    rig.native("ready", serde_json::json!({}));
    assert_eq!(rig.state().state, AutomationState::Ready);
    let mut frames = rig.hub.subscribe(rig.terminal.as_ref().unwrap()).unwrap();
    let request = uuid::Uuid::new_v4();
    let revision = rig.state().revision;
    rig.submit(&mut session, request, revision, "task one");
    let mut order = Vec::new();
    while order.len() < 2 {
        match tokio::time::timeout(Duration::from_secs(2), frames.next()).await {
            Ok(Some(TerminalFrame::TurnAccepted { .. })) => order.push("accepted"),
            Ok(Some(TerminalFrame::AutomationState { snapshot }))
                if snapshot.state == AutomationState::Submitting =>
            {
                order.push("submitting")
            }
            Ok(Some(_)) => {}
            other => panic!("missing frames {order:?}: {other:?}"),
        }
    }
    assert_eq!(order, ["accepted", "submitting"]);
    assert_eq!(rig.written(), vec![encoded("task one")]);
    rig.native(
        "prompt_accepted",
        serde_json::json!({"native_turn_id": "t-1", "payload": {"prompt_sha256": prompt_digest("task one")}}),
    );
    rig.native(
        "settled",
        serde_json::json!({"native_turn_id": "t-1", "payload": {"outcome": "success", "final_message": format!("used {secret} ok")}}),
    );
    let stored = rig.turns()[0].result.clone().unwrap();
    assert_eq!(stored.final_message.as_deref(), Some("used *** ok"));
    let published = loop {
        match tokio::time::timeout(Duration::from_secs(2), frames.next()).await {
            Ok(Some(TerminalFrame::TurnResult { result, .. })) => break result,
            Ok(Some(_)) => {}
            other => panic!("no turn result frame: {other:?}"),
        }
    };
    assert_eq!(published.final_message.as_deref(), Some("used *** ok"));
    let finished = rig.finish();
    assert_eq!(finished.final_message.as_deref(), Some("used *** ok"));
    // The execution is gone from the controller; the durable request is not.
    let replay = rig.submit(&mut session, request, revision, "task one");
    assert!(
        matches!(
            &replay[..],
            [
                TerminalFrame::TurnAccepted { .. },
                TerminalFrame::TurnResult { .. }
            ]
        ),
        "{replay:?}"
    );
    let conflict = rig.submit(&mut session, request, revision, "other");
    assert_eq!(refusal(&conflict), Some(codes::IDEMPOTENCY_CONFLICT));
    let fresh = rig.submit(&mut session, uuid::Uuid::new_v4(), revision, "again");
    assert_eq!(refusal(&fresh), Some(codes::STALE_EXECUTION));
    assert!(rig.written().is_empty(), "nothing is ever redelivered");
}

/// Automation finding 2: the write is authorized at the writer, not at
/// enqueue. A cancel that wins before the writer pump runs revokes the
/// queued task with zero bytes and the turn ends cancelled.
#[tokio::test(flavor = "current_thread")]
async fn a_cancel_before_writer_ingress_revokes_the_queued_task_with_zero_bytes() {
    let mut rig = Rig::new(
        "command::automation_admission::a_cancel_before_writer_ingress_revokes_the_queued_task_with_zero_bytes",
        Options::default(),
    );
    let mut session = rig.session();
    session
        .handle_client_frame(TerminalClientFrame::TakeControl)
        .unwrap();
    rig.osc_state("state=idle");
    // Accepted on this (single) runtime thread: the pump cannot run before
    // the cancel below.
    session
        .handle_client_frame(TerminalClientFrame::SubmitTurn {
            request: SubmitTurnRequest {
                request_id: uuid::Uuid::new_v4(),
                execution_id: rig.owned.execution_id().clone(),
                expected_revision: rig.state().revision,
                prompt: "never written".into(),
            },
        })
        .unwrap();
    assert_eq!(rig.state().state, AutomationState::Submitting);
    rig.owned.cancel();
    // Let the (ready) pump task run on this thread now.
    for _ in 0..16 {
        tokio::task::yield_now().await;
    }
    assert!(
        rig.writes.try_recv().is_err(),
        "zero bytes after a revoked permit"
    );
    let result = rig.turns()[0].result.clone().unwrap();
    assert_eq!(result.status, TurnStatus::Cancelled);
    rig.finish();
}

/// Automation finding 3: the binary attach transport's control changes and
/// raw bytes pass the same gate as typed terminal operations.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn attach_transport_control_and_raw_input_pass_the_automation_gate() {
    let mut rig = Rig::new(
        "command::automation_admission::attach_transport_control_and_raw_input_pass_the_automation_gate",
        Options::default(),
    );
    let terminal = rig.terminal.clone().unwrap();
    rig.osc_state("state=idle");
    assert_eq!(rig.state().state, AutomationState::Ready);
    // Taking control through the attach path invalidates readiness.
    let grant = rig.hub.take_control_gated(&terminal).unwrap();
    assert_ne!(rig.state().state, AutomationState::Ready);
    rig.osc_state("state=idle");
    assert_eq!(rig.state().state, AutomationState::Ready);
    // Raw bytes in ready are admitted and invalidate readiness.
    assert!(rig.hub.admit_raw_input(&terminal));
    assert_eq!(rig.state().state, AutomationState::Unknown);
    // While a guarded task is in flight, raw attach bytes are refused.
    rig.osc_state("state=idle");
    let revision = rig.state().revision;
    let request = SubmitTurnRequest {
        request_id: uuid::Uuid::new_v4(),
        execution_id: rig.owned.execution_id().clone(),
        expected_revision: revision,
        prompt: "guarded".into(),
    };
    let reply = rig
        .controller
        .submit(&terminal, &ViewerIdentity::local(), Some(&grant), request);
    assert!(reply.is_ok(), "{reply:?}");
    rig.settle();
    assert_eq!(rig.state().state, AutomationState::Submitting);
    assert!(!rig.hub.admit_raw_input(&terminal));
    rig.osc_state("state=working");
    assert!(!rig.hub.admit_raw_input(&terminal));
    assert_eq!(rig.written(), vec![encoded("guarded")]);
    drop(grant);
    rig.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn credential_uncertainty_revokes_fresh_readiness_after_next_request_staging() {
    use awman::command::automation::controller::{CredentialDriver, StagedCredential, UpdateFence};
    use awman::data::{
        credential_update::*,
        secret::{CommandSecrets, SecretString},
    };
    struct Delivery;
    impl CredentialDriver for Delivery {
        fn stage<'a>(
            &'a self,
            _: &'a UpdateFence,
            _: Arc<CommandSecrets>,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = Result<StagedCredential, &'static str>>
                    + Send
                    + 'a,
            >,
        > {
            Box::pin(async {
                Ok(StagedCredential {
                    admission_while_pending: true,
                })
            })
        }
    }
    let mut rig=Rig::new("command::automation_admission::credential_uncertainty_revokes_fresh_readiness_after_next_request_staging",Options{terminal:false,..Default::default()});
    rig.osc_state("state=idle");
    assert_eq!(rig.state().state, AutomationState::Ready);
    let record = rig.store.read_execution(rig.owned.execution_id()).unwrap();
    let slot =
        crate::data::credential_updates::enroll(&rig.store, &record, CredentialUpdateMethod::Live);
    let host = AutomationHost {
        store: rig.store.clone(),
        command_id: rig.command.clone(),
        attempt: 1,
        owner_key: record.owner_key.clone(),
        events: None,
        finished: None,
    };
    let controller = rig.owned.controller().clone();
    let enrollment = controller
        .register_credentials(
            host.clone(),
            record.execution_id.clone(),
            Arc::new(Delivery),
            Default::default(),
        )
        .unwrap();
    let request = crate::data::credential_updates::intent(&record, &slot);
    let op = controller
        .stage_update(
            &ViewerIdentity::local(),
            &host,
            CredentialUpdateRequest {
                intent: request,
                secrets: Some(Arc::new(
                    CommandSecrets::new(vec![(
                        "OPENAI_API_KEY".into(),
                        SecretString::new("test-secret-next-request"),
                    )])
                    .unwrap(),
                )),
            },
            None,
        )
        .unwrap();
    awman_test_support::wait_for_async(
        "proven next-request staging",
        Duration::from_secs(1),
        || {
            let store = rig.store.clone();
            let record = record.clone();
            async move {
                store
                    .effective_credentials(&record.owner_key, &record.execution_id)
                    .ok()
                    .filter(|c| c.slots[0].admission_while_pending)
            }
        },
    )
    .await;
    awman_test_support::wait_for_async("admission restored after staging",Duration::from_secs(1),||{let owned=rig.owned.clone();async move{(owned.snapshot().state!=AutomationState::CredentialUpdating).then_some(())}}).await;
    rig.osc_state("state=idle");
    assert_eq!(rig.state().state, AutomationState::Ready);
    controller.fail_update(&rig.command, op.request_id, "application_unconfirmed");
    rig.osc_state("state=idle");
    assert_eq!(rig.state().state, AutomationState::CredentialUpdating);
    assert_eq!(
        rig.store
            .effective_credentials(&record.owner_key, &record.execution_id)
            .unwrap()
            .slots[0]
            .generation_state,
        GenerationState::Unknown
    );
    drop(enrollment);
    rig.finish();
}
