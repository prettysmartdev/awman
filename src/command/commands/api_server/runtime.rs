//! API daemon lifecycle owned by Layer 2.
//!
//! Bootstrap order is load-bearing: the shared database must move before it is
//! opened; the store must migrate before recovery; and setup recovery happens
//! before workers can claim a command.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use super::cancel::{CommandCanceller, TokenRegistry};
use super::event_bus::EventBus;
use super::owned_runtime::stop_command_containers;
use super::queue_worker::{ApiCommandFrontendFactory, QueueWorker, QueueWorkerDeps};
use super::ApiServeConfig;
use crate::command::dispatch::Engines;
use crate::command::error::CommandError;
use crate::data::api_lifecycle_store::CommandLifecycleStore;
use crate::data::command_status::CommandStatus;
use crate::data::fs::api_db::{CommandRecord, LabelOwner, SqliteSessionStore};
use crate::data::fs::api_labels::Labels;
use crate::data::fs::api_paths::ApiPaths;
use crate::data::session::{Session, SessionOpenOptions, SessionType, StaticGitRootResolver};
use crate::data::session_manager::SessionManager;
use crate::data::session_setup_event::{SessionSetupError, SessionSetupStatus};
use crate::data::session_status::SessionStatus;
use crate::engine::auth::AuthMode;

/// Outcome of the one API session drain-and-close state machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CloseOutcome {
    Closed,
    Draining {
        running_command_id: String,
        cancelled: Vec<String>,
    },
    AlreadyClosing,
    NotFound,
}

/// Command-layer projection consumed by the API queue endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionQueueView {
    pub queue_depth: i64,
    pub running: Option<QueueCommandView>,
    pub queued: Vec<QueueCommandView>,
    pub recent_completed: Vec<CompletedCommandView>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueCommandView {
    pub command_id: String,
    pub subcommand: String,
    pub args: serde_json::Value,
    pub timestamp: Option<String>,
    pub position: Option<i64>,
    pub started_at: Option<String>,
    pub worker_id: Option<String>,
    pub labels: Labels,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletedCommandView {
    pub command_id: String,
    pub subcommand: String,
    pub status: CommandStatus,
    pub exit_code: Option<i32>,
    pub finished_at: Option<String>,
    pub labels: Labels,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionStatusFallback {
    pub setup_status: SessionSetupStatus,
    pub labels: Labels,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkflowStateAvailability {
    NotStarted,
    NeverStarted,
    NotWorkflow,
}

#[derive(Debug, thiserror::Error)]
pub enum QueueViewError {
    #[error("failed to read session: {0}")]
    Session(#[source] crate::data::error::DataError),
    #[error("failed to read queue: {0}")]
    Queue(#[source] crate::data::error::DataError),
    #[error("failed to read command labels: {0}")]
    Labels(#[source] crate::data::error::DataError),
}

#[derive(Debug, thiserror::Error)]
pub enum StatusFallbackError {
    #[error("failed to read session: {0}")]
    Session(#[source] crate::data::error::DataError),
    #[error("failed to read session labels: {0}")]
    Labels(#[source] crate::data::error::DataError),
}

pub enum SessionStatusView {
    Snapshot {
        labels: Labels,
        state: Box<crate::data::session_setup_event::SessionSetupState>,
    },
    Fallback(SessionStatusFallback),
}

#[derive(Debug, thiserror::Error)]
pub enum CancelSessionError {
    #[error("Failed to cancel command")]
    Cancel(#[source] CommandError),
    #[error("Failed to read run")]
    Run(#[source] crate::data::error::DataError),
    #[error("Failed to close cancelled run session")]
    Close(#[source] CommandError),
}

/// Command-layer classification used by HTTP handlers to map setup admission
/// to a status code without recreating setup policy in a route.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetupReadiness {
    Ready,
    Pending {
        status: String,
    },
    Failed {
        status: String,
        error: Option<serde_json::Value>,
    },
    NotFound,
}

impl SetupReadiness {
    pub fn status(&self) -> &str {
        match self {
            Self::Ready => "ready",
            Self::Pending { status } | Self::Failed { status, .. } => status,
            Self::NotFound => "not_found",
        }
    }

    pub fn is_ready(&self) -> bool {
        matches!(self, Self::Ready)
    }

    pub fn error(&self) -> Option<&serde_json::Value> {
        match self {
            Self::Failed { error, .. } => error.as_ref(),
            _ => None,
        }
    }
}

/// The API daemon's Layer 2 state. The HTTP frontend receives this fully
/// bootstrapped runtime and only builds a router, listener, and shutdown loop.
pub struct ApiServerRuntime {
    pub store: Arc<SqliteSessionStore>,
    pub sessions: Arc<SessionManager>,
    pub event_buses: Arc<tokio::sync::Mutex<HashMap<String, Arc<EventBus>>>>,
    pub paths: ApiPaths,
    pub auth_mode: AuthMode,
    pub workdirs: Vec<std::path::PathBuf>,
    pub workdir_roots: Vec<std::path::PathBuf>,
    pub started_at: Instant,
    pub task_handles: tokio::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>,
    pub engines: Engines,
    pub worker_count: usize,
    pub bind_ip: std::net::IpAddr,
    pub port: u16,
    pub tls_material: Option<crate::engine::auth::TlsMaterial>,
    /// Cancel markers and `/v1/runs` linkage (WI 0135).
    pub lifecycle_store: CommandLifecycleStore,
    /// Tokens of running commands, keyed by command id; registered by the
    /// worker that claimed each one.
    pub command_cancels: Arc<TokenRegistry>,
    /// Tokens of in-progress session setups, keyed by session id; registered
    /// by the route that spawns the setup.
    pub setup_cancels: Arc<TokenRegistry>,
}

impl ApiServerRuntime {
    /// Construct all daemon state before Layer 3 starts listening.
    pub fn bootstrap(config: ApiServeConfig, engines: Engines) -> Result<Self, CommandError> {
        // Reuse the `ApiPaths` the caller's `Engines` was built with, rather
        // than re-deriving one from the process environment: the two must
        // agree, or auth-mode resolution below checks a key hash at a
        // different root than the one that was just written to.
        let paths = engines.auth_engine.api_paths().clone();
        paths.ensure_root().map_err(CommandError::Data)?;

        // The database is shared with the squad daemon and lives under
        // `<data_home>/data/`. Relocate a pre-squad `<api_root>/awman.db` *before*
        // any connection is opened, exactly as the squad daemon does — otherwise the
        // two daemons would open different files and API-mode data would appear to
        // vanish after squad migrated it.
        let data_paths = paths.data_paths();
        let migration = data_paths
            .migrate_legacy_db(paths.root())
            .map_err(CommandError::Data)?;
        tracing::info!(migration = ?migration, "api database migration outcome");

        let db_path = paths.db_path();
        tracing::info!(db = %db_path.display(), "Opening session store");
        let store = Arc::new(SqliteSessionStore::open_at(&db_path).map_err(CommandError::Data)?);
        // The WI 0135 lifecycle tables (cancel markers, `/v1/runs` linkage)
        // must exist before any recovery below reads them.
        let lifecycle_store =
            CommandLifecycleStore::open(Arc::clone(&store)).map_err(CommandError::Data)?;

        // Startup cleanup: remove closed sessions older than 24 hours.
        if let Ok(deleted) = store.delete_closed_sessions_older_than(24) {
            for (sid, cmd_count) in &deleted {
                tracing::info!(session_id = %sid, commands = cmd_count, "Purging stale closed session");
            }
        }
        match lifecycle_store.prune_orphans() {
            Ok(0) => {}
            Ok(pruned) => tracing::info!(pruned, "Pruned lifecycle rows of purged sessions"),
            Err(error) => tracing::warn!(%error, "Failed to prune lifecycle rows"),
        }

        let auth_mode = engines
            .auth_engine
            .request_auth_mode(config.dangerously_skip_auth)?;
        // The config the daemon's engine bundle was assembled from, not a
        // fresh read: the two could otherwise see different files (F-31).
        let global_config = (*engines.global_config).clone();

        // Restore in-memory sessions for any active sessions persisted in SQLite
        // from a previous server lifetime. This ensures session continuity across
        // server restarts.
        tracing::info!("Restoring active sessions from previous server lifetime");
        let sessions = Arc::new(SessionManager::in_memory(config.env.clone()));
        if let Ok(records) = store
            .list_sessions_by_status(None)
            .inspect_err(|error| tracing::error!(%error, "Failed to list sessions to restore"))
        {
            for rec in records
                .into_iter()
                .filter(|rec| rec.status != SessionStatus::Closed)
            {
                let workdir_path = std::path::PathBuf::from(&rec.workdir);
                if rec.session_type == "remote" && !workdir_path.exists() {
                    tracing::warn!(session_id = %rec.id, workdir = %rec.workdir, "Remote session clone no longer exists; closing session");
                    if let Err(error) =
                        store.close_session_force(&rec.id, &chrono::Utc::now().to_rfc3339())
                    {
                        tracing::error!(session_id = %rec.id, %error, "Failed to close session whose clone is gone");
                    }
                    continue;
                }
                let resolver = StaticGitRootResolver::new(&workdir_path);
                match Session::open_or_workdir_fallback(
                    workdir_path,
                    &resolver,
                    SessionOpenOptions {
                        env: Some(config.env.clone()),
                        ..Default::default()
                    },
                ) {
                    Ok(mut session) => {
                        if rec.session_type == "remote" {
                            if let Some(cloned) = rec.cloned_path.as_deref() {
                                // Rows written before WI 0133 have no repo_url/branch
                                // and restore with empty strings, as they always did.
                                session.set_session_type(SessionType::Remote {
                                    repo_url: rec.repo_url.clone().unwrap_or_default(),
                                    branch: rec.branch.clone().unwrap_or_default(),
                                    cloned_path: std::path::PathBuf::from(cloned),
                                });
                            }
                        }
                        sessions.insert_with_key(rec.id.clone(), session)?;
                        tracing::info!(session_id = %rec.id, workdir = %rec.workdir, "Restored session");
                    }
                    Err(error) => {
                        tracing::warn!(session_id = %rec.id, workdir = %rec.workdir, %error, "Failed to restore session (workdir may no longer exist)")
                    }
                }
            }
        }

        // Mark sessions with in-progress setup as failed (server restarted mid-setup).
        // Authoritative source is the DB's setup_status column; we also persist
        // a setup_state.json for the /status endpoint's disk fallback path so
        // that the failure reason is visible to clients.
        if let Ok(records) = store.list_sessions_with_in_progress_setup().inspect_err(
            |error| tracing::error!(%error, "Failed to list sessions with in-progress setup"),
        ) {
            for rec in &records {
                tracing::warn!(session_id = %rec.id, previous_status = rec.setup_status.as_str(), "Marking session as failed (server restarted during setup)");
                if let Err(error) = store.update_setup_status(&rec.id, SessionSetupStatus::Failed) {
                    tracing::error!(session_id = %rec.id, %error, "Failed to mark interrupted setup as failed");
                }
                let mut setup_state = paths.read_setup_state(&rec.id).unwrap_or_default();
                setup_state.status = SessionSetupStatus::Failed;
                setup_state.current_stage =
                    Some("Server restarted during session setup".to_string());
                setup_state.error = Some(SessionSetupError {
                    stage: "server_restart".to_string(),
                    message: "Server restarted during session setup".to_string(),
                });
                if let Err(error) = paths.save_setup_state(&rec.id, &setup_state) {
                    tracing::warn!(session_id = %rec.id, %error, "Failed to save interrupted setup state");
                }
                // A `/v1/runs` command queued behind this setup can never run.
                if let Ok(Some(run)) =
                    lifecycle_store.run_for_session(&rec.id).inspect_err(|error| {
                        tracing::error!(session_id = %rec.id, %error, "Failed to read run of interrupted setup")
                    })
                {
                    let now = chrono::Utc::now().to_rfc3339();
                    let result = run_setup_error_result(
                        serde_json::to_value(setup_state.error.as_ref())
                            .ok()
                            .as_ref(),
                    );
                    if let Err(error) =
                        lifecycle_store.fail_if_queued(&run.command_id, &now, &result)
                    {
                        tracing::error!(session_id = %rec.id, command_id = %run.command_id, %error, "Failed to fail run command of interrupted setup");
                    }
                    if let Err(error) = super::queue_worker::finalize_unowned_command(
                        &store,
                        &paths,
                        &run.command_id,
                    ) {
                        tracing::error!(session_id = %rec.id, command_id = %run.command_id, %error, "Failed to finalize run command of interrupted setup");
                    }
                    if run.close_on_finish {
                        if let Err(error) = store.close_session_force(&rec.id, &now) {
                            tracing::error!(session_id = %rec.id, command_id = %run.command_id, %error, "Failed to close session of interrupted setup");
                        }
                        forget_restored_session(&sessions, &rec.id);
                    }
                }
                if rec.session_type == "remote" {
                    if let Some(cloned) = rec.cloned_path.as_deref() {
                        if let Err(error) = engines
                            .git_engine
                            .delete_directory(&std::path::PathBuf::from(cloned))
                        {
                            tracing::warn!(session_id = %rec.id, %error, "Failed to delete clone of interrupted setup");
                        }
                    }
                }
            }
        }

        // A completion recorded before the restart is what happened, so it goes
        // first: a cancel accepted after it cannot change the published outcome,
        // and the stale recovery below must not run the command a second time.
        super::queue_worker::finalize_completed_from_sidecars(&store, &paths)
            .map_err(CommandError::Data)?;

        // A command that was asked to cancel before the restart is finalised
        // as `cancelled` here, so the stale recovery below (which requeues
        // every `running` row) never brings it back.
        match lifecycle_store.finalize_interrupted_cancellations(&chrono::Utc::now().to_rfc3339()) {
            Ok(cancelled) => {
                for id in &cancelled {
                    if let Err(error) =
                        super::queue_worker::finalize_unowned_command(&store, &paths, id)
                    {
                        tracing::warn!(%error, %id, "Failed to publish recovered cancellation");
                    }
                }
                if !cancelled.is_empty() {
                    tracing::info!(
                        count = cancelled.len(),
                        "Finalised commands cancelled before the restart"
                    );
                }
            }
            Err(error) => {
                tracing::warn!(%error, "Failed to finalise interrupted cancellations");
                return Err(CommandError::Data(error));
            }
        };

        // Stale command recovery: at startup, every command still in `running`
        // is unequivocally stale — the worker that owned it died with the
        // previous server process. Use a zero-second threshold so we recover all
        // of them immediately rather than leaving recently-started ones stuck.
        let recovered = match store.recover_stale_commands(0) {
            Ok(recovered) => {
                if !recovered.is_empty() {
                    tracing::info!(
                        count = recovered.len(),
                        "Recovered stale running commands back to queued"
                    );
                }
                recovered
            }
            Err(error) => {
                tracing::warn!(%error, "Failed to recover stale commands");
                Vec::new()
            }
        };

        // Containers survive the daemon. Complete each interrupted
        // cancellation by stopping what its command launched (and stop a
        // requeued command's old containers before it runs again); every
        // container is named after its command, so no other agent is hit.
        let cancelled = lifecycle_store
            .cancelled_command_ids()
            .map_err(CommandError::Data)?;
        for command_id in cancelled.iter().chain(&recovered) {
            match stop_command_containers(engines.runtime.as_ref(), command_id) {
                Ok(0) => {}
                Ok(count) => {
                    tracing::info!(
                        command_id,
                        count,
                        "Stopped containers left by the previous daemon"
                    )
                }
                Err(error) => {
                    return Err(CommandError::from(error));
                }
            }
        }

        reconcile_runs(&store, &lifecycle_store, &paths, &engines, &sessions);
        finish_drained_closes(&store, &paths, &engines, &sessions);

        Ok(Self {
            store,
            sessions,
            event_buses: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            paths,
            auth_mode,
            workdirs: config.workdirs,
            workdir_roots: config.workdir_roots,
            started_at: Instant::now(),
            task_handles: tokio::sync::Mutex::new(Vec::new()),
            engines,
            worker_count: global_config.workers() as usize,
            bind_ip: config.bind_ip,
            port: config.port,
            tls_material: config.tls_material,
            lifecycle_store,
            command_cancels: Arc::new(TokenRegistry::new()),
            setup_cancels: Arc::new(TokenRegistry::new()),
        })
    }

    /// The cancel decision owner, sharing this daemon's stores and registries.
    pub fn canceller(&self) -> CommandCanceller {
        CommandCanceller::new(
            Arc::clone(&self.store),
            self.lifecycle_store.clone(),
            Arc::clone(&self.command_cancels),
            Arc::clone(&self.setup_cancels),
        )
        .with_completion_paths(self.paths.clone())
    }

    pub fn engines(&self) -> &Engines {
        &self.engines
    }

    pub fn bind_addr(&self) -> std::net::SocketAddr {
        std::net::SocketAddr::from((self.bind_ip, self.port))
    }

    pub fn tls_material(&self) -> Option<crate::engine::auth::TlsMaterial> {
        self.tls_material.clone()
    }

    pub fn lifecycle(&self) -> ApiSessionLifecycle {
        ApiSessionLifecycle::new(
            Arc::clone(&self.store),
            self.engines.clone(),
            Arc::clone(&self.sessions),
            self.paths.clone(),
        )
    }

    /// Admission shares this daemon's stores, session policy and engines.
    pub fn admission(&self) -> super::admission::ApiAdmission {
        super::admission::ApiAdmission::new(
            Arc::clone(&self.store),
            self.lifecycle_store.clone(),
            self.paths.clone(),
            self.engines.clone(),
            Arc::clone(&self.sessions),
            self.workdirs.clone(),
            self.workdir_roots.clone(),
        )
    }

    /// Start exactly the configured worker pool after Layer 3 provides its
    /// frontend factory. The command layer never names an API frontend type.
    pub fn spawn_workers<F>(&self, frontend_factory: F)
    where
        F: ApiCommandFrontendFactory + 'static,
    {
        if self.worker_count == 0 {
            tracing::warn!("workers config is 0 — no queue workers will run; commands will be enqueued but never processed");
        }
        let frontend_factory = Arc::new(frontend_factory);
        let mut workers = Vec::new();
        for worker_index in 0..self.worker_count {
            let worker = QueueWorker::new(
                format!("worker-{worker_index}-{}", uuid::Uuid::new_v4()),
                QueueWorkerDeps {
                    store: Arc::clone(&self.store),
                    engines: self.engines.clone(),
                    sessions: Arc::clone(&self.sessions),
                    event_buses: Arc::clone(&self.event_buses),
                    paths: self.paths.clone(),
                    frontend_factory: Arc::clone(&frontend_factory),
                    lifecycle: self.lifecycle(),
                    lifecycle_store: self.lifecycle_store.clone(),
                    cancels: Arc::clone(&self.command_cancels),
                },
            );
            workers.push(worker);
            tracing::debug!(worker_index, "Spawned queue worker");
        }
        tracing::info!(count = self.worker_count, "Queue workers started");
        tokio::spawn(async move {
            if let Some(worker) = workers.first() {
                if let Err(error) = worker.recover_cancelled_cleanup().await {
                    tracing::error!(%error, "Cancelled cleanup recovery failed; queue workers were not started");
                    return;
                }
            }
            for worker in workers {
                tokio::spawn(worker.run());
            }
        });
    }
}

/// The single owner of API session closure and setup admission policy.
#[derive(Clone)]
pub struct ApiSessionLifecycle {
    store: Arc<SqliteSessionStore>,
    engines: Engines,
    sessions: Arc<SessionManager>,
    paths: ApiPaths,
}

impl ApiSessionLifecycle {
    pub fn new(
        store: Arc<SqliteSessionStore>,
        engines: Engines,
        sessions: Arc<SessionManager>,
        paths: ApiPaths,
    ) -> Self {
        Self {
            store,
            engines,
            sessions,
            paths,
        }
    }

    pub fn queue_position(
        &self,
        command: &CommandRecord,
    ) -> Result<Option<i64>, crate::data::error::DataError> {
        if command.status.is_claimable() {
            self.store
                .queue_position_for_command(&command.id, &command.session_id)
        } else {
            Ok(None)
        }
    }

    pub fn setup_error(
        &self,
        session: &crate::data::fs::api_db::SessionRecord,
    ) -> Option<SessionSetupError> {
        (session.setup_status == SessionSetupStatus::Failed)
            .then(|| {
                self.paths
                    .read_setup_state(&session.id)
                    .and_then(|state| state.error)
            })
            .flatten()
    }

    /// An in-memory snapshot wins over disk, which wins over the legacy row.
    pub fn session_status_view(
        &self,
        session_id: &str,
        snapshot: Option<crate::data::session_setup_event::SessionSetupState>,
    ) -> Result<SessionStatusView, StatusFallbackError> {
        let labels = self
            .store
            .labels_for(LabelOwner::Session, session_id)
            .map_err(StatusFallbackError::Labels)?;
        match snapshot.or_else(|| self.paths.read_setup_state(session_id)) {
            Some(state) => Ok(SessionStatusView::Snapshot {
                labels,
                state: Box::new(state),
            }),
            None => self
                .status_fallback(session_id)
                .map(SessionStatusView::Fallback),
        }
    }

    /// Setup owns cleanup while its token is registered. Otherwise a terminal
    /// one-shot command has no worker left to perform close-on-finish.
    pub async fn cancel_command(
        &self,
        command_id: &str,
        canceller: &CommandCanceller,
        setup_cancels: &TokenRegistry,
        lifecycle: &CommandLifecycleStore,
    ) -> Result<super::CancelOutcome, CancelSessionError> {
        let outcome = canceller
            .cancel(command_id)
            .map_err(CancelSessionError::Cancel)?;
        if let super::CancelOutcome::AlreadyTerminal(row) | super::CancelOutcome::Cancelled(row) =
            &outcome
        {
            if !setup_cancels.is_registered(&row.session_id) {
                if let Some(run) = lifecycle
                    .run_for_command(command_id)
                    .map_err(CancelSessionError::Run)?
                {
                    if run.close_on_finish {
                        self.close(&run.session_id)
                            .await
                            .map_err(CancelSessionError::Close)?;
                    }
                }
            }
        }
        Ok(outcome)
    }

    /// A cancelled row can predate setup token registration. On a read error,
    /// cancel setup rather than starting work whose cancellation is unknown.
    pub fn reconcile_setup_cancel(
        &self,
        run: &crate::data::api_lifecycle_store::RunRecord,
        token: &tokio_util::sync::CancellationToken,
    ) -> Result<(), crate::data::error::DataError> {
        match self.store.get_command(&run.command_id) {
            Ok(command) => {
                if command.is_some_and(|c| c.status == CommandStatus::Cancelled) {
                    token.cancel();
                }
                Ok(())
            }
            Err(error) => {
                token.cancel();
                Err(error)
            }
        }
    }

    /// Build the typed queue projection shared by API consumers. Stored args
    /// pass through the same decoder used by command execution.
    pub fn queue_view(&self, session_id: &str) -> Result<Option<SessionQueueView>, QueueViewError> {
        if self
            .store
            .get_session(session_id)
            .map_err(QueueViewError::Session)?
            .is_none()
        {
            return Ok(None);
        }
        let records = self
            .store
            .session_queue_records(session_id)
            .map_err(QueueViewError::Queue)?;
        let ids: Vec<String> = records
            .recent
            .iter()
            .chain(records.running.iter())
            .map(|record| record.id.clone())
            .collect();
        let labels = self
            .store
            .labels_for_many(LabelOwner::Command, &ids)
            .map_err(QueueViewError::Labels)?;
        let labels_for = |id: &str| labels.get(id).cloned().unwrap_or_else(Labels::empty);

        let running = records
            .running
            .map(|record| decode_queue_command(record, None, labels_for));
        let mut queued_records: Vec<_> = records
            .recent
            .iter()
            .filter(|record| record.status.is_claimable())
            .collect();
        queued_records.reverse();
        let queued = queued_records
            .into_iter()
            .enumerate()
            .map(|(position, record)| {
                decode_queue_command(record.clone(), Some(position as i64), labels_for)
            })
            .collect();

        let mut completed: Vec<_> = records
            .recent
            .into_iter()
            .filter(|record| record.status.is_terminal())
            .collect();
        completed.sort_by(|a, b| b.finished_at.cmp(&a.finished_at));
        let recent_completed = completed
            .into_iter()
            .take(10)
            .map(|record| CompletedCommandView {
                command_id: record.id.clone(),
                subcommand: record.subcommand,
                status: record.status,
                exit_code: record.exit_code,
                finished_at: record.finished_at,
                labels: labels_for(&record.id),
            })
            .collect();

        Ok(Some(SessionQueueView {
            queue_depth: records.queue_depth,
            running,
            queued,
            recent_completed,
        }))
    }

    /// Last-resort setup-status read for sessions without setup state files.
    /// Older sessions with no row retain the historical `ready` fallback.
    pub fn status_fallback(
        &self,
        session_id: &str,
    ) -> Result<SessionStatusFallback, StatusFallbackError> {
        let setup_status = self
            .store
            .get_session(session_id)
            .map_err(StatusFallbackError::Session)?
            .map_or(SessionSetupStatus::Ready, |session| session.setup_status);
        let labels = self
            .store
            .labels_for(crate::data::fs::api_db::LabelOwner::Session, session_id)
            .map_err(StatusFallbackError::Labels)?;
        Ok(SessionStatusFallback {
            setup_status,
            labels,
        })
    }

    /// Classify the absent-workflow response without inspecting persisted
    /// lifecycle spellings in the transport adapter.
    pub fn workflow_state_availability(
        &self,
        command: &CommandRecord,
    ) -> WorkflowStateAvailability {
        let path: Vec<&str> = command.subcommand.split_whitespace().collect();
        if crate::command::dispatch::catalogue::CommandCatalogue::get().canonical_path(&path)
            != ["exec", "workflow"]
        {
            WorkflowStateAvailability::NotWorkflow
        } else if command.status.is_terminal() {
            WorkflowStateAvailability::NeverStarted
        } else {
            WorkflowStateAvailability::NotStarted
        }
    }

    /// Reconcile a one-shot run after setup relinquishes ownership, including
    /// cancellation racing setup's final ready publication.
    pub async fn finish_run_setup(
        &self,
        lifecycle: &CommandLifecycleStore,
        run: &crate::data::api_lifecycle_store::RunRecord,
    ) -> Result<(), CommandError> {
        let readiness = self.setup_readiness(&run.session_id).await;
        if !readiness.is_ready() {
            lifecycle.fail_if_queued(
                &run.command_id,
                &chrono::Utc::now().to_rfc3339(),
                &run_setup_error_result(readiness.error()),
            )?;
        }
        let command = self.store.get_command(&run.command_id)?;
        if command.as_ref().is_some_and(|c| {
            c.started_at.is_none()
                && matches!(c.status, CommandStatus::Error | CommandStatus::Cancelled)
        }) {
            super::queue_worker::finalize_unowned_command(
                &self.store,
                &self.paths,
                &run.command_id,
            )?;
        }
        let terminal = command.as_ref().is_some_and(|c| c.status.is_terminal());
        // A setup cancel also closes the session with the default close flag.
        let cancelled_during_setup = command
            .as_ref()
            .is_some_and(|c| c.status == CommandStatus::Cancelled && c.started_at.is_none());
        if cancelled_during_setup || (run.close_on_finish && terminal) {
            self.close(&run.session_id).await?;
        }
        Ok(())
    }

    pub async fn close(&self, session_id: &str) -> Result<CloseOutcome, CommandError> {
        let session = match self.store.get_session(session_id)? {
            None => return Ok(CloseOutcome::NotFound),
            Some(session) if session.status == SessionStatus::Closed => {
                return Ok(CloseOutcome::Closed)
            }
            Some(session) if session.status == SessionStatus::Closing => {
                if self
                    .store
                    .running_command_for_session(session_id)?
                    .is_some()
                    || self.pending_cleanup_command(session_id)?.is_some()
                {
                    return Ok(CloseOutcome::AlreadyClosing);
                }
                self.finish_close(session_id, &session).await?;
                return Ok(CloseOutcome::Closed);
            }
            Some(session) => session,
        };

        // Step 1: mark closing before cancelling queued work, closing the
        // command-admission gate before a racing POST can enqueue another job.
        self.store
            .update_session_status(session_id, SessionStatus::Closing)?;
        // Step 2: cancel any queued jobs that entered before the gate closed.
        let cancelled = self.store.cancel_queued_for_session(session_id)?;
        // Step 3: a running command owns the drain; worker post-check reenters
        // this same method once it finishes.
        if let Some(running) = self.store.running_command_for_session(session_id)? {
            return Ok(CloseOutcome::Draining {
                running_command_id: running.id,
                cancelled,
            });
        }
        // Recovery owns the clone until cancelled teardown has finished,
        // even though its command remains terminal and is never requeued.
        if let Some(command_id) = self.pending_cleanup_command(session_id)? {
            return Ok(CloseOutcome::Draining {
                running_command_id: command_id,
                cancelled,
            });
        }

        self.finish_close(session_id, &session).await?;
        Ok(CloseOutcome::Closed)
    }

    fn pending_cleanup_command(&self, session_id: &str) -> Result<Option<String>, CommandError> {
        let rows =
            self.store
                .list_commands_filtered(&crate::data::fs::api_db::CommandListFilter {
                    session_id: Some(session_id),
                    status: Some(CommandStatus::Cancelled),
                    labels: &[],
                    limit: i64::MAX,
                })?;
        for command in rows {
            if self
                .paths
                .read_command_cleanup(session_id, &command.id)?
                .is_some()
            {
                return Ok(Some(command.id));
            }
        }
        Ok(None)
    }

    async fn finish_close(
        &self,
        session_id: &str,
        session: &crate::data::fs::api_db::SessionRecord,
    ) -> Result<(), CommandError> {
        if session.session_type == "remote" {
            if let Some(cloned_path) = session.cloned_path.as_deref() {
                let git = Arc::clone(&self.engines.git_engine);
                let path = std::path::PathBuf::from(cloned_path);
                tokio::task::spawn_blocking(move || git.delete_directory(&path))
                    .await
                    .map_err(|error| {
                        CommandError::Other(format!("Delete directory task panicked: {error}"))
                    })??;
            }
        }
        self.store
            .close_session_force(session_id, &chrono::Utc::now().to_rfc3339())?;
        if let Some(session) = self.sessions.get_by_key(session_id) {
            let id = session.read().await.id();
            self.sessions.remove_by_key(session_id, id)?;
        }
        tracing::info!(session_id, "Session closed after drain");
        Ok(())
    }

    pub async fn setup_readiness(&self, session_id: &str) -> SetupReadiness {
        if let Some(state) = self.paths.read_setup_state(session_id) {
            return readiness_from_state(state.status, state.error);
        }
        match self.store.get_session(session_id) {
            Ok(Some(session)) => readiness_from_state(session.setup_status, None),
            Ok(None) => SetupReadiness::NotFound,
            Err(error) => {
                tracing::error!(session_id, %error, "Failed to read session setup status");
                SetupReadiness::NotFound
            }
        }
    }
}

fn decode_queue_command(
    record: CommandRecord,
    position: Option<i64>,
    labels: impl FnOnce(&str) -> Labels,
) -> QueueCommandView {
    let args = stored_args_view(&record);
    QueueCommandView {
        command_id: record.id.clone(),
        subcommand: record.subcommand,
        args,
        timestamp: record.queued_at,
        position,
        started_at: record.started_at,
        worker_id: record.worker_id,
        labels: labels(&record.id),
    }
}

/// Read projections retain legacy JSON values (and `[]` for invalid JSON).
/// Execution still rejects invalid argv through `decode_args`; one bad row
/// must not hide the rest of a session's queue from an operator.
pub fn stored_args_view(record: &CommandRecord) -> serde_json::Value {
    match record.decode_args() {
        Ok(args) => serde_json::json!(args),
        Err(error) => {
            tracing::warn!(command_id = %record.id, %error, "Invalid stored command args in read projection");
            serde_json::from_str(&record.args).unwrap_or_else(|_| serde_json::json!([]))
        }
    }
}

/// The `result` JSON of a `/v1/runs` command whose session setup failed:
/// `error` is the literal `"setup_error"`; the setup's own `{stage, message}`
/// (or `null`) rides in `setup_error`. Written with exit code 1.
pub fn run_setup_error_result(setup_error: Option<&serde_json::Value>) -> String {
    use crate::data::fs::api_db::{failure_codes, CommandFailure, CommandResult};
    let message = setup_error
        .and_then(|e| e.get("message"))
        .and_then(|m| m.as_str())
        .unwrap_or("Session setup failed");
    serde_json::to_string(&CommandResult {
        exit_code: Some(1),
        outcome: Some(crate::data::execution_event::CommandStatusKind::Error),
        failure: Some(CommandFailure {
            stage: "setup".into(),
            code: failure_codes::SETUP_FAILED.into(),
            message: message.into(),
            step: None,
        }),
        error: Some("setup_error".into()),
        error_status: Some(409),
        stage: Some("setup".into()),
        setup_error: Some(setup_error.cloned().unwrap_or(serde_json::Value::Null)),
        ..CommandResult::default()
    })
    .expect("command result is serializable")
}

/// Bootstrap: finish every `/v1/runs` lifecycle hook a crash interrupted,
/// whatever state its setup reached. A run whose setup failed cannot start,
/// so a still-queued command becomes `error` (a cancelled one stays
/// cancelled); a run asking to `close_on_finish` whose command is terminal is
/// closed the way `ApiSessionLifecycle::close` finishes a close (remote clone
/// deleted). Repeatable: every step is conditional on the current rows.
fn reconcile_runs(
    store: &SqliteSessionStore,
    lifecycle_store: &CommandLifecycleStore,
    paths: &ApiPaths,
    engines: &Engines,
    sessions: &SessionManager,
) {
    let runs = match lifecycle_store.list_runs() {
        Ok(runs) => runs,
        Err(error) => {
            tracing::warn!(%error, "Failed to list runs for restart reconciliation");
            return;
        }
    };
    for run in runs {
        let session = match store.get_session(&run.session_id) {
            Ok(Some(session)) => session,
            Ok(None) => continue,
            Err(error) => {
                tracing::error!(session_id = %run.session_id, command_id = %run.command_id, %error, "Failed to read session for restart reconciliation");
                continue;
            }
        };
        if session.status == SessionStatus::Closed {
            continue;
        }
        let now = chrono::Utc::now().to_rfc3339();
        if session.setup_status == SessionSetupStatus::Failed {
            let error = paths
                .read_setup_state(&run.session_id)
                .and_then(|state| state.error)
                .and_then(|error| serde_json::to_value(error).ok());
            let result = run_setup_error_result(error.as_ref());
            if let Err(error) = lifecycle_store.fail_if_queued(&run.command_id, &now, &result) {
                tracing::warn!(command_id = %run.command_id, %error, "Failed to fail a run whose setup failed");
            }
        }
        if let Err(error) =
            super::queue_worker::finalize_unowned_command(store, paths, &run.command_id)
        {
            tracing::warn!(%error, command_id = %run.command_id, "Failed to publish terminal run result");
        }
        let terminal = store
            .get_command(&run.command_id)
            .ok()
            .flatten()
            .is_some_and(|c| c.status.is_terminal());
        // The worker finishes durable cancelled cleanup before closing a
        // one-shot session and deleting the workspace it still needs.
        if matches!(store.get_command(&run.command_id), Ok(Some(c)) if c.status == CommandStatus::Cancelled)
            && !matches!(
                paths.read_command_cleanup(&run.session_id, &run.command_id),
                Ok(None)
            )
        {
            continue;
        }
        if !(run.close_on_finish && terminal) {
            continue;
        }
        close_finished_session(&session, store, engines, sessions);
    }
}

/// Bootstrap: a session left `closing` waits on a worker's post-check, which
/// never runs if the daemon died after the row was finished or the row finalized
/// from its sidecar. Runs after stale recovery, so a requeued command counts as
/// the work that will still drain it.
fn finish_drained_closes(
    store: &Arc<SqliteSessionStore>,
    paths: &ApiPaths,
    engines: &Engines,
    sessions: &Arc<SessionManager>,
) {
    let closing = match store.list_sessions_by_status(Some(SessionStatus::Closing)) {
        Ok(closing) => closing,
        Err(error) => {
            tracing::error!(%error, "Failed to list closing sessions");
            return;
        }
    };
    let lifecycle = ApiSessionLifecycle::new(
        Arc::clone(store),
        engines.clone(),
        Arc::clone(sessions),
        paths.clone(),
    );
    for session in closing {
        let session_id = session.id.as_str();
        match has_unfinished_command(store, session_id).and_then(|busy| {
            if busy {
                return Ok(true);
            }
            Ok(lifecycle.pending_cleanup_command(session_id)?.is_some())
        }) {
            Ok(true) => {}
            Ok(false) => close_finished_session(&session, store, engines, sessions),
            Err(error) => {
                tracing::error!(session_id, %error, "Failed to check work of draining session");
            }
        }
    }
}

fn has_unfinished_command(
    store: &SqliteSessionStore,
    session_id: &str,
) -> Result<bool, CommandError> {
    // The `Queued` filter also selects rows with the legacy `pending` spelling.
    for status in [CommandStatus::Running, CommandStatus::Queued] {
        let rows = store.list_commands_filtered(&crate::data::fs::api_db::CommandListFilter {
            session_id: Some(session_id),
            status: Some(status),
            labels: &[],
            limit: 1,
        })?;
        if !rows.is_empty() {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Close a session at bootstrap the way `ApiSessionLifecycle::close` finishes
/// one (remote clone deleted first; a failed delete retains the session).
fn close_finished_session(
    session: &crate::data::fs::api_db::SessionRecord,
    store: &SqliteSessionStore,
    engines: &Engines,
    sessions: &SessionManager,
) {
    if session.session_type == "remote" {
        if let Some(cloned) = session.cloned_path.as_deref() {
            if let Err(error) = engines
                .git_engine
                .delete_directory(&std::path::PathBuf::from(cloned))
            {
                tracing::warn!(session_id = %session.id, %error, "Failed to clean finished session clone; retaining session for recovery");
                return;
            }
        }
    }
    match store.close_session_force(&session.id, &chrono::Utc::now().to_rfc3339()) {
        Ok(_) => {
            forget_restored_session(sessions, &session.id);
            tracing::info!(session_id = %session.id, "Closed finished session after restart");
        }
        Err(error) => {
            tracing::warn!(session_id = %session.id, %error, "Failed to close finished session")
        }
    }
}

/// Drop a session restored earlier in bootstrap that bootstrap then closed.
/// Nothing else holds its lock yet, so `try_read` cannot contend.
fn forget_restored_session(sessions: &SessionManager, key: &str) {
    if let Some(session) = sessions.get_by_key(key) {
        let id = session.try_read().map(|s| s.id()).ok();
        if let Some(id) = id {
            let _ = sessions.remove_by_key(key, id);
        }
    }
}

/// Classify an on-disk `setup_state.json` snapshot. Terminal `ready` admits
/// work, `failed` carries the error payload a route renders, and every other
/// status is still in progress.
fn readiness_from_state(
    status: SessionSetupStatus,
    error: Option<SessionSetupError>,
) -> SetupReadiness {
    match status {
        SessionSetupStatus::Ready => SetupReadiness::Ready,
        SessionSetupStatus::Failed => SetupReadiness::Failed {
            status: SessionSetupStatus::Failed.as_str().to_string(),
            error: error.and_then(|e| serde_json::to_value(e).ok()),
        },
        other => SetupReadiness::Pending {
            status: other.as_str().to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lifecycle(tmp: &std::path::Path) -> (ApiSessionLifecycle, Arc<SqliteSessionStore>) {
        let store = Arc::new(SqliteSessionStore::open(tmp).unwrap());
        let lifecycle = ApiSessionLifecycle::new(
            Arc::clone(&store),
            Engines::for_tests(tmp),
            Arc::new(SessionManager::in_memory(
                crate::data::config::env::EnvSnapshot::empty(),
            )),
            ApiPaths::at_root(tmp),
        );
        (lifecycle, store)
    }

    fn insert_active(store: &SqliteSessionStore, id: &str) {
        store
            .insert_session(id, "/tmp", &chrono::Utc::now().to_rfc3339())
            .unwrap();
    }

    #[test]
    fn queue_view_assigns_oldest_first_positions_and_preserves_legacy_args_projection() {
        let tmp = tempfile::tempdir().unwrap();
        let (lifecycle, store) = lifecycle(tmp.path());
        insert_active(&store, "queue");
        store
            .enqueue_command("first", "queue", "agents status", "[\"a\"]", "log")
            .unwrap();
        store
            .enqueue_command("second", "queue", "agents status", "[]", "log")
            .unwrap();

        let view = lifecycle.queue_view("queue").unwrap().unwrap();
        assert_eq!(view.queue_depth, 2);
        assert_eq!(
            view.queued
                .iter()
                .map(|command| (command.command_id.as_str(), command.position))
                .collect::<Vec<_>>(),
            vec![("first", Some(0)), ("second", Some(1))]
        );
        assert_eq!(view.queued[0].args, serde_json::json!(["a"]));

        for (id, args, expected) in [
            ("bad", "{", serde_json::json!([])),
            ("object", "{}", serde_json::json!({})),
            ("numbers", "[1]", serde_json::json!([1])),
        ] {
            store
                .enqueue_command(id, "queue", "agents status", args, "log")
                .unwrap();
            let view = lifecycle.queue_view("queue").unwrap().unwrap();
            assert_eq!(
                view.queued
                    .iter()
                    .find(|c| c.command_id == id)
                    .unwrap()
                    .args,
                expected
            );
            assert_eq!(view.queued[0].command_id, "first");
            assert_eq!(view.queued[1].command_id, "second");
            assert!(store
                .get_command(id)
                .unwrap()
                .unwrap()
                .decode_args()
                .is_err());
        }
    }

    #[test]
    fn status_fallback_keeps_ready_for_legacy_missing_session() {
        let tmp = tempfile::tempdir().unwrap();
        let (lifecycle, _) = lifecycle(tmp.path());
        let fallback = lifecycle.status_fallback("legacy").unwrap();
        assert_eq!(fallback.setup_status, SessionSetupStatus::Ready);
        assert!(fallback.labels.is_empty());
    }

    #[test]
    fn session_status_view_prefers_memory_then_disk_then_row() {
        use crate::data::session_setup_event::SessionSetupState;
        let tmp = tempfile::tempdir().unwrap();
        let (lifecycle, store) = lifecycle(tmp.path());
        insert_active(&store, "s");
        store
            .update_setup_status("s", SessionSetupStatus::Ready)
            .unwrap();
        let mut disk = SessionSetupState::new();
        disk.mark_failed("clone", "disk failure");
        lifecycle.paths.save_setup_state("s", &disk).unwrap();
        let mut memory = SessionSetupState::new();
        memory.current_stage = Some("from memory".into());
        match lifecycle.session_status_view("s", Some(memory)).unwrap() {
            SessionStatusView::Snapshot { state, .. } => {
                assert_eq!(state.current_stage.as_deref(), Some("from memory"))
            }
            _ => panic!("memory snapshot lost"),
        }
        match lifecycle.session_status_view("s", None).unwrap() {
            SessionStatusView::Snapshot { state, .. } => {
                assert_eq!(state.status, SessionSetupStatus::Failed)
            }
            _ => panic!("disk snapshot lost"),
        }
        std::fs::write(
            lifecycle.paths.session_dir("s").join("setup_state.json"),
            "{",
        )
        .unwrap();
        match lifecycle.session_status_view("s", None).unwrap() {
            SessionStatusView::Fallback(fallback) => {
                assert_eq!(fallback.setup_status, SessionSetupStatus::Ready)
            }
            _ => panic!("malformed disk state hid row fallback"),
        }
    }

    #[test]
    fn early_setup_cancel_carries_cancelled_row_and_fails_closed_on_read_error() {
        let tmp = tempfile::tempdir().unwrap();
        let (lifecycle, store) = lifecycle(tmp.path());
        insert_active(&store, "s");
        store
            .enqueue_command("c", "s", "exec prompt", "[]", "log")
            .unwrap();
        let run = crate::data::api_lifecycle_store::RunRecord {
            session_id: "s".into(),
            command_id: "c".into(),
            close_on_finish: true,
            created_at: "now".into(),
        };
        let token = tokio_util::sync::CancellationToken::new();
        lifecycle.reconcile_setup_cancel(&run, &token).unwrap();
        assert!(!token.is_cancelled());
        store.cancel_queued_for_session("s").unwrap();
        lifecycle.reconcile_setup_cancel(&run, &token).unwrap();
        assert!(token.is_cancelled());
        store
            .with_conn(|conn| {
                conn.execute_batch("ALTER TABLE commands RENAME TO unavailable_commands")?;
                Ok(())
            })
            .unwrap();
        let token = tokio_util::sync::CancellationToken::new();
        assert!(lifecycle.reconcile_setup_cancel(&run, &token).is_err());
        assert!(token.is_cancelled());
    }

    #[tokio::test]
    async fn cancel_run_defers_close_to_setup_owner_and_reports_close_failure() {
        for setup_owns in [false, true] {
            let tmp = tempfile::tempdir().unwrap();
            let (lifecycle, store) = lifecycle(tmp.path());
            insert_active(&store, "s");
            let runs = CommandLifecycleStore::open(Arc::clone(&store)).unwrap();
            runs.record_run(&crate::data::api_lifecycle_store::RunRecord {
                session_id: "s".into(),
                command_id: "c".into(),
                close_on_finish: true,
                created_at: "now".into(),
            })
            .unwrap();
            store
                .enqueue_command("c", "s", "exec prompt", "[]", "log")
                .unwrap();
            let setups = Arc::new(TokenRegistry::new());
            let token = setup_owns.then(|| setups.register("s"));
            let canceller = CommandCanceller::new(
                Arc::clone(&store),
                runs.clone(),
                Arc::new(TokenRegistry::new()),
                Arc::clone(&setups),
            );
            if !setup_owns {
                store.with_conn(|conn| { conn.execute_batch("CREATE TRIGGER fail_close BEFORE UPDATE OF status ON sessions BEGIN SELECT RAISE(ABORT, 'close failed'); END;")?; Ok(()) }).unwrap();
                assert!(matches!(
                    lifecycle
                        .cancel_command("c", &canceller, &setups, &runs)
                        .await,
                    Err(CancelSessionError::Close(_))
                ));
                store
                    .with_conn(|conn| {
                        conn.execute_batch("DROP TRIGGER fail_close;")?;
                        Ok(())
                    })
                    .unwrap();
            }
            lifecycle
                .cancel_command("c", &canceller, &setups, &runs)
                .await
                .unwrap();
            assert_eq!(
                store.get_session("s").unwrap().unwrap().status,
                if setup_owns {
                    SessionStatus::Active
                } else {
                    SessionStatus::Closed
                }
            );
            if let Some(token) = token {
                assert!(token.is_cancelled());
            }
        }
    }

    #[tokio::test]
    async fn close_outcome_closes_session_without_a_queue() {
        let tmp = tempfile::tempdir().unwrap();
        let (lifecycle, store) = lifecycle(tmp.path());
        insert_active(&store, "empty");
        assert_eq!(
            lifecycle.close("empty").await.unwrap(),
            CloseOutcome::Closed
        );
        assert_eq!(
            store.get_session("empty").unwrap().unwrap().status.as_str(),
            "closed"
        );
    }

    #[tokio::test]
    async fn close_outcome_drains_a_running_command_then_reports_already_closing() {
        let tmp = tempfile::tempdir().unwrap();
        let (lifecycle, store) = lifecycle(tmp.path());
        insert_active(&store, "busy");
        store
            .enqueue_command("running", "busy", "agents status", "[]", "log")
            .unwrap();
        store.claim_next_command("worker").unwrap();
        assert_eq!(
            lifecycle.close("busy").await.unwrap(),
            CloseOutcome::Draining {
                running_command_id: "running".into(),
                cancelled: Vec::new()
            }
        );
        assert_eq!(
            lifecycle.close("busy").await.unwrap(),
            CloseOutcome::AlreadyClosing
        );
    }

    #[tokio::test]
    async fn close_outcome_reports_unknown_id() {
        let tmp = tempfile::tempdir().unwrap();
        let (lifecycle, _) = lifecycle(tmp.path());
        assert_eq!(
            lifecycle.close("missing").await.unwrap(),
            CloseOutcome::NotFound
        );
    }

    #[tokio::test]
    async fn close_waits_for_cancelled_cleanup_before_retiring_the_session() {
        let tmp = tempfile::tempdir().unwrap();
        let (lifecycle, store) = lifecycle(tmp.path());
        insert_active(&store, "cleanup");
        store
            .enqueue_command("command", "cleanup", "exec workflow", "[]", "log")
            .unwrap();
        store
            .complete_command("command", CommandStatus::Cancelled, Some(130), None)
            .unwrap();
        lifecycle
            .paths
            .prepare_command_dir("cleanup", "command")
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
            workdir: tmp.path().to_path_buf(),
            mount_path: tmp.path().to_path_buf(),
            state_root: None,
            image: "test".into(),
            cli_overlays: vec![],
            worktree_git_mount: None,
        };
        lifecycle
            .paths
            .write_command_cleanup("cleanup", "command", &cleanup)
            .unwrap();
        assert!(matches!(
            lifecycle.close("cleanup").await.unwrap(),
            CloseOutcome::Draining { .. }
        ));
        assert_eq!(
            store
                .get_session("cleanup")
                .unwrap()
                .unwrap()
                .status
                .as_str(),
            "closing"
        );
        assert_eq!(
            lifecycle.close("cleanup").await.unwrap(),
            CloseOutcome::AlreadyClosing
        );
        lifecycle
            .paths
            .remove_command_cleanup("cleanup", "command")
            .unwrap();
        assert_eq!(
            lifecycle.close("cleanup").await.unwrap(),
            CloseOutcome::Closed
        );
        assert_eq!(
            store
                .get_session("cleanup")
                .unwrap()
                .unwrap()
                .status
                .as_str(),
            "closed"
        );
    }
}
