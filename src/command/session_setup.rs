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
use crate::engine::git::{GitFrontend, MergeRefOutcome};
use crate::engine::ready::frontend::ReadyFrontend;
use crate::engine::ready::{ReadyEngine, ReadyEngineOptions};

/// `error.stage` of a setup abandoned through [`SessionSetup::with_cancellation`].
pub const SETUP_CANCELLED_STAGE: &str = "cancelled";

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
    /// Fired to abandon the setup (a cancelled `/v1/worker/runs` command). Never fires
    /// unless the caller installs one with [`SessionSetup::with_cancellation`].
    cancel: tokio_util::sync::CancellationToken,
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
            cancel: tokio_util::sync::CancellationToken::new(),
        }
    }

    /// Observe `token`: between stages, and around the ready checks. A fired
    /// token records `failed` with `error.stage = "cancelled"`,
    /// `message = "session setup cancelled"`, deletes a remote clone, persists
    /// `"failed"` and finishes. A blocking git operation already running is
    /// not interrupted; the token is honoured at the next check.
    pub fn with_cancellation(mut self, token: tokio_util::sync::CancellationToken) -> Self {
        self.cancel = token;
        self
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
        self.state(|s| s.enter_status(status));
        let _ = self.store.update_setup_status(&self.session_id, status);
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

    /// Persist a terminal status to the session row.
    fn persist_status(&self, status: SessionSetupStatus) {
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

    /// Record a remote-stage failure and retire the run: broadcast it, delete
    /// the partial clone, persist `"failed"`, then finish. Every failure in
    /// [`Self::prepare_clone`] ends here so none can leave a clone behind.
    async fn fail_remote(
        &self,
        presenter: &mut dyn SessionSetupPresenter,
        stage: &str,
        error: &str,
    ) {
        self.mark_failed(presenter, stage, error);
        self.delete_clone().await;
        self.persist_status(SessionSetupStatus::Failed);
        self.finish(presenter).await;
    }

    /// Record a cancelled setup and retire the run: broadcast it, delete a
    /// remote session's clone, persist `"failed"`, then finish.
    async fn fail_cancelled(&self, presenter: &mut dyn SessionSetupPresenter) {
        tracing::info!(session_id = %self.session_id, "Session setup cancelled");
        self.mark_failed(presenter, SETUP_CANCELLED_STAGE, "session setup cancelled");
        if self.plan.kind == SessionKind::Remote {
            self.delete_clone().await;
        }
        self.persist_status(SessionSetupStatus::Failed);
        self.finish(presenter).await;
    }

    /// One cancellation check point: `true` once the token has fired, after
    /// the cancel has been recorded and the run retired; the caller stops.
    async fn cancelled_here(&self, presenter: &mut dyn SessionSetupPresenter) -> bool {
        if !self.cancel.is_cancelled() {
            return false;
        }
        self.fail_cancelled(presenter).await;
        true
    }

    /// Run one blocking git operation off the async runtime, turning a panic
    /// in the task into an [`EngineError::Git`] naming `what`.
    async fn blocking_git<T: Send + 'static>(
        &self,
        what: &'static str,
        op: impl FnOnce(&crate::engine::git::GitEngine) -> Result<T, EngineError> + Send + 'static,
    ) -> Result<T, EngineError> {
        let git = Arc::clone(&self.engines.git_engine);
        tokio::task::spawn_blocking(move || op(&git))
            .await
            .unwrap_or_else(|join_err| {
                Err(EngineError::Git(format!(
                    "{what} task panicked: {join_err}"
                )))
            })
    }

    /// The remote-only stages: clone the default branch, set up the session
    /// branch, then merge `merge_refs` into it. `Err(())` means a stage failed
    /// and the failure has already been recorded, the clone deleted and the
    /// run finished; the caller just stops.
    pub(crate) async fn prepare_clone(
        &self,
        presenter: &mut dyn SessionSetupPresenter,
    ) -> Result<(), ()> {
        let session_id = &self.session_id;

        // ── Stage 1: clone repository ────────────────────────────────────────
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
        let dest_for_clone = dest.clone();
        let mut clone_sink = presenter.git_log_sink();
        // Clone the repository's default branch regardless of `plan.branch`.
        // A full clone, so every remote branch is available as
        // `origin/<name>` to the branch and merge stages below. A cancel kills
        // the clone, which may otherwise wait on an unresponsive origin forever.
        let cancel = self.cancel.clone();
        let clone_result = self
            .blocking_git("clone", move |git| {
                git.clone_repo_logged(&url, None, &dest_for_clone, &mut *clone_sink, &cancel)
            })
            .await;
        if self.cancelled_here(presenter).await {
            return Err(());
        }
        if let Err(e) = clone_result {
            tracing::error!(session_id = %session_id, error = %e, "Clone failed");
            self.fail_remote(presenter, "clone", &e.to_string()).await;
            return Err(());
        }
        tracing::info!(session_id = %session_id, "Repository cloned");
        presenter.emit(SetupEventPayload::StageChanged {
            stage: "cloning_repository_done".to_string(),
            message: "Repository cloned".to_string(),
        });

        // ── Stage 2: set up branch ───────────────────────────────────────────
        let Some(branch) = self.plan.branch.clone() else {
            return Ok(());
        };
        self.enter_status(SessionSetupStatus::SettingUpBranch);
        let msg = format!("Checking out branch '{branch}'...");
        self.stage(presenter, "setting_up_branch", &msg);
        presenter.log(&format!(
            "state → {:?}: branch={branch}",
            SessionSetupStatus::SettingUpBranch
        ));
        tracing::info!(session_id = %session_id, branch = %branch, "Setting up branch");

        match self.setup_branch(presenter, &dest, &branch).await {
            Ok(disposition) => {
                tracing::info!(
                    session_id = %session_id,
                    branch = %branch,
                    disposition = %disposition,
                    "Branch ready"
                );
                presenter.emit(SetupEventPayload::StageChanged {
                    stage: "branch_ready".to_string(),
                    message: format!("Branch '{branch}' {disposition}"),
                });
            }
            Err(e) => {
                tracing::error!(session_id = %session_id, error = %e, "Branch setup failed");
                self.fail_remote(presenter, "branch", &e).await;
                return Err(());
            }
        }
        if self.cancelled_here(presenter).await {
            return Err(());
        }

        // ── Stage 2b: merge refs ─────────────────────────────────────────────
        if self.plan.merge_refs.is_empty() {
            return Ok(());
        }
        self.enter_status(SessionSetupStatus::MergingRefs);
        presenter.log(&format!(
            "state → {:?}: merge_refs={:?}",
            SessionSetupStatus::MergingRefs,
            self.plan.merge_refs
        ));
        if let Err(e) = self.merge_refs(presenter, &dest).await {
            tracing::error!(session_id = %session_id, error = %e, "Merging refs failed");
            self.fail_remote(presenter, "merging_refs", &e).await;
            return Err(());
        }
        Ok(())
    }

    /// Put the clone on `branch`. A branch already on origin is checked out
    /// (and refuses a `base_ref`, which would otherwise be silently ignored);
    /// a new one is cut from `base_ref` or `origin/HEAD`. Returns how the
    /// branch was obtained, for the `branch_ready` message.
    async fn setup_branch(
        &self,
        presenter: &mut dyn SessionSetupPresenter,
        dest: &std::path::Path,
        branch: &str,
    ) -> Result<String, String> {
        let exists_on_origin = {
            let (dest, branch) = (dest.to_path_buf(), branch.to_string());
            self.blocking_git("branch", move |git| {
                Ok(git.remote_ref_sha(&dest, "origin", &branch).is_some())
            })
            .await
            .map_err(|e| e.to_string())?
        };
        let base_ref = self.plan.base_ref.clone();
        let (dest, branch) = (dest.to_path_buf(), branch.to_string());
        let mut sink = presenter.git_log_sink();
        match (exists_on_origin, base_ref) {
            (true, Some(_)) => Err(format!(
                "branch '{branch}' already exists on origin; omit base_ref to continue it"
            )),
            (true, None) => self
                .blocking_git("branch", move |git| {
                    git.checkout_remote_branch_logged(&dest, &branch, &mut *sink)
                })
                .await
                .map(str::to_string)
                .map_err(|e| e.to_string()),
            (false, base_ref) => self
                .blocking_git("branch", move |git| {
                    let start = match base_ref {
                        Some(base) => git.resolve_base_ref_logged(&dest, &base, &mut *sink)?,
                        None => "refs/remotes/origin/HEAD".to_string(),
                    };
                    git.create_branch_from_logged(&dest, &branch, &start, &mut *sink)?;
                    Ok(format!("created from {start}"))
                })
                .await
                .map_err(|e| e.to_string()),
        }
    }

    /// Merge each `merge_refs` entry, in order, as
    /// `git merge --no-ff --no-edit origin/<ref>`. The first missing ref or
    /// conflict stops the stage; the engine has already aborted a conflicting
    /// merge, and its error names the conflicting paths.
    async fn merge_refs(
        &self,
        presenter: &mut dyn SessionSetupPresenter,
        dest: &std::path::Path,
    ) -> Result<(), String> {
        // The clone may have no `user.name` / `user.email` (a fresh server);
        // the engine supplies awman's fallback for exactly the missing keys.
        let identity = {
            let dest = dest.to_path_buf();
            self.blocking_git("merge", move |git| git.commit_identity(&dest))
                .await
                .map_err(|e| e.to_string())?
        };
        presenter.log(&identity.describe());

        let total = self.plan.merge_refs.len();
        for (i, merge_ref) in self.plan.merge_refs.iter().enumerate() {
            let msg = format!("Merging origin/{merge_ref} ({}/{total})...", i + 1);
            self.stage(presenter, "merging_refs", &msg);
            let (dest, r, id) = (dest.to_path_buf(), merge_ref.clone(), identity.clone());
            let mut sink = presenter.git_log_sink();
            let outcome = self
                .blocking_git("merge", move |git| {
                    git.merge_ref_logged(&dest, &r, &id, &mut *sink)
                })
                .await
                .map_err(|e| e.to_string())?;
            let message = match outcome {
                MergeRefOutcome::Merged { sha } => {
                    format!("Merged origin/{merge_ref} ({})", &sha[..sha.len().min(8)])
                }
                MergeRefOutcome::AlreadyUpToDate => format!("origin/{merge_ref} already merged"),
            };
            tracing::info!(session_id = %self.session_id, merge_ref = %merge_ref, "{message}");
            presenter.emit(SetupEventPayload::StageChanged {
                stage: "merge_ref_done".to_string(),
                message,
            });
        }
        Ok(())
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
        if self.cancelled_here(presenter).await {
            return;
        }

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

        // ── [remote only] Stages 1-2b: clone, branch, merges ─────────────────
        if self.plan.kind == SessionKind::Remote && self.prepare_clone(presenter).await.is_err() {
            return;
        }
        // After the merges (or straight after the clone/branch when there are
        // none), and before the session is opened.
        if self.cancelled_here(presenter).await {
            return;
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
                self.persist_status(SessionSetupStatus::Failed);
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
                self.persist_status(SessionSetupStatus::Failed);
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
                self.persist_status(SessionSetupStatus::Failed);
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
        let (resolved_builtin, data_home) = match ReadyEngine::builtin_inputs(
            &session_guard,
            container_runtime.as_ref(),
        )
        .and_then(|resolved| {
            crate::data::fs::data_paths::data_home(session_guard.effective_config().env())
                .map(|data_home| (resolved, data_home))
        }) {
            Ok(resolved) => resolved,
            Err(e) => {
                drop(session_guard);
                tracing::error!(session_id = %session_id, error = %e, "Invalid builtin configuration");
                self.mark_failed(presenter, "ready", &e.to_string());
                self.persist_status(SessionSetupStatus::Failed);
                self.finish(presenter).await;
                return;
            }
        };
        let mut ready_engine = ReadyEngine::for_session_setup(
            resolved_builtin,
            data_home,
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
        //
        // A cancel drops the ready future, which cancels an in-flight import.
        let ready_fut = ready_engine.run_to_completion(&mut *setup_frontend);
        let cancel = self.cancel.clone();
        let ready_outcome = tokio::select! {
            // A cancel and a finished ready run that are both ready: the
            // cancel wins (and is rechecked below before `ready` is published).
            biased;
            _ = cancel.cancelled() => {
                self.fail_cancelled(presenter).await;
                return;
            }
            outcome = tokio::time::timeout(Duration::from_secs(600), ready_fut) => outcome,
        };
        if self.cancelled_here(presenter).await {
            return;
        }

        match ready_outcome {
            // The engine records a failed build or import as a summary row,
            // not an error. Such a session must not be announced as ready:
            // its agent would run on a stale (or no) image.
            Ok(Ok(summary)) if !summary.is_ready() => {
                let reason = summary
                    .not_ready_reason()
                    .unwrap_or_else(|| "a required image stage did not complete".to_string());
                tracing::error!(
                    session_id = %session_id,
                    error = %reason,
                    "Session setup did not prepare its images"
                );
                // Keep the summary so clients can show which stage failed.
                self.state(|s| s.ready_summary = Some(summary.clone()));
                self.mark_failed(presenter, "ready", &reason);
                if self.plan.kind == SessionKind::Remote {
                    self.delete_clone().await;
                }
                self.persist_status(SessionSetupStatus::Failed);
            }
            Ok(Ok(summary)) => {
                self.state(|s| s.set_ready(summary.clone()));
                presenter.emit(SetupEventPayload::SetupComplete {
                    ready_summary: Box::new(summary),
                });
                self.persist_status(SessionSetupStatus::Ready);
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
                self.persist_status(SessionSetupStatus::Failed);
            }
            Err(_elapsed) => {
                let msg = "ReadyEngine exceeded the 600s setup deadline".to_string();
                tracing::error!(session_id = %session_id, "{msg}");
                self.mark_failed(presenter, "ready_timeout", &msg);
                if self.plan.kind == SessionKind::Remote {
                    self.delete_clone().await;
                }
                self.persist_status(SessionSetupStatus::Failed);
            }
        }

        self.finish(presenter).await;
    }
}
