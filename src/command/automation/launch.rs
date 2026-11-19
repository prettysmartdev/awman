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
        let paths = crate::data::fs::DataPaths::from_env(session.env())?;
        let db_path = paths.db_path();
        let db = Arc::new(SqliteSessionStore::open_at(&db_path)?);
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
    ) -> crate::data::fs::api_db::CommandResult {
        let result = crate::data::fs::api_db::CommandResult {
            automation: Some(finished.automation.clone()),
            exit_code: Some(exit_code),
            final_message: finished.final_message.clone(),
            ..Default::default()
        };
        let completion = CommandCompletion {
            attempt: 1,
            status: if finished.failure.is_none() {
                CommandStatus::Done
            } else {
                CommandStatus::Error
            },
            exit_code,
            result_json: serde_json::to_string(&result).unwrap_or_default(),
            finished_at: chrono::Utc::now().to_rfc3339(),
        };
        let _ = self
            .db
            .finalize_running_command(&self.command_id, &completion);
        let _ = self
            .db
            .close_session(&self.session_id, &chrono::Utc::now().to_rfc3339());
        result
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
    pub host: Option<AutomationHost>,
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
    let (foreground, host) = match request.host {
        Some(host) => (None, host),
        None => {
            let (record, host) =
                ForegroundRecord::create(request.session, &request.command.join(" "))?;
            (Some(record), host)
        }
    };
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
    Ok(AutomationRun {
        owned,
        osc_enabled,
        channel: request.channel,
        drain: timeouts.output_drain.to_duration(),
        foreground,
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
        let finished = self.owned.finish_deferred(ExitFacts {
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
            let result = record.finish(&finished, code);
            // Close result (if any), the one logical exit, lease release.
            self.owned
                .controller()
                .command_finalized(&record.command, &result);
        }
        finished
    }
}

/// Map a finished execution's primary failure to the command error the
/// existing CLI exit classes and API envelope report.
pub fn into_command_result(finished: &FinishedExecution) -> Result<(), CommandError> {
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
    let execution = match instance.run_with_frontend(wrapped) {
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
        let (foreground, host) = match request.host {
            Some(host) => (None, host),
            None => {
                let (record, host) =
                    ForegroundRecord::create(request.session, &request.command.join(" "))?;
                (Some(record), host)
            }
        };
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
        let mut run = Self::reserve(
            host,
            snapshot,
            request.profile.execution_binding(),
            request.runtime.clone(),
            request.requested_native.clone(),
            &timeouts,
            task,
        )?;
        run.foreground = foreground;
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
            events(payload);
        }
    }

    /// The provider launch operation begins: the task is in its argv/stdin
    /// from here on, so a crash can no longer prove it was not delivered.
    pub fn launching(&mut self) {
        use crate::data::automation_records::DeliveryPhase;
        // The one absolute startup deadline starts at the launch operation.
        self.startup_deadline =
            Some(tokio::time::Instant::now() + self.timeouts.startup.to_duration());
        let store = &self.host.store;
        let _ = store.mark_launching(&self.snapshot.execution_id);
        let _ = store.mark_delivery(
            &self.turn.turn_id,
            DeliveryPhase::WriteStarted,
            &self.snapshot.execution_id,
        );
    }

    /// The provider process is running.
    pub fn running(&self) {
        let _ = self.host.store.mark_running(&self.snapshot.execution_id);
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
        // Native identity: the execution's root conversation, recorded once
        // (never another conversation's identity).
        if let Some(id) = &observed.identity {
            let mut next = self.snapshot.clone();
            next.revision += 1;
            next.native_session_id = Some(id.clone());
            next.state = AutomationState::Working;
            next.status_support = StatusSupport::Full;
            next.status_source = StatusSource::Native;
            next.status_reason = None;
            if store
                .transition_execution(self.snapshot.revision, &next, None)
                .is_ok()
            {
                self.snapshot = next;
                let _ = store.mark_delivery(
                    &self.turn.turn_id,
                    DeliveryPhase::Acknowledged,
                    &execution_id,
                );
                let _ = store.observe_turn(
                    &self.turn.turn_id,
                    &execution_id,
                    Some(id),
                    None,
                    Some(chrono::Utc::now()),
                );
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
        }
        let exit_code = exit.as_ref().ok().copied();
        let (mut result, mut failure) = headless_result(observed, exit, StatusSupport::Full);
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
        let mut stopping = self.snapshot.clone();
        stopping.revision += 1;
        stopping.active_turn_id = None;
        stopping.state = AutomationState::Stopping;
        let _ = store.complete_turn(&self.turn.turn_id, &result, &stopping, None);
        self.emit(EventPayload::AgentTurnCompleted {
            request_id: self.turn.request_id,
            execution_id: execution_id.clone(),
            turn_id: self.turn.turn_id.clone(),
            result: result.clone(),
        });
        let mut ended = stopping.clone();
        ended.revision += 1;
        ended.state = AutomationState::Ended;
        let _ = store.transition_execution(stopping.revision, &ended, None);
        self.emit(EventPayload::AgentState {
            snapshot: Box::new(ended.clone()),
        });
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
        let _ = store.finish_execution(&execution_id, &automation, failure.as_ref());
        if let Some(sink) = &self.host.finished {
            sink(&automation);
        }
        let finished = FinishedExecution {
            automation,
            failure,
            // The shared strict result carries the confirmed answer.
            final_message: (result.status == TurnStatus::Completed && result.completion_verified)
                .then(|| result.final_message.clone())
                .flatten(),
            cancelled,
        };
        if let Some(record) = &self.foreground {
            let code = match exit_code {
                Some(c) if finished.failure.is_some() && c != 0 => c,
                Some(_) if finished.failure.is_some() => 1,
                Some(c) => c,
                None => 1,
            };
            record.finish(&finished, if cancelled { 130 } else { code });
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
