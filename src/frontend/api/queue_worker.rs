//! Queue worker — claims commands from the SQLite queue and executes them.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::RwLock;

use super::command_frontend::ApiDispatchFrontend;
use super::event_bus::EventBus;
use crate::command::dispatch::{CommandOutcome, Dispatch, Engines};
use crate::data::execution_event::EventPayload;
use crate::data::fs::api_db::SqliteSessionStore;
use crate::data::fs::api_paths::ApiPaths;
use crate::data::session::Session;

pub struct QueueWorker {
    worker_id: String,
    store: Arc<SqliteSessionStore>,
    engines: Engines,
    sessions: Arc<tokio::sync::Mutex<HashMap<String, Arc<RwLock<Session>>>>>,
    event_buses: Arc<tokio::sync::Mutex<HashMap<String, Arc<EventBus>>>>,
    paths: ApiPaths,
}

impl QueueWorker {
    pub fn new(
        worker_id: String,
        store: Arc<SqliteSessionStore>,
        engines: Engines,
        sessions: Arc<tokio::sync::Mutex<HashMap<String, Arc<RwLock<Session>>>>>,
        event_buses: Arc<tokio::sync::Mutex<HashMap<String, Arc<EventBus>>>>,
        paths: ApiPaths,
    ) -> Self {
        Self {
            worker_id,
            store,
            engines,
            sessions,
            event_buses,
            paths,
        }
    }

    pub async fn run(self) {
        loop {
            let claimed = self.store.claim_next_command(&self.worker_id);
            match claimed {
                Ok(Some(cmd)) => {
                    tracing::info!(
                        worker_id = %self.worker_id,
                        command_id = %cmd.id,
                        session_id = %cmd.session_id,
                        subcommand = %cmd.subcommand,
                        "Worker claimed command"
                    );
                    self.execute_command(cmd).await;
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

    async fn execute_command(&self, cmd: crate::data::fs::api_db::CommandRecord) {
        let command_id = cmd.id.clone();
        let session_id = cmd.session_id.clone();
        let subcommand = cmd.subcommand.clone();
        let args: Vec<String> = serde_json::from_str(&cmd.args).unwrap_or_default();

        tracing::info!(
            worker_id = %self.worker_id,
            command_id = %command_id,
            session_id = %session_id,
            subcommand = %subcommand,
            args = ?args,
            "Worker executing command"
        );

        // Create command directory.
        let cmd_dir = self.paths.command_dir(&session_id, &command_id);
        if let Err(e) = tokio::fs::create_dir_all(&cmd_dir).await {
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
            self.post_execution_check(&session_id).await;
            return;
        }

        let log_path = cmd_dir.join("output.log");

        // Write initial metadata.
        {
            let metadata = serde_json::json!({
                "command_id": command_id,
                "session_id": session_id,
                "subcommand": subcommand,
                "args": args,
                "started_at": cmd.started_at,
                "worker_id": self.worker_id,
            });
            let meta_path = self.paths.command_metadata_path(&session_id, &command_id);
            let _ = tokio::fs::write(
                &meta_path,
                serde_json::to_string_pretty(&metadata).unwrap_or_default(),
            )
            .await;
        }

        // Create EventBus for this command execution.
        let event_bus = Arc::new(EventBus::new(4096));

        // Spawn logfile writer task.
        {
            let mut log_rx = event_bus.subscribe();
            let events_log_path = self.paths.command_events_log_path(&session_id, &command_id);
            let output_log_path = log_path.clone();
            tokio::spawn(async move {
                use tokio::io::AsyncWriteExt;
                let mut events_file = match tokio::fs::File::create(&events_log_path).await {
                    Ok(f) => f,
                    Err(e) => {
                        tracing::error!(error = %e, "Failed to create events.log");
                        return;
                    }
                };
                let mut output_file = match tokio::fs::File::create(&output_log_path).await {
                    Ok(f) => f,
                    Err(e) => {
                        tracing::error!(error = %e, "Failed to create output.log");
                        return;
                    }
                };
                loop {
                    match log_rx.recv().await {
                        Ok(event) => {
                            if let Ok(json) = serde_json::to_string(&event) {
                                let _ = events_file.write_all(format!("{json}\n").as_bytes()).await;
                            }
                            if let Some(text) = event.payload.to_plain_text() {
                                let _ = output_file.write_all(format!("{text}\n").as_bytes()).await;
                            }
                            if matches!(event.payload, EventPayload::Done) {
                                let _ = events_file.flush().await;
                                let _ = output_file.flush().await;
                                break;
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            tracing::warn!(lagged = n, "Logfile writer lagged");
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            });
        }

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

        // Construct ApiDispatchFrontend.
        let frontend = ApiDispatchFrontend::new(&subcommand, &args, event_bus.sender());

        // Look up the Session.
        let session = match self.sessions.lock().await.get(&session_id).cloned() {
            Some(s) => s,
            None => {
                tracing::error!(
                    command_id = %command_id,
                    session_id = %session_id,
                    "Session not found in memory"
                );
                drop(frontend);
                let result_json = serde_json::to_string(&serde_json::json!({
                    "error": "Session not found in memory",
                }))
                .ok();
                let _ =
                    self.store
                        .complete_command(&command_id, "error", None, result_json.as_deref());
                self.cleanup_event_bus(&command_id).await;
                self.post_execution_check(&session_id).await;
                return;
            }
        };

        // Dispatch through Layer 2.
        let path_parts: Vec<&str> = subcommand.split_whitespace().collect();
        let dispatch = Dispatch::new(frontend, session, self.engines.clone());
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

        let (status, exit_code) = derive_command_status(&result);
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
                error = result.as_ref().err().map(|e| e.to_string()).unwrap_or_default(),
                "Command execution finished with failure"
            );
        }

        let result_json = match &result {
            Ok(_) => serde_json::to_string(&serde_json::json!({
                "exit_code": exit_code.unwrap_or(0),
            }))
            .ok(),
            Err(e) => serde_json::to_string(&serde_json::json!({
                "exit_code": exit_code.unwrap_or(1),
                "error": e.to_string(),
            }))
            .ok(),
        };

        if let Err(ref e) = result {
            tracing::error!(command_id = %command_id, error = %e, "Command failed");
        }

        let _ = self
            .store
            .complete_command(&command_id, status, exit_code, result_json.as_deref());

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
            });
            let meta_path = self.paths.command_metadata_path(&session_id, &command_id);
            let _ = tokio::fs::write(
                &meta_path,
                serde_json::to_string_pretty(&metadata).unwrap_or_default(),
            )
            .await;
        }

        self.cleanup_event_bus(&command_id).await;
        self.post_execution_check(&session_id).await;
    }

    async fn cleanup_event_bus(&self, command_id: &str) {
        let buses = Arc::clone(&self.event_buses);
        let cmd_id = command_id.to_string();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            buses.lock().await.remove(&cmd_id);
        });
    }

    async fn post_execution_check(&self, session_id: &str) {
        // Check if the session is in 'closing' state. If so, trigger final cleanup.
        let session_record = match self.store.get_session(session_id) {
            Ok(Some(r)) => r,
            _ => return,
        };

        if session_record.status != "closing" {
            return;
        }

        // Check if there's still a running command for this session.
        if let Ok(Some(_)) = self.store.running_command_for_session(session_id) {
            return;
        }

        tracing::info!(
            worker_id = %self.worker_id,
            session_id = %session_id,
            "Session is closing and no running commands remain; running final cleanup"
        );

        // For remote sessions, delete the cloned directory.
        if session_record.session_type == "remote" {
            if let Some(ref cloned_path) = session_record.cloned_path {
                let path = std::path::PathBuf::from(cloned_path);
                let git = Arc::clone(&self.engines.git_engine);
                let path_for_delete = path.clone();
                let delete_result =
                    tokio::task::spawn_blocking(move || git.delete_directory(&path_for_delete))
                        .await;
                match delete_result {
                    Ok(Ok(())) => {
                        tracing::info!(session_id = %session_id, "Remote session clone deleted");
                    }
                    Ok(Err(e)) => {
                        tracing::error!(
                            session_id = %session_id,
                            error = %e,
                            "Failed to delete remote session clone"
                        );
                        return; // don't mark as closed
                    }
                    Err(e) => {
                        tracing::error!(
                            session_id = %session_id,
                            error = %e,
                            "Delete directory task panicked"
                        );
                        return;
                    }
                }
            }
        }

        // Mark session as closed.
        let closed_at = chrono::Utc::now().to_rfc3339();
        let _ = self.store.close_session_force(session_id, &closed_at);

        // Remove from in-memory sessions map.
        self.sessions.lock().await.remove(session_id);

        tracing::info!(
            worker_id = %self.worker_id,
            session_id = %session_id,
            "Session closed after drain"
        );
    }
}

/// Map a dispatch `Result` to the (status, exit_code) pair stored in the
/// command queue. For exec-workflow / exec-prompt the inner `exit_code` is
/// authoritative — an aborted/failed workflow returns `Ok(...)` from Layer
/// 2 but with a non-zero `exit_code`, and reporting that as `status="done"`
/// would lie to clients tailing the queue.
fn derive_command_status(
    result: &Result<CommandOutcome, crate::command::error::CommandError>,
) -> (&'static str, Option<i32>) {
    match result {
        Ok(CommandOutcome::ExecWorkflow(o)) => match o.exit_code {
            Some(0) | None => ("done", Some(0)),
            Some(code) => ("error", Some(code)),
        },
        Ok(CommandOutcome::ExecPrompt(o)) => match o.exit_code {
            Some(0) | None => ("done", Some(0)),
            Some(code) => ("error", Some(code)),
        },
        Ok(_) => ("done", Some(0)),
        Err(_) => ("error", Some(1)),
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
                    } => match to_status.as_str() {
                        "failed" => tracing::error!(
                            command_id = %command_id,
                            session_id = %session_id,
                            subcommand = %subcommand,
                            step_index = step_index,
                            step = %step_name,
                            from = %from_status,
                            to = %to_status,
                            "Workflow step failed"
                        ),
                        "cancelled" | "skipped" => tracing::warn!(
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
                        if status == "failed" {
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
                    } => match status.as_str() {
                        "done" => tracing::info!(
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
mod tests {
    use super::*;
    use crate::command::commands::exec_prompt::ExecPromptOutcome;
    use crate::command::commands::exec_workflow::ExecWorkflowOutcome;
    use crate::command::error::CommandError;

    fn ok_workflow(exit_code: Option<i32>) -> Result<CommandOutcome, CommandError> {
        Ok(CommandOutcome::ExecWorkflow(ExecWorkflowOutcome {
            workflow: "wf.toml".into(),
            exit_code,
            worktree_used: false,
        }))
    }

    fn ok_prompt(exit_code: Option<i32>) -> Result<CommandOutcome, CommandError> {
        Ok(CommandOutcome::ExecPrompt(ExecPromptOutcome {
            agent: None,
            exit_code,
        }))
    }

    #[test]
    fn workflow_completed_with_exit_zero_reports_done() {
        assert_eq!(
            derive_command_status(&ok_workflow(Some(0))),
            ("done", Some(0))
        );
    }

    #[test]
    fn workflow_aborted_with_nonzero_exit_reports_error() {
        // Aborted workflow now propagates exit_code = Some(1) from
        // exec_workflow.rs — must surface as status=error in the queue,
        // not as the old "status=done success=true".
        assert_eq!(
            derive_command_status(&ok_workflow(Some(1))),
            ("error", Some(1))
        );
    }

    #[test]
    fn workflow_failed_step_propagates_step_exit_code() {
        assert_eq!(
            derive_command_status(&ok_workflow(Some(42))),
            ("error", Some(42))
        );
    }

    #[test]
    fn workflow_paused_reports_done() {
        // Paused is not a failure — the workflow is awaiting user input.
        assert_eq!(derive_command_status(&ok_workflow(None)), ("done", Some(0)));
    }

    #[test]
    fn prompt_nonzero_exit_reports_error() {
        assert_eq!(
            derive_command_status(&ok_prompt(Some(2))),
            ("error", Some(2))
        );
    }

    #[test]
    fn dispatch_err_reports_error_with_generic_exit_one() {
        let err: Result<CommandOutcome, CommandError> = Err(CommandError::Other("boom".into()));
        assert_eq!(derive_command_status(&err), ("error", Some(1)));
    }

    #[test]
    fn empty_outcome_reports_done() {
        let ok: Result<CommandOutcome, CommandError> = Ok(CommandOutcome::Empty);
        assert_eq!(derive_command_status(&ok), ("done", Some(0)));
    }
}
