use std::sync::{Arc, Mutex, MutexGuard};

use super::ExecWorkflowCommandFrontend;
use crate::command::commands::worktree_lifecycle::{
    ExistingWorktreeDecision, FinalizeOutcome, FinalizePolicy, MergeFailure, WorktreeLifecycle,
};
use crate::command::error::CommandError;
use crate::data::message::{UserMessage, UserMessageSink};
use crate::engine::agent_runtime::frontend::AgentFrontend;
use crate::engine::workflow::frontend::WorkflowFrontend;
use std::path::PathBuf;

#[derive(Clone)]
pub(crate) struct SharedWorkflowFrontend(Arc<Mutex<Box<dyn ExecWorkflowCommandFrontend>>>);

impl SharedWorkflowFrontend {
    pub(crate) fn new(frontend: Box<dyn ExecWorkflowCommandFrontend>) -> Self {
        Self(Arc::new(Mutex::new(frontend)))
    }

    pub(crate) fn lock(
        &self,
    ) -> std::sync::LockResult<MutexGuard<'_, Box<dyn ExecWorkflowCommandFrontend>>> {
        self.0.lock()
    }

    pub(crate) fn with<T>(
        &self,
        operation: impl FnOnce(&mut dyn ExecWorkflowCommandFrontend) -> T,
    ) -> T {
        operation(self.lock().unwrap().as_mut())
    }

    pub(crate) fn set_pty_active(&self, active: bool) {
        self.with(|frontend| frontend.set_pty_active(active));
    }

    pub(crate) fn replay_queued(&self) {
        self.with(|frontend| frontend.replay_queued());
    }

    pub(crate) fn into_agent_frontend(self) -> Box<dyn AgentFrontend> {
        Box::new(self.0)
    }

    pub(crate) fn into_workflow_frontend(self) -> Box<dyn WorkflowFrontend> {
        Box::new(self.0)
    }

    pub(crate) async fn prepare_worktree(
        &self,
        lifecycle: WorktreeLifecycle,
        preselected: Option<ExistingWorktreeDecision>,
    ) -> Result<(WorktreeLifecycle, PathBuf), CommandError> {
        let frontend = self.clone();
        tokio::task::spawn_blocking(move || {
            let path = frontend.with(|frontend| {
                tokio::runtime::Handle::current()
                    .block_on(lifecycle.prepare_with_existing(frontend, preselected))
            })?;
            Ok((lifecycle, path))
        })
        .await
        .map_err(|error| CommandError::Other(format!("worktree prepare task failed: {error}")))?
    }

    /// Runs `finalize` with the frontend locked. A requested merge-fix agent
    /// is not run here: it reports through this same frontend, so the caller
    /// runs it after this returns and then calls
    /// [`Self::complete_merge_after_fix`].
    pub(crate) async fn finalize_worktree(
        &self,
        lifecycle: &WorktreeLifecycle,
        had_error: bool,
        policy: FinalizePolicy,
    ) -> Result<FinalizeOutcome, CommandError> {
        let frontend = self.clone();
        let lifecycle = lifecycle.clone();
        tokio::task::spawn_blocking(move || {
            frontend.with(|frontend| {
                tokio::runtime::Handle::current()
                    .block_on(lifecycle.finalize(frontend, had_error, policy))
            })
        })
        .await
        .map_err(|error| CommandError::Other(format!("worktree finalize task failed: {error}")))?
    }

    pub(crate) async fn complete_merge_after_fix(
        &self,
        lifecycle: &WorktreeLifecycle,
        failure: &MergeFailure,
        agent_ran: bool,
    ) -> Result<(), CommandError> {
        let frontend = self.clone();
        let lifecycle = lifecycle.clone();
        let failure = failure.clone();
        tokio::task::spawn_blocking(move || {
            frontend.with(|frontend| {
                tokio::runtime::Handle::current()
                    .block_on(lifecycle.complete_merge_after_fix(frontend, &failure, agent_ran))
            })
        })
        .await
        .map_err(|error| CommandError::Other(format!("merge completion task failed: {error}")))?
    }

    pub(crate) fn report_workflow_context_path(&self, path: &std::path::Path) {
        self.with(|frontend| frontend.report_workflow_context_path(path));
    }
}

impl UserMessageSink for SharedWorkflowFrontend {
    fn write_message(&mut self, message: UserMessage) {
        self.with(|frontend| frontend.write_message(message));
    }

    fn replay_queued(&mut self) {
        SharedWorkflowFrontend::replay_queued(self);
    }
}
