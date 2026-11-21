//! Launch admission and the launch/finish glue for `chat --automation` and
//! `exec prompt --automation` (WI 0070 B1/B7).

use std::sync::Arc;

use super::admission::AdmissionConfig;
use super::controller::{
    global, local_owner_key, AutomationHost, ControllerLaunch, ExitFacts, FinishedExecution,
    OwnedExecution,
};
use crate::command::commands::launch_policy::settings_profile::{ProfileRequest, ProfileSession};
use crate::command::error::CommandError;
use crate::data::agent_policy::ResolvedAgentPolicy;
use crate::data::automation::{
    AutomationMode, AutomationState, CleanupStatus, OutputStatus, StatusSource, StatusSupport,
};
use crate::data::automation_records::AutomationSnapshot;
use crate::data::command_status::{CommandCompletion, CommandStatus};
use crate::data::fs::api_db::SqliteSessionStore;
use crate::data::session::Session;
use crate::engine::agent::automation::observer::ChannelMount;
use crate::engine::agent::automation::{adapter_for, NativeLifecycle, PromptDelivery};
use crate::engine::agent::AgentDefinition;

fn usage(command: &[&str], reason: impl Into<String>) -> CommandError {
    CommandError::InvalidFlagValue {
        command: command.iter().map(|s| (*s).to_owned()).collect(),
        flag: "automation".into(),
        reason: reason.into(),
    }
}

/// B1 argument admission, before image preparation, profile creation or
/// launch: automation requires a settings profile; `chat --automation`
/// requires an effective interactive mode. (The exec task requirement and
/// chat's refusal of prompt/work-item input are enforced by their parsers.)
pub fn validate_request(
    command: &[&str],
    mode: Option<AutomationMode>,
    profile: &ProfileRequest,
    headless: bool,
) -> Result<(), CommandError> {
    let Some(_) = mode else {
        return Ok(());
    };
    if profile.name.is_none() {
        return Err(usage(command, "--automation requires --settings-profile"));
    }
    if headless && command == ["chat"] {
        return Err(usage(
            command,
            "chat --automation requires an interactive terminal; use exec prompt --headless --automation for a one-shot task",
        ));
    }
    Ok(())
}

/// The definition must select an adapter to support automation at all.
pub fn require_adapter(command: &[&str], definition: &AgentDefinition) -> Result<(), CommandError> {
    if definition.automation_adapter.is_none() {
        return Err(CommandError::NativeCapabilityUnavailable(format!(
            "{}: agent `{}` declares no automation adapter",
            command.join(" "),
            definition.name
        )));
    }
    Ok(())
}

/// Create the execution-private native lifecycle channel for an
/// interactive automation launch when the selected adapter has a guest
/// lifecycle source and the runtime mounts host directories. Its options
/// go into the launch's `AgentRunOptions::automation_channel` before
/// option resolution; the same mount is passed to [`start`]. Without a
/// channel the native source simply stays unestablished.
pub fn prepare_channel(
    session: &Session,
    definition: &AgentDefinition,
    profile: &ProfileSession,
    container_runtime: bool,
) -> Result<Option<ChannelMount>, CommandError> {
    let Some(adapter) = adapter_for(definition) else {
        return Ok(None);
    };
    if adapter.native_lifecycle() != NativeLifecycle::GuestChannel || !container_runtime {
        return Ok(None);
    }
    ChannelMount::create(&session.env().temp_dir(), profile.execution_id().clone())
        .map(Some)
        .map_err(|e| CommandError::Other(format!("create the automation channel: {e}")))
}

/// Where an automation execution is hosted, as the launching frontend
/// declares it.
#[derive(Debug)]
pub enum AutomationHosting {
    /// An intentional foreground invocation (CLI, TUI): the run records
    /// itself through [`ForegroundRecord`] in the shared data-home store.
    Foreground,
    /// A daemon/API command: the run uses the command's existing records in
    /// the daemon's shared store.
    Shared(AutomationHost),
}

/// Resolve the frontend's hosting answer into the host the run uses. A
/// hosting failure is returned before anything is written; only an explicit
/// `Foreground` creates foreground records.
fn host_for(
    hosting: Result<AutomationHosting, CommandError>,
    session: &Session,
    command: &[&str],
) -> Result<(Option<ForegroundRecord>, AutomationHost), CommandError> {
    match hosting? {
        AutomationHosting::Shared(host) => Ok((None, host)),
        AutomationHosting::Foreground => {
            let (record, host) = ForegroundRecord::create(session, &command.join(" "))?;
            Ok((Some(record), host))
        }
    }
}

/// A foreground run records itself in the data-home database: a local
/// session row and a running command row, so the same durable execution,
/// turn and dedup records exist without a daemon.
pub struct ForegroundRecord {
    db: Arc<SqliteSessionStore>,
    session_id: String,
    command_id: String,
    command: crate::data::ids::CommandId,
}

impl ForegroundRecord {
    pub fn create(
        session: &Session,
        subcommand: &str,
    ) -> Result<(Self, AutomationHost), CommandError> {
        // The same migration-aware opener the daemon core uses: a legacy
        // database is relocated (or a conflict refused) before any row is
        // written, and no second database is ever created beside it.
        let paths = crate::data::fs::DataPaths::from_env(session.env())?;
        let legacy = crate::data::fs::ApiPaths::from_env(session.env())?;
        let db_path = paths.db_path();
        let db = paths.open_session_store(legacy.root())?;
        let session_id = crate::data::ids::WorkerSessionId::new();
        let command_id = crate::data::ids::CommandId::new();
        db.insert_session(
            session_id.as_str(),
            &session.working_dir().display().to_string(),
            &chrono::Utc::now().to_rfc3339(),
        )?;
        db.insert_running_lease_command(
            command_id.as_str(),
            session_id.as_str(),
            subcommand,
            "[]",
            false,
        )?;
        let store = crate::data::automation_store::AutomationStore::open(db.clone())?;
        let host = AutomationHost {
            store,
            command_id: command_id.clone(),
            attempt: 1,
            owner_key: local_owner_key(&db_path),
            events: None,
            finished: None,
        };
        Ok((
            Self {
                db,
                session_id: session_id.as_str().to_owned(),
                command_id: command_id.as_str().to_owned(),
                command: command_id.clone(),
            },
            host,
        ))
    }

    /// Freeze the foreground command row and close its session row.
    pub fn finish(
        &self,
        finished: &FinishedExecution,
        exit_code: i32,
    ) -> Result<crate::data::fs::api_db::CommandResult, CommandError> {
        let result = crate::data::fs::api_db::CommandResult {
            automation: Some(finished.automation.clone()),
            error: finished.failure.as_ref().map(|f| f.message.clone()),
            failure: finished
                .failure
                .as_ref()
                .map(|f| crate::data::fs::api_db::CommandFailure {
                    stage: "step".into(),
                    code: f.code.clone(),
                    message: f.message.clone(),
                    step: None,
                }),
            outcome: Some(if finished.cancelled {
                crate::data::execution_event::CommandStatusKind::Cancelled
            } else if finished.failure.is_some() {
                crate::data::execution_event::CommandStatusKind::Error
            } else {
                crate::data::execution_event::CommandStatusKind::Done
            }),
            outputs: Some(Default::default()),
            exit_code: Some(exit_code),
            final_message: finished.final_message.clone(),
            ..Default::default()
        };
        let completion = CommandCompletion {
            attempt: 1,
            status: if finished.cancelled {
                CommandStatus::Cancelled
            } else if finished.failure.is_none() {
                CommandStatus::Done
            } else {
                CommandStatus::Error
            },
            exit_code,
            result_json: serde_json::to_string(&result)
                .map_err(|e| CommandError::Other(e.to_string()))?,
            finished_at: chrono::Utc::now().to_rfc3339(),
        };
        if !self
            .db
            .finalize_running_command(&self.command_id, &completion)?
        {
            return Err(CommandError::Data(crate::data::error::DataError::Other(
                "command completion compare-and-set failed".into(),
            )));
        }
        if !self
            .db
            .close_session(&self.session_id, &chrono::Utc::now().to_rfc3339())?
        {
            return Err(CommandError::Data(crate::data::error::DataError::Other(
                "foreground session completion compare-and-set failed".into(),
            )));
        }
        Ok(result)
    }
}

/// A started automation execution and its foreground record, if any.
pub struct AutomationRun {
    pub owned: OwnedExecution,
    pub osc_enabled: bool,
    channel: Option<ChannelMount>,
    /// The saved output-drain budget (agents.automation.outputDrainTimeout).
    drain: std::time::Duration,
    foreground: Option<ForegroundRecord>,
}

/// Inputs to [`start`].
pub struct StartRequest<'a> {
    pub command: &'a [&'a str],
    pub session: &'a Session,
    pub definition: &'a AgentDefinition,
    pub profile: &'a ProfileSession,
    pub policy: &'a ResolvedAgentPolicy,
    pub mode: AutomationMode,
    pub requested_native: Option<crate::data::settings_profile::NativeSessionId>,
    pub runtime: String,
    /// The frontend's answer from `AgentLaunchFrontend::automation_host`.
    /// An error (a daemon command whose shared store could not be bound)
    /// aborts the start before any record is written; it never becomes a
    /// foreground run.
    pub host: Result<AutomationHosting, CommandError>,
    pub initial_task: Option<String>,
    /// Interactive (PTY) launch; headless uses the strict one-shot policy.
    pub interactive: bool,
    /// The execution-private native lifecycle channel mounted into the
    /// guest ([`prepare_channel`]); `None` leaves the native source
    /// unestablished (unknown, never claimed).
    pub channel: Option<ChannelMount>,
}

/// Reserve the execution (adopting the profile's execution ID) and its
/// initial `starting` snapshot. The startup deadline begins at
/// [`OwnedExecution::launching`].
pub fn start(request: StartRequest<'_>) -> Result<AutomationRun, CommandError> {
    let adapter = adapter_for(request.definition).ok_or_else(|| {
        CommandError::NativeCapabilityUnavailable(format!(
            "agent `{}` declares no automation adapter",
            request.definition.name
        ))
    })?;
    let config = request.session.effective_config();
    let timeouts = config.automation_timeouts()?;
    let osc_enabled = config.program_status_enabled() && request.interactive;
    let (foreground, host) = host_for(request.host, request.session, request.command)?;
    let capabilities = adapter.declarations(request.definition, &request.runtime, osc_enabled);
    let snapshot = AutomationSnapshot {
        execution_id: request.profile.execution_id().clone(),
        provider: request.definition.name.clone(),
        provider_version: None,
        native_session_id: None,
        automation_mode: request.mode,
        minimum_status_support: request.mode.minimum_status_support(),
        state: AutomationState::Starting,
        revision: 0,
        active_turn_id: None,
        native_start_mode: request.policy.native_start_mode,
        workspace_access: request.policy.workspace_access,
        status_support: StatusSupport::None,
        status_source: StatusSource::None,
        status_reason: None,
        capabilities,
    };
    let completion_host = host.clone();
    let owned = global()
        .start(ControllerLaunch {
            host,
            execution_id: request.profile.execution_id().clone(),
            snapshot,
            config: AdmissionConfig {
                minimum: request.mode.minimum_status_support(),
                osc_enabled,
                native_channel: request.interactive && request.channel.is_some(),
                delivery: adapter.prompt_delivery() == PromptDelivery::BracketedPaste,
            },
            adapter,
            timeouts: timeouts.clone(),
            requested_native: request.requested_native,
            profile_binding: request.profile.execution_binding(),
            runtime: request.runtime,
            initial_task: request.initial_task,
        })
        .map_err(CommandError::Other)?;
    if let Err(error) = bind_profile_completion(request.profile, completion_host, foreground) {
        owned.clone().abandon();
        return Err(error);
    }
    Ok(AutomationRun {
        owned,
        osc_enabled,
        channel: request.channel,
        drain: timeouts.output_drain.to_duration(),
        foreground: None,
    })
}

impl AutomationRun {
    /// Finish the execution after the provider exited (or was stopped) and
    /// output drained; freeze its assurance.
    pub fn finish(
        self,
        exit: &Result<
            crate::engine::agent_runtime::AgentExitInfo,
            crate::engine::error::EngineError,
        >,
        cleanup: CleanupStatus,
        output: OutputStatus,
    ) -> FinishedExecution {
        let (exit_code, wait_error) = match exit {
            Ok(info) => (Some(info.exit_code), None),
            Err(e) => (None, Some(e.to_string())),
        };
        // The terminal's exit follows the stored command result: the daemon's
        // queue worker publishes it (command_finalized); a foreground run
        // stores its own record first.
        let mut finished = self.owned.finish_deferred(ExitFacts {
            exit_code,
            wait_error,
            output_status: output,
            cleanup_status: cleanup,
        });
        if let Some(record) = &self.foreground {
            let code = if finished.failure.is_some() {
                exit_code.filter(|c| *c != 0).unwrap_or(1)
            } else {
                exit_code.unwrap_or(0)
            };
            match record.finish(&finished, code) {
                Ok(result) => self
                    .owned
                    .controller()
                    .command_finalized(&record.command, &result),
                Err(error) => {
                    finished.storage_failed(&error.to_string());
                    self.owned.clone().abandon();
                }
            }
        }
        finished
    }
}

/// Map a finished execution's primary failure to the command error the
/// existing CLI exit classes and API envelope report.
pub fn into_command_result(finished: &FinishedExecution) -> Result<(), CommandError> {
    if let Some(f) = finished
        .failure
        .as_ref()
        .filter(|f| f.code == "storage_error")
    {
        return Err(CommandError::Data(crate::data::error::DataError::Other(
            f.message.clone(),
        )));
    }
    if finished.cancelled {
        return Err(CommandError::Aborted);
    }
    match &finished.failure {
        None => Ok(()),
        Some(f) => Err(CommandError::AutomationFailed {
            code: f.code.clone(),
            message: f.message.clone(),
            automation: Some(Box::new(finished.automation.clone())),
            final_message: finished.final_message.clone(),
        }),
    }
}

/// Run an interactive automation execution to completion: wrap the PTY
/// frontend, start the startup clock at the launch operation, race the
/// provider's exit against command cancellation and controller stop
/// requests, apply the one absolute cancellation budget (half graceful,
/// then force-stop only the owned resource), and freeze the result.
pub async fn run_interactive(
    run: AutomationRun,
    instance: Box<dyn crate::engine::agent_runtime::AgentInstance>,
    container_frontend: Box<dyn crate::engine::agent_runtime::AgentFrontend>,
    runtime: Arc<dyn crate::engine::agent_runtime::AgentRuntimeEngine>,
    cancel: tokio_util::sync::CancellationToken,
    cancel_timeout: std::time::Duration,
) -> (
    Result<crate::engine::agent_runtime::AgentExitInfo, crate::engine::error::EngineError>,
    FinishedExecution,
) {
    // The launch's shared redaction set (credentials and env grants the
    // runtime registered) protects the native channel and every durable
    // turn result, not only ordinary output.
    let redaction = instance.output_redaction();
    redaction.absorb(&container_frontend.output_redaction());
    run.owned.set_redaction(redaction.clone());
    // The native lifecycle reader runs from launch until the provider has
    // exited; its tail is drained within the output-drain budget.
    let reader = run.channel.as_ref().map(|channel| {
        let stop = tokio_util::sync::CancellationToken::new();
        let owned = run.owned.clone();
        let sink: Arc<dyn Fn(Vec<crate::data::automation_observation::Observation>) + Send + Sync> =
            Arc::new(move |observations| owned.observe(observations));
        let task = tokio::spawn(crate::engine::agent::automation::observer::read_channel(
            channel.host_dir().to_path_buf(),
            Arc::new(std::sync::Mutex::new(channel.decoder())),
            sink,
            redaction.clone(),
            stop.clone(),
        ));
        (stop, task)
    });
    let wrapped = super::frontend::AutomationFrontend::wrap(
        container_frontend,
        run.owned.clone(),
        run.osc_enabled,
    );
    run.owned.launching();
    let launch = if run.owned.stop_requested().is_cancelled() {
        Err(crate::engine::error::EngineError::Other(
            "automation launch persistence failed".into(),
        ))
    } else {
        instance.run_with_frontend(wrapped)
    };
    let execution = match launch {
        Ok(execution) => execution,
        Err(e) => {
            let exit = Err(e);
            let output = stop_reader(reader, run.drain).await;
            let finished = run.finish(&exit, CleanupStatus::Complete, output);
            return (exit, finished);
        }
    };
    let handle = execution.handle().clone();
    let mut cancel_handle = execution.cancel_handle();
    let stuck = execution.stuck_sender();
    let (wait_tx, mut wait_rx) = tokio::sync::oneshot::channel();
    let mut execution = execution;
    tokio::spawn(async move {
        let _ = wait_tx.send(execution.wait().await);
    });
    let dropped = || {
        Err(crate::engine::error::EngineError::Other(
            "automation wait task dropped unexpectedly".into(),
        ))
    };
    let stop = run.owned.stop_requested();
    let mut cleanup = CleanupStatus::Complete;
    // Set by an accepted cancel: every later wait, including the output
    // drain, ends by this one absolute deadline.
    let mut cancel_deadline: Option<tokio::time::Instant> = None;
    let exit = tokio::select! {
        biased;
        result = &mut wait_rx => result.unwrap_or_else(|_| dropped()),
        _ = async { tokio::select! { _ = cancel.cancelled() => {}, _ = stop.cancelled() => {} } } => {
            if cancel.is_cancelled() {
                run.owned.cancel();
            }
            // One absolute deadline from acceptance; repeated cancels do not
            // extend it.
            let accepted = tokio::time::Instant::now();
            let deadline = accepted + cancel_timeout;
            cancel_deadline = Some(deadline);
            // The runtime's graceful interrupt may block (a hung runtime
            // CLI): it runs off the executor and never holds the deadline.
            if let Some(ch) = cancel_handle.take() {
                drop(tokio::task::spawn_blocking(move || ch.cancel()));
            }
            let graceful = accepted + cancel_timeout / 2;
            match tokio::time::timeout_at(graceful, &mut wait_rx).await {
                Ok(result) => result.unwrap_or_else(|_| dropped()),
                Err(_) => {
                    let runtime = runtime.clone();
                    let handle = handle.clone();
                    drop(tokio::task::spawn_blocking(move || {
                        crate::command::commands::agents::kill::stop_and_remove_agent(
                            runtime.as_ref(),
                            &handle,
                        )
                    }));
                    match tokio::time::timeout_at(deadline, &mut wait_rx).await {
                        Ok(result) => result.unwrap_or_else(|_| dropped()),
                        Err(_) => {
                            // Publish by the deadline; the owned cleanup
                            // obligation remains (profile stays closing).
                            cleanup = CleanupStatus::Pending;
                            Err(crate::engine::error::EngineError::Other(
                                "the agent did not stop before the cancellation deadline".into(),
                            ))
                        }
                    }
                }
            }
        }
    };
    drop(stuck);
    let budget = match cancel_deadline {
        Some(deadline) => run
            .drain
            .min(deadline.saturating_duration_since(tokio::time::Instant::now())),
        None => run.drain,
    };
    // One drain deadline for every lifecycle consumer: the PTY's OSC
    // decoder first (its stream ends with the provider), then the native
    // channel reader's tail.
    let drain_deadline = tokio::time::Instant::now() + budget;
    let osc = run.owned.join_drains(budget).await;
    let native = stop_reader(
        reader,
        drain_deadline.saturating_duration_since(tokio::time::Instant::now()),
    )
    .await;
    let output = if osc == OutputStatus::Complete && native == OutputStatus::Complete {
        OutputStatus::Complete
    } else {
        OutputStatus::Incomplete
    };
    let finished = run.finish(&exit, cleanup, output);
    (exit, finished)
}

/// A headless automation one-shot (B1): one execution and exactly one turn,
/// reserved durably before the provider's noninteractive entrypoint runs.
/// The turn is admitted at native/full (the headless contract requires a
/// native identity and terminal outcome) while the snapshot stays
/// `starting`; native acceptance moves it to `working`. No terminal grant
/// or interactive readiness is involved, and nothing is ever replayed:
/// crash recovery turns an unacknowledged turn into `delivery_unknown`.
pub struct HeadlessRun {
    host: AutomationHost,
    snapshot: AutomationSnapshot,
    turn: crate::data::automation_records::TurnRecord,
    foreground: Option<ForegroundRecord>,
    timeouts: crate::data::config::agents::AutomationTimeouts,
    /// The absolute startup deadline, set when the launch operation begins.
    startup_deadline: Option<tokio::time::Instant>,
    /// A primary failure the run recorded while running (startup expiry).
    stop_failure: Option<crate::data::automation_records::AutomationFailure>,
    /// Whether the owned provider was confirmed stopped.
    cleanup: CleanupStatus,
    /// This process's liveness lease on the execution (crash recovery
    /// never touches it while held).
    lease: Option<crate::data::automation_store::ExecutionLease>,
}

impl HeadlessRun {
    /// Reserve the execution and its single turn. `task` is the complete
    /// validated task text; only its digest is stored.
    pub fn start(request: StartRequest<'_>, task: &str) -> Result<Self, CommandError> {
        let adapter = adapter_for(request.definition).ok_or_else(|| {
            CommandError::NativeCapabilityUnavailable(format!(
                "agent `{}` declares no automation adapter",
                request.definition.name
            ))
        })?;
        let timeouts = request.session.effective_config().automation_timeouts()?;
        let (foreground, host) = host_for(request.host, request.session, request.command)?;
        let execution_id = request.profile.execution_id().clone();
        let snapshot = AutomationSnapshot {
            execution_id: execution_id.clone(),
            provider: request.definition.name.clone(),
            provider_version: None,
            native_session_id: None,
            automation_mode: request.mode,
            minimum_status_support: request.mode.minimum_status_support(),
            state: AutomationState::Starting,
            revision: 0,
            active_turn_id: None,
            native_start_mode: request.policy.native_start_mode,
            workspace_access: request.policy.workspace_access,
            status_support: StatusSupport::None,
            status_source: StatusSource::None,
            status_reason: Some("headless one-shot: native identity and outcome pending".into()),
            capabilities: adapter.declarations(request.definition, &request.runtime, false),
        };
        let completion_host = host.clone();
        let run = Self::reserve(
            host,
            snapshot,
            request.profile.execution_binding(),
            request.runtime.clone(),
            request.requested_native.clone(),
            &timeouts,
            task,
        )?;
        bind_profile_completion(request.profile, completion_host, foreground)?;
        Ok(run)
    }

    /// Reserve the execution and its one turn in `host` (no foreground
    /// record). `snapshot` is the revision-0 `starting` snapshot.
    pub fn reserve(
        host: AutomationHost,
        mut snapshot: AutomationSnapshot,
        profile_binding: crate::data::automation_records::ExecutionProfileBinding,
        runtime: String,
        requested_native: Option<crate::data::settings_profile::NativeSessionId>,
        timeouts: &crate::data::config::agents::AutomationTimeouts,
        task: &str,
    ) -> Result<Self, CommandError> {
        use crate::data::automation_records::TurnRecord;
        use crate::data::automation_store::{operation_digest, Operation, Reservation};
        let execution_id = snapshot.execution_id.clone();
        let other = |e: crate::data::error::DataError| CommandError::Other(e.to_string());
        if let Some(events) = &host.events {
            events.ensure_available()?;
        }
        let lease = host.store.acquire_lease(&execution_id).map_err(other)?;
        super::controller::reserve_execution_record(
            &host,
            &execution_id,
            &snapshot,
            profile_binding,
            runtime,
            requested_native,
            timeouts,
        )
        .map_err(CommandError::Other)?;
        let request_id = uuid::Uuid::new_v4();
        let digest =
            operation_digest(Operation::SubmitTurn, &execution_id, 0, task).map_err(other)?;
        // Every page: a long command history must not reuse an ordinal.
        let ordinal = super::controller::next_turn_ordinal(&host.store, &host.command_id)
            .map_err(CommandError::Other)?;
        let turn = TurnRecord {
            request_id,
            turn_id: crate::data::ids::TurnId::new(),
            execution_id: execution_id.clone(),
            command_id: host.command_id.clone(),
            attempt: host.attempt,
            ordinal,
            native_session_id: None,
            native_turn_id: None,
            accepted_at: chrono::DateTime::from_timestamp_millis(
                chrono::Utc::now().timestamp_millis(),
            )
            .expect("valid time"),
            started_at: None,
            completed_at: None,
            accepted_revision: 0,
            status_support: StatusSupport::Full,
            status_source: StatusSource::Native,
            result: None,
        };
        snapshot.revision = 1;
        snapshot.active_turn_id = Some(turn.turn_id.clone());
        host.store
            .reserve_turn(
                Reservation {
                    turn: turn.clone(),
                    owner_key: host.owner_key.clone(),
                    digest,
                    snapshot: snapshot.clone(),
                    publication: None,
                },
                0,
            )
            .map_err(other)?;
        let run = Self {
            host,
            snapshot,
            turn,
            foreground: None,
            timeouts: timeouts.clone(),
            startup_deadline: None,
            stop_failure: None,
            cleanup: CleanupStatus::Complete,
            lease,
        };
        run.emit(crate::data::execution_event::EventPayload::AgentState {
            snapshot: Box::new(run.snapshot.clone()),
        });
        Ok(run)
    }

    fn emit(&self, payload: crate::data::execution_event::EventPayload) {
        if let Some(events) = &self.host.events {
            events.emit(payload);
        }
    }

    /// The provider launch operation begins: the task is in its argv/stdin
    /// from here on, so a crash can no longer prove it was not delivered.
    pub fn launching(&mut self) -> Result<(), CommandError> {
        use crate::data::automation_records::DeliveryPhase;
        // The one absolute startup deadline starts at the launch operation.
        self.startup_deadline =
            Some(tokio::time::Instant::now() + self.timeouts.startup.to_duration());
        let store = &self.host.store;
        store.mark_launching(&self.snapshot.execution_id)?;
        store.mark_delivery(
            &self.turn.turn_id,
            DeliveryPhase::WriteStarted,
            &self.snapshot.execution_id,
        )?;
        Ok(())
    }

    /// The provider process is running.
    pub fn running(&self) -> Result<(), CommandError> {
        self.host.store.mark_running(&self.snapshot.execution_id)?;
        Ok(())
    }

    /// The saved cancellation budget (agents.automation.cancelTimeout).
    pub fn cancel_timeout(&self) -> std::time::Duration {
        self.timeouts.cancel.to_duration()
    }

    /// Resolves when the startup deadline passes before the provider
    /// reported its native identity (the headless lifecycle evidence that
    /// replaces the first-byte grace). Pending forever once observed.
    pub fn startup_expiry(
        &self,
        stream: &crate::engine::agent_output::native_observation::NativeStream,
    ) -> impl std::future::Future<Output = ()> + Send + 'static {
        let deadline = self.startup_deadline;
        let stream = stream.clone();
        async move {
            let Some(deadline) = deadline else {
                return std::future::pending().await;
            };
            loop {
                if stream.identity().is_some() || stream.ended() {
                    return std::future::pending().await;
                }
                if tokio::time::Instant::now() >= deadline {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        }
    }

    /// The provider was not confirmed stopped by the cancellation deadline.
    pub fn cleanup_pending(&mut self) {
        self.cleanup = CleanupStatus::Pending;
    }

    /// Record the startup expiry as the run's primary failure.
    pub fn startup_expired(&mut self) {
        self.stop_failure.get_or_insert_with(|| {
            super::admission::failure(
                super::admission::codes::STARTUP_TIMEOUT,
                "the provider reported no native session identity before the startup deadline",
            )
        });
    }

    /// Freeze the one turn and the execution from the shared translator's
    /// native observations and the actual wait result.
    pub async fn finish(
        self,
        stream: &crate::engine::agent_output::native_observation::NativeStream,
        exit: &Result<
            crate::engine::agent_runtime::AgentExitInfo,
            crate::engine::error::EngineError,
        >,
        cancelled: bool,
    ) -> FinishedExecution {
        // The translator task is a downstream consumer of the provider's
        // stdout: let it reach its output boundary (bounded by the saved
        // output-drain budget) before its facts are frozen.
        let deadline = tokio::time::Instant::now() + self.timeouts.output_drain.to_duration();
        while !stream.ended() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let drained = stream.ended();
        let mismatch = stream.mismatch.is_cancelled();
        let observed = super::results::HeadlessObservation {
            identity: if mismatch { None } else { stream.identity() },
            outcome: if stream.failed() {
                Some(Err("the provider reported a failed turn".into()))
            } else if stream.complete() {
                Some(Ok(stream.final_message()))
            } else {
                None
            },
            protocol_error: if mismatch {
                Some("the provider returned another conversation's identity".into())
            } else if let Some(reason) = stream.protocol_error() {
                Some(reason)
            } else if !drained {
                Some("the native output did not drain before the output-drain deadline".into())
            } else {
                None
            },
        };
        let exit = match exit {
            Ok(info) => Ok(info.exit_code),
            Err(e) => Err(e.to_string()),
        };
        self.finish_observed(&observed, exit, cancelled)
    }

    /// [`finish`](Self::finish) over already-decoded translator facts: the
    /// observed root identity (absent when the provider returned another
    /// conversation), the correlated native outcome and the wait result.
    pub fn finish_observed(
        mut self,
        observed: &super::results::HeadlessObservation,
        exit: Result<i32, String>,
        cancelled: bool,
    ) -> FinishedExecution {
        use super::results::headless_result;
        use crate::data::automation::{CompletionSource, TurnStatus};
        use crate::data::automation_records::{CommandAutomationResult, DeliveryPhase};
        use crate::data::execution_event::EventPayload;
        let store = self.host.store.clone();
        let execution_id = self.snapshot.execution_id.clone();
        let observed_write = (|| -> Result<(), crate::data::error::DataError> {
            if let Some(id) = &observed.identity {
                let mut next = self.snapshot.clone();
                next.revision += 1;
                next.native_session_id = Some(id.clone());
                next.state = AutomationState::Working;
                next.status_support = StatusSupport::Full;
                next.status_source = StatusSource::Native;
                next.status_reason = None;
                store.transition_execution(self.snapshot.revision, &next, None)?;
                self.snapshot = next;
                store.mark_delivery(
                    &self.turn.turn_id,
                    DeliveryPhase::Acknowledged,
                    &execution_id,
                )?;
                store.observe_turn(
                    &self.turn.turn_id,
                    &execution_id,
                    Some(id),
                    None,
                    Some(chrono::Utc::now()),
                )?;
                self.emit(EventPayload::AgentIdentity {
                    execution_id: execution_id.clone(),
                    native_session_id: id.clone(),
                });
                self.emit(EventPayload::AgentTurnStarted {
                    request_id: self.turn.request_id,
                    execution_id: execution_id.clone(),
                    turn_id: self.turn.turn_id.clone(),
                    native_session_id: Some(id.clone()),
                    native_turn_id: None,
                    status_support: StatusSupport::Full,
                    status_source: StatusSource::Native,
                });
            }
            Ok(())
        })();
        let exit_code = exit.as_ref().ok().copied();
        let (mut result, mut failure) = headless_result(observed, exit, StatusSupport::Full);
        if let Some(events) = &self.host.events {
            let redaction = events.redaction();
            result.final_message = result
                .final_message
                .map(|message| redaction.redact(&message));
            if let Some(failure) = &mut result.failure {
                failure.message = redaction.redact(&failure.message);
            }
            if let Some(failure) = &mut failure {
                failure.message = redaction.redact(&failure.message);
            }
        }
        if cancelled && result.completion_source != CompletionSource::Native {
            result.status = TurnStatus::Cancelled;
            result.completion_source = CompletionSource::None;
            result.completion_verified = false;
            result.final_message = None;
            result.failure = None;
            failure = None;
        }
        // A startup expiry recorded while running is the primary failure
        // unless the provider settled the turn natively.
        if let Some(stop) = self.stop_failure.clone() {
            if result.completion_source != CompletionSource::Native {
                result.status = TurnStatus::Failed;
                result.completion_verified = false;
                result.final_message = None;
                result.failure = Some(stop.clone());
                failure = Some(stop);
            }
        }
        if !super::results::bound_turn_result(&mut result) {
            failure = result.failure.clone();
        }
        let mut ended = self.snapshot.clone();
        ended.revision += 1;
        ended.active_turn_id = None;
        ended.state = AutomationState::Ended;
        let automation = CommandAutomationResult {
            execution_id: execution_id.clone(),
            native_session_id: ended.native_session_id.clone(),
            last_turn_id: Some(self.turn.turn_id.clone()),
            native_start_mode: ended.native_start_mode,
            workspace_access: ended.workspace_access,
            status_support: StatusSupport::Full,
            completion_verified: result.completion_verified
                && result.status == TurnStatus::Completed,
            output_status: result.output_status,
            cleanup_status: self.cleanup,
        };
        if failure.is_none() && !cancelled && self.cleanup != CleanupStatus::Complete {
            failure = Some(super::admission::failure(
                super::admission::codes::CLEANUP_TIMEOUT,
                "the agent was not confirmed stopped before the cancellation deadline",
            ));
        }
        if cancelled && failure.is_none() {
            failure = Some(super::admission::failure(
                super::admission::codes::CANCELLED,
                "cancelled",
            ));
        }
        let mut finished = FinishedExecution {
            automation,
            failure,
            // The shared strict result carries the confirmed answer.
            final_message: (result.status == TurnStatus::Completed && result.completion_verified)
                .then(|| result.final_message.clone())
                .flatten(),
            cancelled,
        };
        let committed = observed_write.and_then(|()| {
            let projection = self
                .host
                .events
                .as_ref()
                .map(|events| {
                    events.reserve(vec![
                        EventPayload::AgentTurnCompleted {
                            request_id: self.turn.request_id,
                            execution_id: execution_id.clone(),
                            turn_id: self.turn.turn_id.clone(),
                            result: result.clone(),
                        },
                        EventPayload::AgentState {
                            snapshot: Box::new(ended.clone()),
                        },
                    ])
                })
                .transpose()?;
            store.complete_execution(
                &self.turn.turn_id,
                &result,
                &ended,
                &finished.automation,
                finished.failure.as_ref(),
                projection.as_ref(),
            )?;
            finished.failure = store.read_execution(&execution_id)?.end_failure;
            finished.cancelled |= finished
                .failure
                .as_ref()
                .is_some_and(|f| f.code == super::admission::codes::CANCELLED);
            if let (Some(events), Some(projection)) = (&self.host.events, projection) {
                events.publish(&projection)?;
                store.clear_pending_events(&execution_id, projection.cursor)?;
            }
            Ok(())
        });
        if let Err(error) = committed {
            if let Some(events) = &self.host.events {
                events.abort(&error.to_string());
            }
            finished.storage_failed(&error.to_string());
            // The lease drops with this run. Recovery owns any committed batch;
            // never replace it with this transient publication/storage failure.
            return finished;
        }
        if let Some(sink) = &self.host.finished {
            sink(&finished.automation, finished.final_message.as_deref());
        }
        if let Some(record) = &self.foreground {
            let code = match exit_code {
                Some(c) if finished.failure.is_some() && c != 0 => c,
                Some(_) if finished.failure.is_some() => 1,
                Some(c) => c,
                None => 1,
            };
            if let Err(error) =
                record.finish(&finished, if finished.cancelled { 130 } else { code })
            {
                finished.storage_failed(&error.to_string());
            }
            // The command result is stored: the execution is no longer live.
            drop(self.lease.take());
        } else {
            // The daemon stores the result later; keep the lease until then.
            super::controller::global().hold_lease(&self.host.command_id, self.lease.take());
        }
        finished
    }
}

/// Stop the native lifecycle reader after the provider exited and wait for
/// its final drain within `budget` (the output-drain budget, already capped
/// by the caller at any cancellation deadline). A reader that does not
/// finish in time leaves the output incomplete; its late records cannot
/// publish into a finished execution (the controller fences them).
async fn stop_reader(
    reader: Option<(
        tokio_util::sync::CancellationToken,
        tokio::task::JoinHandle<()>,
    )>,
    budget: std::time::Duration,
) -> OutputStatus {
    let Some((stop, task)) = reader else {
        return OutputStatus::Complete;
    };
    stop.cancel();
    match tokio::time::timeout(budget, task).await {
        Ok(Ok(())) => OutputStatus::Complete,
        Ok(Err(_)) | Err(_) => OutputStatus::Incomplete,
    }
}

/// Settings-profile cleanup after the execution was not confirmed (writer
/// shutdown, resource removal or sanitized export). An automation command
/// must not then report success or complete cleanup: a known primary
/// failure stays primary (with the corrected assurance); otherwise the
/// command fails `cleanup_timeout`.
pub fn cleanup_unconfirmed(
    finished: &FinishedExecution,
    previous: Option<CommandError>,
    detail: &str,
) -> CommandError {
    let mut automation = finished.automation.clone();
    if automation.cleanup_status == CleanupStatus::Complete {
        automation.cleanup_status = CleanupStatus::Pending;
    }
    match previous {
        Some(CommandError::AutomationFailed {
            code,
            message,
            final_message,
            ..
        }) => CommandError::AutomationFailed {
            code,
            message,
            automation: Some(Box::new(automation)),
            final_message,
        },
        Some(other) => other,
        None => CommandError::AutomationFailed {
            code: super::admission::codes::CLEANUP_TIMEOUT.into(),
            message: format!("settings-profile cleanup was not confirmed: {detail}"),
            automation: Some(Box::new(automation)),
            final_message: finished.final_message.clone(),
        },
    }
}

/// The profile owner resolves this journal only after export and writer cleanup.
/// A crash leaves pending cleanup, which recovery must downgrade, never bless.
fn bind_profile_completion(
    profile: &ProfileSession,
    host: AutomationHost,
    foreground: Option<ForegroundRecord>,
) -> Result<(), CommandError> {
    let id = profile.execution_id().clone();
    let cleanup_store = host.store.clone();
    let cleanup_id = id.clone();
    profile.on_cleanup(move |confirmed| {
        if !confirmed && host.store.read_execution(&id)?.final_assurance.is_none() {
            host.store
                .fail_uncommitted_storage(&id, "profile owner ended before execution completion")?;
        }
        host.store.finish_profile_cleanup(&id, confirmed)?;
        if let Some(record) = foreground {
            let e = host.store.read_execution(&id)?;
            if let Some(automation) = e.final_assurance {
                let row = record
                    .db
                    .get_command(&record.command_id)?
                    .ok_or_else(|| CommandError::Other("missing foreground command".into()))?;
                let committed: crate::data::fs::api_db::CommandResult =
                    serde_json::from_str(row.result.as_deref().unwrap_or("{}"))
                        .map_err(|e| CommandError::Other(e.to_string()))?;
                let finished = FinishedExecution {
                    automation,
                    final_message: committed.final_message,
                    cancelled: e
                        .end_failure
                        .as_ref()
                        .is_some_and(|f| f.code == "cancelled"),
                    failure: e.end_failure,
                };
                let code = if finished.cancelled {
                    130
                } else if finished.failure.is_some() {
                    1
                } else {
                    0
                };
                let result = record.finish(&finished, code)?;
                global().command_finalized(&record.command, &result);
            }
        }
        Ok(())
    });
    cleanup_store.begin_profile_cleanup(&cleanup_id)?;
    Ok(())
}
