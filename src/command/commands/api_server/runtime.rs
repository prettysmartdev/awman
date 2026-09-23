//! API daemon lifecycle owned by Layer 2.
//!
//! Bootstrap order is load-bearing: the shared database must move before it is
//! opened; the store must migrate before recovery; and setup recovery happens
//! before workers can claim a command.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use super::event_bus::EventBus;
use super::queue_worker::{ApiCommandFrontendFactory, QueueWorker, QueueWorkerDeps};
use super::ApiServeConfig;
use crate::command::dispatch::Engines;
use crate::command::error::CommandError;
use crate::data::fs::api_db::SqliteSessionStore;
use crate::data::fs::api_paths::ApiPaths;
use crate::data::session::{Session, SessionOpenOptions, SessionType, StaticGitRootResolver};
use crate::data::session_manager::SessionManager;
use crate::data::session_setup_event::{SessionSetupError, SessionSetupStatus};
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
    pub started_at: Instant,
    pub task_handles: tokio::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>,
    pub engines: Engines,
    pub worker_count: usize,
    pub bind_ip: std::net::IpAddr,
    pub port: u16,
    pub tls_material: Option<crate::engine::auth::TlsMaterial>,
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
        let store = SqliteSessionStore::open_at(&db_path).map_err(CommandError::Data)?;

        // Startup cleanup: remove closed sessions older than 24 hours.
        if let Ok(deleted) = store.delete_closed_sessions_older_than(24) {
            for (sid, cmd_count) in &deleted {
                tracing::info!(session_id = %sid, commands = cmd_count, "Purging stale closed session");
            }
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
        let sessions = Arc::new(SessionManager::in_memory());
        if let Ok(records) = store.list_sessions_by_status(Some("active")) {
            for rec in records {
                let workdir_path = std::path::PathBuf::from(&rec.workdir);
                if rec.session_type == "remote" && !workdir_path.exists() {
                    tracing::warn!(session_id = %rec.id, workdir = %rec.workdir, "Remote session clone no longer exists; closing session");
                    let _ = store.close_session_force(&rec.id, &chrono::Utc::now().to_rfc3339());
                    continue;
                }
                let resolver = StaticGitRootResolver::new(&workdir_path);
                match Session::open_or_workdir_fallback(
                    workdir_path,
                    &resolver,
                    SessionOpenOptions::default(),
                ) {
                    Ok(mut session) => {
                        if rec.session_type == "remote" {
                            if let Some(cloned) = rec.cloned_path.as_deref() {
                                // Known gap: SQLite does not persist repo_url or branch yet.
                                // Follow-up: persist both fields without changing this schema here.
                                session.set_session_type(SessionType::Remote {
                                    repo_url: String::new(),
                                    branch: String::new(),
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
        if let Ok(records) = store.list_sessions_with_in_progress_setup() {
            for rec in &records {
                tracing::warn!(session_id = %rec.id, previous_status = %rec.setup_status, "Marking session as failed (server restarted during setup)");
                let _ = store.update_setup_status(&rec.id, "failed");
                if rec.session_type == "remote" {
                    if let Some(cloned) = rec.cloned_path.as_deref() {
                        let _ = engines
                            .git_engine
                            .delete_directory(&std::path::PathBuf::from(cloned));
                    }
                }
                let mut setup_state = paths.read_setup_state(&rec.id).unwrap_or_default();
                setup_state.status = SessionSetupStatus::Failed;
                setup_state.current_stage =
                    Some("Server restarted during session setup".to_string());
                setup_state.error = Some(SessionSetupError {
                    stage: "server_restart".to_string(),
                    message: "Server restarted during session setup".to_string(),
                });
                let _ = paths.save_setup_state(&rec.id, &setup_state);
            }
        }

        let store = Arc::new(store);
        // Stale command recovery: at startup, every command still in `running`
        // is unequivocally stale — the worker that owned it died with the
        // previous server process. Use a zero-second threshold so we recover all
        // of them immediately rather than leaving recently-started ones stuck.
        match store.recover_stale_commands(0) {
            Ok(recovered) if !recovered.is_empty() => tracing::info!(
                count = recovered.len(),
                "Recovered stale running commands back to queued"
            ),
            Err(error) => tracing::warn!(%error, "Failed to recover stale commands"),
            _ => {}
        }

        Ok(Self {
            store,
            sessions,
            event_buses: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            paths,
            auth_mode,
            workdirs: config.workdirs,
            started_at: Instant::now(),
            task_handles: tokio::sync::Mutex::new(Vec::new()),
            engines,
            worker_count: global_config.workers() as usize,
            bind_ip: config.bind_ip,
            port: config.port,
            tls_material: config.tls_material,
        })
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
                },
            );
            tokio::spawn(worker.run());
            tracing::debug!(worker_index, "Spawned queue worker");
        }
        tracing::info!(count = self.worker_count, "Queue workers started");
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

    pub async fn close(&self, session_id: &str) -> Result<CloseOutcome, CommandError> {
        let session = match self.store.get_session(session_id)? {
            None => return Ok(CloseOutcome::NotFound),
            Some(session) if session.status == "closed" => return Ok(CloseOutcome::Closed),
            Some(session) if session.status == "closing" => {
                if self
                    .store
                    .running_command_for_session(session_id)?
                    .is_some()
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
        self.store.update_session_status(session_id, "closing")?;
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

        self.finish_close(session_id, &session).await?;
        Ok(CloseOutcome::Closed)
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
            Ok(Some(session)) if session.setup_status == "ready" => SetupReadiness::Ready,
            Ok(Some(session)) if session.setup_status == "failed" => SetupReadiness::Failed {
                status: session.setup_status,
                error: None,
            },
            Ok(Some(session)) => SetupReadiness::Pending {
                status: session.setup_status,
            },
            Ok(None) | Err(_) => SetupReadiness::NotFound,
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
            Arc::new(SessionManager::in_memory()),
            ApiPaths::at_root(tmp),
        );
        (lifecycle, store)
    }

    fn insert_active(store: &SqliteSessionStore, id: &str) {
        store
            .insert_session(id, "/tmp", &chrono::Utc::now().to_rfc3339())
            .unwrap();
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
            store.get_session("empty").unwrap().unwrap().status,
            "closed"
        );
    }

    #[tokio::test]
    async fn close_outcome_drains_a_running_command_then_reports_already_closing() {
        let tmp = tempfile::tempdir().unwrap();
        let (lifecycle, store) = lifecycle(tmp.path());
        insert_active(&store, "busy");
        store
            .enqueue_command("running", "busy", "status", "[]", "log")
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
}
