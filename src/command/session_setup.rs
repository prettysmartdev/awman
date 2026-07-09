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
//! drift. The frontend supplies a [`SessionSetupObserver`] to receive the
//! presentation/state side effects (progress events, status persistence,
//! in-memory session registration, the ready-checks frontend) exactly the way
//! [`ReadyEngine`](crate::engine::ready::ReadyEngine) accepts a
//! [`ReadyFrontend`]. The orchestrator itself performs no HTTP, event-bus, or
//! in-memory-map work of its own.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::RwLock;

use crate::command::commands::resolve_agent;
use crate::command::dispatch::Engines;
use crate::command::session_create::SessionCreatePlan;
use crate::data::message::UserMessageSink;
use crate::data::ready_summary::ReadySummary;
use crate::data::session::{
    Session, SessionOpenOptions, SessionType, StaticGitRootResolver,
};
use crate::data::session_setup_event::SessionSetupStatus;
use crate::engine::error::EngineError;
use crate::engine::ready::frontend::ReadyFrontend;
use crate::engine::ready::{ReadyEngine, ReadyEngineOptions};

/// Presentation and frontend-state side effects the [`SessionSetup`]
/// orchestrator delegates to the calling frontend.
///
/// Every method corresponds to a side effect the API frontend previously
/// performed inline in its `run_session_setup`: emitting progress on the
/// session-setup event bus, persisting the setup status column, registering the
/// opened session in the in-memory map, and vending the ready-checks frontend.
/// The orchestrator owns the *sequence and cleanup rules*; the observer owns
/// *how each step is surfaced and persisted* for a given frontend.
#[async_trait]
pub trait SessionSetupObserver: Send {
    /// Enter a new setup status: surface it to the frontend and persist it
    /// (frontends map the enum's [`SessionSetupStatus::as_str`] to their store).
    fn enter_status(&mut self, status: SessionSetupStatus);

    /// Update the human-readable "current stage" line.
    fn set_stage(&mut self, message: &str);

    /// Emit a stage-changed progress event (`stage` is the machine key).
    fn stage_changed(&mut self, stage: &str, message: &str);

    /// Surface a terminal failure for `stage` with `error` (no persistence — the
    /// orchestrator calls [`persist_status`](Self::persist_status) separately so
    /// it controls ordering relative to on-disk clone cleanup).
    fn mark_failed(&mut self, stage: &str, error: &str);

    /// Surface successful completion carrying the ready summary.
    fn set_ready(&mut self, summary: &ReadySummary);

    /// Persist the terminal status string (`"ready"` / `"failed"`).
    fn persist_status(&mut self, status: &str);

    /// Append a line to the frontend's session-scoped setup log.
    fn log(&mut self, line: &str);

    /// Register the freshly opened session in the frontend's in-memory map.
    async fn register_session(&mut self, session: Arc<RwLock<Session>>);

    /// Vend the ready-checks frontend used to drive [`ReadyEngine`].
    fn ready_frontend(&mut self) -> Box<dyn ReadyFrontend>;

    /// Vend a message sink that captures git clone/branch output.
    fn git_log_sink(&mut self) -> Box<dyn UserMessageSink + Send>;

    /// Persist the final setup snapshot and schedule frontend-state cleanup.
    async fn persist_and_cleanup(&mut self);
}

/// Layer 2 orchestrator that drives a validated [`SessionCreatePlan`] through
/// clone → branch → open → ready, delegating presentation to a
/// [`SessionSetupObserver`].
pub struct SessionSetup {
    session_id: String,
    plan: SessionCreatePlan,
    engines: Engines,
}

impl SessionSetup {
    pub fn new(session_id: String, plan: SessionCreatePlan, engines: Engines) -> Self {
        Self {
            session_id,
            plan,
            engines,
        }
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
    /// `observer`. Returns when setup reaches a terminal state (ready or failed)
    /// and the frontend-state cleanup has been scheduled.
    pub async fn run(&self, observer: &mut dyn SessionSetupObserver) {
        let session_id = &self.session_id;

        // Delay setup work briefly so the frontend's acknowledgement (the API's
        // 202) can be flushed before any setup work runs. Critical when the
        // tokio runtime is single-threaded (e.g. `#[tokio::test]`).
        tokio::time::sleep(Duration::from_millis(50)).await;

        tracing::info!(
            session_id = %session_id,
            session_type = %self.plan.session_type,
            workdir = %self.plan.resolved_workdir.display(),
            repo_url = self.plan.repo_url.as_deref().unwrap_or(""),
            branch = self.plan.branch.as_deref().unwrap_or(""),
            "Beginning session setup"
        );

        observer.log(&format!(
            "state → {:?}: starting setup (type={}, workdir={})",
            SessionSetupStatus::Initializing,
            self.plan.session_type,
            self.plan.resolved_workdir.display()
        ));

        // ── [remote only] Stage 1: clone repository ──────────────────────────
        if self.plan.session_type == "remote" {
            observer.enter_status(SessionSetupStatus::CloningRepository);
            let msg = format!(
                "Cloning {}...",
                self.plan.repo_url.as_deref().unwrap_or("repository")
            );
            observer.set_stage(&msg);
            observer.stage_changed("cloning_repository", &msg);
            observer.log(&format!(
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
            let mut clone_sink = observer.git_log_sink();
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
                observer.mark_failed("clone", &e.to_string());
                // Cleanup any partial clone.
                self.delete_clone().await;
                observer.persist_status("failed");
                observer.persist_and_cleanup().await;
                return;
            }
            tracing::info!(session_id = %session_id, "Repository cloned");
            observer.stage_changed("cloning_repository_done", "Repository cloned");

            // ── [remote only] Stage 2: set up branch ─────────────────────────
            if let Some(branch) = self.plan.branch.as_deref() {
                observer.enter_status(SessionSetupStatus::SettingUpBranch);
                let msg = format!("Checking out branch '{branch}'...");
                observer.set_stage(&msg);
                observer.stage_changed("setting_up_branch", &msg);
                observer.log(&format!(
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
                let mut branch_sink = observer.git_log_sink();
                let branch_result = tokio::task::spawn_blocking(move || {
                    git.checkout_or_create_branch_logged(
                        &dest_for_branch,
                        &branch_owned,
                        &mut *branch_sink,
                    )
                })
                .await
                .unwrap_or_else(|join_err| {
                    Err(EngineError::Git(format!("branch task panicked: {join_err}")))
                });
                match branch_result {
                    Ok(disposition) => {
                        tracing::info!(
                            session_id = %session_id,
                            branch = %branch,
                            disposition = disposition,
                            "Branch ready"
                        );
                        observer.stage_changed(
                            "branch_ready",
                            &format!("Branch '{branch}' {disposition}"),
                        );
                    }
                    Err(e) => {
                        tracing::error!(session_id = %session_id, error = %e, "Branch setup failed");
                        observer.mark_failed("branch", &e.to_string());
                        self.delete_clone().await;
                        observer.persist_status("failed");
                        observer.persist_and_cleanup().await;
                        return;
                    }
                }
            }
        }

        // ── Stage 3 (all): open Session ──────────────────────────────────────
        observer.enter_status(SessionSetupStatus::RunningReady);
        observer.set_stage("Opening session...");
        observer.stage_changed(
            "running_ready",
            "Opening session and running ready checks...",
        );
        observer.log(&format!(
            "state → {:?}: opening session at {}",
            SessionSetupStatus::RunningReady,
            self.plan.resolved_workdir.display()
        ));
        tracing::info!(
            session_id = %session_id,
            workdir = %self.plan.resolved_workdir.display(),
            "Opening session"
        );

        let resolver = StaticGitRootResolver::new(&self.plan.resolved_workdir);
        let session = match Session::open_or_workdir_fallback(
            self.plan.resolved_workdir.clone(),
            &resolver,
            SessionOpenOptions::default(),
        ) {
            Ok(s) => Arc::new(RwLock::new(s)),
            Err(e) => {
                tracing::error!(
                    session_id = %session_id,
                    error = %e,
                    "Session setup failed: could not open session"
                );
                observer.mark_failed("session_open", &e.to_string());
                if self.plan.session_type == "remote" {
                    self.delete_clone().await;
                }
                observer.persist_status("failed");
                observer.persist_and_cleanup().await;
                return;
            }
        };

        // For remote sessions, replace the default Local session_type so that
        // downstream consumers (e.g. worktree suppression in ExecWorkflowCommand)
        // see the correct variant.
        if self.plan.session_type == "remote" {
            if let Some(cloned_path) = self.plan.cloned_path.clone() {
                let repo_url = self.plan.repo_url.clone().unwrap_or_default();
                let branch = self.plan.branch.clone().unwrap_or_default();
                session
                    .write()
                    .await
                    .set_session_type(SessionType::Remote {
                        repo_url,
                        branch,
                        cloned_path,
                    });
            }
        }

        observer.register_session(Arc::clone(&session)).await;
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
        let agent = match resolve_agent(&None, &session_guard) {
            Ok(a) => a,
            Err(e) => {
                drop(session_guard);
                tracing::error!(session_id = %session_id, error = %e, "Failed to resolve agent");
                observer.mark_failed("resolve_agent", &e.to_string());
                observer.persist_status("failed");
                observer.persist_and_cleanup().await;
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
                observer.mark_failed("ready", &e.to_string());
                observer.persist_status("failed");
                observer.persist_and_cleanup().await;
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

        let mut setup_frontend = observer.ready_frontend();

        // Cap ReadyEngine at 10 minutes — any legitimate run, including a clean
        // base-image build, completes well within this. If the wall-clock exceeds
        // the cap (e.g. Docker daemon is unresponsive), mark the setup as failed
        // so the session row reaches a terminal state and the bus is cleaned up.
        let ready_fut = ready_engine.run_to_completion(&mut *setup_frontend);
        let ready_outcome = tokio::time::timeout(Duration::from_secs(600), ready_fut).await;

        match ready_outcome {
            Ok(Ok(summary)) => {
                observer.set_ready(&summary);
                observer.persist_status("ready");
                tracing::info!(session_id = %session_id, "Session setup complete");
            }
            Ok(Err(e)) => {
                tracing::error!(
                    session_id = %session_id,
                    error = %e,
                    "Session setup failed during ready"
                );
                observer.mark_failed("ready", &e.to_string());
                if self.plan.session_type == "remote" {
                    self.delete_clone().await;
                }
                observer.persist_status("failed");
            }
            Err(_elapsed) => {
                let msg = "ReadyEngine exceeded the 600s setup deadline".to_string();
                tracing::error!(session_id = %session_id, "{msg}");
                observer.mark_failed("ready_timeout", &msg);
                if self.plan.session_type == "remote" {
                    self.delete_clone().await;
                }
                observer.persist_status("failed");
            }
        }

        observer.persist_and_cleanup().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::message::RecordingMessageSink;

    /// Records the observer callbacks a run makes so tests can assert on the
    /// sequence and terminal state without a real frontend, event bus, or store.
    #[derive(Default)]
    struct RecordingObserver {
        statuses: Vec<SessionSetupStatus>,
        failures: Vec<(String, String)>,
        persisted_statuses: Vec<String>,
        registered: bool,
        ready_frontend_called: bool,
        cleanup_called: bool,
    }

    #[async_trait]
    impl SessionSetupObserver for RecordingObserver {
        fn enter_status(&mut self, status: SessionSetupStatus) {
            self.statuses.push(status);
        }
        fn set_stage(&mut self, _message: &str) {}
        fn stage_changed(&mut self, _stage: &str, _message: &str) {}
        fn mark_failed(&mut self, stage: &str, error: &str) {
            self.failures.push((stage.to_string(), error.to_string()));
        }
        fn set_ready(&mut self, _summary: &ReadySummary) {}
        fn persist_status(&mut self, status: &str) {
            self.persisted_statuses.push(status.to_string());
        }
        fn log(&mut self, _line: &str) {}
        async fn register_session(&mut self, _session: Arc<RwLock<Session>>) {
            self.registered = true;
        }
        fn ready_frontend(&mut self) -> Box<dyn ReadyFrontend> {
            // Only reached once setup gets all the way to the ready stage; the
            // clone-failure test below never gets here.
            self.ready_frontend_called = true;
            unreachable!("ready_frontend must not be reached on the clone-failure path");
        }
        fn git_log_sink(&mut self) -> Box<dyn UserMessageSink + Send> {
            Box::new(RecordingMessageSink::new())
        }
        async fn persist_and_cleanup(&mut self) {
            self.cleanup_called = true;
        }
    }

    fn test_engines() -> Engines {
        let runtime = Arc::new(crate::engine::container::ContainerRuntime::docker());
        let overlay = Arc::new(crate::engine::overlay::OverlayEngine::with_auth_resolver(
            crate::data::fs::auth_paths::AuthPathResolver::at_home(std::path::PathBuf::from("/tmp")),
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
            Arc::new(crate::data::EngineWorkflowStateStore::at_git_root(
                tmp.path(),
            ))
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
            session_type: "remote".to_string(),
            resolved_workdir: cloned_path.clone(),
            cloned_path: Some(cloned_path.clone()),
            // A path that does not exist → `git clone` fails immediately.
            repo_url: Some(
                root.path()
                    .join("no-such-repo.git")
                    .display()
                    .to_string(),
            ),
            branch: None,
        };

        let setup = SessionSetup::new("sess-clone-fail".to_string(), plan, test_engines());
        let mut observer = RecordingObserver::default();
        setup.run(&mut observer).await;

        // The partial clone directory must be gone (the failure-cleanup rule).
        assert!(
            !cloned_path.exists(),
            "the partially-cloned directory must be deleted on clone failure"
        );
        // The clone stage was entered and the failure was surfaced for `clone`.
        assert!(observer.statuses.contains(&SessionSetupStatus::CloningRepository));
        assert_eq!(
            observer.failures.len(),
            1,
            "exactly one failure should be reported; got {:?}",
            observer.failures
        );
        assert_eq!(observer.failures[0].0, "clone");
        // Terminal state persisted as failed, cleanup scheduled, ready never run.
        assert_eq!(observer.persisted_statuses, vec!["failed".to_string()]);
        assert!(observer.cleanup_called, "persist_and_cleanup must run");
        assert!(
            !observer.registered && !observer.ready_frontend_called,
            "setup must not reach session registration or the ready stage"
        );
    }
}
