//! Layer 2 async session-setup orchestration.
//!
//! Multi-session frontends (the API server today; desktop apps, editor
//! extensions, or k8s operators tomorrow) all need the same *behavior* after a
//! [`SessionCreatePlan`](crate::command::session_create::SessionCreatePlan) has
//! been validated: clone the remote repo's default branch, check out (or
//! create) the requested branch, open the [`Session`], run the ready checks —
//! and, on any failure along the way, delete a remote session's partially
//! cloned directory so no orphaned clone is left on disk.
//!
//! Per Tenet 2 of the grand architecture that ordered sequence, and in
//! particular the remote-clone failure-cleanup rule, must not live in a
//! frontend — it lives here so every frontend gets it for free and cannot
//! drift.
//!
//! The frontend supplies a [`SessionSetupPresenter`], which does exactly three
//! things: write a line to the frontend's log, broadcast a
//! [`SetupEventPayload`], and vend the two sinks the run needs. It does not
//! persist anything. Before WI 0114 F-41 the trait also demanded
//! `persist_status`, `register_session` and `persist_and_cleanup` of every
//! frontend, so each one had to re-implement where a setup status is stored,
//! when it is written relative to on-disk clone cleanup, and how the
//! `SessionSetupState` transitions work. All of that is now here or in Layer 0:
//! [`SessionSetup`] owns the session store, the setup-state snapshot and the
//! [`SessionManager`], and every state transition is a method on
//! [`SessionSetupState`].

use crate::data::session::SessionKind;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use crate::command::commands::launch_policy::LaunchPolicy;
use crate::command::dispatch::Engines;
use crate::command::session_create::SessionCreatePlan;
use crate::data::fs::api_db::SqliteSessionStore;
use crate::data::fs::api_paths::ApiPaths;
use crate::data::session::{SessionOpenOptions, SessionType};
use crate::data::session_manager::SessionManager;
use crate::data::session_setup_event::{SessionSetupState, SessionSetupStatus, SetupEventPayload};
use crate::engine::error::EngineError;
use crate::engine::git::GitFrontend;
use crate::engine::ready::frontend::ReadyFrontend;
use crate::engine::ready::{ReadyEngine, ReadyEngineOptions};

/// Presentation-only side effects the [`SessionSetup`] orchestrator delegates
/// to the calling frontend.
///
/// Deliberately small. Every state transition has already been applied to the
/// shared [`SessionSetupState`] by the orchestrator before a presenter method
/// is called, and every persistence decision — which store column, which file,
/// in what order relative to deleting a partial clone — belongs to the
/// orchestrator. A presenter surfaces what happened; it never decides what it
/// means (WI 0114 F-41).
#[async_trait]
pub trait SessionSetupPresenter: Send {
    /// Append a line to the frontend's session-scoped setup log.
    fn log(&mut self, line: &str);

    /// Broadcast one setup event to whatever is watching this session.
    ///
    /// The single broadcast method replaces the old trait's
    /// `stage_changed` / `mark_failed` / `set_ready` triple: with the state
    /// mutation gone, those differed only in which [`SetupEventPayload`] they
    /// built, so building it is the orchestrator's job and sending it is the
    /// frontend's.
    fn emit(&mut self, event: SetupEventPayload);

    /// Vend the ready-checks frontend used to drive [`ReadyEngine`].
    fn ready_frontend(&mut self) -> Box<dyn ReadyFrontend>;

    /// Vend a message sink that captures git clone/branch output.
    fn git_log_sink(&mut self) -> Box<dyn GitFrontend + Send>;

    /// The run has reached a terminal state and the orchestrator has finished
    /// persisting. Frontends use this to retire per-session broadcast
    /// machinery; nothing about the session's recorded outcome depends on it.
    async fn finished(&mut self);
}

/// Layer 2 orchestrator that drives a validated [`SessionCreatePlan`] through
/// clone → branch → open → ready, delegating presentation to a
/// [`SessionSetupPresenter`].
pub struct SessionSetup {
    session_id: String,
    plan: SessionCreatePlan,
    engines: Engines,
    sessions: Arc<SessionManager>,
    /// The live setup snapshot. Shared with the frontend, which serves and
    /// streams it; only this orchestrator and the ready frontend write to it,
    /// and only through [`SessionSetupState`]'s own transition methods.
    setup_state: Arc<std::sync::RwLock<SessionSetupState>>,
    /// The session row whose `setup_status` column this run advances.
    store: Arc<SqliteSessionStore>,
    /// Where the terminal `setup_state.json` snapshot is written.
    paths: ApiPaths,
}

impl SessionSetup {
    pub fn new(
        session_id: String,
        plan: SessionCreatePlan,
        engines: Engines,
        sessions: Arc<SessionManager>,
        setup_state: Arc<std::sync::RwLock<SessionSetupState>>,
        store: Arc<SqliteSessionStore>,
        paths: ApiPaths,
    ) -> Self {
        Self {
            session_id,
            plan,
            engines,
            sessions,
            setup_state,
            store,
            paths,
        }
    }

    /// Apply one transition to the shared setup state. Every write this
    /// orchestrator makes goes through here, so the lock is never held across
    /// an await and the transition rules stay in Layer 0.
    fn state<T>(&self, apply: impl FnOnce(&mut SessionSetupState) -> T) -> T {
        let mut state = self
            .setup_state
            .write()
            .expect("session setup state lock poisoned");
        apply(&mut state)
    }

    /// Enter a lifecycle status and persist it to the session row.
    fn enter_status(&self, status: SessionSetupStatus) {
        let persisted = status.as_str();
        self.state(|s| s.enter_status(status));
        let _ = self.store.update_setup_status(&self.session_id, persisted);
    }

    /// Record a terminal failure in the state and broadcast it. The caller
    /// still decides when to persist the `"failed"` status relative to
    /// deleting a partial clone.
    fn mark_failed(&self, presenter: &mut dyn SessionSetupPresenter, stage: &str, error: &str) {
        self.state(|s| s.mark_failed(stage, error));
        presenter.emit(SetupEventPayload::SetupFailed {
            stage: stage.to_string(),
            error: error.to_string(),
        });
    }

    /// Set the human-readable stage line and broadcast the machine-keyed
    /// stage change together, which is how every stage in the run reports.
    fn stage(&self, presenter: &mut dyn SessionSetupPresenter, stage: &str, message: &str) {
        self.state(|s| s.set_stage(message));
        presenter.emit(SetupEventPayload::StageChanged {
            stage: stage.to_string(),
            message: message.to_string(),
        });
    }

    /// Persist the terminal status string (`"ready"` / `"failed"`) to the
    /// session row.
    fn persist_status(&self, status: &str) {
        let _ = self.store.update_setup_status(&self.session_id, status);
    }

    /// Write the final snapshot to `setup_state.json` and let the frontend
    /// retire its per-session machinery.
    async fn finish(&self, presenter: &mut dyn SessionSetupPresenter) {
        let snapshot = self
            .setup_state
            .read()
            .expect("session setup state lock poisoned")
            .clone();
        if let Err(e) = self.paths.save_setup_state(&self.session_id, &snapshot) {
            tracing::error!(
                session_id = %self.session_id,
                error = %e,
                "Failed to persist setup_state.json"
            );
        }
        presenter.finished().await;
    }

    /// Delete a remote session's cloned directory, ignoring errors — used on the
    /// failure paths that must not leave an orphaned clone behind. Runs the
    /// blocking filesystem removal off the async runtime, matching the frontend's
    /// previous behavior.
    async fn delete_clone(&self) {
        if let Some(dest) = self.plan.cloned_path.clone() {
            let git = Arc::clone(&self.engines.git_engine);
            let _ = tokio::task::spawn_blocking(move || git.delete_directory(&dest)).await;
        }
    }

    /// Run the full setup sequence, reporting progress and terminal state via
    /// `presenter`. Returns when setup reaches a terminal state (ready or
    /// failed), the snapshot is on disk, and the frontend has been told to
    /// retire its per-session machinery.
    pub async fn run(&self, presenter: &mut dyn SessionSetupPresenter) {
        let session_id = &self.session_id;

        // Delay setup work briefly so the frontend's acknowledgement (the API's
        // 202) can be flushed before any setup work runs. Critical when the
        // tokio runtime is single-threaded (e.g. `#[tokio::test]`).
        tokio::time::sleep(Duration::from_millis(50)).await;

        tracing::info!(
            session_id = %session_id,
            session_type = %self.plan.kind,
            workdir = %self.plan.resolved_workdir.display(),
            repo_url = self.plan.repo_url.as_deref().unwrap_or(""),
            branch = self.plan.branch.as_deref().unwrap_or(""),
            "Beginning session setup"
        );

        presenter.log(&format!(
            "state → {:?}: starting setup (type={}, workdir={})",
            SessionSetupStatus::Initializing,
            self.plan.kind,
            self.plan.resolved_workdir.display()
        ));

        // ── [remote only] Stage 1: clone repository ──────────────────────────
        if self.plan.kind == SessionKind::Remote {
            self.enter_status(SessionSetupStatus::CloningRepository);
            let msg = format!(
                "Cloning {}...",
                self.plan.repo_url.as_deref().unwrap_or("repository")
            );
            self.stage(presenter, "cloning_repository", &msg);
            presenter.log(&format!(
                "state → {:?}: clone stage",
                SessionSetupStatus::CloningRepository
            ));

            let url = self.plan.repo_url.clone().unwrap_or_default();
            let dest = self
                .plan
                .cloned_path
                .clone()
                .expect("remote sessions have cloned_path");
            tracing::info!(
                session_id = %session_id,
                repo_url = %url,
                dest = %dest.display(),
                "Cloning remote repository (default branch)"
            );
            let git = Arc::clone(&self.engines.git_engine);
            let dest_for_clone = dest.clone();
            let mut clone_sink = presenter.git_log_sink();
            // Clone the repository's default branch regardless of `plan.branch`.
            // The requested branch (which may not exist on the remote) is created
            // or checked out in the dedicated branch-setup stage below.
            let clone_result = tokio::task::spawn_blocking(move || {
                git.clone_repo_logged(&url, None, &dest_for_clone, &mut *clone_sink)
            })
            .await
            .unwrap_or_else(|join_err| {
                Err(EngineError::Git(format!("clone task panicked: {join_err}")))
            });
            if let Err(e) = clone_result {
                tracing::error!(session_id = %session_id, error = %e, "Clone failed");
                self.mark_failed(presenter, "clone", &e.to_string());
                // Cleanup any partial clone.
                self.delete_clone().await;
                self.persist_status("failed");
                self.finish(presenter).await;
                return;
            }
            tracing::info!(session_id = %session_id, "Repository cloned");
            presenter.emit(SetupEventPayload::StageChanged {
                stage: "cloning_repository_done".to_string(),
                message: "Repository cloned".to_string(),
            });

            // ── [remote only] Stage 2: set up branch ─────────────────────────
            if let Some(branch) = self.plan.branch.as_deref() {
                self.enter_status(SessionSetupStatus::SettingUpBranch);
                let msg = format!("Checking out branch '{branch}'...");
                self.stage(presenter, "setting_up_branch", &msg);
                presenter.log(&format!(
                    "state → {:?}: branch={branch}",
                    SessionSetupStatus::SettingUpBranch
                ));
                tracing::info!(
                    session_id = %session_id,
                    branch = %branch,
                    "Setting up branch"
                );

                let git = Arc::clone(&self.engines.git_engine);
                let dest_for_branch = dest.clone();
                let branch_owned = branch.to_string();
                let mut branch_sink = presenter.git_log_sink();
                let branch_result = tokio::task::spawn_blocking(move || {
                    git.checkout_or_create_branch_logged(
                        &dest_for_branch,
                        &branch_owned,
                        &mut *branch_sink,
                    )
                })
                .await
                .unwrap_or_else(|join_err| {
                    Err(EngineError::Git(format!(
                        "branch task panicked: {join_err}"
                    )))
                });
                match branch_result {
                    Ok(disposition) => {
                        tracing::info!(
                            session_id = %session_id,
                            branch = %branch,
                            disposition = disposition,
                            "Branch ready"
                        );
                        presenter.emit(SetupEventPayload::StageChanged {
                            stage: "branch_ready".to_string(),
                            message: format!("Branch '{branch}' {disposition}"),
                        });
                    }
                    Err(e) => {
                        tracing::error!(session_id = %session_id, error = %e, "Branch setup failed");
                        self.mark_failed(presenter, "branch", &e.to_string());
                        self.delete_clone().await;
                        self.persist_status("failed");
                        self.finish(presenter).await;
                        return;
                    }
                }
            }
        }

        // ── Stage 3 (all): open Session ──────────────────────────────────────
        self.enter_status(SessionSetupStatus::RunningReady);
        self.state(|s| s.set_stage("Opening session..."));
        presenter.emit(SetupEventPayload::StageChanged {
            stage: "running_ready".to_string(),
            message: "Opening session and running ready checks...".to_string(),
        });
        presenter.log(&format!(
            "state → {:?}: opening session at {}",
            SessionSetupStatus::RunningReady,
            self.plan.resolved_workdir.display()
        ));
        tracing::info!(
            session_id = %session_id,
            workdir = %self.plan.resolved_workdir.display(),
            "Opening session"
        );

        let session = match self.sessions.open_or_create_with_key(
            Some(self.session_id.clone()),
            self.plan.resolved_workdir.clone(),
            SessionOpenOptions::default(),
        ) {
            Ok(session) => session,
            Err(e) => {
                tracing::error!(
                    session_id = %session_id,
                    error = %e,
                    "Session setup failed: could not open session"
                );
                self.mark_failed(presenter, "session_open", &e.to_string());
                if self.plan.kind == SessionKind::Remote {
                    self.delete_clone().await;
                }
                self.persist_status("failed");
                self.finish(presenter).await;
                return;
            }
        };

        // For remote sessions, replace the default Local session_type so that
        // downstream consumers (e.g. worktree suppression in ExecWorkflowCommand)
        // see the correct variant.
        if self.plan.kind == SessionKind::Remote {
            if let Some(cloned_path) = self.plan.cloned_path.clone() {
                let repo_url = self.plan.repo_url.clone().unwrap_or_default();
                let branch = self.plan.branch.clone().unwrap_or_default();
                session.write().await.set_session_type(SessionType::Remote {
                    repo_url,
                    branch,
                    cloned_path,
                });
            }
        }

        // The session is registered by `open_or_create_with_key` above; this
        // is the assertion the API observer used to make on the orchestrator's
        // behalf (WI 0114 F-41). `session` is held for the ready stage below.
        if self.sessions.get_by_key(session_id).is_none() {
            tracing::error!(session_id = %session_id, "SessionSetup did not register its session");
        }
        tracing::info!(session_id = %session_id, "Session opened, running ReadyEngine");

        // ── Stage 4 (all): run ReadyEngine ───────────────────────────────────
        // Use the same agent name and idempotency semantics as the CLI/TUI
        // `awman ready` (no `--build`, no `--refresh`): the engine checks
        // `image_exists` and `Dockerfile.<agent>` on disk and skips re-building
        // / re-downloading when they're already present. The agent is read from
        // the cloned repo's `.awman/config.json` (with global-config and
        // hard-coded "claude" fallbacks), matching the CLI/TUI path — anything
        // else mis-targets the per-agent Dockerfile lookup and re-downloads the
        // template every session.
        let session_guard = session.read().await;
        let agent = match LaunchPolicy::for_session(&session_guard).resolve_agent(&None) {
            Ok(a) => a,
            Err(e) => {
                drop(session_guard);
                tracing::error!(session_id = %session_id, error = %e, "Failed to resolve agent");
                self.mark_failed(presenter, "resolve_agent", &e.to_string());
                self.persist_status("failed");
                self.finish(presenter).await;
                return;
            }
        };
        // ReadyEngine drives the container-paradigm image flow; under the
        // (stubbed) sandbox runtime this surfaces NotImplemented instead of
        // panicking. The sandbox ready flow lands in WI 0090.
        let container_runtime = match self.engines.require_container_runtime() {
            Ok(rt) => Arc::clone(rt),
            Err(e) => {
                drop(session_guard);
                tracing::error!(session_id = %session_id, error = %e, "Runtime unsupported for session setup");
                self.mark_failed(presenter, "ready", &e.to_string());
                self.persist_status("failed");
                self.finish(presenter).await;
                return;
            }
        };
        let ready_options = ReadyEngineOptions {
            agent,
            refresh: false,
            build: false,
            no_cache: false,
            allow_docker: true,
            non_interactive: true,
            env_passthrough: None,
        };
        let mut ready_engine = ReadyEngine::new(
            Arc::new(session_guard.clone()),
            Arc::clone(&self.engines.git_engine),
            Arc::clone(&self.engines.overlay_engine),
            container_runtime,
            Arc::clone(&self.engines.agent_engine),
            ready_options,
        );
        drop(session_guard);

        let mut setup_frontend = presenter.ready_frontend();

        // Cap ReadyEngine at 10 minutes — any legitimate run, including a clean
        // base-image build, completes well within this. If the wall-clock exceeds
        // the cap (e.g. Docker daemon is unresponsive), mark the setup as failed
        // so the session row reaches a terminal state and the bus is cleaned up.
        let ready_fut = ready_engine.run_to_completion(&mut *setup_frontend);
        let ready_outcome = tokio::time::timeout(Duration::from_secs(600), ready_fut).await;

        match ready_outcome {
            Ok(Ok(summary)) => {
                self.state(|s| s.set_ready(summary.clone()));
                presenter.emit(SetupEventPayload::SetupComplete {
                    ready_summary: Box::new(summary),
                });
                self.persist_status("ready");
                tracing::info!(session_id = %session_id, "Session setup complete");
            }
            Ok(Err(e)) => {
                tracing::error!(
                    session_id = %session_id,
                    error = %e,
                    "Session setup failed during ready"
                );
                self.mark_failed(presenter, "ready", &e.to_string());
                if self.plan.kind == SessionKind::Remote {
                    self.delete_clone().await;
                }
                self.persist_status("failed");
            }
            Err(_elapsed) => {
                let msg = "ReadyEngine exceeded the 600s setup deadline".to_string();
                tracing::error!(session_id = %session_id, "{msg}");
                self.mark_failed(presenter, "ready_timeout", &msg);
                if self.plan.kind == SessionKind::Remote {
                    self.delete_clone().await;
                }
                self.persist_status("failed");
            }
        }

        self.finish(presenter).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::message::RecordingMessageSink;
    use crate::data::session_setup_event::SetupEventPayload;

    /// The whole presenter surface, recorded. Since WI 0114 F-41 that is five
    /// methods and none of them persist anything: what the run *decided* is
    /// read back from the shared `SessionSetupState` and the session store,
    /// not from the frontend.
    #[derive(Default)]
    struct RecordingPresenter {
        events: Vec<SetupEventPayload>,
        ready_frontend_called: bool,
        finished_called: bool,
    }

    impl RecordingPresenter {
        /// The `(stage, error)` pairs of every `SetupFailed` event broadcast.
        fn failures(&self) -> Vec<(String, String)> {
            self.events
                .iter()
                .filter_map(|e| match e {
                    SetupEventPayload::SetupFailed { stage, error } => {
                        Some((stage.clone(), error.clone()))
                    }
                    _ => None,
                })
                .collect()
        }

        /// The machine keys of every `StageChanged` event broadcast, in order.
        fn stage_keys(&self) -> Vec<String> {
            self.events
                .iter()
                .filter_map(|e| match e {
                    SetupEventPayload::StageChanged { stage, .. } => Some(stage.clone()),
                    _ => None,
                })
                .collect()
        }
    }

    #[async_trait]
    impl SessionSetupPresenter for RecordingPresenter {
        fn log(&mut self, _line: &str) {}
        fn emit(&mut self, event: SetupEventPayload) {
            self.events.push(event);
        }
        fn ready_frontend(&mut self) -> Box<dyn ReadyFrontend> {
            // Only reached once setup gets all the way to the ready stage; the
            // clone-failure test below never gets here.
            self.ready_frontend_called = true;
            unreachable!("ready_frontend must not be reached on the clone-failure path");
        }
        fn git_log_sink(&mut self) -> Box<dyn GitFrontend + Send> {
            Box::new(RecordingMessageSink::new())
        }
        async fn finished(&mut self) {
            self.finished_called = true;
        }
    }

    fn test_engines() -> Engines {
        let runtime = Arc::new(crate::engine::container::ContainerRuntime::docker());
        let overlay = Arc::new(crate::engine::overlay::OverlayEngine::with_auth_resolver(
            crate::data::fs::auth_paths::AuthPathResolver::at_home(std::path::PathBuf::from(
                "/tmp",
            )),
        ));
        let git_engine = Arc::new(crate::engine::git::GitEngine::new());
        let agent_engine = Arc::new(crate::engine::agent::AgentEngine::new(
            overlay.clone(),
            runtime.clone(),
        ));
        let auth_engine = Arc::new(crate::engine::auth::AuthEngine::with_paths(
            crate::data::fs::auth_paths::AuthPathResolver::at_home("/tmp"),
            crate::data::fs::api_paths::ApiPaths::at_root("/tmp"),
        ));
        let workflow_state_store = {
            let tmp = tempfile::tempdir().unwrap();
            Arc::new(crate::data::WorkflowStateStore::at_git_root(tmp.path()))
        };
        Engines {
            runtime: runtime.clone(),
            container_runtime: Some(runtime),
            sandbox_runtime: None,
            git_engine,
            overlay_engine: overlay,
            auth_engine,
            agent_engine,
            workflow_state_store,
            credential_monitor: None,
            global_config: std::sync::Arc::new(Default::default()),
        }
    }

    /// Finding D core rule: a remote session whose clone fails must have its
    /// partially-cloned directory deleted and reach the `failed` terminal state,
    /// without ever advancing to the ready stage. Uses a bogus local repo URL so
    /// `git clone` fails fast — no network or Docker required.
    #[tokio::test]
    async fn remote_clone_failure_deletes_clone_and_reports_failed() {
        let root = tempfile::tempdir().unwrap();
        let cloned_path = root.path().join("clone-dest");
        std::fs::create_dir_all(&cloned_path).unwrap();
        // Drop a marker so we can be certain the directory is removed, not merely
        // emptied by some other path.
        std::fs::write(cloned_path.join("marker"), b"partial").unwrap();
        assert!(cloned_path.exists());

        let plan = SessionCreatePlan {
            kind: SessionKind::Remote,
            resolved_workdir: cloned_path.clone(),
            cloned_path: Some(cloned_path.clone()),
            // A path that does not exist → `git clone` fails immediately.
            repo_url: Some(root.path().join("no-such-repo.git").display().to_string()),
            branch: None,
        };

        // The orchestrator now owns persistence, so the test gives it a real
        // store and a real API directory rather than a frontend that fakes
        // them (F-41).
        let api_root = tempfile::tempdir().unwrap();
        let paths = ApiPaths::from_root(api_root.path());
        let store = Arc::new(SqliteSessionStore::open_from_paths(&paths).unwrap());
        store
            .insert_session_full(crate::data::fs::api_db::NewSessionRow {
                id: "sess-clone-fail",
                workdir: &cloned_path.display().to_string(),
                created_at: "2026-09-22T00:00:00Z",
                setup_status: SessionSetupStatus::Initializing,
                kind: SessionKind::Remote,
                cloned_path: Some(&cloned_path.display().to_string()),
            })
            .unwrap();
        let setup_state = Arc::new(std::sync::RwLock::new(SessionSetupState::new()));

        let setup = SessionSetup::new(
            "sess-clone-fail".to_string(),
            plan,
            test_engines(),
            Arc::new(SessionManager::in_memory()),
            Arc::clone(&setup_state),
            Arc::clone(&store),
            paths.clone(),
        );
        let mut presenter = RecordingPresenter::default();
        setup.run(&mut presenter).await;

        // The partial clone directory must be gone (the failure-cleanup rule).
        assert!(
            !cloned_path.exists(),
            "the partially-cloned directory must be deleted on clone failure"
        );
        // The clone stage was entered and broadcast.
        assert!(
            presenter
                .stage_keys()
                .contains(&"cloning_repository".to_string()),
            "the clone stage must be broadcast; got {:?}",
            presenter.stage_keys()
        );
        // The failure was surfaced once, for `clone`.
        let failures = presenter.failures();
        assert_eq!(
            failures.len(),
            1,
            "exactly one failure should be reported; got {failures:?}"
        );
        assert_eq!(failures[0].0, "clone");

        // The Layer 0 state reached the terminal failure, with the stage named.
        let final_state = setup_state.read().unwrap().clone();
        assert_eq!(final_state.status, SessionSetupStatus::Failed);
        assert_eq!(
            final_state.error.as_ref().map(|e| e.stage.as_str()),
            Some("clone")
        );

        // The orchestrator persisted the terminal status to the session row
        // and the snapshot to disk — neither went through the frontend.
        let row = store.get_session("sess-clone-fail").unwrap().unwrap();
        assert_eq!(row.setup_status, "failed");
        let snapshot_path = paths.session_setup_state_path("sess-clone-fail");
        assert!(
            snapshot_path.exists(),
            "setup_state.json must be written by the orchestrator"
        );
        let snapshot: SessionSetupState =
            serde_json::from_str(&std::fs::read_to_string(&snapshot_path).unwrap()).unwrap();
        assert_eq!(snapshot.status, SessionSetupStatus::Failed);

        // The frontend was told the run finished, and the ready stage never ran.
        assert!(presenter.finished_called, "finished() must run");
        assert!(
            !presenter.ready_frontend_called,
            "setup must not reach the ready stage"
        );
    }
}
