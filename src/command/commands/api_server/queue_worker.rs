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
use crate::data::command_status::{CommandStatus, CANCELLED_EXIT_CODE};
use crate::data::execution_event::{
    CommandStatusKind, EventPayload, PhaseStatusKind, StepStatusKind, FIRST_ATTEMPT,
};
use crate::data::fs::api_command_log::{CommandLogWriter, CommandStateWriter};
use crate::data::fs::api_db::{CommandRecord, CommandResult, SqliteSessionStore};
use crate::data::fs::api_paths::ApiPaths;
use crate::data::session_manager::SessionManager;

const OUTPUT_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

async fn wait_for_output_drains(event_bus: &EventBus, command_id: &str) {
    if !event_bus.wait_output_drained(OUTPUT_DRAIN_TIMEOUT).await {
        tracing::warn!(
            command_id = %command_id,
            timeout_seconds = OUTPUT_DRAIN_TIMEOUT.as_secs(),
            "output drain did not finish; trailing agent output may be missing"
        );
    }
}

/// The completion projection persisted by the queue worker, shared by the
/// command row, its result JSON and final metadata.
struct QueuedCommandResult {
    exit_code: i32,
    status: &'static str,
    result_json: String,
}
impl QueuedCommandResult {
    fn new(
        result: &Result<CommandOutcome, crate::command::error::CommandError>,
        push_outcome: &Result<PushBackOutcome, PushBackError>,
    ) -> Self {
        let mut exit_code = result.as_ref().map_or_else(
            |error| i32::from(error.exit_code()),
            CommandOutcome::exit_code,
        );
        let mut json = CommandResult {
            worktree_path: match result {
                Err(crate::command::error::CommandError::WorktreeExists { path, .. })
                    if path.is_dir() =>
                {
                    Some(path.display().to_string())
                }
                _ => None,
            },
            exit_code: Some(exit_code),
            error: result.as_ref().err().map(ToString::to_string),
            error_status: result.as_ref().err().map(|error| error.http_status()),
            stage: None,
            pushed: None,
        };
        match push_outcome {
            Ok(PushBackOutcome::NotConfigured) => {}
            Ok(PushBackOutcome::NothingToPush) => json.pushed = Some(None),
            Ok(PushBackOutcome::Pushed(pushed)) => json.pushed = Some(Some(pushed.clone())),
            Err(error) => {
                exit_code = 1;
                json.exit_code = Some(exit_code);
                json.error = Some(error.message.clone());
                json.error_status = Some(500);
                json.stage = Some(error.stage.into());
                json.pushed = Some(None);
            }
        }
        let status = if result.is_ok() && exit_code == 0 {
            "done"
        } else {
            "error"
        };
        Self {
            exit_code,
            status,
            result_json: serde_json::to_string(&json).expect("command result is serializable"),
        }
    }

    fn with_worktree(mut self, path: Option<std::path::PathBuf>) -> Self {
        let mut result: CommandResult =
            serde_json::from_str(&self.result_json).expect("serialized command result");
        result.worktree_path = path
            .map(|path| path.display().to_string())
            .or(result.worktree_path);
        self.result_json = serde_json::to_string(&result).expect("command result is serializable");
        self
    }

    /// Turn this completion into a `cancelled` one: exit 130, `error`
    /// `"cancelled"` and `cancelled_at`. Re-serialised through
    /// `serde_json::Value` so `CommandResult` keeps its shape; the worktree
    /// and push fields survive.
    fn cancelled(mut self, requested_at: &str) -> Self {
        let mut result: serde_json::Value =
            serde_json::from_str(&self.result_json).expect("serialized command result");
        if let Some(fields) = result.as_object_mut() {
            fields.insert("exit_code".into(), CANCELLED_EXIT_CODE.into());
            fields.insert("error".into(), "cancelled".into());
            fields.insert("cancelled_at".into(), requested_at.into());
            fields.remove("error_status");
            fields.remove("stage");
        }
        self.exit_code = CANCELLED_EXIT_CODE;
        self.status = CommandStatus::Cancelled.as_str();
        self.result_json = result.to_string();
        self
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
}

/// Factory implemented by the HTTP frontend. Queue lifecycle code only knows
/// the dispatch capability it needs, never the concrete API frontend type.
pub trait ApiCommandFrontendFactory: Send + Sync {
    type Frontend: crate::command::dispatch::DispatchFrontend;

    /// The worker emits `Done` after post-command stages. The returned frontend
    /// must leave that sentinel to the worker, including when dropped on error.
    fn frontend_for(
        &self,
        cmd: &CommandRecord,
        event_bus: super::event_bus::EventBusSender,
        state_writer: CommandStateWriter,
    ) -> Self::Frontend;
}

pub struct QueueWorker<F: ApiCommandFrontendFactory> {
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
}

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
    /// Finish cancelled teardown before starting the normal worker pool.
    pub async fn recover_cancelled_cleanup(
        &self,
    ) -> Result<(), crate::command::error::CommandError> {
        for id in self.lifecycle_store.cancelled_command_ids()? {
            let Some(cmd) = self.store.get_command(&id)? else {
                continue;
            };
            let Some(cleanup) = self.paths.read_command_cleanup(&cmd.session_id, &id)? else {
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
            let writer = CommandStateWriter::new(self.paths.clone(), &cmd.session_id, &id);
            let frontend = self
                .frontend_factory
                .frontend_for(&cmd, bus.sender(), writer);
            let mut engines = self.engines.clone();
            engines.runtime = Arc::new(super::owned_runtime::CommandOwnedRuntime::new(
                Arc::clone(&engines.runtime),
                &id,
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
            self.paths.remove_command_cleanup(&cmd.session_id, &id)?;
            self.post_execution_check(&cmd.session_id, &id).await;
        }
        Ok(())
    }

    pub fn new(worker_id: String, deps: QueueWorkerDeps<F>) -> Self {
        Self {
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
        }
    }

    pub async fn run(self) {
        loop {
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
                        "Worker claimed command"
                    );
                    let command_id = cmd.id.clone();
                    self.execute_command(cmd, cancel).await;
                    self.cancels.unregister(&command_id);
                }
                Ok(None) => {
                    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                }
                Err(e) => {
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
        let args: Vec<String> = serde_json::from_str(&cmd.args).unwrap_or_default();

        // A cancel recorded before this worker owned the command (it lost the
        // race with the claim) still applies.
        if matches!(
            self.lifecycle_store.cancel_requested_at(&command_id),
            Ok(Some(_))
        ) {
            cancel.cancel();
        }

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
            let result_json = serde_json::to_string(&serde_json::json!({
                "error": format!("Failed to create command directory: {e}"),
            }))
            .ok();
            let _ = self
                .store
                .complete_command(&command_id, "error", None, result_json.as_deref());
            self.post_execution_check(&session_id, &command_id).await;
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
            });
            let _ = self
                .paths
                .write_command_metadata(&session_id, &command_id, &metadata);
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
                phase: "queue".into(),
                message: format!("re-queued after restart (attempt {})", cmd.attempt),
            });
        }

        // The API frontend supplies the presentation adapter through its
        // factory; Layer 2 owns everything around command execution.
        let state_writer = CommandStateWriter::new(self.paths.clone(), &session_id, &command_id);
        let frontend =
            self.frontend_factory
                .frontend_for(&cmd, event_bus.sender(), state_writer.clone());

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
                let result_json = serde_json::to_string(&serde_json::json!({
                    "error": "Session not found in memory",
                }))
                .ok();
                event_bus.sender().emit(EventPayload::Done);
                let _ = log_writer.await;
                let _ =
                    self.store
                        .complete_command(&command_id, "error", None, result_json.as_deref());
                self.cleanup_event_bus(&command_id).await;
                self.post_execution_check(&session_id, &command_id).await;
                return;
            }
        };

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
        engines.runtime = Arc::new(super::owned_runtime::CommandOwnedRuntime::new(
            Arc::clone(&engines.runtime),
            &command_id,
        ));
        let dispatch = Dispatch::new(frontend, session, engines).with_cancellation(cancel);
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
        let dispatch_outcome = std::panic::AssertUnwindSafe(dispatch.run_command(&path_parts))
            .catch_unwind()
            .await;
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
        event_bus.emit_deferred_command_status();

        // A cancel accepted before push-back begins wins over a clean exit:
        // the cancelled work is not published and the command is `cancelled`.
        // Push already in flight cannot be undone; a cancel received while
        // it runs still determines the final command status below.
        let cancelled_before_push = if command_succeeded_for_push(&result) {
            self.lifecycle_store
                .cancel_requested_at(&command_id)
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
            event_bus.sender().emit(EventPayload::CommandStatus {
                status: CommandStatusKind::Error,
                exit_code: Some(1),
                error: Some(format!("{}: {}", error.stage, error.message)),
            });
        }

        let mut completion = QueuedCommandResult::new(&result, &push_outcome)
            .with_worktree(state_writer.kept_worktree());
        // An accepted cancellation wins until the terminal row is published,
        // including a request received during automatic push-back.
        if let Some(requested_at) = &cancelled_before_push {
            completion = completion.cancelled(requested_at);
        } else if let Ok(Some(requested_at)) = self.lifecycle_store.cancel_requested_at(&command_id)
        {
            completion = completion.cancelled(&requested_at);
        }
        let exit_code = completion.exit_code;
        let status = completion.status;
        let success = status == "done";

        if success {
            tracing::info!(
                worker_id = %self.worker_id,
                command_id = %command_id,
                subcommand = %subcommand,
                status = status,
                exit_code = ?exit_code,
                success = true,
                "Command execution finished"
            );
        } else {
            tracing::error!(
                worker_id = %self.worker_id,
                command_id = %command_id,
                subcommand = %subcommand,
                status = status,
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
            let _ = self
                .paths
                .write_command_metadata(&session_id, &command_id, &metadata);
        }

        event_bus.sender().emit(EventPayload::Done);
        let _ = log_writer.await;
        // Cleanup has finished. Retire its recovery recipe before publishing
        // the terminal row; a crash after this point needs no repeated cleanup.
        if let Err(error) = self.paths.remove_command_cleanup(&session_id, &command_id) {
            tracing::warn!(%error, "Failed to retire command cleanup recipe");
        }
        if let Err(error) = completion.persist(&self.store, &command_id) {
            tracing::error!(%error, %command_id, "Failed to persist command completion");
        }
        self.cleanup_event_bus(&command_id).await;
        self.post_execution_check(&session_id, &command_id).await;
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
        let closing = matches!(self.store.get_session(session_id), Ok(Some(ref session)) if session.status == "closing");
        let run_finished = matches!(
            self.lifecycle_store.run_for_session(session_id),
            Ok(Some(ref run)) if run.close_on_finish && run.command_id == command_id
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
                    | EventPayload::StatusMessage { .. } => {}
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
        assert_eq!(unconfigured.result_json, r#"{"exit_code":0}"#);

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
        assert_eq!(completion.status, "done");
        assert_eq!(json.pushed, Some(Some(pushed)));
    }

    #[test]
    fn push_failure_overrides_success_and_persists_stderr() {
        let failure = PushBackError::from(EngineError::Git(
            "git push failed: rejected (non-fast-forward)".into(),
        ));
        let completion = QueuedCommandResult::new(&Ok(CommandOutcome::Empty), &Err(failure));
        let json: CommandResult = serde_json::from_str(&completion.result_json).unwrap();
        assert_eq!(completion.status, "error");
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
            .insert_session("session", "/work", "2026-10-02")
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
                .enqueue_command(&id, "session", "prompt", "{}", "log")
                .unwrap();
            let command = store.claim_next_command("worker").unwrap().unwrap();
            assert_eq!(command.id, id);
            let completion =
                QueuedCommandResult::new(&Err(error), &Ok(PushBackOutcome::NotConfigured));
            completion.persist(&store, &id).unwrap();
            let saved = store.get_command(&id).unwrap().unwrap();
            assert_eq!(saved.status, "error");
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
            let writer = CommandStateWriter::new(paths, "session", "command");
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
            assert_eq!(completion.status, "error");
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
