//! Queue worker — claims commands from the SQLite queue and executes them.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use super::cancel::TokenRegistry;
use super::event_bus::EventBus;
use super::push_back::{
    capture_baseline, publish_after_success, PushBackContext, PushBackError, PushBackOutcome,
    PushEventSink,
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

pub(crate) async fn wait_for_output_drains(event_bus: &EventBus, command_id: &str) {
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

/// Foreground lease hosts use the same result projection as pushed and
/// daemon-pulled commands. Post-command lease publication happens afterwards.
pub fn foreground_result(
    result: &Result<CommandOutcome, crate::command::error::CommandError>,
    contributions: ResultContributions,
) -> CommandResult {
    let projected =
        QueuedCommandResult::build(result, &Ok(PushBackOutcome::NotConfigured), contributions);
    serde_json::from_str(&projected.result_json).expect("typed command result")
}

pub(crate) fn finish_result_fields(
    value: &mut CommandResult,
    outcome: &Result<CommandOutcome, crate::command::error::CommandError>,
    path: &[&str],
    git: Option<crate::data::fs::api_db::GitResult>,
    output_dir: &std::path::Path,
) {
    value.git = git;
    value.outputs = Some(crate::data::fs::command_outputs::summarize_output_dir(
        output_dir,
    ));
    if crate::command::dispatch::catalogue::CommandCatalogue::get()
        .canonical_path(path)
        .join(" ")
        != "exec prompt"
    {
        value.final_message = None;
    }
    if value.data.is_none() {
        if let Ok(outcome) = outcome {
            value.data = catalogue_json(outcome);
        }
    }
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
        if sender.redaction().is_empty() {
            self.publish_redacted(store, paths, command, sender).await;
            return;
        }
        let mut result: CommandResult =
            serde_json::from_str(&self.result_json).expect("typed command result");
        sender.redaction().redact_result(&mut result);
        let redacted = Self {
            exit_code: self.exit_code,
            status: self.status,
            result_json: serde_json::to_string(&result).expect("typed command result"),
        };
        redacted
            .publish_redacted(store, paths, command, sender)
            .await;
    }

    async fn publish_redacted(
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
    /// Hosts with an actual dialog response channel may opt in. A PTY alone
    /// does not make unattended workflow control-board defaults interactive.
    fn supports_workflow_dialogs(&self) -> bool {
        false
    }

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
#[async_trait::async_trait]
pub(crate) trait LeasePostProcess: Send + Sync {
    async fn process(&self, result: CommandResult) -> CommandResult;
}
pub struct QueueWorker<F: ApiCommandFrontendFactory> {
    /// The daemon's one worker share (WI 0143): pushed commands acquire
    /// `JobClass::Push` slots, alternating with pulled leases.
    share: Option<Arc<crate::engine::worker::executor_share::WorkerExecutorShare>>,
    /// Job origins and `outputs_mounted` beside the command rows.
    jobs: Option<crate::data::worker::job_store::JobStore>,
    lease_event_bus: Option<Arc<EventBus>>,
    lease_post_process: Option<Arc<dyn LeasePostProcess>>,
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
    /// Cancel markers and `/v1/worker/runs` linkage (WI 0135).
    pub lifecycle_store: CommandLifecycleStore,
    /// The daemon's command-cancel tokens; this worker registers each
    /// command it claims.
    pub cancels: Arc<TokenRegistry>,
}

impl<F: ApiCommandFrontendFactory> QueueWorker<F> {
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
        let jobs = match crate::data::worker::job_store::JobStore::open(deps.store.clone()) {
            Ok(jobs) => Some(jobs),
            Err(error) => {
                tracing::error!(%error, "failed to open the worker job store");
                None
            }
        };
        Self {
            share: None,
            lease_event_bus: None,
            lease_post_process: None,
            jobs,
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

    /// Execute a pull-origin row already admitted with a Pull permit. It is
    /// inserted running, so the push pool can never steal it.
    pub(crate) async fn execute_lease(
        &self,
        command: CommandRecord,
        cancel: tokio_util::sync::CancellationToken,
    ) {
        self.execute_command(command, cancel).await;
    }
    pub(crate) fn with_lease_post_process(mut self, post: Arc<dyn LeasePostProcess>) -> Self {
        self.lease_post_process = Some(post);
        self
    }
    pub(crate) fn with_lease_events(mut self, bus: Arc<EventBus>) -> Self {
        self.lease_event_bus = Some(bus);
        self
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
        self.share = Some(super::push_job::worker_share(&executor));
        self
    }

    /// Record `cmd` as a push-origin job (unless a lease runner already gave
    /// it a pull origin) and its `outputs_mounted` flag. Best effort: a store
    /// failure is logged and never fails the command.
    fn record_job(
        &self,
        cmd: &CommandRecord,
        args: &[String],
        interaction: crate::data::config::daemon::Interaction,
    ) {
        let Some(jobs) = &self.jobs else {
            return;
        };
        let session = self.store.get_session(&cmd.session_id).ok().flatten();
        let spec = super::push_job::push_job_spec(cmd, session.as_ref(), args, interaction);
        if let Err(error) = super::push_job::record_push_job(jobs, cmd, &spec) {
            tracing::warn!(command_id = %cmd.id, %error, "failed to record the push job");
        }
        if let Err(error) = jobs.set_outputs_mounted(&cmd.id, false) {
            tracing::warn!(command_id = %cmd.id, %error, "failed to record outputs_mounted");
        }
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
            let permit = if let Some(share) = &self.share {
                Some(
                    tokio::select! { biased; _=stop.cancelled()=>return, permit=share.acquire(crate::data::worker::JobClass::Push)=>permit },
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

        let secret_id =
            crate::data::ids::CommandId::parse(&command_id).expect("persisted command id");
        // Cancellation wins the claim race even if queued secrets were retired.
        if let Ok(Some(requested)) = self.lifecycle_store.cancel_requested_at(&secret_id) {
            cancel.cancel();
            let completion = QueuedCommandResult::new(
                &Err(crate::command::error::CommandError::Other(
                    "command cancelled".into(),
                )),
                &Ok(PushBackOutcome::NotConfigured),
            )
            .cancelled(&requested);
            self.finish_without_log(&cmd, completion).await;
            return;
        }
        let command_secrets = match self
            .lifecycle_store
            .secret_names(&secret_id)
            .map_err(crate::command::error::CommandError::Data)
            .and_then(|names| {
                self.engines
                    .ephemeral_secrets
                    .for_execution(&secret_id, names.as_deref())
            }) {
            Ok(secrets) => secrets,
            Err(error) => {
                let completion =
                    QueuedCommandResult::new(&Err(error), &Ok(PushBackOutcome::NotConfigured))
                        .map_result(|r| {
                            if let Some(failure) = &mut r.failure {
                                failure.stage = "preflight".into();
                            }
                        });
                self.finish_without_log(&cmd, completion).await;
                return;
            }
        };

        // WI 0145: a command of a `git_auth` session queued before a restart
        // cannot publish with the caller's credential, and must never fall
        // back to the host's: fail before any setup or container.
        let session_record = match self.store.get_session(&session_id) {
            Ok(Some(record)) => Some(record),
            result => {
                let error = match result {
                    Err(e) => crate::command::error::CommandError::Data(e),
                    _ => crate::command::error::CommandError::Other(
                        "session credential policy unavailable".into(),
                    ),
                };
                self.finish_without_log(
                    &cmd,
                    QueuedCommandResult::new(&Err(error), &Ok(PushBackOutcome::NotConfigured)),
                )
                .await;
                return;
            }
        };
        if let Err(error) = crate::command::session_git_transport::check_session_credentials(
            &self.engines,
            &session_id,
            session_record
                .as_ref()
                .and_then(|row| row.git_auth_kind.as_deref()),
        ) {
            let completion =
                QueuedCommandResult::new(&Err(error), &Ok(PushBackOutcome::NotConfigured));
            self.finish_without_log(&cmd, completion).await;
            return;
        }

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
        let event_bus = self
            .lease_event_bus
            .clone()
            .unwrap_or_else(|| Arc::new(EventBus::for_attempt(4096, cmd.attempt)));

        if let Some(secrets) = &command_secrets {
            for secret in secrets.values() {
                event_bus.redaction().add(secret.clone());
            }
        }
        self.engines
            .ephemeral_secrets
            .register_session_redaction(&session_id, &event_bus.redaction());

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
        let job_interaction = interaction.interaction;
        // Lease prompt bodies are private session input. Materialize them only
        // for catalogue construction, never in the durable row or metadata.
        let mut dispatch_cmd = cmd.clone();
        if cmd.subcommand == "exec prompt" && self.lease_event_bus.is_some() {
            if let Some(session) = self.sessions.get_by_key(&session_id) {
                let session = session.read().await;
                if let Some(path) = session.inline_work_item() {
                    let body = crate::data::fs::private_files::open_private_file(path).and_then(
                        |mut file| {
                            use std::io::Read;
                            let mut body = String::new();
                            file.read_to_string(&mut body).map_err(|e| {
                                crate::data::error::DataError::Io {
                                    path: path.to_path_buf(),
                                    source: e,
                                }
                            })?;
                            Ok(body)
                        },
                    );
                    match body {
                        Ok(body) => {
                            let mut input = args.clone();
                            input.extend(["--".into(), body]);
                            dispatch_cmd.args =
                                serde_json::to_string(&input).expect("string arguments");
                        }
                        Err(error) => {
                            tracing::error!(%error, "could not read private lease prompt")
                        }
                    }
                }
            }
        }
        let frontend = self.frontend_factory.frontend_for(
            &dispatch_cmd,
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

        let context_tracker = crate::command::context::change_tracker::ContextChangeTracker::begin(
            &self.paths,
            &*session.read().await,
            &self.engines.git_engine,
            &session_id,
            &command_id,
            cmd.attempt,
        );
        let mut context_finish = ContextFinishGuard(Some(context_tracker));
        let checkout_root = session.read().await.git_root().to_path_buf();
        let git = Arc::clone(&self.engines.git_engine);
        let root = checkout_root.clone();
        let base_sha = tokio::task::spawn_blocking(move || git.command_base_sha(&root))
            .await
            .ok()
            .flatten();

        // Capture before dispatch: explicit workflow teardown may publish itself.
        // The command's own token bounds the remote head check and push-back
        // (retry backoff included), so a cancel stops them promptly (F1).
        let push_cancel = cancel.clone();
        let baseline = match self.store.get_session(&session_id) {
            Ok(Some(record)) => match self.push_context(&record, &push_cancel) {
                Ok(ctx) => {
                    capture_baseline(
                        (*self.engines.git_engine).clone(),
                        record,
                        ctx,
                        PushEventSink(event_bus.sender()),
                    )
                    .await
                }
                Err(error) => Err(error),
            },
            Ok(None) => Ok(None),
            Err(e) => Err(PushBackError::from(crate::engine::error::EngineError::Git(
                e.to_string(),
            ))),
        };

        // WI 0143: the command is a push-origin job of the worker's one job
        // model, and records whether `/awman/output` is actually mounted.
        self.record_job(&cmd, &args, job_interaction);
        // Dispatch through Layer 2.
        let path_parts: Vec<&str> = subcommand.split_whitespace().collect();
        // Every container the command launches is named after it, so a
        // restart can find and stop what a cancelled command left running.
        let mut engines = self.engines.clone();
        // WI 0145: container `push_branch`/`pull_branch` of a `git_auth`
        // session reach the credential through a per-command socket.
        let container_git_socket = match self.container_git_socket(&session_id) {
            Ok(socket) => socket,
            Err(error) => {
                let completion =
                    QueuedCommandResult::new(&Err(error), &Ok(PushBackOutcome::NotConfigured));
                completion
                    .publish(&self.store, &self.paths, &cmd, &event_bus.sender())
                    .await;
                let _ = log_writer.await;
                self.cleanup_event_bus(&command_id).await;
                self.post_execution_check(&session_id, &command_id).await;
                return;
            }
        };
        engines.ephemeral_secrets = Arc::new(
            engines
                .ephemeral_secrets
                .with_container_git_socket(container_git_socket.clone()),
        );
        engines.runtime = Arc::new(
            super::owned_runtime::CommandOwnedRuntime::new(
                Arc::clone(&engines.runtime),
                &command_id,
            )
            .with_output_writer(state_writer.clone())
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
            .with_command_secrets(command_secrets)
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
                Ok(Some(record)) => match (baseline, self.push_context(&record, &push_cancel)) {
                    (Err(error), _) | (Ok(_), Err(error)) => Err(error),
                    (Ok(baseline), Ok(ctx)) => {
                        publish_after_success(
                            (*self.engines.git_engine).clone(),
                            record,
                            subcommand.clone(),
                            args.clone(),
                            baseline,
                            ctx,
                            PushEventSink(event_bus.sender()),
                        )
                        .await
                    }
                },
                Ok(None) => Ok(PushBackOutcome::NotConfigured),
                Err(error) => Err(PushBackError::from(crate::engine::error::EngineError::Git(
                    format!("failed to read session push configuration: {error}"),
                ))),
            }
        } else {
            Ok(PushBackOutcome::NotConfigured)
        };
        // The command (and its teardown containers) has finished: close its
        // container credential socket and remove the directory.
        drop(container_git_socket);
        if let Err(error) = &push_outcome {
            tracing::error!(command_id = %command_id, stage = error.stage, code = error.code(), "Push-back failed");
        }

        if let Some(jobs) = &self.jobs {
            if let Err(error) = jobs.set_outputs_mounted(
                &command_id,
                state_writer.result_contributions().outputs_mounted == Some(true),
            ) {
                tracing::warn!(%command_id, %error, "failed to record observed output mount");
            }
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
            _ => {
                tracing::warn!(%command_id, "Could not collect command git result");
                None
            }
        };
        completion = completion.map_result(|value| {
            finish_result_fields(
                value,
                &result,
                &path_parts,
                git_result,
                &state_writer.output_dir(),
            );
        });
        // Git collection and output summarization can take time; observe a
        // cancellation accepted during either before publishing the row.
        if let Ok(Some(requested_at)) = self.lifecycle_store.cancel_requested_at(
            &crate::data::ids::CommandId::parse(&command_id)
                .expect("validated persisted resource id"),
        ) {
            completion = completion.cancelled(&requested_at);
        }
        if let Some(post) = &self.lease_post_process {
            let result = read_command_result(&completion.result_json).expect("serialized result");
            completion = QueuedCommandResult::from_result(post.process(result).await);
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
                "Command execution finished with failure"
            );
        }

        if let Err(ref e) = result {
            // Text is logged only by the subscriber after bus redaction.
            tracing::error!(command_id = %command_id, stage = e.stage(), code = e.code(), "Command failed");
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
        context_finish
            .publish(
                &completion,
                &self.store,
                &self.paths,
                &cmd,
                &event_bus.sender(),
            )
            .await;
        let _ = log_writer.await;
        self.cleanup_event_bus(&command_id).await;
        self.post_execution_check(&session_id, &command_id).await;
    }

    /// Bounds of this command's push-back: its cancellation token and its
    /// session's transport (the caller credential through the daemon socket
    /// for a `git_auth` session, else the host's own Git configuration).
    fn push_context(
        &self,
        record: &crate::data::fs::api_db::SessionRecord,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<PushBackContext, PushBackError> {
        Ok(PushBackContext {
            cancel: cancel.clone(),
            deadline: None,
            transport: crate::command::session_git_transport::session_transport(
                &self.engines,
                &record.id,
                record.git_auth_kind.as_deref(),
            )?,
        })
    }

    /// The container credential listener of a `git_auth` session's command
    /// (teardown/setup `push_branch`, `pull_branch`). Lives for the command.
    fn container_git_socket(
        &self,
        session_id: &str,
    ) -> Result<
        Option<Arc<crate::engine::git::credential_socket::CacheSocketListener>>,
        crate::command::error::CommandError,
    > {
        let Some(credential) = self.engines.ephemeral_secrets.session_git_auth(session_id) else {
            return Ok(None);
        };
        match crate::engine::git::credential_socket::CacheSocketListener::start(
            &crate::command::session_git_transport::credential_socket_dir(&self.engines),
            Arc::clone(&credential.spec),
            credential.scope.clone(),
        ) {
            Ok(listener) => Ok(Some(Arc::new(listener))),
            Err(error) => {
                let _ = error;
                Err(crate::command::error::CommandError::Other("session_git_auth_unavailable_in_container: could not start the command credential socket".into()))
            }
        }
    }

    /// Complete a command that failed before its log writer started.
    async fn finish_without_log(&self, cmd: &CommandRecord, completion: QueuedCommandResult) {
        let bus = self
            .lease_event_bus
            .clone()
            .unwrap_or_else(|| Arc::new(EventBus::for_attempt(16, cmd.attempt)));
        let id = crate::data::ids::CommandId::parse(&cmd.id).expect("persisted command id");
        self.engines
            .ephemeral_secrets
            .register_session_redaction(&cmd.session_id, &bus.redaction());
        if let Some(secrets) = self.engines.ephemeral_secrets.get(&id) {
            for secret in secrets.values() {
                bus.redaction().add(secret.clone());
            }
        }
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
        self.engines
            .ephemeral_secrets
            .remove(&crate::data::ids::CommandId::parse(command_id).expect("persisted command id"));
        let buses = Arc::clone(&self.event_buses);
        let cmd_id = command_id.to_string();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            buses.lock().await.remove(&cmd_id);
        });
    }

    /// Finish closing a draining session, and close a `/v1/worker/runs` session
    /// asked to `close_on_finish` once its run command (`command_id`) is
    /// terminal — `done`, `error` and `cancelled` alike. Both go through the
    /// one `ApiSessionLifecycle::close`, so a remote clone is deleted exactly
    /// as `DELETE /v1/worker/sessions/{id}` would.
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
                    // WI 0145: retries are client events, like status lines.
                    | EventPayload::PushRetry { .. }
                    | EventPayload::WorkflowPrerequisites { .. }
                    | EventPayload::WorkflowState(_) => {}
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

/// Every terminal path, including a panic, finalizes the attempt's snapshot.
struct ContextFinishGuard(Option<crate::command::context::change_tracker::ContextChangeTracker>);
impl ContextFinishGuard {
    async fn publish(
        &mut self,
        completion: &QueuedCommandResult,
        store: &SqliteSessionStore,
        paths: &ApiPaths,
        command: &CommandRecord,
        events: &crate::command::commands::api_server::event_bus::EventBusSender,
    ) {
        self.finish();
        completion.publish(store, paths, command, events).await;
    }
    fn finish(&mut self) {
        if let Some(tracker) = self.0.take() {
            tracker.finish();
        }
    }
}
impl Drop for ContextFinishGuard {
    fn drop(&mut self) {
        self.finish();
    }
}

#[cfg(test)]
mod command_secrets_tests {
    use super::*;

    #[tokio::test]
    async fn terminal_publication_freezes_context_before_cleanup_or_later_writes() {
        use crate::data::{
            context::{CommandContextRecord, ContextKey},
            session::{RepoContextBinding, Session, SessionOpenOptions},
        };
        let sandbox = awman_test_support::TestSandbox::for_test(awman_test_support::TestCase::component("command::queue_worker::terminal_publication_freezes_context_before_cleanup_or_later_writes"));
        let env = crate::data::config::env::EnvSnapshot::from_instance_env(
            sandbox
                .env()
                .iter()
                .map(|(k, v)| {
                    (
                        k.to_string_lossy().into_owned(),
                        v.to_string_lossy().into_owned(),
                    )
                })
                .collect(),
            sandbox.work().to_path_buf(),
            vec![],
        );
        let paths = ApiPaths::from_root(sandbox.work().join("api"));
        let store = SqliteSessionStore::open_from_paths(&paths).unwrap();
        let sid = crate::data::ids::WorkerSessionId::new();
        let cid = crate::data::ids::CommandId::new();
        store
            .insert_session(
                sid.as_str(),
                sandbox.work().to_str().unwrap(),
                &chrono::Utc::now().to_rfc3339(),
            )
            .unwrap();
        store
            .enqueue_command(cid.as_str(), sid.as_str(), "exec prompt", "[]", "unused")
            .unwrap();
        let command = store.claim_next_command("worker").unwrap().unwrap();
        let mut session = Session::open_at_directory_root(
            sandbox.work().to_path_buf(),
            SessionOpenOptions {
                env: Some(env.clone()),
                ..Default::default()
            },
        )
        .unwrap();
        let key = ContextKey::parse("terminal").unwrap();
        session.set_repo_context(RepoContextBinding::Keyed(key.clone()));
        let resolver = crate::data::fs::context_dirs::ContextDirResolver::from_env(&env).unwrap();
        let dir = resolver.keyed_repo_dir(&key);
        std::fs::create_dir_all(&dir).unwrap();
        let tracker = crate::command::context::change_tracker::ContextChangeTracker::begin(
            &paths,
            &session,
            &crate::engine::git::GitEngine::with_env(env),
            sid.as_str(),
            cid.as_str(),
            command.attempt,
        );
        let mut guard = ContextFinishGuard(Some(tracker));
        std::fs::write(dir.join("first"), b"first job").unwrap();
        let bus = EventBus::new(32);
        let mut events = bus.subscribe();
        let completion = QueuedCommandResult::from_result(crate::data::fs::api_db::CommandResult {
            outcome: Some(crate::data::execution_event::CommandStatusKind::Done),
            exit_code: Some(0),
            ..Default::default()
        });
        // This is the same boundary execute_command uses, before awaited log
        // draining and session cleanup. Keep the guard alive across the barrier.
        guard
            .publish(&completion, &store, &paths, &command, &bus.sender())
            .await;
        let terminal = events.try_recv().unwrap();
        assert!(matches!(
            terminal.payload,
            EventPayload::CommandStatus { .. }
        ));
        assert!(store
            .get_command(cid.as_str())
            .unwrap()
            .unwrap()
            .status
            .is_terminal());
        let persisted = CommandContextRecord::read_final(&paths, sid.as_str(), cid.as_str())
            .unwrap()
            .unwrap();
        assert!(
            matches!(&persisted, CommandContextRecord::Final { changes, .. } if changes.len() == 1 && changes[0].path == "first")
        );
        std::fs::write(dir.join("next"), b"next job").unwrap();
        drop(guard);
        assert_eq!(
            CommandContextRecord::read_final(&paths, sid.as_str(), cid.as_str()).unwrap(),
            Some(persisted)
        );
    }

    #[tokio::test]
    async fn completion_row_sidecar_and_rebuilt_terminal_events_are_redacted() {
        let sandbox = awman_test_support::TestSandbox::for_test(awman_test_support::TestCase::component("command::queue_worker::completion_row_sidecar_and_rebuilt_terminal_events_are_redacted"));
        let secret = "test-secret-persist";
        sandbox.redact(secret);
        let paths = ApiPaths::from_root(sandbox.work().join("api"));
        let store = SqliteSessionStore::open_from_paths(&paths).unwrap();
        let sid = crate::data::ids::WorkerSessionId::new();
        let cid = crate::data::ids::CommandId::new();
        store
            .insert_session(
                sid.as_str(),
                sandbox.work().to_str().unwrap(),
                &chrono::Utc::now().to_rfc3339(),
            )
            .unwrap();
        paths
            .prepare_command_dir(sid.as_str(), cid.as_str())
            .unwrap();
        store
            .enqueue_command(cid.as_str(), sid.as_str(), "exec prompt", "[]", "unused")
            .unwrap();
        let command = store.claim_next_command("worker").unwrap().unwrap();
        let bus = EventBus::new(16);
        bus.redaction()
            .add(crate::data::secret::SecretString::new(secret));
        let completion = QueuedCommandResult::build(
            &Err(crate::command::error::CommandError::Other(secret.into())),
            &Ok(PushBackOutcome::NotConfigured),
            ResultContributions {
                final_message: Some(secret.into()),
                ..Default::default()
            },
        );
        store.with_conn(|conn| { conn.execute_batch("CREATE TRIGGER fail_completion BEFORE UPDATE ON commands BEGIN SELECT RAISE(FAIL, 'fixture'); END;")?; Ok(()) }).unwrap();
        completion
            .publish(&store, &paths, &command, &bus.sender())
            .await;
        let sidecar = paths
            .read_command_completion(sid.as_str(), cid.as_str())
            .unwrap()
            .unwrap();
        assert!(!sidecar.result_json.contains(secret));
        assert!(sidecar.result_json.contains("***"));
        store
            .with_conn(|conn| {
                conn.execute_batch("DROP TRIGGER fail_completion")?;
                Ok(())
            })
            .unwrap();
        let bus = EventBus::new(16);
        bus.redaction()
            .add(crate::data::secret::SecretString::new(secret));
        completion
            .publish(&store, &paths, &command, &bus.sender())
            .await;
        let row = store.get_command(cid.as_str()).unwrap().unwrap();
        assert!(!row.result.as_ref().unwrap().contains(secret));
        assert!(!serde_json::to_string(&terminal_payload(&row))
            .unwrap()
            .contains(secret));
        CommandLogWriter::append_terminal(&paths, &row, terminal_payload(&row)).unwrap();
        for path in [
            paths.command_events_log_path(sid.as_str(), cid.as_str()),
            paths.command_log_path(sid.as_str(), cid.as_str()),
        ] {
            assert!(!std::fs::read_to_string(path).unwrap().contains(secret));
        }
    }
}
