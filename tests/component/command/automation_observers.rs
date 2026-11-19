//! WI 0070 observer and headless-result journeys through production code:
//! the execution-private guest channel (owned directory, host reader,
//! nonce/sequence decoder, controller), the durable headless one-shot turn
//! and crash recovery. The "guest helper" is the test appending JSONL to the
//! owned record file; no provider runs. Real short deadlines.
use awman::command::automation::admission::{codes, AdmissionConfig};
use awman::command::automation::controller::{
    global, AutomationHost, ControllerLaunch, ExecutionController, ExitFacts, OwnedExecution,
};
use awman::command::automation::launch::HeadlessRun;
use awman::command::automation::results::HeadlessObservation;
use awman::data::automation::{
    AutomationAdapterKind, AutomationMode, AutomationState, CleanupStatus, CompletionSource,
    OutputStatus, StatusSupport, TurnStatus,
};
use awman::data::automation_records::*;
use awman::data::automation_store::AutomationStore;
use awman::data::config::agents::AutomationTimeouts;
use awman::data::config::units::DurationStr;
use awman::data::execution_event::EventPayload;
use awman::data::fs::api_db::SqliteSessionStore;
use awman::data::ids::{CommandId, ExecutionId, WorkerSessionId};
use awman::data::settings_profile::{NativeSessionId, SettingsProfileName};
use awman::engine::agent::automation::adapter;
use awman::engine::agent::automation::observer::{
    read_channel, ChannelMount, CHANNEL_DIR_ENV, CHANNEL_FILE, EXECUTION_ENV, NONCE_ENV,
};
use awman::engine::auth::redaction::RedactionSet;
use awman::engine::container::options::ContainerOption;
use std::io::Write as _;
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn fixture_snapshot() -> AutomationSnapshot {
    let fx: serde_json::Value = serde_json::from_str(include_str!(
        "../../../web/test/lib/fixtures/terminal-frames.json"
    ))
    .unwrap();
    serde_json::from_value(fx["automation_examples"]["snapshot"].clone()).unwrap()
}

fn timeouts() -> AutomationTimeouts {
    AutomationTimeouts {
        startup: DurationStr::from_secs(10),
        cancel: DurationStr::from_secs(2),
        output_drain: DurationStr::from_secs(1),
    }
}

fn binding() -> ExecutionProfileBinding {
    ExecutionProfileBinding {
        repository_key: "123456789abcdef0".into(),
        root_sha256: "123456789abcdef0".repeat(4),
        provider: fixture_snapshot().provider,
        name: SettingsProfileName::parse("fixture").unwrap(),
    }
}

/// A durable command row plus its automation store.
struct Host {
    sandbox: awman_test_support::TestSandbox,
    db: Arc<SqliteSessionStore>,
    store: AutomationStore,
    command: CommandId,
    events: Arc<Mutex<Vec<EventPayload>>>,
}

impl Host {
    fn new(name: &'static str) -> Self {
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
        db.insert_running_lease_command(
            command.as_str(),
            session.as_str(),
            "exec prompt",
            "[]",
            false,
        )
        .unwrap();
        Self {
            store: AutomationStore::open(db.clone()).unwrap(),
            db,
            sandbox,
            command,
            events: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn host(&self) -> AutomationHost {
        let sink = self.events.clone();
        AutomationHost {
            store: self.store.clone(),
            command_id: self.command.clone(),
            attempt: 1,
            owner_key: "uid:test:fixture".into(),
            events: Some(Arc::new(move |p| sink.lock().unwrap().push(p))),
            finished: None,
        }
    }

    fn event_names(&self) -> Vec<&'static str> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .map(|e| e.sse_event_type())
            .collect()
    }
}

fn interactive(host: &Host, execution: &ExecutionId) -> OwnedExecution {
    let owned = global()
        .start(ControllerLaunch {
            host: host.host(),
            execution_id: execution.clone(),
            snapshot: fixture_snapshot(),
            config: AdmissionConfig {
                minimum: AutomationMode::RequireFull.minimum_status_support(),
                osc_enabled: false,
                native_channel: true,
                delivery: true,
            },
            adapter: adapter(AutomationAdapterKind::Claude),
            timeouts: timeouts(),
            requested_native: None,
            profile_binding: binding(),
            runtime: "fixture".into(),
            initial_task: None,
        })
        .unwrap();
    owned.launching();
    owned.running();
    owned
}

/// The awman guest helper's view of the channel: append whole JSONL
/// records to the fixed file in the mounted directory.
struct GuestHelper {
    file: std::path::PathBuf,
    execution: String,
    nonce: String,
    sequence: u64,
}

impl GuestHelper {
    fn new(mount: &ChannelMount) -> Self {
        let options = mount.container_options();
        let nonce = options
            .iter()
            .find_map(|o| match o {
                ContainerOption::EnvLiteral(e) if e.key == NONCE_ENV => Some(e.value.clone()),
                _ => None,
            })
            .unwrap();
        Self {
            file: mount.host_dir().join(CHANNEL_FILE),
            execution: mount.execution_id().as_str().to_owned(),
            nonce,
            sequence: 0,
        }
    }

    fn append(&mut self, kind: &str, payload: serde_json::Value) {
        self.sequence += 1;
        let mut line = serde_json::to_vec(&serde_json::json!({
            "schema": 1,
            "execution_id": self.execution,
            "nonce": self.nonce,
            "sequence": self.sequence,
            "native_session_id": "sess-1",
            "native_turn_id": null,
            "root": true,
            "kind": kind,
            "payload": payload,
        }))
        .unwrap();
        line.push(b'\n');
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.file)
            .unwrap();
        f.write_all(&line).unwrap();
    }
}

/// Wait (with the owned test foundation's bounded backoff) until `check`.
async fn until(what: &str, mut check: impl FnMut() -> bool) {
    awman_test_support::wait::wait_for_async(what, Duration::from_secs(5), || {
        let done = check();
        async move { done.then_some(()) }
    })
    .await;
}

fn spawn_reader(
    mount: &ChannelMount,
    owned: &OwnedExecution,
) -> (
    tokio_util::sync::CancellationToken,
    tokio::task::JoinHandle<()>,
) {
    let stop = tokio_util::sync::CancellationToken::new();
    let sink_owner = owned.clone();
    let task = tokio::spawn(read_channel(
        mount.host_dir().to_path_buf(),
        Arc::new(Mutex::new(mount.decoder())),
        Arc::new(move |o| sink_owner.observe(o)),
        RedactionSet::default(),
        stop.clone(),
    ));
    (stop, task)
}

#[tokio::test]
async fn guest_channel_mount_reader_and_controller_establish_full_and_drain_the_tail() {
    let host = Host::new(
        "command::automation_observers::guest_channel_mount_reader_and_controller_establish_full_and_drain_the_tail",
    );
    let execution = ExecutionId::new();
    let mount = ChannelMount::create(host.sandbox.root(), execution.clone()).unwrap();
    // The guest sees only the owned directory at /awman/automation plus
    // its binding; never the profile root, a socket or a host path.
    let options = mount.container_options();
    assert!(options.iter().any(|o| matches!(o,
        ContainerOption::TemporaryDirectoryMount { container_path, env_var, .. }
            if container_path == std::path::Path::new("/awman/automation")
                && env_var.as_deref() == Some(CHANNEL_DIR_ENV))));
    assert!(
        options.iter().any(|o| matches!(o,
        ContainerOption::EnvLiteral(e) if e.key == EXECUTION_ENV && e.value == execution.as_str()))
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(mount.host_dir())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
    }
    let owned = interactive(&host, &execution);
    let (stop, task) = spawn_reader(&mount, &owned);
    let mut guest = GuestHelper::new(&mount);
    // An early session hook is activity only: no readiness, no full tier.
    guest.append("session_start", serde_json::json!({}));
    until("identity", || owned.snapshot().native_session_id.is_some()).await;
    assert_eq!(owned.snapshot().state, AutomationState::Starting);
    assert_eq!(owned.snapshot().status_support, StatusSupport::None);
    guest.append("facility", serde_json::json!({"full_lifecycle": true}));
    guest.append("ready", serde_json::json!({}));
    until("full readiness", || {
        owned.snapshot().state == AutomationState::Ready
    })
    .await;
    assert_eq!(owned.snapshot().status_support, StatusSupport::Full);
    // A record appended as the reader stops is still drained; the end of
    // the generation is a source loss, not idle. The blocked veto survives
    // the loss.
    guest.append("blocked", serde_json::json!({"reason": "permission"}));
    stop.cancel();
    task.await.unwrap();
    let after = owned.snapshot();
    assert_eq!(after.state, AutomationState::BlockedPermission);
    assert_eq!(after.status_support, StatusSupport::None);
    assert!(after.status_reason.unwrap().contains("lost"));
    let finished = owned.finish(ExitFacts {
        exit_code: Some(0),
        wait_error: None,
        output_status: OutputStatus::Complete,
        cleanup_status: CleanupStatus::Complete,
    });
    // No turn was accepted: nothing is verified.
    assert!(!finished.automation.completion_verified);
    assert!(host.event_names().contains(&"agent_identity"));
}

#[tokio::test]
async fn a_symlinked_or_foreign_record_file_is_never_followed_or_trusted() {
    let host = Host::new(
        "command::automation_observers::a_symlinked_or_foreign_record_file_is_never_followed_or_trusted",
    );
    // (1) Records with another execution's nonce cannot affect this root.
    let execution = ExecutionId::new();
    let mount = ChannelMount::create(host.sandbox.root(), execution.clone()).unwrap();
    let owned = interactive(&host, &execution);
    let (stop, task) = spawn_reader(&mount, &owned);
    let other = ChannelMount::create(host.sandbox.root(), execution.clone()).unwrap();
    let mut foreign = GuestHelper::new(&other);
    foreign.file = mount.host_dir().join(CHANNEL_FILE);
    foreign.append("ready", serde_json::json!({}));
    // A later valid record of this execution proves the reader consumed
    // (and ignored) the foreign one: identity only, no readiness.
    let mut own = GuestHelper::new(&mount);
    own.append("session_start", serde_json::json!({}));
    until("own identity", || {
        owned.snapshot().native_session_id.is_some()
    })
    .await;
    assert_eq!(owned.snapshot().state, AutomationState::Starting);
    assert_eq!(owned.snapshot().status_support, StatusSupport::None);
    stop.cancel();
    task.await.unwrap();
    owned.finish(ExitFacts {
        exit_code: Some(0),
        wait_error: None,
        output_status: OutputStatus::Complete,
        cleanup_status: CleanupStatus::Complete,
    });

    // (2) A record file replaced by a symlink to valid records is a source
    // failure; the target is never read.
    #[cfg(unix)]
    {
        let host = Host::new(
            "command::automation_observers::a_symlinked_or_foreign_record_file_is_never_followed_or_trusted::symlink",
        );
        let execution = ExecutionId::new();
        let mount = ChannelMount::create(host.sandbox.root(), execution.clone()).unwrap();
        let target = host.sandbox.root().join("elsewhere.jsonl");
        let mut guest = GuestHelper::new(&mount);
        guest.file = target.clone();
        guest.append("ready", serde_json::json!({}));
        std::os::unix::fs::symlink(&target, mount.host_dir().join(CHANNEL_FILE)).unwrap();
        let owned = interactive(&host, &execution);
        let (stop, task) = spawn_reader(&mount, &owned);
        until("source failure", || {
            owned
                .snapshot()
                .status_reason
                .is_some_and(|r| r.contains("lost"))
        })
        .await;
        assert_ne!(owned.snapshot().state, AutomationState::Ready);
        stop.cancel();
        task.await.unwrap();
        owned.finish(ExitFacts {
            exit_code: Some(0),
            wait_error: None,
            output_status: OutputStatus::Complete,
            cleanup_status: CleanupStatus::Complete,
        });
    }
}

fn headless(host: &Host) -> HeadlessRun {
    let execution = ExecutionId::new();
    let mut snapshot = fixture_snapshot();
    snapshot.execution_id = execution;
    snapshot.state = AutomationState::Starting;
    snapshot.revision = 0;
    snapshot.active_turn_id = None;
    snapshot.native_session_id = None;
    snapshot.status_support = StatusSupport::None;
    snapshot.status_source = awman::data::automation::StatusSource::None;
    let mut run = HeadlessRun::reserve(
        host.host(),
        snapshot,
        binding(),
        "fixture".into(),
        None,
        &timeouts(),
        "Review the diff",
    )
    .unwrap();
    run.launching();
    run.running();
    run
}

fn observed(
    identity: bool,
    outcome: Option<Result<Option<String>, String>>,
) -> HeadlessObservation {
    HeadlessObservation {
        identity: identity.then(|| NativeSessionId::parse("sess-1").unwrap()),
        outcome,
        protocol_error: None,
    }
}

#[tokio::test]
async fn headless_automation_reserves_one_durable_turn_and_needs_identity_outcome_and_exit() {
    let host = Host::new(
        "command::automation_observers::headless_automation_reserves_one_durable_turn_and_needs_identity_outcome_and_exit",
    );
    // Before the provider runs, one durable turn exists and the execution
    // is still `starting` with that turn active.
    let run = headless(&host);
    let page = host.store.list_turns(&host.command, None, None).unwrap();
    assert_eq!(page.items.len(), 1);
    assert!(page.items[0].result.is_none());
    let current = host
        .store
        .current_execution(&host.command)
        .unwrap()
        .unwrap();
    let snapshot = current.snapshot.unwrap();
    assert_eq!(snapshot.state, AutomationState::Starting);
    assert_eq!(snapshot.active_turn_id, Some(page.items[0].turn_id.clone()));
    // Identity + native success + exit 0 is a verified completion, even
    // with an explicitly empty answer.
    let finished =
        run.finish_observed(&observed(true, Some(Ok(Some(String::new())))), Ok(0), false);
    assert!(finished.failure.is_none());
    assert!(finished.automation.completion_verified);
    assert_eq!(
        finished
            .automation
            .native_session_id
            .as_ref()
            .map(|s| s.as_str()),
        Some("sess-1")
    );
    let turns = host
        .store
        .list_turns(&host.command, None, None)
        .unwrap()
        .items;
    let result = turns[0].result.as_ref().unwrap();
    assert_eq!(result.status, TurnStatus::Completed);
    assert_eq!(result.completion_source, CompletionSource::Native);
    assert_eq!(
        turns[0].native_session_id.as_ref().map(|s| s.as_str()),
        Some("sess-1")
    );
    let record = host
        .store
        .read_execution(&finished.automation.execution_id)
        .unwrap();
    assert_eq!(record.launch_phase, LaunchPhase::Ended);
    assert_eq!(record.final_assurance.as_ref(), Some(&finished.automation));
    let names = host.event_names();
    let started = names
        .iter()
        .position(|n| *n == "agent_turn_started")
        .unwrap();
    let completed = names
        .iter()
        .position(|n| *n == "agent_turn_completed")
        .unwrap();
    assert!(started < completed);
    assert_eq!(names.last(), Some(&"agent_state"));

    // Exit zero alone, a candidate-only stream or a missing identity never
    // succeed; a nonzero exit defeats a success record; known provider
    // failure stays primary.
    for (identity, outcome, exit, code) in [
        (false, Some(Ok(None)), Ok(0), codes::AGENT_PROTOCOL_ERROR),
        (true, None, Ok(0), codes::AGENT_PROTOCOL_ERROR),
        (true, Some(Ok(None)), Ok(3), codes::AGENT_TURN_FAILED),
        (
            true,
            Some(Err("model refused".into())),
            Ok(1),
            codes::AGENT_TURN_FAILED,
        ),
        (
            true,
            Some(Ok(None)),
            Err("wait failed".into()),
            codes::RUNTIME_WAIT_FAILED,
        ),
    ] {
        let host = Host::new(
            "command::automation_observers::headless_automation_reserves_one_durable_turn_and_needs_identity_outcome_and_exit::case",
        );
        let run = headless(&host);
        let finished = run.finish_observed(&observed(identity, outcome), exit, false);
        assert_eq!(
            finished.failure.as_ref().map(|f| f.code.as_str()),
            Some(code)
        );
        assert!(!finished.automation.completion_verified);
        let turns = host
            .store
            .list_turns(&host.command, None, None)
            .unwrap()
            .items;
        assert_ne!(
            turns[0].result.as_ref().unwrap().status,
            TurnStatus::Completed
        );
    }

    // An accepted cancel before a native settlement is cancelled, not failed.
    let host = Host::new(
        "command::automation_observers::headless_automation_reserves_one_durable_turn_and_needs_identity_outcome_and_exit::cancel",
    );
    let finished = headless(&host).finish_observed(&observed(true, None), Ok(130), true);
    assert!(finished.cancelled);
    let turns = host
        .store
        .list_turns(&host.command, None, None)
        .unwrap()
        .items;
    assert_eq!(
        turns[0].result.as_ref().unwrap().status,
        TurnStatus::Cancelled
    );
}

#[tokio::test]
async fn a_crash_during_a_headless_one_shot_recovers_as_delivery_unknown_without_requeue() {
    let host = Host::new(
        "command::automation_observers::a_crash_during_a_headless_one_shot_recovers_as_delivery_unknown_without_requeue",
    );
    let run = headless(&host);
    // The process "crashes": the run is dropped without finishing.
    drop(run);
    let affected = ExecutionController::recover(&host.store).unwrap();
    assert_eq!(affected, vec![host.command.clone()]);
    let turns = host
        .store
        .list_turns(&host.command, None, None)
        .unwrap()
        .items;
    let result = turns[0].result.as_ref().unwrap();
    // The task was in the provider's argv: delivery cannot be disproved.
    assert_eq!(result.status, TurnStatus::DeliveryUnknown);
    let record = host
        .store
        .current_execution(&host.command)
        .unwrap()
        .unwrap();
    assert_eq!(record.launch_phase, LaunchPhase::Ended);
    assert_eq!(
        record.end_failure.as_ref().map(|f| f.code.as_str()),
        Some(codes::EXECUTION_LOST)
    );
    // Recovery is idempotent and never replays: the ended execution's
    // command is still reported for finalization (nothing stored it), but
    // no turn or result changes.
    assert_eq!(
        ExecutionController::recover(&host.store).unwrap(),
        vec![host.command.clone()]
    );
    assert_eq!(
        host.store
            .list_turns(&host.command, None, None)
            .unwrap()
            .items
            .len(),
        1
    );
}

#[tokio::test]
async fn a_finalized_command_never_gains_another_execution() {
    let host = Host::new(
        "command::automation_observers::a_finalized_command_never_gains_another_execution",
    );
    let finished = headless(&host).finish_observed(&observed(true, Some(Ok(None))), Ok(0), false);
    assert!(finished.failure.is_none());
    let completion = awman::data::command_status::CommandCompletion {
        attempt: 1,
        status: awman::data::command_status::CommandStatus::Done,
        exit_code: 0,
        result_json: "{}".into(),
        finished_at: chrono::Utc::now().to_rfc3339(),
    };
    assert!(host
        .db
        .finalize_running_command(host.command.as_str(), &completion)
        .unwrap());
    // A late replacement or buggy caller after finalization is refused: the
    // frozen result can never be orphaned behind a new active execution.
    let mut snapshot = fixture_snapshot();
    snapshot.execution_id = ExecutionId::new();
    snapshot.revision = 0;
    snapshot.active_turn_id = None;
    snapshot.native_session_id = None;
    let late = HeadlessRun::reserve(
        host.host(),
        snapshot,
        binding(),
        "fixture".into(),
        None,
        &timeouts(),
        "Another task",
    );
    assert!(late.is_err());
    assert_eq!(
        host.store
            .list_turns(&host.command, None, None)
            .unwrap()
            .items
            .len(),
        1
    );
}

/// Settle `count` turns on `execution` directly through the store (a long
/// command history without running a provider per turn).
fn store_history(host: &Host, execution: &ExecutionId, count: u64) {
    use awman::data::automation_store::{operation_digest, Operation, Reservation};
    let result = awman::data::automation_records::TurnResult {
        status: TurnStatus::Completed,
        status_support: StatusSupport::Full,
        readiness_source: awman::data::automation::ReadinessSource::Native,
        completion_source: CompletionSource::Native,
        completion_verified: true,
        native_turn_id: None,
        final_message: Some(String::new()),
        output_status: OutputStatus::Complete,
        failure: None,
    };
    for ordinal in 1..=count {
        let current = host.store.read_execution(execution).unwrap();
        let mut snapshot = current.snapshot.clone().unwrap();
        let turn = TurnRecord {
            request_id: uuid::Uuid::new_v4(),
            turn_id: awman::data::ids::TurnId::new(),
            execution_id: execution.clone(),
            command_id: host.command.clone(),
            attempt: 1,
            ordinal,
            native_session_id: None,
            native_turn_id: None,
            accepted_at: chrono::DateTime::from_timestamp_millis(
                chrono::Utc::now().timestamp_millis(),
            )
            .unwrap(),
            started_at: None,
            completed_at: None,
            accepted_revision: current.revision,
            status_support: StatusSupport::Full,
            status_source: awman::data::automation::StatusSource::Native,
            result: None,
        };
        snapshot.revision = current.revision + 1;
        snapshot.state = AutomationState::Submitting;
        snapshot.active_turn_id = Some(turn.turn_id.clone());
        let turn_id = turn.turn_id.clone();
        host.store
            .reserve_turn(
                Reservation {
                    digest: operation_digest(
                        Operation::SubmitTurn,
                        execution,
                        current.revision,
                        "history",
                    )
                    .unwrap(),
                    owner_key: "uid:test:fixture".into(),
                    turn,
                    snapshot: snapshot.clone(),
                    publication: None,
                },
                current.revision,
            )
            .unwrap();
        snapshot.revision += 1;
        snapshot.state = AutomationState::Ready;
        snapshot.active_turn_id = None;
        host.store
            .complete_turn(&turn_id, &result, &snapshot, None)
            .unwrap();
    }
    // The history's execution has ended (the owner's in-memory cell never
    // saw these store-level turns).
    let current = host.store.read_execution(execution).unwrap();
    let mut ended = current.snapshot.clone().unwrap();
    ended.revision = current.revision + 1;
    ended.state = AutomationState::Ended;
    host.store
        .transition_execution(current.revision, &ended, None)
        .unwrap();
}

/// Automation findings 15–17: recovery never finalizes an execution whose
/// owner is alive; a finished execution whose command result was never
/// stored is reported once its owner's lease is gone; and a crashed turn
/// beyond the first page of history is recovered (with a fresh ordinal).
#[tokio::test]
async fn recovery_respects_live_owners_finalizes_ended_commands_and_reaches_turn_201() {
    let host = Host::new(
        "command::automation_observers::recovery_respects_live_owners_finalizes_ended_commands_and_reaches_turn_201",
    );
    // A command with a 200-turn history in one finished execution.
    let execution = ExecutionId::new();
    let owned = interactive(&host, &execution);
    store_history(&host, &execution, 200);
    owned.finish(ExitFacts {
        exit_code: Some(0),
        wait_error: None,
        output_status: OutputStatus::Complete,
        cleanup_status: CleanupStatus::Complete,
    });
    let turns = |store: &AutomationStore| {
        let mut all = Vec::new();
        let mut cursor = None;
        loop {
            let page = store
                .list_turns(&host.command, cursor.as_deref(), Some(200))
                .unwrap();
            all.extend(page.items);
            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => return all,
            }
        }
    };
    // Turn 201: a daemon-hosted one-shot that finished but whose command
    // result is not stored yet keeps its owner's lease: never touched.
    headless(&host).finish_observed(&observed(true, Some(Ok(None))), Ok(0), false);
    assert_eq!(
        turns(&host.store).pop().unwrap().ordinal,
        201,
        "ordinals never collide past one page"
    );
    assert!(
        ExecutionController::recover(&host.store)
            .unwrap()
            .is_empty(),
        "a live owner's finished execution is never touched"
    );
    // The owner stored nothing and is gone: the command needs finalizing.
    global().release_leases(&host.command);
    assert_eq!(
        ExecutionController::recover(&host.store).unwrap(),
        vec![host.command.clone()]
    );
    // Turn 202 is alive: untouched, though it lies beyond any one page.
    let run = headless(&host);
    assert!(ExecutionController::recover(&host.store)
        .unwrap()
        .is_empty());
    assert!(turns(&host.store).pop().unwrap().result.is_none());
    // Its owner dies: the turn is recovered and worker recovery succeeds.
    drop(run);
    assert_eq!(
        ExecutionController::recover(&host.store).unwrap(),
        vec![host.command.clone()]
    );
    let recovered = turns(&host.store).pop().unwrap();
    assert_eq!(recovered.ordinal, 202);
    assert_eq!(
        recovered.result.as_ref().unwrap().status,
        TurnStatus::DeliveryUnknown
    );
}

/// Automation finding 1: the native channel is redacted with the launch's
/// set before parsing, even when a secret is split across reads; a record
/// whose identity the redaction touched is refused, never adopted.
#[tokio::test]
async fn launch_secrets_split_across_channel_reads_never_reach_state_or_identity() {
    let host = Host::new(
        "command::automation_observers::launch_secrets_split_across_channel_reads_never_reach_state_or_identity",
    );
    let execution = ExecutionId::new();
    let mount = ChannelMount::create(host.sandbox.root(), execution.clone()).unwrap();
    let owned = interactive(&host, &execution);
    let secret = "sk-launch-credential-7f3a9c";
    let redaction = RedactionSet::new();
    redaction.add(awman::data::secret::SecretString::new(secret));
    let stop = tokio_util::sync::CancellationToken::new();
    let sink_owner = owned.clone();
    let task = tokio::spawn(read_channel(
        mount.host_dir().to_path_buf(),
        Arc::new(Mutex::new(mount.decoder())),
        Arc::new(move |o| sink_owner.observe(o)),
        redaction,
        stop.clone(),
    ));
    let mut guest = GuestHelper::new(&mount);
    guest.append("session_start", serde_json::json!({}));
    until("identity", || owned.snapshot().native_session_id.is_some()).await;
    // A facility reason carrying the credential, written in two halves
    // that the host reads separately (it follows one complete record).
    guest.sequence += 2;
    let line = serde_json::to_vec(&serde_json::json!({
        "schema": 1,
        "execution_id": guest.execution,
        "nonce": guest.nonce,
        "sequence": guest.sequence,
        "native_session_id": "sess-1",
        "native_turn_id": null,
        "root": true,
        "kind": "facility",
        "payload": {"full_lifecycle": false, "reason": format!("hook token {secret} rejected")},
    }))
    .unwrap();
    let split = line
        .windows(secret.len())
        .position(|w| w == secret.as_bytes())
        .unwrap()
        + 6;
    let append = |bytes: &[u8]| {
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&guest.file)
            .unwrap();
        f.write_all(bytes).unwrap();
    };
    // A complete record plus the first half: once the complete record's
    // effect is visible, the host has read past the split point.
    let blocked = serde_json::to_vec(&serde_json::json!({
        "schema": 1,
        "execution_id": guest.execution,
        "nonce": guest.nonce,
        "sequence": guest.sequence - 1,
        "native_session_id": "sess-1",
        "native_turn_id": null,
        "root": true,
        "kind": "blocked",
        "payload": {"reason": "input"},
    }))
    .unwrap();
    let mut first = blocked;
    first.push(b'\n');
    first.extend_from_slice(&line[..split]);
    append(&first);
    until("the complete record", || {
        owned.snapshot().state == AutomationState::BlockedInput
    })
    .await;
    append(&line[split..]);
    append(b"\n");
    until("facility evidence", || {
        owned
            .snapshot()
            .status_reason
            .is_some_and(|r| r.contains("unavailable"))
    })
    .await;
    let reason = owned.snapshot().status_reason.unwrap();
    assert!(!reason.contains(secret), "{reason}");
    assert!(reason.contains("***"), "{reason}");
    let stored = host.store.read_execution(&execution).unwrap();
    assert!(!serde_json::to_string(&stored.snapshot)
        .unwrap()
        .contains(secret));
    stop.cancel();
    task.await.unwrap();
    owned.finish(ExitFacts {
        exit_code: Some(0),
        wait_error: None,
        output_status: OutputStatus::Complete,
        cleanup_status: CleanupStatus::Complete,
    });
}
