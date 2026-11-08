//! Queue worker — claims commands from the SQLite queue and executes them.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use super::cancel::TokenRegistry;
use super::event_bus::EventBus;
use super::push_back::{
    push_after_success, push_baseline, PushBackError, PushBackOutcome, PushEventSink,
};
use super::runtime::ApiSessionLifecycle;
use crate::command::dispatch::{CommandOutcome, Dispatch, Engines};
use crate::data::api_lifecycle_store::CommandLifecycleStore;
use crate::data::command_status::{CommandCompletion, CommandStatus, CANCELLED_EXIT_CODE};
use crate::data::execution_event::{
    CommandStatusKind, EventPayload, PhaseStatusKind, StepStatusKind, FIRST_ATTEMPT,
};
use crate::data::fs::api_command_log::{CommandLogWriter, CommandStateWriter, ResultContributions};
use crate::data::fs::api_db::{
    failure_codes, CommandFailure, CommandRecord, CommandResult, FailedStepPhase,
    SqliteSessionStore,
};
use crate::data::fs::api_paths::ApiPaths;
use crate::data::session_manager::SessionManager;
use crate::data::session_status::SessionStatus;

const OUTPUT_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);
/// Bounded row-write attempts for one completion; the sidecar covers the rest.
const COMPLETION_WRITE_ATTEMPTS: usize = 3;
const COMPLETION_WRITE_RETRY_DELAY: Duration = Duration::from_millis(100);

async fn wait_for_output_drains(event_bus: &EventBus, command_id: &str) {
    if !event_bus.wait_output_drained(OUTPUT_DRAIN_TIMEOUT).await {
        tracing::warn!(
            command_id = %command_id,
            timeout_seconds = OUTPUT_DRAIN_TIMEOUT.as_secs(),
            "output drain did not finish; trailing agent output may be missing"
        );
        event_bus.close_output_drains();
        // Closing the receivers drains already queued bytes and finalizes the
        // translator even if a broken backend retained a sender after exit.
        event_bus.wait_output_drained(OUTPUT_DRAIN_TIMEOUT).await;
    }
}

/// The single projection persisted by the worker and then read back for SSE.
struct QueuedCommandResult {
    exit_code: i32,
    status: CommandStatus,
    result_json: String,
}

fn failure(
    stage: &str,
    code: &str,
    message: impl Into<String>,
    step: Option<String>,
) -> CommandFailure {
    CommandFailure {
        stage: stage.into(),
        code: code.into(),
        message: message.into(),
        step,
    }
}

/// Preserve the legacy explicit setup_error:null through the foundation
/// type's Option<Value> deserialization (which otherwise treats null as absent).
fn read_command_result(json: &str) -> Option<CommandResult> {
    let mut result: CommandResult = serde_json::from_str(json).ok()?;
    if result.setup_error.is_none() {
        result.setup_error = serde_json::from_str::<serde_json::Value>(json)
            .ok()?
            .get("setup_error")
            .cloned();
    }
    Some(result)
}

impl QueuedCommandResult {
    fn new(
        result: &Result<CommandOutcome, crate::command::error::CommandError>,
        push: &Result<PushBackOutcome, PushBackError>,
    ) -> Self {
        Self::build(result, push, Default::default())
    }

    fn build(
        result: &Result<CommandOutcome, crate::command::error::CommandError>,
        push: &Result<PushBackOutcome, PushBackError>,
        contributions: ResultContributions,
    ) -> Self {
        use crate::command::error::CommandError;
        let exit_code = result.as_ref().map_or_else(
            |error| i32::from(error.exit_code()),
            CommandOutcome::exit_code,
        );
        let mut outcome = match result {
            Err(CommandError::Aborted) => CommandStatusKind::Aborted,
            Err(_) => CommandStatusKind::Error,
            Ok(CommandOutcome::ExecWorkflow(o)) if o.exit_code.is_none() => {
                CommandStatusKind::Paused
            }
            Ok(_) => contributions.workflow_outcome.unwrap_or(if exit_code == 0 {
                CommandStatusKind::Done
            } else {
                CommandStatusKind::Error
            }),
        };
        if outcome == CommandStatusKind::Done && exit_code != 0 {
            outcome = CommandStatusKind::Error;
        }
        let mut json = CommandResult {
            exit_code: Some(exit_code),
            outcome: Some(outcome),
            error: result.as_ref().err().map(ToString::to_string),
            error_status: result.as_ref().err().map(|error| error.http_status()),
            // Legacy `stage`, filled exactly as before WI 0137: only for an
            // unresolved prerequisite (WI 0149) and, below, a push failure.
            // `failure.stage` is set for every failure.
            stage: match result {
                Err(error @ CommandError::WorkflowPrerequisitesUnresolved { .. }) => {
                    Some(error.stage().to_string())
                }
                _ => None,
            },
            failure: result
                .as_ref()
                .err()
                .map(|error| failure(error.stage(), error.code(), error.to_string(), None)),
            worktree_path: match result {
                Err(CommandError::WorktreeExists { path, .. }) if path.is_dir() => {
                    Some(path.display().to_string())
                }
                _ => None,
            },
            pull_request: contributions.pull_request,
            final_message: contributions.final_message,
            data: contributions.data,
            ..CommandResult::default()
        };
        let failed = &contributions.failed_steps;
        if exit_code != 0 {
            if let Ok(CommandOutcome::ExecWorkflow(workflow)) = result {
                if let Some(report) = &workflow.prerequisites {
                    let error = CommandError::WorkflowPrerequisitesUnresolved {
                        report: Box::new(report.clone()),
                    };
                    json.failure = Some(failure(
                        error.stage(),
                        error.code(),
                        error.to_string(),
                        None,
                    ));
                    json.error = Some(error.to_string());
                }
            }
        }
        // Setup aborts return an Engine error; the phase's own failure is the
        // useful stage and step, while the legacy error still describes it.
        if result.is_err()
            && !contributions.workflow_main_started
            && json.failure.as_ref().is_some_and(|f| f.stage == "step")
        {
            if let Some(step) = failed
                .iter()
                .rev()
                .find(|s| s.phase == FailedStepPhase::Setup)
            {
                json.failure = Some(failure(
                    "setup",
                    failure_codes::SETUP_FAILED,
                    json.error.clone().unwrap_or_else(|| "Setup failed".into()),
                    Some(step.name.clone()),
                ));
            }
        }
        if !contributions.container_started {
            if let Some(reason) = json
                .failure
                .as_mut()
                .filter(|f| f.stage == "step" && f.code != failure_codes::ABORTED)
            {
                reason.stage = "preflight".into();
            }
        }
        if outcome != CommandStatusKind::Done && json.failure.is_none() {
            let reason = contributions
                .workflow_failure
                .unwrap_or_else(|| match outcome {
                    CommandStatusKind::Paused => {
                        failure("step", failure_codes::PAUSED, "Workflow paused", None)
                    }
                    CommandStatusKind::Aborted => {
                        failure("step", failure_codes::ABORTED, "Workflow aborted", None)
                    }
                    CommandStatusKind::Cancelled => {
                        failure("step", failure_codes::CANCELLED, "cancelled", None)
                    }
                    _ => {
                        let step =
                            if contributions.workflow_outcome == Some(CommandStatusKind::Done) {
                                failed
                                    .iter()
                                    .rev()
                                    .find(|s| s.phase == FailedStepPhase::Teardown)
                            } else {
                                failed
                                    .iter()
                                    .rev()
                                    .find(|s| s.phase == FailedStepPhase::Main)
                                    .or_else(|| failed.last())
                            };
                        match step {
                            Some(step) => {
                                let (stage, code) = match step.phase {
                                    FailedStepPhase::Setup => {
                                        ("setup", failure_codes::SETUP_FAILED)
                                    }
                                    FailedStepPhase::Main => ("step", failure_codes::STEP_FAILED),
                                    FailedStepPhase::Teardown => {
                                        ("teardown", failure_codes::TEARDOWN_FAILED)
                                    }
                                };
                                failure(
                                    stage,
                                    code,
                                    format!("Step '{}' failed", step.name),
                                    Some(step.name.clone()),
                                )
                            }
                            None => failure(
                                "step",
                                if matches!(
                                    result,
                                    Ok(CommandOutcome::ExecPrompt(_)
                                        | CommandOutcome::ExecWorkflow(_))
                                ) {
                                    failure_codes::STEP_FAILED
                                } else {
                                    failure_codes::COMMAND_FAILED
                                },
                                format!("Command exited with code {exit_code}"),
                                None,
                            ),
                        }
                    }
                });
            json.error = Some(reason.message.clone());
            json.failure = Some(reason);
        }
        if !failed.is_empty() {
            json.failed_steps = Some(contributions.failed_steps);
        }
        match push {
            Ok(PushBackOutcome::NotConfigured) => {}
            Ok(PushBackOutcome::NothingToPush) => json.pushed = Some(None),
            Ok(PushBackOutcome::Pushed(pushed)) => json.pushed = Some(Some(pushed.clone())),
            Err(error) => {
                outcome = CommandStatusKind::Error;
                json.outcome = Some(outcome);
                json.exit_code = Some(1);
                json.error = Some(error.message.clone());
                json.error_status = Some(500);
                json.stage = Some(error.stage.into());
                json.failure = Some(failure(
                    error.stage,
                    error.code(),
                    error.message.clone(),
                    None,
                ));
                json.pushed = Some(None);
            }
        }
        Self::from_result(json)
    }

    fn from_result(result: CommandResult) -> Self {
        let status = match result.outcome.unwrap_or(CommandStatusKind::Error) {
            CommandStatusKind::Done => CommandStatus::Done,
            CommandStatusKind::Cancelled => CommandStatus::Cancelled,
            _ => CommandStatus::Error,
        };
        Self {
            exit_code: result.exit_code.unwrap_or(0),
            status,
            result_json: serde_json::to_string(&result).expect("command result is serializable"),
        }
    }

    fn map_result(self, update: impl FnOnce(&mut CommandResult)) -> Self {
        let mut result = read_command_result(&self.result_json).expect("serialized command result");
        update(&mut result);
        Self::from_result(result)
    }

    fn with_worktree(self, path: Option<std::path::PathBuf>) -> Self {
        self.map_result(|result| {
            result.worktree_path = path
                .map(|p| p.display().to_string())
                .or(result.worktree_path.take());
        })
    }

    fn cancelled(self, requested_at: &str) -> Self {
        self.map_result(|result| {
            let stage = result
                .failure
                .as_ref()
                .map(|f| f.stage.clone())
                .unwrap_or_else(|| "step".into());
            result.outcome = Some(CommandStatusKind::Cancelled);
            result.failure = Some(failure(&stage, failure_codes::CANCELLED, "cancelled", None));
            result.exit_code = Some(CANCELLED_EXIT_CODE);
            result.error = Some("cancelled".into());
            result.cancelled_at = Some(requested_at.into());
            // Preserve WI 0135's legacy cancellation fields.
            result.error_status = None;
            result.stage = None;
        })
    }

    fn persist(
        &self,
        store: &SqliteSessionStore,
        command_id: &str,
    ) -> Result<(), crate::data::error::DataError> {
        store.complete_command(
            command_id,
            self.status,
            Some(self.exit_code),
            Some(&self.result_json),
        )
    }

    fn sidecar(&self, attempt: u32) -> CommandCompletion {
        CommandCompletion {
            attempt,
            status: self.status,
            exit_code: self.exit_code,
            result_json: self.result_json.clone(),
            finished_at: chrono::Utc::now().to_rfc3339(),
        }
    }

    /// Write the row with a bounded retry. On failure the last error is
    /// returned and the sidecar remains for the next startup.
    async fn persist_with_retry(
        &self,
        store: &SqliteSessionStore,
        command: &CommandRecord,
    ) -> Result<(), crate::data::error::DataError> {
        let mut attempts_left = COMPLETION_WRITE_ATTEMPTS;
        loop {
            attempts_left -= 1;
            match self.persist(store, &command.id) {
                Ok(()) => return Ok(()),
                Err(error) if attempts_left == 0 => return Err(error),
                Err(error) => {
                    tracing::warn!(%error, session_id = %command.session_id, command_id = %command.id, "Retrying command completion write");
                    tokio::time::sleep(COMPLETION_WRITE_RETRY_DELAY).await;
                }
            }
        }
    }

    async fn publish(
        &self,
        store: &SqliteSessionStore,
        paths: &ApiPaths,
        command: &CommandRecord,
        sender: &super::event_bus::EventBusSender,
    ) {
        // Durable before the row: from here on a restart finalizes this
        // attempt rather than running its side effects again.
        if let Err(error) = paths.write_command_completion(
            &command.session_id,
            &command.id,
            &self.sidecar(command.attempt),
        ) {
            tracing::error!(%error, session_id = %command.session_id, command_id = %command.id, "Failed to write completion sidecar; a failed row write would requeue this command");
        }
        let persisted = self.persist_with_retry(store, command).await;
        if let Err(error) = &persisted {
            tracing::error!(%error, session_id = %command.session_id, command_id = %command.id, attempt = command.attempt, "Failed to store command completion; startup recovery will finalize the row from its sidecar");
        }
        let stored = persisted.is_ok().then(|| {
            store.get_command(&command.id).unwrap_or_else(|error| {
                tracing::error!(%error, session_id = %command.session_id, command_id = %command.id, "Failed to read back command completion");
                None
            })
        });
        let row = stored.flatten().unwrap_or_else(|| {
            let mut row = command.clone();
            row.status = self.status;
            row.exit_code = Some(self.exit_code);
            row.result = Some(self.result_json.clone());
            row
        });
        sender.finish(terminal_payload(&row));
        if persisted.is_ok() {
            if let Err(error) = paths.remove_command_completion(&command.session_id, &command.id) {
                tracing::warn!(%error, session_id = %command.session_id, command_id = %command.id, "Failed to remove completion sidecar");
            }
        }
    }
}

/// Read the terminal event from the same row the status route serves. The row
/// must have been persisted before this function is called.
pub(crate) fn terminal_payload(command: &CommandRecord) -> EventPayload {
    let result: Option<CommandResult> = command.result.as_deref().and_then(read_command_result);
    EventPayload::CommandStatus {
        status: result.as_ref().and_then(|r| r.outcome).unwrap_or(
            command
                .status
                .terminal_wire_kind()
                .unwrap_or(CommandStatusKind::Error),
        ),
        exit_code: result
            .as_ref()
            .and_then(|r| r.exit_code)
            .or(command.exit_code),
        // Exactly the row's `result.error` (WI 0137 §2): the event and the
        // status route are two views of one value. The pre-0137 push-failure
        // event's `push: ` prefix is gone; `result.failure.stage` names it.
        error: result.as_ref().and_then(|r| r.error.clone()),
        result: result.map(Box::new),
    }
}

/// Finish a command which never had a worker (setup failed, queued cancel),
/// or repair its final pair at startup. No active bus exists for such a row.
pub(super) fn finalize_unowned_command(
    store: &SqliteSessionStore,
    paths: &ApiPaths,
    id: &str,
) -> Result<(), crate::data::error::DataError> {
    // Setup completion and a queued cancellation can race. Both write through
    // here, so only one can inspect/append a command's final pair at a time.
    static FINALIZATION: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = FINALIZATION
        .lock()
        .expect("terminal finalization lock poisoned");
    let Some(command) = store.get_command(id)? else {
        return Ok(());
    };
    if !command.status.is_terminal() {
        return Ok(());
    }
    // Completed history (including pre-0137 rows) is immutable. A restart
    // must not describe today's checkout or change the original finish time.
    if paths
        .read_command_events(&command.session_id, id)
        .iter()
        .any(|event| {
            event.attempt == command.attempt && matches!(event.payload, EventPayload::Done)
        })
    {
        return Ok(());
    }
    let mut result: CommandResult = command
        .result
        .as_deref()
        .and_then(read_command_result)
        .unwrap_or_default();
    if result.outcome.is_some() && result.outputs.is_some() {
        // An already structured terminal row is immutable. Only repair a
        // missing terminal pair after a crash; never recompute checkout facts
        // from a checkout that may have changed since this command ended.
        return CommandLogWriter::append_terminal(paths, &command, terminal_payload(&command));
    }
    result.exit_code = result.exit_code.or(command.exit_code);
    if result.outcome.is_none() {
        result.outcome = command.status.terminal_wire_kind();
    }
    if result.failure.is_none() && result.outcome != Some(CommandStatusKind::Done) {
        let cancelled = result.outcome == Some(CommandStatusKind::Cancelled);
        let setup = result.setup_error.is_some();
        result.failure = Some(failure(
            if setup { "setup" } else { "preflight" },
            if cancelled {
                failure_codes::CANCELLED
            } else if setup {
                failure_codes::SETUP_FAILED
            } else {
                failure_codes::INTERNAL
            },
            result.error.clone().unwrap_or_else(|| {
                if cancelled {
                    "cancelled".into()
                } else {
                    "Command failed".into()
                }
            }),
            None,
        ));
    }
    if result.outputs.is_none() {
        result.outputs = Some(crate::data::fs::command_outputs::summarize_output_dir(
            &paths.command_output_dir(&command.session_id, id),
        ));
    }
    // There is no captured base for an unowned command. Never attribute the
    // user's existing files to a command that did not start.
    let completion = QueuedCommandResult::from_result(result);
    completion.persist(store, id)?;
    if let Some(command) = store.get_command(id)? {
        CommandLogWriter::append_terminal(paths, &command, terminal_payload(&command))?;
    }
    Ok(())
}

/// Startup: apply terminal sidecars left by the previous daemon. A `running`
/// row finalizes from its sidecar instead of being requeued, and a sidecar whose
/// row is already terminal is removed without changing the row. Returns how many
/// rows were finalized. Every other row is still processed, but any error is
/// returned afterwards and refuses startup: the requeue that follows would run a
/// command that may already have finished a second time.
pub(crate) fn finalize_completed_from_sidecars(
    store: &SqliteSessionStore,
    paths: &ApiPaths,
) -> Result<usize, crate::data::error::DataError> {
    let commands = store.list_commands_filtered(&crate::data::fs::api_db::CommandListFilter {
        session_id: None,
        status: None,
        labels: &[],
        limit: i64::MAX,
    })?;
    let mut finalized = 0;
    let mut failures = Vec::new();
    for command in commands {
        let (session_id, id) = (&command.session_id, &command.id);
        match command.status {
            status if status.is_terminal() => {
                if matches!(paths.read_command_completion(session_id, id), Ok(None)) {
                    continue;
                }
                // The previous daemon stopped between the row and the sidecar's
                // removal, possibly before logging the terminal pair (a no-op
                // when the pair is already there).
                if let Err(error) =
                    CommandLogWriter::append_terminal(paths, &command, terminal_payload(&command))
                {
                    tracing::error!(%error, session_id, command_id = id, "Failed to append terminal events");
                }
                remove_applied_sidecar(paths, session_id, id);
            }
            CommandStatus::Running => match finalize_running_from_sidecar(store, paths, &command) {
                Ok(true) => finalized += 1,
                Ok(false) => {}
                Err(error) => {
                    tracing::error!(%error, session_id, command_id = id, "Failed to apply completion sidecar");
                    failures.push(error.to_string());
                }
            },
            _ => {}
        }
    }
    if failures.is_empty() {
        Ok(finalized)
    } else {
        Err(crate::data::error::DataError::Other(failures.join("; ")))
    }
}

/// Finalize a `running` row nobody owns from its sidecar, returning whether the
/// row is terminal now. A sidecar that exists but cannot be read is an error, not
/// absence: it may record a finished command, so the caller must neither requeue
/// nor cancel the row.
pub(super) fn finalize_running_from_sidecar(
    store: &SqliteSessionStore,
    paths: &ApiPaths,
    command: &CommandRecord,
) -> Result<bool, crate::data::error::DataError> {
    let (session_id, id) = (&command.session_id, &command.id);
    let completion = match paths.read_command_completion(session_id, id) {
        Ok(Some(completion)) => completion,
        Ok(None) => return Ok(false),
        Err(error) => {
            return Err(crate::data::error::DataError::Other(format!(
                "command {id}: completion sidecar {} is unreadable, so whether it finished is unknown; restore the file, or remove it to let the command run again: {error}",
                paths.command_completion_path(session_id, id).display()
            )));
        }
    };
    if completion.attempt != command.attempt {
        tracing::warn!(
            session_id,
            command_id = id,
            sidecar_attempt = completion.attempt,
            row_attempt = command.attempt,
            "Removing completion sidecar from another attempt"
        );
        remove_applied_sidecar(paths, session_id, id);
        return Ok(false);
    }
    if !store.finalize_running_command(id, &completion)? {
        return Ok(false);
    }
    // Skips the pair if the previous owner already logged the in-memory
    // terminal event carrying these same values.
    if let Err(error) = finalize_unowned_command(store, paths, id) {
        tracing::error!(%error, session_id, command_id = id, "Failed to append terminal events");
    }
    remove_applied_sidecar(paths, session_id, id);
    tracing::info!(session_id, command_id = id, attempt = completion.attempt, status = %completion.status, "Finalized command from completion sidecar");
    Ok(true)
}

fn remove_applied_sidecar(paths: &ApiPaths, session_id: &str, command_id: &str) {
    if let Err(error) = paths.remove_command_completion(session_id, command_id) {
        tracing::warn!(%error, session_id, command_id, "Failed to remove completion sidecar");
    }
}

/// Factory implemented by the HTTP frontend. Queue lifecycle code only knows
/// the dispatch capability it needs, never the concrete API frontend type.
pub trait ApiCommandFrontendFactory: Send + Sync {
    type Frontend: crate::command::dispatch::DispatchFrontend;

    /// The worker emits `Done` after post-command stages. The returned frontend
    /// must leave that sentinel to the worker, including when dropped on error.
    ///
    /// `interaction` is the run's resolved interaction (WI 0141): the frontend
    /// sets `AgentIo::initial_size = interaction.pty_size` and reports
    /// `headless` from it, and decides nothing else about it.
    fn frontend_for(
        &self,
        cmd: &CommandRecord,
        event_bus: super::event_bus::EventBusSender,
        state_writer: CommandStateWriter,
        interaction: crate::command::interaction::InteractionDecision,
    ) -> Self::Frontend;
}

/// The interaction a claimed push command runs with (WI 0141).
///
/// Admission already resolved it (refusing a disallowed override) and
/// recorded it in the stored args; this re-reads that choice and sizes the
/// PTY from `settings.terminal` as configured now, so a live size change
/// applies to new terminals only. It never refuses: a setting changed after
/// admission does not fail an accepted command. A row with no recorded
/// choice (written before WI 0141) takes push's configured default.
pub fn push_run_interaction(
    cmd: &CommandRecord,
    settings: &crate::data::config::daemon::DaemonSettings,
) -> crate::command::interaction::InteractionDecision {
    use crate::command::interaction::{decision_for, requested_from_flags, InteractionSlot};
    let path: Vec<&str> = cmd.subcommand.split_whitespace().collect();
    let requested = cmd
        .decode_args()
        .ok()
        .and_then(|args| {
            crate::command::dispatch::catalogue::CommandCatalogue::get()
                .parse_raw_args(&path, &args)
                .ok()
        })
        .and_then(|parsed| {
            requested_from_flags(
                parsed.flag_bool("interactive"),
                parsed.flag_bool("headless"),
            )
        });
    let interaction =
        requested.unwrap_or_else(|| InteractionSlot::WorkerPush.configured(settings).0);
    decision_for(interaction, settings)
}
pub struct QueueWorker<F: ApiCommandFrontendFactory> {
    executor: Option<Arc<crate::engine::daemon::executor::SharedExecutor>>,
    worker_id: String,
    store: Arc<SqliteSessionStore>,
    engines: Engines,
    sessions: Arc<SessionManager>,
    event_buses: Arc<tokio::sync::Mutex<HashMap<String, Arc<EventBus>>>>,
    paths: ApiPaths,
    frontend_factory: Arc<F>,
    lifecycle: ApiSessionLifecycle,
    lifecycle_store: CommandLifecycleStore,
    cancels: Arc<TokenRegistry>,
    /// The daemon settings as they are now (live reloads included), read when
    /// a command starts so its PTY size is the one configured at spawn.
    /// `None` reads `engines.global_config`.
    daemon_settings: Option<DaemonSettingsSource>,
}

/// Reads the current `daemon` settings.
pub type DaemonSettingsSource =
    Arc<dyn Fn() -> crate::data::config::daemon::DaemonSettings + Send + Sync>;

/// Dependencies collected by API runtime bootstrap before a queue worker is
/// spawned. Keeping this bundle typed avoids a parallel constructor argument
/// list as shared daemon state evolves.
pub struct QueueWorkerDeps<F: ApiCommandFrontendFactory> {
    pub store: Arc<SqliteSessionStore>,
    pub engines: Engines,
    pub sessions: Arc<SessionManager>,
    pub event_buses: Arc<tokio::sync::Mutex<HashMap<String, Arc<EventBus>>>>,
    pub paths: ApiPaths,
    pub frontend_factory: Arc<F>,
    pub lifecycle: ApiSessionLifecycle,
    /// Cancel markers and `/v1/runs` linkage (WI 0135).
    pub lifecycle_store: CommandLifecycleStore,
    /// The daemon's command-cancel tokens; this worker registers each
    /// command it claims.
    pub cancels: Arc<TokenRegistry>,
}

impl<F: ApiCommandFrontendFactory> QueueWorker<F> {
    /// Exercise worker early exits from Layer 3's frontend integration tests.
    #[cfg(test)]
    pub(crate) async fn execute_for_test(&self, command: CommandRecord) {
        self.execute_command(command, Default::default()).await;
    }
    /// Finish cancelled teardown before starting the normal worker pool.
    pub async fn recover_cancelled_cleanup(
        &self,
    ) -> Result<(), crate::command::error::CommandError> {
        for id in self.lifecycle_store.cancelled_command_ids()? {
            let Some(cmd) = self.store.get_command(id.as_str())? else {
                continue;
            };
            let Some(cleanup) = self
                .paths
                .read_command_cleanup(&cmd.session_id, id.as_str())?
            else {
                continue;
            };
            let session = self.sessions.get_by_key(&cmd.session_id).ok_or_else(|| {
                crate::command::error::CommandError::Other(format!(
                    "Cannot recover cleanup for session {}",
                    cmd.session_id
                ))
            })?;
            let session = session.read().await.clone();
            let bus = EventBus::for_attempt(4096, cmd.attempt);
            let writer = CommandStateWriter::new(self.paths.clone(), &cmd.session_id, id.as_str());
            let interaction = self.interaction_for(&cmd);
            let frontend =
                self.frontend_factory
                    .frontend_for(&cmd, bus.sender(), writer, interaction);
            let mut engines = self.engines.clone();
            engines.runtime = Arc::new(super::owned_runtime::CommandOwnedRuntime::new(
                Arc::clone(&engines.runtime),
                id.as_str(),
            ));
            crate::command::commands::exec_workflow::finish_cancelled_cleanup(
                &engines,
                &session,
                cleanup,
                crate::command::commands::exec_workflow::SharedWorkflowFrontend::new(Box::new(
                    frontend,
                )),
            )
            .await?;
            // Recovery only finishes teardown for an already-cancelled row.
            // Its private bus is intentionally neither published nor logged;
            // restarting sequence zero must not append duplicate event ids.
            self.paths
                .remove_command_cleanup(&cmd.session_id, id.as_str())?;
            self.post_execution_check(&cmd.session_id, id.as_str())
                .await;
        }
        Ok(())
    }

    pub fn new(worker_id: String, deps: QueueWorkerDeps<F>) -> Self {
        Self {
            executor: None,
            worker_id,
            store: deps.store,
            engines: deps.engines,
            sessions: deps.sessions,
            event_buses: deps.event_buses,
            paths: deps.paths,
            frontend_factory: deps.frontend_factory,
            lifecycle: deps.lifecycle,
            lifecycle_store: deps.lifecycle_store,
            cancels: deps.cancels,
            daemon_settings: None,
        }
    }

    /// Read the live `daemon` settings when a command starts (WI 0141).
    pub fn with_daemon_settings(mut self, source: DaemonSettingsSource) -> Self {
        self.daemon_settings = Some(source);
        self
    }

    /// [`push_run_interaction`] against the live settings.
    fn interaction_for(
        &self,
        cmd: &CommandRecord,
    ) -> crate::command::interaction::InteractionDecision {
        let settings = match &self.daemon_settings {
            Some(source) => source(),
            None => crate::data::config::daemon::DaemonSettings::from_merged(
                &self
                    .engines
                    .global_config
                    .daemon
                    .clone()
                    .unwrap_or_default(),
            ),
        };
        push_run_interaction(cmd, &settings)
    }
    pub fn with_executor(
        mut self,
        executor: Arc<crate::engine::daemon::executor::SharedExecutor>,
    ) -> Self {
        self.executor = Some(executor);
        self
    }

    pub async fn run(self) {
        self.run_until(tokio_util::sync::CancellationToken::new())
            .await
    }
    pub async fn run_until(self, stop: tokio_util::sync::CancellationToken) {
        loop {
            if stop.is_cancelled() {
                return;
            }
            let permit = if let Some(executor) = &self.executor {
                Some(
                    tokio::select! { biased; _=stop.cancelled()=>return, permit=executor.acquire(crate::data::daemon_state::FeatureName::Worker)=>permit },
                )
            } else {
                None
            };
            if stop.is_cancelled() {
                return;
            }
            // Claim and register the cancel token in one step, so a cancel
            // request never finds this command running without an owner.
            let claimed = self.cancels.claim(
                || self.store.claim_next_command(&self.worker_id),
                |cmd| cmd.id.clone(),
            );
            match claimed {
                Ok(Some((cmd, cancel))) => {
                    tracing::info!(
                        worker_id = %self.worker_id,
                        command_id = %cmd.id,
                        session_id = %cmd.session_id,
                        subcommand = %cmd.subcommand,
                        trace_parent = ?cmd.trace_parent,
                        listener = %cmd.listener,
                        "Worker claimed command"
                    );
                    let command_id = cmd.id.clone();
                    Box::pin(self.execute_command(cmd, cancel)).await;
                    self.cancels.unregister(&command_id);
                }
                Ok(None) => {
                    drop(permit);
                    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                }
                Err(e) => {
                    drop(permit);
                    tracing::error!(
                        worker_id = %self.worker_id,
                        error = %e,
                        "Worker failed to claim command"
                    );
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                }
            }
        }
    }

    async fn execute_command(
        &self,
        cmd: crate::data::fs::api_db::CommandRecord,
        cancel: tokio_util::sync::CancellationToken,
    ) {
        let command_id = cmd.id.clone();
        let session_id = cmd.session_id.clone();
        let subcommand = cmd.subcommand.clone();

        // A cancel recorded before this worker owned the command (it lost the
        // race with the claim) still applies.
        let cancel_requested_at = self
            .lifecycle_store
            .cancel_requested_at(
                &crate::data::ids::CommandId::parse(&command_id)
                    .expect("validated persisted resource id"),
            )
            .ok()
            .flatten();
        if cancel_requested_at.is_some() {
            cancel.cancel();
        }

        let args = match cmd.decode_args() {
            Ok(args) => args,
            Err(error) => {
                tracing::error!(session_id = %session_id, command_id = %command_id, %error, "Invalid queue record");
                let mut completion = QueuedCommandResult::new(
                    &Err(crate::command::error::CommandError::Other(
                        error.to_string(),
                    )),
                    &Ok(PushBackOutcome::NotConfigured),
                )
                .map_result(|r| {
                    r.outputs = Some(Default::default());
                    if let Some(failure) = &mut r.failure {
                        failure.stage = "preflight".into();
                        failure.code = failure_codes::INVALID_QUEUE_RECORD.into();
                    }
                });
                if let Some(requested_at) = &cancel_requested_at {
                    completion = completion.cancelled(requested_at);
                }
                self.finish_without_log(&cmd, completion).await;
                return;
            }
        };

        tracing::info!(
            worker_id = %self.worker_id,
            command_id = %command_id,
            session_id = %session_id,
            subcommand = %subcommand,
            args = ?args,
            "Worker executing command"
        );

        // Create command directory (Layer 0).
        if let Err(e) = self.paths.prepare_command_dir(&session_id, &command_id) {
            tracing::error!(
                command_id = %command_id,
                error = %e,
                "Failed to create command directory"
            );
            let completion = QueuedCommandResult::new(
                &Err(crate::command::error::CommandError::Other(format!(
                    "Failed to create command directory: {e}"
                ))),
                &Ok(PushBackOutcome::NotConfigured),
            )
            .map_result(|r| {
                r.outputs = Some(Default::default());
                if let Some(failure) = &mut r.failure {
                    failure.stage = "preflight".into();
                }
            });
            self.finish_without_log(&cmd, completion).await;
            return;
        }

        // Write initial metadata (Layer 0).
        {
            let metadata = serde_json::json!({
                "command_id": command_id,
                "session_id": session_id,
                "subcommand": subcommand,
                "args": args,
                "attempt": cmd.attempt,
                "started_at": cmd.started_at,
                "worker_id": self.worker_id,
                "listener":cmd.listener,
                "trace_parent":cmd.trace_parent,
            });
            self.write_metadata(&session_id, &command_id, &metadata);
        }

        // Create EventBus for this command execution.
        let event_bus = Arc::new(EventBus::for_attempt(4096, cmd.attempt));

        // Spawn logfile writer task. The two log files are owned by a Layer 0
        // writer so this frontend performs no filesystem calls of its own.
        let log_writer = {
            let mut log_rx = event_bus.subscribe_log();
            let paths = self.paths.clone();
            let session_id_for_log = session_id.clone();
            let command_id_for_log = command_id.clone();
            tokio::spawn(async move {
                let mut writer = match CommandLogWriter::open_append(
                    &paths,
                    &session_id_for_log,
                    &command_id_for_log,
                )
                .await
                {
                    Ok(w) => w,
                    Err(e) => {
                        tracing::error!(error = %e, "Failed to create command log files");
                        return;
                    }
                };
                while let Some(event) = log_rx.recv().await {
                    writer.write_event(&event).await;
                    if matches!(event.payload, EventPayload::Done) {
                        writer.flush().await;
                        break;
                    }
                }
            })
        };

        // Spawn server-side tracing task — emits structured tracing records
        // for workflow step + phase transitions and the engine-reported
        // CommandStatus. Without this the operator only sees the worker's
        // "Worker executing command" / "Command execution finished" lines and
        // has no signal that an individual step container failed.
        spawn_tracing_subscriber(
            event_bus.subscribe(),
            command_id.clone(),
            session_id.clone(),
            subcommand.clone(),
        );

        // Store EventBus handle for SSE subscribers.
        self.event_buses
            .lock()
            .await
            .insert(command_id.clone(), Arc::clone(&event_bus));

        // A re-queued command says so as the first event of its new attempt,
        // so a client reading the appended log or attached live sees why the
        // output restarts (WI 0136).
        if cmd.attempt > FIRST_ATTEMPT {
            event_bus.sender().emit(EventPayload::StatusMessage {
                phase: "ses_a9d1cbf71942327e98b40cf5ef38a960".into(),
                message: format!("re-queued after restart (attempt {})", cmd.attempt),
            });
        }

        // The API frontend supplies the presentation adapter through its
        // factory; Layer 2 owns everything around command execution.
        let state_writer = CommandStateWriter::new(self.paths.clone(), &session_id, &command_id);
        let interaction = self.interaction_for(&cmd);
        let frontend = self.frontend_factory.frontend_for(
            &cmd,
            event_bus.sender(),
            state_writer.clone(),
            interaction,
        );

        // Look up the Session.
        let session = match self.sessions.get_by_key(&session_id) {
            Some(s) => s,
            None => {
                tracing::error!(
                    command_id = %command_id,
                    session_id = %session_id,
                    "Session not found in memory"
                );
                drop(frontend);
                wait_for_output_drains(&event_bus, &command_id).await;
                let completion = QueuedCommandResult::new(
                    &Err(crate::command::error::CommandError::Other(
                        "Session not found in memory".into(),
                    )),
                    &Ok(PushBackOutcome::NotConfigured),
                )
                .map_result(|r| {
                    r.outputs = Some(crate::data::fs::command_outputs::summarize_output_dir(
                        &state_writer.output_dir(),
                    ));
                    if let Some(failure) = &mut r.failure {
                        failure.stage = "preflight".into();
                    }
                });
                completion
                    .publish(&self.store, &self.paths, &cmd, &event_bus.sender())
                    .await;
                let _ = log_writer.await;
                self.cleanup_event_bus(&command_id).await;
                self.post_execution_check(&session_id, &command_id).await;
                return;
            }
        };

        let checkout_root = session.read().await.git_root().to_path_buf();
        let git = Arc::clone(&self.engines.git_engine);
        let root = checkout_root.clone();
        let base_sha = tokio::task::spawn_blocking(move || git.command_base_sha(&root))
            .await
            .ok()
            .flatten();

        // Capture before dispatch: explicit workflow teardown may publish itself.
        let baseline = match self.store.get_session(&session_id) {
            Ok(Some(record)) => {
                let git = Arc::clone(&self.engines.git_engine);
                tokio::task::spawn_blocking(move || push_baseline(&git, &record))
                    .await
                    .unwrap_or_else(|e| {
                        Err(PushBackError {
                            stage: "push",
                            message: e.to_string(),
                        })
                    })
            }
            Ok(None) => Ok(None),
            Err(e) => Err(PushBackError {
                stage: "push",
                message: e.to_string(),
            }),
        };

        // Dispatch through Layer 2.
        let path_parts: Vec<&str> = subcommand.split_whitespace().collect();
        // Every container the command launches is named after it, so a
        // restart can find and stop what a cancelled command left running.
        let mut engines = self.engines.clone();
        engines.runtime = Arc::new(
            super::owned_runtime::CommandOwnedRuntime::new(
                Arc::clone(&engines.runtime),
                &command_id,
            )
            .with_owner(crate::data::terminal::TerminalOwnerInfo {
                owner: crate::data::terminal::TerminalOwner::Command(
                    crate::data::ids::CommandId::parse(&command_id).expect("stored command id"),
                ),
                feature: crate::data::daemon_state::FeatureName::Worker,
                source: None,
                attempt: cmd.attempt,
                allow_remote_input: cmd.allow_remote_input,
            }),
        );
        // The command's `/awman/output` directory (WI 0137 §3): the command
        // that mounts it creates (or, for `exec prompt`, clears) it.
        let dispatch = Dispatch::new(frontend, session, engines)
            .with_cancellation(cancel)
            .with_output_dir(self.paths.command_output_dir(&session_id, &command_id));
        tracing::info!(
            worker_id = %self.worker_id,
            command_id = %command_id,
            path = ?path_parts,
            "Dispatching command to Layer 2"
        );
        // Catch panics from Layer 2 so a panicking workflow step doesn't
        // silently kill the worker task and leave the command marked
        // `running` forever. Without this, any unwrap()/expect() that
        // fires inside the engine would vanish into tokio's task harness.
        use futures_util::FutureExt as _;
        // Dispatch includes the concrete command futures. Construct and poll
        // them on a job thread with room for those futures, while I/O and
        // spawned tasks continue using the daemon's Tokio runtime. This also
        // keeps a long synchronous command off a listener's runtime thread.
        let (result_tx, result_rx) = tokio::sync::oneshot::channel();
        let handle = tokio::runtime::Handle::current();
        let command_path = subcommand.clone();
        let span = tracing::Span::current();
        let job = std::thread::Builder::new()
            .name("awman-command".into())
            .stack_size(16 * 1024 * 1024)
            .spawn(move || {
                let _span = span.enter();
                let outcome = handle.block_on(
                    std::panic::AssertUnwindSafe(async move {
                        let parts: Vec<_> = command_path.split_whitespace().collect();
                        dispatch.run_command(&parts).await
                    })
                    .catch_unwind(),
                );
                let _ = result_tx.send(outcome);
            });
        let dispatch_outcome = match job {
            Ok(_) => result_rx
                .await
                .unwrap_or_else(|e| Err(Box::new(e) as Box<dyn std::any::Any + Send>)),
            Err(e) => Err(Box::new(e) as Box<dyn std::any::Any + Send>),
        };
        let result = match dispatch_outcome {
            Ok(r) => r,
            Err(panic_payload) => {
                let panic_msg = panic_payload
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| {
                        panic_payload
                            .downcast_ref::<&'static str>()
                            .map(|s| s.to_string())
                    })
                    .unwrap_or_else(|| "command task panicked (unknown payload)".to_string());
                tracing::error!(
                    worker_id = %self.worker_id,
                    command_id = %command_id,
                    panic = %panic_msg,
                    "Command task panicked"
                );
                Err(crate::command::error::CommandError::Other(format!(
                    "command panicked: {panic_msg}"
                )))
            }
        };

        // Workflow completion is synchronous inside the frontend, so it is
        // deferred until the tracked output tasks have emitted their final
        // buffered lines. The timeout keeps cancellation from waiting forever
        // if a backend leaves an output sender open.
        wait_for_output_drains(&event_bus, &command_id).await;

        // A cancel accepted before push-back begins wins over a clean exit:
        // the cancelled work is not published and the command is `cancelled`.
        // Push already in flight cannot be undone; a cancel received while
        // it runs still determines the final command status below.
        let cancelled_before_push = if command_succeeded_for_push(&result) {
            self.lifecycle_store
                .cancel_requested_at(
                    &crate::data::ids::CommandId::parse(&command_id)
                        .expect("validated persisted resource id"),
                )
                .ok()
                .flatten()
        } else {
            None
        };
        let push_outcome = if command_succeeded_for_push(&result) && cancelled_before_push.is_none()
        {
            match self.store.get_session(&session_id) {
                Ok(Some(record)) => {
                    let git = Arc::clone(&self.engines.git_engine);
                    let subcommand = subcommand.clone();
                    let args = args.clone();
                    let sender = event_bus.sender();
                    tokio::task::spawn_blocking(move || {
                        let baseline = baseline?;
                        push_after_success(
                            &git,
                            &record,
                            &subcommand,
                            &args,
                            baseline,
                            &mut PushEventSink(sender),
                        )
                    })
                    .await
                    .unwrap_or_else(|error| {
                        Err(PushBackError {
                            stage: "push",
                            message: format!("push task failed: {error}"),
                        })
                    })
                }
                Ok(None) => Ok(PushBackOutcome::NotConfigured),
                Err(error) => Err(PushBackError {
                    stage: "push",
                    message: format!("failed to read session push configuration: {error}"),
                }),
            }
        } else {
            Ok(PushBackOutcome::NotConfigured)
        };
        if let Err(error) = &push_outcome {
            tracing::error!(command_id = %command_id, error = %error.message, "Push-back failed");
        }

        let mut completion =
            QueuedCommandResult::build(&result, &push_outcome, state_writer.result_contributions())
                .with_worktree(state_writer.kept_worktree());
        // An accepted cancellation wins until the terminal row is published,
        // including a request received during automatic push-back.
        if let Some(requested_at) = &cancelled_before_push {
            completion = completion.cancelled(requested_at);
        } else if let Ok(Some(requested_at)) = self.lifecycle_store.cancel_requested_at(
            &crate::data::ids::CommandId::parse(&command_id)
                .expect("validated persisted resource id"),
        ) {
            completion = completion.cancelled(&requested_at);
        }
        // This runs before post_execution_check can remove a remote clone.
        let canonical_path = crate::command::dispatch::catalogue::CommandCatalogue::get()
            .canonical_path(&path_parts);
        let collect_git = matches!(
            canonical_path.as_slice(),
            ["exec", "prompt" | "workflow"] | ["chat"]
        ) && !matches!(
            result,
            Err(crate::command::error::CommandError::WorktreeExists { .. })
        );
        let root = state_writer.kept_worktree().unwrap_or(checkout_root);
        let git = Arc::clone(&self.engines.git_engine);
        let git_result = tokio::task::spawn_blocking(move || {
            if collect_git {
                git.command_result(&root, base_sha.as_deref())
            } else {
                Ok(None)
            }
        })
        .await;
        let git_result = match git_result {
            Ok(Ok(value)) => value,
            error => {
                tracing::warn!(?error, %command_id, "Could not collect command git result");
                None
            }
        };
        completion = completion.map_result(|value| {
            value.git = git_result;
            value.outputs = Some(crate::data::fs::command_outputs::summarize_output_dir(
                &state_writer.output_dir(),
            ));
            // Workflow agents' messages are step output; only exec prompt has
            // a command-level final message.
            if crate::command::dispatch::catalogue::CommandCatalogue::get()
                .canonical_path(&path_parts)
                .join(" ")
                != "exec prompt"
            {
                value.final_message = None;
            }
            if value.data.is_none() {
                if let Ok(outcome) = &result {
                    value.data = catalogue_json(outcome);
                }
            }
        });
        // Git collection and output summarization can take time; observe a
        // cancellation accepted during either before publishing the row.
        if let Ok(Some(requested_at)) = self.lifecycle_store.cancel_requested_at(
            &crate::data::ids::CommandId::parse(&command_id)
                .expect("validated persisted resource id"),
        ) {
            completion = completion.cancelled(&requested_at);
        }
        let exit_code = completion.exit_code;
        let status = completion.status;
        let success = status == CommandStatus::Done;

        if success {
            tracing::info!(
                worker_id = %self.worker_id,
                command_id = %command_id,
                subcommand = %subcommand,
                status = status.as_str(),
                exit_code = ?exit_code,
                success = true,
                "Command execution finished"
            );
        } else {
            tracing::error!(
                worker_id = %self.worker_id,
                command_id = %command_id,
                subcommand = %subcommand,
                status = status.as_str(),
                exit_code = ?exit_code,
                success = false,
                error = push_outcome.as_ref().err().map(|e| e.message.clone())
                    .or_else(|| result.as_ref().err().map(ToString::to_string)).unwrap_or_default(),
                "Command execution finished with failure"
            );
        }

        if let Err(ref e) = result {
            tracing::error!(command_id = %command_id, error = %e, "Command failed");
        }

        // Write final metadata.
        {
            let finished_at = chrono::Utc::now().to_rfc3339();
            let metadata = serde_json::json!({
                "command_id": command_id,
                "session_id": session_id,
                "subcommand": subcommand,
                "args": args,
                "started_at": cmd.started_at,
                "finished_at": finished_at,
                "exit_code": exit_code,
                "status": status,
                "worker_id": self.worker_id,
                "attempt": cmd.attempt,
            });
            self.write_metadata(&session_id, &command_id, &metadata);
        }

        // Cleanup has finished. Retire its recovery recipe before publishing
        // the terminal row; a crash after this point needs no repeated cleanup.
        if let Err(error) = self.paths.remove_command_cleanup(&session_id, &command_id) {
            tracing::warn!(%error, "Failed to retire command cleanup recipe");
        }
        completion
            .publish(&self.store, &self.paths, &cmd, &event_bus.sender())
            .await;
        let _ = log_writer.await;
        self.cleanup_event_bus(&command_id).await;
        self.post_execution_check(&session_id, &command_id).await;
    }

    /// Complete a command that failed before its log writer started.
    async fn finish_without_log(&self, cmd: &CommandRecord, completion: QueuedCommandResult) {
        let bus = Arc::new(EventBus::for_attempt(16, cmd.attempt));
        self.event_buses
            .lock()
            .await
            .insert(cmd.id.clone(), Arc::clone(&bus));
        completion
            .publish(&self.store, &self.paths, cmd, &bus.sender())
            .await;
        self.cleanup_event_bus(&cmd.id).await;
        self.post_execution_check(&cmd.session_id, &cmd.id).await;
    }

    /// `metadata.json` has no reader in the tree, so a failed write only warns.
    fn write_metadata(&self, session_id: &str, command_id: &str, metadata: &serde_json::Value) {
        if let Err(error) = self
            .paths
            .write_command_metadata(session_id, command_id, metadata)
        {
            tracing::warn!(%error, session_id, command_id, "Failed to write command metadata");
        }
    }

    async fn cleanup_event_bus(&self, command_id: &str) {
        let buses = Arc::clone(&self.event_buses);
        let cmd_id = command_id.to_string();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            buses.lock().await.remove(&cmd_id);
        });
    }

    /// Finish closing a draining session, and close a `/v1/runs` session
    /// asked to `close_on_finish` once its run command (`command_id`) is
    /// terminal — `done`, `error` and `cancelled` alike. Both go through the
    /// one `ApiSessionLifecycle::close`, so a remote clone is deleted exactly
    /// as `DELETE /v1/sessions/{id}` would.
    async fn post_execution_check(&self, session_id: &str, command_id: &str) {
        let closing = match self.store.get_session(session_id) {
            Ok(session) => session.is_some_and(|s| s.status == SessionStatus::Closing),
            Err(error) => {
                tracing::error!(session_id, command_id, %error, "Failed to read session after command");
                false
            }
        };
        let run_finished = matches!(
            self.lifecycle_store.run_for_session(&crate::data::ids::WorkerSessionId::parse(session_id).expect("validated persisted resource id")),
            Ok(Some(ref run)) if run.close_on_finish && run.command_id.as_str() == command_id
        );
        if closing || run_finished {
            if let Err(error) = self.lifecycle.close(session_id).await {
                tracing::error!(worker_id = %self.worker_id, %error, session_id, "Failed to finish closing session");
            }
        }
    }
}

/// A paused/interrupted execution has no process exit code, even though the
/// general command projection maps it to zero. Only a real exit 0 may push.
fn command_succeeded_for_push(
    result: &Result<CommandOutcome, crate::command::error::CommandError>,
) -> bool {
    match result {
        Ok(CommandOutcome::ExecWorkflow(outcome)) => outcome.exit_code == Some(0),
        Ok(CommandOutcome::ExecPrompt(outcome)) => outcome.exit_code == Some(0),
        _ => false,
    }
}

/// JSON values used by the catalogue's CLI presenters. Agents record their
/// exact status/list/detail/kill value in the frontend instead.
fn catalogue_json(outcome: &CommandOutcome) -> Option<serde_json::Value> {
    match outcome {
        CommandOutcome::Ready(outcome) if outcome.json_requested => Some(outcome.to_legacy_json()),
        CommandOutcome::Squad(outcome) => serde_json::to_value(outcome).ok(),
        _ => None,
    }
}

/// Subscribe to a per-command event bus and emit `tracing` records for the
/// events an operator cares about — workflow step/phase transitions and the
/// final `CommandStatus`. Container stdout/stderr is excluded (already lands
/// in `output.log`); only structural events are tracing-worthy.
fn spawn_tracing_subscriber(
    mut rx: tokio::sync::broadcast::Receiver<crate::data::execution_event::ExecutionEvent>,
    command_id: String,
    session_id: String,
    subcommand: String,
) {
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(event) => match event.payload {
                    EventPayload::WorkflowStepTransition {
                        step_name,
                        step_index,
                        from_status,
                        to_status,
                    } => match to_status {
                        StepStatusKind::Failed => tracing::error!(
                            command_id = %command_id,
                            session_id = %session_id,
                            subcommand = %subcommand,
                            step_index = step_index,
                            step = %step_name,
                            from = %from_status,
                            to = %to_status,
                            "Workflow step failed"
                        ),
                        StepStatusKind::Cancelled | StepStatusKind::Skipped => tracing::warn!(
                            command_id = %command_id,
                            session_id = %session_id,
                            subcommand = %subcommand,
                            step_index = step_index,
                            step = %step_name,
                            from = %from_status,
                            to = %to_status,
                            "Workflow step did not run"
                        ),
                        _ => tracing::info!(
                            command_id = %command_id,
                            session_id = %session_id,
                            subcommand = %subcommand,
                            step_index = step_index,
                            step = %step_name,
                            from = %from_status,
                            to = %to_status,
                            "Workflow step transition"
                        ),
                    },
                    EventPayload::WorkflowPhaseTransition {
                        phase,
                        step_desc,
                        status,
                    } => {
                        if matches!(
                            status,
                            PhaseStatusKind::Failed | PhaseStatusKind::TeardownFailed
                        ) {
                            tracing::error!(
                                command_id = %command_id,
                                session_id = %session_id,
                                subcommand = %subcommand,
                                phase = %phase,
                                status = %status,
                                desc = %step_desc,
                                "Workflow phase failed"
                            );
                        } else {
                            tracing::info!(
                                command_id = %command_id,
                                session_id = %session_id,
                                subcommand = %subcommand,
                                phase = %phase,
                                status = %status,
                                desc = %step_desc,
                                "Workflow phase transition"
                            );
                        }
                    }
                    EventPayload::CommandStatus {
                        status,
                        exit_code,
                        error,
                        ..
                    } => match status {
                        CommandStatusKind::Done => tracing::info!(
                            command_id = %command_id,
                            session_id = %session_id,
                            subcommand = %subcommand,
                            exit_code = ?exit_code,
                            "Engine reported command status: done"
                        ),
                        _ => tracing::error!(
                            command_id = %command_id,
                            session_id = %session_id,
                            subcommand = %subcommand,
                            status = %status,
                            exit_code = ?exit_code,
                            error = ?error,
                            "Engine reported command status: failure"
                        ),
                    },
                    EventPayload::WorkflowParallelStepLaunched {
                        step_name,
                        step_index,
                        agent,
                        model,
                    } => tracing::info!(
                        command_id = %command_id,
                        session_id = %session_id,
                        subcommand = %subcommand,
                        step_index = step_index,
                        step = %step_name,
                        agent = %agent,
                        model = ?model,
                        "Parallel step launched"
                    ),
                    EventPayload::WorkflowParallelStepExited {
                        step_name,
                        step_index,
                        exit_code,
                    } => tracing::info!(
                        command_id = %command_id,
                        session_id = %session_id,
                        subcommand = %subcommand,
                        step_index = step_index,
                        step = %step_name,
                        exit_code = exit_code,
                        "Parallel step exited"
                    ),
                    EventPayload::WorkflowParallelGroupFinished => tracing::info!(
                        command_id = %command_id,
                        session_id = %session_id,
                        subcommand = %subcommand,
                        "Parallel group finished"
                    ),
                    EventPayload::Done => break,
                    EventPayload::StdoutLine(_)
                    | EventPayload::StderrLine(_)
                    | EventPayload::StatusMessage { .. }
                    | EventPayload::StepActivity { .. }
                    | EventPayload::WorkflowPrerequisites { .. } => {}
                    // WI 0141 status and terminal events are for clients, not the
                    // daemon log (terminals-api owns any logging it wants here).
                    EventPayload::ProgramStatus { .. }
                    | EventPayload::TerminalOpened { .. }
                    | EventPayload::TerminalClosed { .. }
                    | EventPayload::TerminalControl { .. }
                    | EventPayload::TerminalViewerDetached { .. } => {}
                },
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(
                        command_id = %command_id,
                        lagged = n,
                        "Tracing subscriber lagged"
                    );
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}

#[cfg(test)]
mod completion_tests {
    use super::*;
    use crate::command::error::CommandError;
    use crate::engine::error::EngineError;

    #[test]
    fn push_requires_an_explicit_successful_execution_exit() {
        for exit_code in [None, Some(0), Some(1), Some(130)] {
            let workflow = Ok(CommandOutcome::ExecWorkflow(
                crate::command::commands::exec_workflow::ExecWorkflowOutcome {
                    prerequisites: None,
                    workflow: "workflow.toml".into(),
                    exit_code,
                    worktree_used: false,
                },
            ));
            let prompt = Ok(CommandOutcome::ExecPrompt(
                crate::command::commands::exec_prompt::ExecPromptOutcome {
                    agent: None,
                    exit_code,
                },
            ));
            assert_eq!(command_succeeded_for_push(&workflow), exit_code == Some(0));
            assert_eq!(command_succeeded_for_push(&prompt), exit_code == Some(0));
        }
        assert!(!command_succeeded_for_push(&Err(CommandError::Aborted)));
        assert!(!command_succeeded_for_push(&Ok(CommandOutcome::Empty)));
    }

    #[test]
    fn push_completion_records_absent_null_and_pushed_ref() {
        use crate::data::fs::api_db::PushedRef;

        let result = Ok(CommandOutcome::Empty);
        let unconfigured = QueuedCommandResult::new(&result, &Ok(PushBackOutcome::NotConfigured));
        assert_eq!(
            unconfigured.result_json,
            r#"{"exit_code":0,"outcome":"done"}"#
        );

        let nothing = QueuedCommandResult::new(&result, &Ok(PushBackOutcome::NothingToPush));
        let json: serde_json::Value = serde_json::from_str(&nothing.result_json).unwrap();
        assert!(json["pushed"].is_null());
        assert!(json.as_object().unwrap().contains_key("pushed"));

        let pushed = PushedRef {
            remote: "origin".into(),
            branch: "feature".into(),
            sha: "1234567890".into(),
        };
        let completion =
            QueuedCommandResult::new(&result, &Ok(PushBackOutcome::Pushed(pushed.clone())));
        let json: CommandResult = serde_json::from_str(&completion.result_json).unwrap();
        assert_eq!(completion.status.as_str(), "done");
        assert_eq!(json.pushed, Some(Some(pushed)));
    }

    #[test]
    fn push_failure_overrides_success_and_persists_stderr() {
        let failure = PushBackError::from(EngineError::Git(
            "git push failed: rejected (non-fast-forward)".into(),
        ));
        let completion = QueuedCommandResult::new(&Ok(CommandOutcome::Empty), &Err(failure));
        let json: CommandResult = serde_json::from_str(&completion.result_json).unwrap();
        assert_eq!(completion.status.as_str(), "error");
        assert_eq!(completion.exit_code, 1);
        assert_eq!(json.exit_code, Some(1));
        assert_eq!(json.stage.as_deref(), Some("push"));
        assert!(json.error.unwrap().contains("rejected (non-fast-forward)"));
        assert_eq!(json.error_status, Some(500));
        assert_eq!(json.pushed, Some(None));
    }

    #[test]
    fn queued_failures_persist_the_shared_code_in_row_and_json() {
        let temp = tempfile::tempdir().unwrap();
        let store = SqliteSessionStore::open(temp.path()).unwrap();
        store
            .insert_session(
                "ses_21d6f40cfb511982e4424e0e250a9557",
                "/work",
                "2026-10-02",
            )
            .unwrap();
        for (index, (error, expected)) in [
            (
                CommandError::Engine(EngineError::ContainerRuntimeUnavailable {
                    binary: "docker".into(),
                }),
                3,
            ),
            (
                CommandError::Engine(EngineError::BuiltinAttachUnavailable {
                    name: "agent".into(),
                    detail: "closed".into(),
                }),
                1,
            ),
            (CommandError::Aborted, 130),
            (
                CommandError::MissingRequiredArgument {
                    command: vec!["prompt".into()],
                    argument: "prompt".into(),
                },
                2,
            ),
        ]
        .into_iter()
        .enumerate()
        {
            let id = index.to_string();
            store
                .enqueue_command(
                    &id,
                    "ses_21d6f40cfb511982e4424e0e250a9557",
                    "prompt",
                    "{}",
                    "log",
                )
                .unwrap();
            let command = store.claim_next_command("worker").unwrap().unwrap();
            assert_eq!(command.id, id);
            let completion =
                QueuedCommandResult::new(&Err(error), &Ok(PushBackOutcome::NotConfigured));
            completion.persist(&store, &id).unwrap();
            let saved = store.get_command(&id).unwrap().unwrap();
            assert_eq!(saved.status.as_str(), "error");
            assert_eq!(saved.exit_code, Some(expected));
            let json: serde_json::Value =
                serde_json::from_str(saved.result.as_ref().unwrap()).unwrap();
            assert_eq!(json["exit_code"], expected);
        }
    }
    #[test]
    fn api_worktree_result_tracks_preparation_resume_early_failure_and_cleanup() {
        for resumed in [false, true] {
            let tmp = tempfile::tempdir().unwrap();
            let paths = ApiPaths::at_root(tmp.path());
            let writer = CommandStateWriter::new(
                paths,
                "ses_21d6f40cfb511982e4424e0e250a9557",
                "cmd_1dccadfed7bcbb036c56a4afb97e906f",
            );
            let wt = tmp.path().join("worktree");
            std::fs::create_dir(&wt).unwrap();
            let prepared_writer = writer.clone();
            prepared_writer.record_worktree(&wt);
            if resumed {
                prepared_writer.record_worktree(&wt);
            }
            // No finalization/kept callback is required. A setup error right
            // after preparation still exposes the checkout through result.
            let result = Err(crate::command::error::CommandError::Other(
                "setup failed".into(),
            ));
            let completion = QueuedCommandResult::new(&result, &Ok(PushBackOutcome::NotConfigured))
                .with_worktree(writer.kept_worktree());
            let json: CommandResult = serde_json::from_str(&completion.result_json).unwrap();
            assert_eq!(json.worktree_path, Some(wt.display().to_string()));
            assert_eq!(completion.status.as_str(), "error");
            // Successful squash cleanup is reflected by the existence check,
            // even when no explicit discard callback follows merge-back.
            std::fs::remove_dir(&wt).unwrap();
            let completion = QueuedCommandResult::new(
                &Ok(CommandOutcome::Empty),
                &Ok(PushBackOutcome::NotConfigured),
            )
            .with_worktree(writer.kept_worktree());
            let json: serde_json::Value = serde_json::from_str(&completion.result_json).unwrap();
            assert!(json.get("worktree_path").is_none());
        }
    }
}

#[cfg(test)]
mod adversarial_tests {
    use super::*;
    use crate::command::error::CommandError;
    use crate::data::execution_event::ExecutionEvent;

    #[test]
    fn reconciliation_preserves_completed_legacy_history_and_finish_time() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = ApiPaths::from_root(tmp.path());
        let store = SqliteSessionStore::open(tmp.path()).unwrap();
        store
            .insert_session("ses_03c7c0ace395d80182db07ae2c30f034", "/work", "then")
            .unwrap();
        store
            .enqueue_command(
                "cmd_4a8a08f09d37b73795649038408b5f33",
                "ses_03c7c0ace395d80182db07ae2c30f034",
                "exec prompt",
                "[]",
                "log",
            )
            .unwrap();
        store
            .complete_command(
                "cmd_4a8a08f09d37b73795649038408b5f33",
                CommandStatus::Done,
                Some(0),
                Some(r#"{"exit_code":0}"#),
            )
            .unwrap();
        paths
            .prepare_command_dir(
                "ses_03c7c0ace395d80182db07ae2c30f034",
                "cmd_4a8a08f09d37b73795649038408b5f33",
            )
            .unwrap();
        let done = ExecutionEvent {
            timestamp: chrono::Utc::now(),
            attempt: 1,
            sequence: 1,
            source: None,
            payload: EventPayload::Done,
        };
        let text = format!("{}\n", serde_json::to_string(&done).unwrap());
        std::fs::write(
            paths.command_events_log_path(
                "ses_03c7c0ace395d80182db07ae2c30f034",
                "cmd_4a8a08f09d37b73795649038408b5f33",
            ),
            &text,
        )
        .unwrap();
        let before = store
            .get_command("cmd_4a8a08f09d37b73795649038408b5f33")
            .unwrap()
            .unwrap();
        finalize_unowned_command(&store, &paths, "cmd_4a8a08f09d37b73795649038408b5f33").unwrap();
        assert_eq!(
            store
                .get_command("cmd_4a8a08f09d37b73795649038408b5f33")
                .unwrap()
                .unwrap(),
            before
        );
        assert_eq!(
            paths
                .read_command_events_raw(
                    "ses_03c7c0ace395d80182db07ae2c30f034",
                    "cmd_4a8a08f09d37b73795649038408b5f33"
                )
                .unwrap(),
            text
        );
    }

    #[test]
    fn prelaunch_internal_errors_are_preflight_but_launched_errors_are_step_failures() {
        use crate::engine::error::EngineError;
        let error = Err(CommandError::Engine(
            EngineError::ContainerRuntimeUnavailable {
                binary: "docker".into(),
            },
        ));
        for started in [false, true] {
            let completion = QueuedCommandResult::build(
                &error,
                &Ok(PushBackOutcome::NotConfigured),
                ResultContributions {
                    container_started: started,
                    ..Default::default()
                },
            );
            let result = read_command_result(&completion.result_json).unwrap();
            assert_eq!(
                result.failure.as_ref().unwrap().stage,
                if started { "step" } else { "preflight" }
            );
            // The legacy `stage` keeps its pre-0137 meaning.
            assert_eq!(result.stage, None);
        }
    }

    #[test]
    fn workflow_outcomes_do_not_add_catalogue_json_and_nonexec_failures_use_their_own_code() {
        let workflow = CommandOutcome::ExecWorkflow(
            crate::command::commands::exec_workflow::ExecWorkflowOutcome {
                workflow: "w".into(),
                exit_code: Some(0),
                worktree_used: false,
                prerequisites: None,
            },
        );
        assert!(catalogue_json(&workflow).is_none());
        let ready = crate::command::commands::ready::ReadyOutcome::from(
            crate::data::ready_summary::ReadySummary::new("docker"),
        );
        assert!(catalogue_json(&CommandOutcome::Ready(ready.clone())).is_none());
        let ready = CommandOutcome::Ready(ready);
        let completion = QueuedCommandResult::new(&Ok(ready), &Ok(PushBackOutcome::NotConfigured));
        let result = read_command_result(&completion.result_json).unwrap();
        assert_eq!(result.failure.as_ref().unwrap().code, "command_failed");
        assert_eq!(
            result.error.as_deref(),
            Some(result.failure.as_ref().unwrap().message.as_str())
        );
        assert_eq!(result.stage, None);
    }

    #[tokio::test]
    async fn persistence_failure_still_publishes_the_in_memory_terminal_pair() {
        let tmp = tempfile::tempdir().unwrap();
        let store = SqliteSessionStore::open(tmp.path()).unwrap();
        store
            .insert_session("ses_03c7c0ace395d80182db07ae2c30f034", "/work", "then")
            .unwrap();
        store
            .enqueue_command(
                "cmd_4a8a08f09d37b73795649038408b5f33",
                "ses_03c7c0ace395d80182db07ae2c30f034",
                "exec prompt",
                "[]",
                "log",
            )
            .unwrap();
        let command = store
            .get_command("cmd_4a8a08f09d37b73795649038408b5f33")
            .unwrap()
            .unwrap();
        let connection = rusqlite::Connection::open(tmp.path().join("awman.db")).unwrap();
        connection.execute_batch("CREATE TRIGGER refuse_completion BEFORE UPDATE ON commands BEGIN SELECT RAISE(FAIL, 'injected'); END;").unwrap();
        let completion = QueuedCommandResult::new(
            &Ok(CommandOutcome::Empty),
            &Ok(PushBackOutcome::NotConfigured),
        );
        let bus = EventBus::new(8);
        let mut events = bus.subscribe();
        completion
            .publish(
                &store,
                &ApiPaths::from_root(tmp.path()),
                &command,
                &bus.sender(),
            )
            .await;
        assert!(
            matches!(events.recv().await.unwrap().payload, EventPayload::CommandStatus { result: Some(result), .. } if result.outcome == Some(CommandStatusKind::Done))
        );
        assert!(matches!(
            events.recv().await.unwrap().payload,
            EventPayload::Done
        ));
        assert!(events.try_recv().is_err());
    }
}

#[cfg(test)]
mod sidecar_tests {
    use super::*;
    use crate::command::commands::api_server::cancel::{
        CancelOutcome, CommandCanceller, TokenRegistry,
    };
    use crate::command::commands::api_server::runtime::WorkerServices;
    use crate::command::commands::api_server::WorkerConfig;
    use crate::command::error::CommandError;
    use crate::data::config::env::EnvSnapshot;
    use crate::data::execution_event::ExecutionEvent;
    use crate::data::fs::data_paths::AWMAN_HOME;

    const REFUSE: &str = "CREATE TRIGGER refuse_completion BEFORE UPDATE ON commands BEGIN SELECT RAISE(FAIL, 'injected'); END;";

    struct Fixture {
        tmp: tempfile::TempDir,
        store: Arc<SqliteSessionStore>,
        paths: ApiPaths,
        side_effects: std::sync::atomic::AtomicUsize,
    }

    impl Fixture {
        fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            // Separate API and data roots, as `serve` resolves them: bootstrap
            // treats a database under the API root as a legacy one to migrate.
            let paths = ApiPaths::from_env(&EnvSnapshot::with_overrides([(
                AWMAN_HOME,
                tmp.path().to_str().unwrap(),
            )]))
            .unwrap();
            paths.data_paths().ensure_root().unwrap();
            let store = Arc::new(SqliteSessionStore::open_from_paths(&paths).unwrap());
            store
                .insert_session("ses_03c7c0ace395d80182db07ae2c30f034", "/missing", "then")
                .unwrap();
            store
                .enqueue_command(
                    "cmd_4a8a08f09d37b73795649038408b5f33",
                    "ses_03c7c0ace395d80182db07ae2c30f034",
                    "exec prompt",
                    "[]",
                    "log",
                )
                .unwrap();
            Self {
                tmp,
                store,
                paths,
                side_effects: Default::default(),
            }
        }

        fn sql(&self, sql: &str) {
            rusqlite::Connection::open(self.paths.db_path())
                .unwrap()
                .execute_batch(sql)
                .unwrap();
        }

        fn claim(&self) -> CommandRecord {
            self.store.claim_next_command("worker").unwrap().unwrap()
        }

        /// One worker cycle of a command with an observable side effect, with
        /// the worker's log writer attached.
        async fn run_fake_command(&self, completion: &QueuedCommandResult, refuse_row: bool) {
            let command = self.claim();
            if refuse_row {
                self.sql(REFUSE);
            }
            self.side_effects
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let bus = Arc::new(EventBus::for_attempt(16, command.attempt));
            let buses = Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::from([
                ("cmd_4a8a08f09d37b73795649038408b5f33".into(), bus.clone()),
            ])));
            let mut stream = super::super::command_log_stream::CommandLogStream::open(
                self.store.clone(),
                self.paths.clone(),
                buses.clone(),
                command.clone(),
                None,
                None,
                Duration::from_millis(5),
            )
            .await;
            let mut log_rx = bus.subscribe_log();
            self.paths
                .prepare_command_dir(
                    "ses_03c7c0ace395d80182db07ae2c30f034",
                    "cmd_4a8a08f09d37b73795649038408b5f33",
                )
                .unwrap();
            let mut writer = CommandLogWriter::open_append(
                &self.paths,
                "ses_03c7c0ace395d80182db07ae2c30f034",
                "cmd_4a8a08f09d37b73795649038408b5f33",
            )
            .await
            .unwrap();
            completion
                .publish(&self.store, &self.paths, &command, &bus.sender())
                .await;
            while let Some(event) = log_rx.recv().await {
                writer.write_event(&event).await;
                if matches!(event.payload, EventPayload::Done) {
                    writer.flush().await;
                    break;
                }
            }
            // Exercise the actual consumer, with the worker's SQL failure still
            // installed. It must end without waiting for restart reconciliation.
            let terminal_cursor = tokio::time::timeout(Duration::from_secs(2), async {
                let mut terminal = None;
                let mut dones = 0;
                while let Some(event) = stream.next().await {
                    match event.payload {
                        EventPayload::CommandStatus { .. } => {
                            let mut expected = command.clone();
                            expected.status = completion.status;
                            expected.exit_code = Some(completion.exit_code);
                            expected.result = Some(completion.result_json.clone());
                            assert_eq!(
                                serde_json::to_value(&event.payload).unwrap(),
                                serde_json::to_value(terminal_payload(&expected)).unwrap()
                            );
                            terminal = Some(event.cursor());
                        }
                        EventPayload::Done => dones += 1,
                        _ => {}
                    }
                }
                assert_eq!(dones, 1);
                terminal.expect("terminal result delivered")
            })
            .await
            .expect("live completion stream ends");
            buses.lock().await.clear();
            let mut resumed = super::super::command_log_stream::CommandLogStream::open(
                self.store.clone(),
                self.paths.clone(),
                buses,
                self.row(),
                Some(terminal_cursor),
                None,
                Duration::from_millis(5),
            )
            .await;
            tokio::time::timeout(Duration::from_secs(2), async {
                let mut dones = 0;
                while let Some(event) = resumed.next().await {
                    assert!(
                        !matches!(event.payload, EventPayload::CommandStatus { .. }),
                        "resume must not duplicate the result"
                    );
                    if matches!(event.payload, EventPayload::Done) {
                        dones += 1;
                    }
                }
                assert_eq!(dones, 1);
            })
            .await
            .expect("reconnect ends from durable completion");
        }

        /// A real daemon restart, through the same bootstrap `serve` runs.
        fn restart(&self) -> Result<WorkerServices, CommandError> {
            let mut engines = Engines::for_tests(self.tmp.path());
            engines.auth_engine = Arc::new(crate::engine::auth::AuthEngine::with_paths(
                crate::data::fs::auth_paths::AuthPathResolver::at_home(self.tmp.path()),
                self.paths.clone(),
            ));
            WorkerServices::bootstrap(
                WorkerConfig {
                    port: 0,
                    bind_ip: std::net::IpAddr::from([127, 0, 0, 1]),
                    workdirs: vec![],
                    workdir_roots: vec![],
                    env: crate::data::config::env::EnvSnapshot::empty(),
                },
                engines,
            )
        }

        fn row(&self) -> CommandRecord {
            self.store
                .get_command("cmd_4a8a08f09d37b73795649038408b5f33")
                .unwrap()
                .unwrap()
        }

        fn sidecar_exists(&self) -> bool {
            self.paths
                .command_completion_path(
                    "ses_03c7c0ace395d80182db07ae2c30f034",
                    "cmd_4a8a08f09d37b73795649038408b5f33",
                )
                .exists()
        }

        fn terminal_events(&self) -> Vec<ExecutionEvent> {
            self.paths
                .read_command_events(
                    "ses_03c7c0ace395d80182db07ae2c30f034",
                    "cmd_4a8a08f09d37b73795649038408b5f33",
                )
                .into_iter()
                .filter(|event| {
                    matches!(
                        event.payload,
                        EventPayload::CommandStatus { .. } | EventPayload::Done
                    )
                })
                .collect()
        }

        fn write_sidecar(&self, attempt: u32, completion: &QueuedCommandResult) {
            self.paths
                .write_command_completion(
                    "ses_03c7c0ace395d80182db07ae2c30f034",
                    "cmd_4a8a08f09d37b73795649038408b5f33",
                    &completion.sidecar(attempt),
                )
                .unwrap();
        }
    }

    fn completed() -> QueuedCommandResult {
        QueuedCommandResult::new(
            &Ok(CommandOutcome::Empty),
            &Ok(PushBackOutcome::NotConfigured),
        )
        .map_result(|r| r.outputs = Some(Default::default()))
    }

    fn assert_one_terminal_pair(fixture: &Fixture, completion: &QueuedCommandResult) {
        let events = fixture.terminal_events();
        assert_eq!(events.len(), 2, "{events:?}");
        let EventPayload::CommandStatus { exit_code, .. } = &events[0].payload else {
            panic!("{events:?}");
        };
        assert_eq!(*exit_code, Some(completion.exit_code));
        assert!(matches!(events[1].payload, EventPayload::Done));
    }

    #[tokio::test]
    async fn failed_completion_write_finalizes_on_restart_without_redispatch() {
        // Success, a real failure exit code, and a partial failure (push) all
        // come back exactly as the worker computed them.
        for completion in [
            completed(),
            QueuedCommandResult::new(
                &Err(CommandError::MissingRequiredArgument {
                    command: vec!["prompt".into()],
                    argument: "prompt".into(),
                }),
                &Ok(PushBackOutcome::NotConfigured),
            )
            .map_result(|r| r.outputs = Some(Default::default())),
            QueuedCommandResult::new(
                &Ok(CommandOutcome::Empty),
                &Err(PushBackError {
                    stage: "push",
                    message: "rejected".into(),
                }),
            )
            .map_result(|r| r.outputs = Some(Default::default())),
        ] {
            let fixture = Fixture::new();
            fixture.run_fake_command(&completion, true).await;
            assert_eq!(fixture.row().status.as_str(), "running");
            let recorded = fixture
                .paths
                .read_command_completion(
                    "ses_03c7c0ace395d80182db07ae2c30f034",
                    "cmd_4a8a08f09d37b73795649038408b5f33",
                )
                .unwrap()
                .expect("sidecar survives a failed row write");
            let live_events = fixture.paths.read_command_events_raw(
                "ses_03c7c0ace395d80182db07ae2c30f034",
                "cmd_4a8a08f09d37b73795649038408b5f33",
            );
            assert_one_terminal_pair(&fixture, &completion);

            fixture.sql("DROP TRIGGER refuse_completion;");
            let runtime = fixture.restart().unwrap();
            let row = fixture.row();
            assert_eq!(row.status, completion.status);
            assert_eq!(row.exit_code, Some(completion.exit_code));
            assert_eq!(row.result.as_deref(), Some(completion.result_json.as_str()));
            assert_eq!(row.finished_at, Some(recorded.finished_at));
            assert_eq!(row.attempt, 1);
            assert!(!fixture.sidecar_exists());
            assert!(runtime.store.claim_next_command("next").unwrap().is_none());
            assert_eq!(
                fixture
                    .side_effects
                    .load(std::sync::atomic::Ordering::SeqCst),
                1
            );
            // The terminal event clients already saw is the only one logged.
            assert_eq!(
                fixture.paths.read_command_events_raw(
                    "ses_03c7c0ace395d80182db07ae2c30f034",
                    "cmd_4a8a08f09d37b73795649038408b5f33"
                ),
                live_events
            );
        }
    }

    #[tokio::test]
    async fn crash_between_sidecar_and_row_finalizes_exactly_once() {
        let fixture = Fixture::new();
        let command = fixture.claim();
        let completion = completed();
        fixture.write_sidecar(command.attempt, &completion);

        fixture.restart().unwrap();
        let row = fixture.row();
        assert_eq!(row.status.as_str(), "done");
        assert!(!fixture.sidecar_exists());
        assert_one_terminal_pair(&fixture, &completion);
        let events = fixture.paths.read_command_events_raw(
            "ses_03c7c0ace395d80182db07ae2c30f034",
            "cmd_4a8a08f09d37b73795649038408b5f33",
        );

        // A second restart has nothing left to do.
        fixture.restart().unwrap();
        assert_eq!(fixture.row(), row);
        assert_eq!(
            fixture.paths.read_command_events_raw(
                "ses_03c7c0ace395d80182db07ae2c30f034",
                "cmd_4a8a08f09d37b73795649038408b5f33"
            ),
            events
        );
    }

    #[test]
    fn crash_before_completion_still_requeues() {
        let fixture = Fixture::new();
        fixture.claim();
        std::thread::sleep(Duration::from_millis(2));
        assert_eq!(
            finalize_completed_from_sidecars(&fixture.store, &fixture.paths).unwrap(),
            0
        );
        assert_eq!(
            fixture.store.recover_stale_commands(0).unwrap(),
            ["cmd_4a8a08f09d37b73795649038408b5f33"]
        );
        let row = fixture.row();
        assert_eq!((row.status.as_str(), row.attempt), ("queued", 2));
    }

    #[test]
    fn sidecar_from_another_attempt_is_removed_and_the_row_requeues() {
        let fixture = Fixture::new();
        fixture.claim();
        fixture.write_sidecar(7, &completed());
        std::thread::sleep(Duration::from_millis(2));
        assert_eq!(
            finalize_completed_from_sidecars(&fixture.store, &fixture.paths).unwrap(),
            0
        );
        assert!(!fixture.sidecar_exists());
        assert_eq!(
            fixture.store.recover_stale_commands(0).unwrap(),
            ["cmd_4a8a08f09d37b73795649038408b5f33"]
        );
    }

    #[test]
    fn malformed_sidecar_refuses_startup_and_never_requeues() {
        let fixture = Fixture::new();
        fixture.claim();
        fixture
            .paths
            .prepare_command_dir(
                "ses_03c7c0ace395d80182db07ae2c30f034",
                "cmd_4a8a08f09d37b73795649038408b5f33",
            )
            .unwrap();
        std::fs::write(
            fixture.paths.command_completion_path(
                "ses_03c7c0ace395d80182db07ae2c30f034",
                "cmd_4a8a08f09d37b73795649038408b5f33",
            ),
            "{",
        )
        .unwrap();
        let error = fixture.restart().err().expect("startup must refuse");
        assert!(
            error.to_string().contains("completion.json"),
            "names the file: {error}"
        );
        assert_eq!(fixture.row().status.as_str(), "running");
        assert_eq!(fixture.row().attempt, 1);
        assert!(fixture.sidecar_exists());
        assert!(fixture.store.claim_next_command("next").unwrap().is_none());

        // The named repair: remove the evidence to allow the command to rerun.
        fixture
            .paths
            .remove_command_completion(
                "ses_03c7c0ace395d80182db07ae2c30f034",
                "cmd_4a8a08f09d37b73795649038408b5f33",
            )
            .unwrap();
        assert_eq!(
            finalize_completed_from_sidecars(&fixture.store, &fixture.paths).unwrap(),
            0
        );
    }

    #[cfg(unix)]
    #[test]
    fn inaccessible_sidecar_refuses_startup_until_its_access_is_restored() {
        use std::os::unix::fs::PermissionsExt;
        let fixture = Fixture::new();
        let command = fixture.claim();
        fixture.write_sidecar(command.attempt, &completed());
        let path = fixture.paths.command_completion_path(
            "ses_03c7c0ace395d80182db07ae2c30f034",
            "cmd_4a8a08f09d37b73795649038408b5f33",
        );
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read(&path).is_ok() {
            return; // Running as root: the mode cannot deny access.
        }
        assert!(fixture.restart().is_err());
        assert_eq!(fixture.row().status.as_str(), "running");

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let runtime = fixture.restart().unwrap();
        assert_eq!(fixture.row().status.as_str(), "done");
        assert!(runtime.store.claim_next_command("next").unwrap().is_none());
    }

    #[test]
    fn one_unreadable_sidecar_does_not_stop_the_others_from_finalizing() {
        let fixture = Fixture::new();
        fixture.claim();
        fixture
            .paths
            .prepare_command_dir(
                "ses_03c7c0ace395d80182db07ae2c30f034",
                "cmd_4a8a08f09d37b73795649038408b5f33",
            )
            .unwrap();
        std::fs::write(
            fixture.paths.command_completion_path(
                "ses_03c7c0ace395d80182db07ae2c30f034",
                "cmd_4a8a08f09d37b73795649038408b5f33",
            ),
            "{",
        )
        .unwrap();
        fixture
            .store
            .insert_session("ses_fac989447cad2edbc89fbcba70003b36", "/missing", "then")
            .unwrap();
        fixture
            .store
            .enqueue_command(
                "cmd_755f85c2723bb39381c7379a604160d8",
                "ses_fac989447cad2edbc89fbcba70003b36",
                "exec prompt",
                "[]",
                "log",
            )
            .unwrap();
        let good = fixture.store.claim_next_command("worker").unwrap().unwrap();
        fixture
            .paths
            .prepare_command_dir(
                "ses_fac989447cad2edbc89fbcba70003b36",
                "cmd_755f85c2723bb39381c7379a604160d8",
            )
            .unwrap();
        fixture
            .paths
            .write_command_completion(
                "ses_fac989447cad2edbc89fbcba70003b36",
                "cmd_755f85c2723bb39381c7379a604160d8",
                &completed().sidecar(good.attempt),
            )
            .unwrap();
        assert!(finalize_completed_from_sidecars(&fixture.store, &fixture.paths).is_err());
        let good = fixture
            .store
            .get_command("cmd_755f85c2723bb39381c7379a604160d8")
            .unwrap()
            .unwrap();
        assert_eq!(good.status.as_str(), "done");
        assert_eq!(fixture.row().status.as_str(), "running");
    }

    fn unowned_canceller(fixture: &Fixture) -> CommandCanceller {
        CommandCanceller::new(
            fixture.store.clone(),
            CommandLifecycleStore::open(fixture.store.clone()).unwrap(),
            Arc::new(TokenRegistry::new()),
            Arc::new(TokenRegistry::new()),
        )
        .with_completion_paths(fixture.paths.clone())
    }

    #[tokio::test]
    async fn cancel_does_not_change_an_already_published_completion() {
        let fixture = Fixture::new();
        let completion = completed();
        fixture.run_fake_command(&completion, true).await;
        fixture.sql("DROP TRIGGER refuse_completion;");
        let events = fixture.paths.read_command_events_raw(
            "ses_03c7c0ace395d80182db07ae2c30f034",
            "cmd_4a8a08f09d37b73795649038408b5f33",
        );
        let recorded = fixture
            .paths
            .read_command_completion(
                "ses_03c7c0ace395d80182db07ae2c30f034",
                "cmd_4a8a08f09d37b73795649038408b5f33",
            )
            .unwrap()
            .unwrap();

        // The worker returned and unregistered; its row write never landed.
        let outcome = unowned_canceller(&fixture)
            .cancel("cmd_4a8a08f09d37b73795649038408b5f33")
            .unwrap();
        let CancelOutcome::AlreadyTerminal(row) = outcome else {
            panic!("a published completion cannot be cancelled: {outcome:?}");
        };
        assert_eq!((row.status.as_str(), row.exit_code), ("done", Some(0)));
        assert_eq!(row.finished_at, Some(recorded.finished_at));
        assert!(!fixture.sidecar_exists());

        // A repeat cancel and a restart both leave what clients were told.
        let row = fixture.row();
        assert!(matches!(
            unowned_canceller(&fixture)
                .cancel("cmd_4a8a08f09d37b73795649038408b5f33")
                .unwrap(),
            CancelOutcome::AlreadyTerminal(_)
        ));
        let runtime = fixture.restart().unwrap();
        assert_eq!(fixture.row(), row);
        assert_eq!(
            fixture.paths.read_command_events_raw(
                "ses_03c7c0ace395d80182db07ae2c30f034",
                "cmd_4a8a08f09d37b73795649038408b5f33"
            ),
            events
        );
        assert!(runtime.store.claim_next_command("next").unwrap().is_none());
    }

    #[test]
    fn cancel_refuses_to_overwrite_an_unreadable_completion() {
        let fixture = Fixture::new();
        fixture.claim();
        fixture
            .paths
            .prepare_command_dir(
                "ses_03c7c0ace395d80182db07ae2c30f034",
                "cmd_4a8a08f09d37b73795649038408b5f33",
            )
            .unwrap();
        std::fs::write(
            fixture.paths.command_completion_path(
                "ses_03c7c0ace395d80182db07ae2c30f034",
                "cmd_4a8a08f09d37b73795649038408b5f33",
            ),
            "{",
        )
        .unwrap();
        assert!(unowned_canceller(&fixture)
            .cancel("cmd_4a8a08f09d37b73795649038408b5f33")
            .is_err());
        assert_eq!(fixture.row().status.as_str(), "running");
        assert!(fixture.sidecar_exists());
    }

    #[test]
    fn cancel_of_a_row_with_another_attempts_sidecar_still_cancels() {
        let fixture = Fixture::new();
        fixture.claim();
        fixture.write_sidecar(7, &completed());
        assert!(matches!(
            unowned_canceller(&fixture)
                .cancel("cmd_4a8a08f09d37b73795649038408b5f33")
                .unwrap(),
            CancelOutcome::Cancelled(_)
        ));
        assert_eq!(fixture.row().status.as_str(), "cancelled");
        assert!(!fixture.sidecar_exists());
    }

    #[test]
    fn sidecar_of_a_terminal_row_is_removed_without_changing_the_row() {
        let fixture = Fixture::new();
        let command = fixture.claim();
        fixture.write_sidecar(command.attempt, &completed());
        fixture
            .store
            .complete_command(
                "cmd_4a8a08f09d37b73795649038408b5f33",
                CommandStatus::Cancelled,
                Some(130),
                Some("{}"),
            )
            .unwrap();
        let before = fixture.row();
        fixture.restart().unwrap();
        assert_eq!(fixture.row(), before);
        assert!(!fixture.sidecar_exists());
    }

    #[test]
    fn failed_recovery_write_refuses_startup_rather_than_requeueing() {
        let fixture = Fixture::new();
        let command = fixture.claim();
        fixture.write_sidecar(command.attempt, &completed());
        fixture.sql(REFUSE);
        assert!(fixture.restart().is_err());
        assert_eq!(fixture.row().status.as_str(), "running");
        assert!(fixture.sidecar_exists());

        fixture.sql("DROP TRIGGER refuse_completion;");
        fixture.restart().unwrap();
        assert_eq!(fixture.row().status.as_str(), "done");
        assert!(!fixture.sidecar_exists());
    }

    #[test]
    fn recorded_completion_outranks_a_cancel_accepted_after_it() {
        let fixture = Fixture::new();
        let command = fixture.claim();
        fixture.write_sidecar(command.attempt, &completed());
        let lifecycle = CommandLifecycleStore::open(fixture.store.clone()).unwrap();
        lifecycle
            .request_cancel(
                &crate::data::ids::CommandId::parse("cmd_4a8a08f09d37b73795649038408b5f33")
                    .unwrap(),
                "later",
            )
            .unwrap();
        fixture.restart().unwrap();
        let row = fixture.row();
        assert_eq!((row.status.as_str(), row.exit_code), ("done", Some(0)));
        assert_one_terminal_pair(&fixture, &completed());
    }

    #[test]
    fn restart_finishes_closing_a_session_drained_by_its_sidecar() {
        let fixture = Fixture::new();
        let command = fixture.claim();
        fixture.write_sidecar(command.attempt, &completed());
        fixture
            .store
            .update_session_status(
                "ses_03c7c0ace395d80182db07ae2c30f034",
                SessionStatus::Closing,
            )
            .unwrap();
        fixture.restart().unwrap();
        assert_eq!(fixture.row().status.as_str(), "done");
        assert_eq!(
            fixture
                .store
                .get_session("ses_03c7c0ace395d80182db07ae2c30f034")
                .unwrap()
                .unwrap()
                .status
                .as_str(),
            "closed"
        );
    }

    fn session_status(fixture: &Fixture) -> String {
        fixture
            .store
            .get_session("ses_03c7c0ace395d80182db07ae2c30f034")
            .unwrap()
            .unwrap()
            .status
            .as_str()
            .to_string()
    }

    #[test]
    fn restart_closes_a_session_whose_row_was_saved_before_the_crash() {
        // Crash after the row, before the worker's post-check; the sidecar is
        // either still there or already removed.
        for keep_sidecar in [true, false] {
            let fixture = Fixture::new();
            let command = fixture.claim();
            fixture
                .store
                .update_session_status(
                    "ses_03c7c0ace395d80182db07ae2c30f034",
                    SessionStatus::Closing,
                )
                .unwrap();
            if keep_sidecar {
                fixture.write_sidecar(command.attempt, &completed());
            }
            fixture
                .store
                .complete_command(
                    "cmd_4a8a08f09d37b73795649038408b5f33",
                    CommandStatus::Done,
                    Some(0),
                    Some("{}"),
                )
                .unwrap();
            fixture.restart().unwrap();
            assert_eq!(session_status(&fixture), "closed", "{keep_sidecar}");
            assert!(!fixture.sidecar_exists());
        }
    }

    #[test]
    fn restart_keeps_a_session_closing_while_it_still_has_work() {
        // A queued command drains through a worker, whose post-check closes.
        let fixture = Fixture::new();
        fixture
            .store
            .update_session_status(
                "ses_03c7c0ace395d80182db07ae2c30f034",
                SessionStatus::Closing,
            )
            .unwrap();
        fixture.restart().unwrap();
        assert_eq!(fixture.row().status.as_str(), "queued");
        assert_eq!(session_status(&fixture), "closing");
    }

    #[test]
    fn restart_keeps_a_session_closing_until_cancelled_cleanup_finishes() {
        let fixture = Fixture::new();
        fixture.claim();
        fixture
            .store
            .update_session_status(
                "ses_03c7c0ace395d80182db07ae2c30f034",
                SessionStatus::Closing,
            )
            .unwrap();
        fixture
            .paths
            .prepare_command_dir(
                "ses_03c7c0ace395d80182db07ae2c30f034",
                "cmd_4a8a08f09d37b73795649038408b5f33",
            )
            .unwrap();
        let cleanup = crate::data::workflow_cleanup::WorkflowCleanup {
            workflow: crate::data::workflow_definition::Workflow {
                title: None,
                steps: vec![],
                agent: None,
                model: None,
                overlays: None,
                setup: vec![],
                teardown: vec![],
                teardown_on_failure: false,
            },
            work_item: None,
            workdir: fixture.tmp.path().to_path_buf(),
            mount_path: fixture.tmp.path().to_path_buf(),
            state_root: None,
            image: "test".into(),
            cli_overlays: vec![],
            worktree_git_mount: None,
        };
        fixture
            .paths
            .write_command_cleanup(
                "ses_03c7c0ace395d80182db07ae2c30f034",
                "cmd_4a8a08f09d37b73795649038408b5f33",
                &cleanup,
            )
            .unwrap();
        fixture
            .store
            .complete_command(
                "cmd_4a8a08f09d37b73795649038408b5f33",
                CommandStatus::Cancelled,
                Some(130),
                Some("{}"),
            )
            .unwrap();
        fixture.restart().unwrap();
        assert_eq!(session_status(&fixture), "closing");
    }

    #[tokio::test]
    async fn successful_publish_removes_the_sidecar() {
        let fixture = Fixture::new();
        fixture.run_fake_command(&completed(), false).await;
        assert_eq!(fixture.row().status.as_str(), "done");
        assert!(!fixture.sidecar_exists());
    }

    #[tokio::test]
    async fn unwritable_sidecar_does_not_stop_the_row_write() {
        let fixture = Fixture::new();
        let command = fixture.claim();
        let directory = fixture
            .paths
            .session_commands_dir("ses_03c7c0ace395d80182db07ae2c30f034");
        std::fs::create_dir_all(directory.parent().unwrap()).unwrap();
        std::fs::write(&directory, "not a directory").unwrap();
        let bus = EventBus::new(8);
        completed()
            .publish(&fixture.store, &fixture.paths, &command, &bus.sender())
            .await;
        assert_eq!(fixture.row().status.as_str(), "done");
    }
}

#[cfg(test)]
mod interaction_tests {
    use super::*;
    use crate::data::config::daemon::{DaemonSettings, Interaction};

    fn row(subcommand: &str, args: &[&str]) -> CommandRecord {
        CommandRecord {
            allow_remote_input: false,
            listener: "local".into(),
            trace_parent: None,
            id: "cmd_00000000000000000000000000000001".into(),
            session_id: "ses_00000000000000000000000000000001".into(),
            subcommand: subcommand.into(),
            args: serde_json::to_string(args).unwrap(),
            status: crate::data::command_status::CommandStatus::Queued,
            exit_code: None,
            started_at: None,
            finished_at: None,
            log_path: String::new(),
            worker_id: None,
            queued_at: None,
            result: None,
            attempt: 1,
        }
    }

    #[test]
    fn a_claimed_command_runs_as_admitted_at_the_live_terminal_size() {
        let mut settings = DaemonSettings::from_merged(&Default::default());
        settings.terminal.cols = 90;
        settings.terminal.rows = 30;
        let interactive =
            push_run_interaction(&row("exec prompt", &["--interactive", "hi"]), &settings);
        assert_eq!(interactive.interaction, Interaction::Interactive);
        assert_eq!(interactive.pty_size, Some((90, 30)));
        // Rows written before WI 0141 carried `--headless=false`.
        let legacy =
            push_run_interaction(&row("exec prompt", &["--headless=false", "hi"]), &settings);
        assert_eq!(legacy.pty_size, Some((90, 30)));
        let headless = push_run_interaction(&row("exec prompt", &["--headless", "hi"]), &settings);
        assert_eq!(headless.pty_size, None);
        // No recorded choice: push's configured default, never a refusal,
        // even when overrides have since been disabled.
        settings.worker.push_interaction = Interaction::Interactive;
        settings.worker.push_allow_override = false;
        let unrecorded = push_run_interaction(&row("exec prompt", &["hi"]), &settings);
        assert_eq!(unrecorded.interaction, Interaction::Interactive);
        let admitted = push_run_interaction(&row("exec prompt", &["--headless", "hi"]), &settings);
        assert_eq!(admitted.interaction, Interaction::Headless);
    }
}
