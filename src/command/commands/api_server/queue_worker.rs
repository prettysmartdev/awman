//! Queue worker — claims commands from the SQLite queue and executes them.

use std::collections::HashMap;
use std::sync::Arc;

use super::event_bus::EventBus;
use super::runtime::ApiSessionLifecycle;
use crate::command::dispatch::{CommandOutcome, Dispatch, Engines};
use crate::data::execution_event::{
    CommandStatusKind, EventPayload, PhaseStatusKind, StepStatusKind,
};
use crate::data::fs::api_command_log::CommandLogWriter;
use crate::data::fs::api_db::{CommandRecord, SqliteSessionStore};
use crate::data::fs::api_paths::ApiPaths;
use crate::data::session_manager::SessionManager;

/// Factory implemented by the HTTP frontend. Queue lifecycle code only knows
/// the dispatch capability it needs, never the concrete API frontend type.
pub trait ApiCommandFrontendFactory: Send + Sync {
    type Frontend: crate::command::dispatch::DispatchFrontend;

    fn frontend_for(
        &self,
        cmd: &CommandRecord,
        event_bus: super::event_bus::EventBusSender,
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
}

impl<F: ApiCommandFrontendFactory> QueueWorker<F> {
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
            self.post_execution_check(&session_id).await;
            return;
        }

        // Write initial metadata (Layer 0).
        {
            let metadata = serde_json::json!({
                "command_id": command_id,
                "session_id": session_id,
                "subcommand": subcommand,
                "args": args,
                "started_at": cmd.started_at,
                "worker_id": self.worker_id,
            });
            let _ = self
                .paths
                .write_command_metadata(&session_id, &command_id, &metadata);
        }

        // Create EventBus for this command execution.
        let event_bus = Arc::new(EventBus::new(4096));

        // Spawn logfile writer task. The two log files are owned by a Layer 0
        // writer so this frontend performs no filesystem calls of its own.
        {
            let mut log_rx = event_bus.subscribe();
            let paths = self.paths.clone();
            let session_id_for_log = session_id.clone();
            let command_id_for_log = command_id.clone();
            tokio::spawn(async move {
                let mut writer = match CommandLogWriter::create(
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
                loop {
                    match log_rx.recv().await {
                        Ok(event) => {
                            writer.write_event(&event).await;
                            if matches!(event.payload, EventPayload::Done) {
                                writer.flush().await;
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

        // The API frontend supplies the presentation adapter through its
        // factory; Layer 2 owns everything around command execution.
        let frontend = self.frontend_factory.frontend_for(&cmd, event_bus.sender());

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

        let exit_code = result.as_ref().map(CommandOutcome::exit_code).unwrap_or(1);
        let status = if result.is_ok() && exit_code == 0 {
            "done"
        } else {
            "error"
        };
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
                "exit_code": exit_code,
            }))
            .ok(),
            Err(e) => serde_json::to_string(&serde_json::json!({
                "exit_code": exit_code,
                "error": e.to_string(),
            }))
            .ok(),
        };

        if let Err(ref e) = result {
            tracing::error!(command_id = %command_id, error = %e, "Command failed");
        }

        let _ = self.store.complete_command(
            &command_id,
            status,
            Some(exit_code),
            result_json.as_deref(),
        );

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
            let _ = self
                .paths
                .write_command_metadata(&session_id, &command_id, &metadata);
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
        if matches!(self.store.get_session(session_id), Ok(Some(ref session)) if session.status == "closing")
        {
            if let Err(error) = self.lifecycle.close(session_id).await {
                tracing::error!(worker_id = %self.worker_id, %error, session_id, "Failed to finish closing session");
            }
        }
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
