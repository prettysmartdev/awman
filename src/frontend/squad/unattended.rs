//! The unattended frontend the squad daemon runs agents and workflows with.
//!
//! squad's whole premise is that no human is present, so every question this
//! frontend is asked has exactly one safe answer and none of them block:
//!
//! * the mount-scope question is answered with the scope captured when the
//!   task was created — it is never widened;
//! * the workflow control board auto-advances rather than waiting for a key;
//! * a step failure is never put to a user: the engine's unattended path runs
//!   its countdown, retries the step once, and fails the run on a second
//!   failure (WI-0115 §3);
//! * a persisted workflow state is discarded so every scheduled run starts
//!   over — the one frontend that deliberately declines to resume;
//! * agent setup and credential consent are accepted, because the task's
//!   agents were already validated against the repo at creation time.
//!
//! Agent output goes to a per-container file in the task run directory rather
//! than the daemon log, and so does every setup/teardown step's output
//! (`setup-<n>-<step>.log` / `teardown-<n>-<step>.log`, WI 0112 Part 5).
//! Every failure line this frontend writes to the daemon log names the file
//! to open. This module holds no policy of its own: every value it returns
//! is either a constant or the task's own captured setting.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use crate::command::commands::agent_auth::{AgentAuthDecision, AgentAuthFrontend};
use crate::command::commands::agent_setup::{AgentSetupDecision, AgentSetupFrontend};
use crate::command::commands::exec_workflow::{
    ExecWorkflowCommandFrontend, WorkflowResumeDecision, WorkflowResumePrompt, WorkflowSummary,
};
use crate::command::commands::mount_scope::{MountScopeDecision, MountScopeFrontend};
use crate::command::commands::squad::evaluation::SquadRunFrontends;
use crate::command::commands::worktree_lifecycle::{
    ExistingWorktreeDecision, PostWorkflowWorktreeAction, PostWorkflowWorktreePrompt,
    PreWorktreeDecision, WorktreeLifecycleFrontend, WorktreeMergeMode,
};
use crate::command::error::CommandError;
use crate::command::headless::HeadlessDefaults;
use crate::data::fs::{RunId, SharedSquadRunLog, SquadRunLog, SquadRunLogError, SquadRunLogs};
use crate::data::message::{MessageLevel, UserMessage, UserMessageSink};
use crate::data::session::AgentName;
use crate::data::workflow_definition::WorkflowStep;
use crate::data::workflow_state::{PhaseKind, WorkflowState};
use crate::engine::agent_runtime::frontend::{AgentFrontend, AgentIo, AgentProgress, AgentStatus};
use crate::engine::error::EngineError;
use crate::engine::workflow::actions::{
    AvailableActions, NextAction, ResumeMismatch, StepOutput, WorkflowOutcome, WorkflowStepStatus,
    YoloTickOutcome,
};
use crate::engine::workflow::frontend::WorkflowFrontend;

/// Builds the daemon's unattended frontends.
pub struct UnattendedFrontends;

impl UnattendedFrontends {
    pub fn shared() -> Arc<dyn SquadRunFrontends> {
        Arc::new(Self)
    }
}

impl SquadRunFrontends for UnattendedFrontends {
    fn leader_frontend(
        &self,
        task: &str,
        run_id: &RunId,
        run_log_dir: &Path,
        label: &str,
        mount_scope: MountScopeDecision,
    ) -> Result<Box<dyn AgentFrontend>, CommandError> {
        Ok(Box::new(UnattendedFrontend::for_run(
            task,
            run_id,
            run_log_dir,
            label,
            mount_scope,
        )?))
    }

    fn workflow_frontend(
        &self,
        task: &str,
        run_id: &RunId,
        run_log_dir: &Path,
        mount_scope: MountScopeDecision,
    ) -> Result<Box<dyn ExecWorkflowCommandFrontend>, CommandError> {
        Ok(Box::new(UnattendedFrontend::for_run(
            task,
            run_id,
            run_log_dir,
            "workflow",
            mount_scope,
        )?))
    }
}

/// One unattended run's frontend.
pub struct UnattendedFrontend {
    context: String,
    task: String,
    run_id: RunId,
    /// The run directory the scheduler created before evaluation was
    /// dispatched, wrapped in the Layer 0 type that owns its file layout. Each
    /// `AgentStatus::Running` opens its own `<container-name>.log` through it
    /// before the runtime starts the container subprocess.
    logs: SquadRunLogs,
    pending_log_files: VecDeque<SharedSquadRunLog>,
    /// Every answer this run gives to a question it cannot ask a human,
    /// including the task's captured mount scope. See
    /// `src/command/headless.rs` for the table and why the squad row differs
    /// from the API's and the CLI's.
    headless: HeadlessDefaults,
    /// The setup/teardown step whose output is being written right now (WI
    /// 0112 Part 5). `None` outside a phase step, which is the normal state
    /// while agent steps run.
    phase_log: Option<SquadRunLog>,
    /// How many setup / teardown steps have started, for the `<n>` in the
    /// step log's filename. The engine fires the hooks strictly in definition
    /// order and never concurrently, so a counter is reliable.
    setup_steps_seen: usize,
    teardown_steps_seen: usize,
    /// The workflow step the engine most recently reported `Running`. The
    /// engine fires that *before* it launches the step's container, so the
    /// next `AgentStatus::Running` belongs to this step.
    current_step: Option<String>,
    /// Container log path per workflow step name, so a step's failure line
    /// can name the file.
    step_logs: HashMap<String, PathBuf>,
    /// The most recently opened container log, for the remediation separator.
    last_container_log: Option<PathBuf>,
}

impl UnattendedFrontend {
    /// Test-only construction. Production always uses `for_run`, which
    /// receives a scheduler-created directory and can report setup errors.
    #[cfg(test)]
    fn new(context: String) -> Self {
        Self::with_mount_scope(context, MountScopeDecision::MountGitRoot)
    }

    #[cfg(test)]
    fn with_mount_scope(context: String, mount_scope: MountScopeDecision) -> Self {
        Self {
            task: context.clone(),
            context,
            run_id: RunId::new(),
            logs: SquadRunLogs::new(std::env::temp_dir().join("awman-unattended-test-logs")),
            pending_log_files: VecDeque::new(),
            headless: HeadlessDefaults::squad(mount_scope),
            phase_log: None,
            setup_steps_seen: 0,
            teardown_steps_seen: 0,
            current_step: None,
            step_logs: HashMap::new(),
            last_container_log: None,
        }
    }

    fn for_run(
        task: &str,
        run_id: &RunId,
        run_log_dir: &Path,
        label: &str,
        mount_scope: MountScopeDecision,
    ) -> Result<Self, CommandError> {
        let logs = SquadRunLogs::new(run_log_dir);
        if !logs.is_prepared() {
            return Err(CommandError::Other(format!(
                "squad run log directory was not prepared before container launch: {}",
                run_log_dir.display()
            )));
        }
        Ok(Self {
            context: format!("{task}/{label}"),
            task: task.to_string(),
            run_id: run_id.clone(),
            logs,
            pending_log_files: VecDeque::new(),
            headless: HeadlessDefaults::squad(mount_scope),
            phase_log: None,
            setup_steps_seen: 0,
            teardown_steps_seen: 0,
            current_step: None,
            step_logs: HashMap::new(),
            last_container_log: None,
        })
    }

    // ── setup / teardown step logs (WI 0112 Part 5) ──────────────────────

    /// Open the log file for a phase step that has just started, write its
    /// header, and record the start in the daemon log. An open failure is
    /// logged and the step runs unlogged rather than not at all.
    fn begin_phase_step(&mut self, phase: &str, index: usize, description: &str) {
        self.finish_phase_log();
        match self.logs.open_step_log(phase, index, description) {
            Ok(log) => {
                tracing::info!(
                    task = %self.task,
                    run_id = %self.run_id,
                    step = description,
                    log_path = %log.path().display(),
                    "squad {phase} step started"
                );
                self.phase_log = Some(log);
            }
            Err(error) => tracing::error!(
                task = %self.task,
                run_id = %self.run_id,
                step = description,
                error = %error,
                "squad failed to open {phase} step log"
            ),
        }
    }

    /// One output line from the running phase step. Flushed per line, the
    /// same durability rule `spawn_file_drain` applies to agent output.
    fn phase_step_line(&mut self, line: &str) {
        if let Some(log) = self.phase_log.as_mut() {
            log.write_line(line);
        }
    }

    /// A remediation attempt for the running phase step: a separator into the
    /// *same* file, so the original output and every retry read in order. The
    /// remediation agent's own container log is named once it is known.
    fn phase_step_fixing(&mut self, phase: &str, description: &str, attempt: u32, of: u32) {
        let agent_log = self
            .last_container_log
            .as_ref()
            .map(|p| format!(" (agent log: {})", p.display()))
            .unwrap_or_default();
        self.phase_step_line(&format!(
            "# on_failure remediation attempt {attempt}/{of}{agent_log}"
        ));
        tracing::info!(
            task = %self.task,
            run_id = %self.run_id,
            step = description,
            attempt,
            of,
            "squad {phase} step remediation started"
        );
    }

    fn phase_step_completed(&mut self, phase: &str, description: &str) {
        let path = self.finish_phase_log();
        tracing::info!(
            task = %self.task,
            run_id = %self.run_id,
            step = description,
            log_path = %path.as_deref().map(|p| p.display().to_string()).unwrap_or_default(),
            "squad {phase} step succeeded"
        );
    }

    /// The step failed for good (remediation, if any, is exhausted). The
    /// engine's `stderr` argument is appended — it is the launch error when
    /// the command never ran, and would otherwise be lost — and the error
    /// line names the file.
    fn phase_step_failed(&mut self, phase: &str, description: &str, exit_code: i32, stderr: &str) {
        if !stderr.is_empty() {
            self.phase_step_line(&format!("# failed (exit {exit_code}):"));
            self.phase_step_line(stderr.trim_end());
        }
        let path = self.finish_phase_log();
        tracing::error!(
            task = %self.task,
            run_id = %self.run_id,
            step = description,
            exit_code,
            log_path = %path.as_deref().map(|p| p.display().to_string()).unwrap_or_default(),
            error = stderr.lines().next().unwrap_or(""),
            "squad {phase} step failed"
        );
    }

    /// Flush and close the open phase log, returning its path.
    fn finish_phase_log(&mut self) -> Option<PathBuf> {
        Some(self.phase_log.take()?.finish())
    }

    fn prepare_container_log(&mut self, container_name: &str) {
        match self.logs.open_container_log(container_name) {
            Ok(log) => {
                let path = log.path().to_path_buf();
                self.pending_log_files.push_back(log);
                tracing::info!(
                    task = %self.task,
                    run_id = %self.run_id,
                    container = %container_name,
                    log_path = %path.display(),
                    "squad agent container launched"
                );
                // WI 0112 Part 5: remember which step this container serves,
                // so the step's failure line can name this file.
                if let Some(step) = self.current_step.clone() {
                    self.step_logs.insert(step, path.clone());
                }
                self.last_container_log = Some(path);
            }
            // A name that is not a single path component reached us from a
            // container backend, never from squad's own validated slug helper.
            Err(SquadRunLogError::UnsafeName { name }) => tracing::error!(
                task = %self.task,
                run_id = %self.run_id,
                container = %name,
                "squad refused unsafe container-log filename"
            ),
            Err(error) => tracing::error!(
                task = %self.task,
                run_id = %self.run_id,
                container = %container_name,
                error = %error,
                "squad failed to open per-container log"
            ),
        }
    }
}

impl Drop for UnattendedFrontend {
    /// A frontend torn down mid-step (the workflow aborted) still flushes
    /// whatever the open phase log holds.
    fn drop(&mut self) {
        self.finish_phase_log();
    }
}

/// F-45: takes the default `command_started`, so the `$ git …` echo line
/// is byte-identical to the one `run_git_logged` composed before.
impl crate::engine::git::GitFrontend for UnattendedFrontend {}

impl UserMessageSink for UnattendedFrontend {
    fn write_message(&mut self, message: UserMessage) {
        match message.level {
            MessageLevel::Error => {
                tracing::error!(task = %self.task, run_id = %self.run_id, squad = %self.context, "{}", message.text)
            }
            MessageLevel::Warning => {
                tracing::warn!(task = %self.task, run_id = %self.run_id, squad = %self.context, "{}", message.text)
            }
            _ => {
                tracing::info!(task = %self.task, run_id = %self.run_id, squad = %self.context, "{}", message.text)
            }
        }
    }
    fn replay_queued(&mut self) {}
}

#[async_trait]
impl AgentFrontend for UnattendedFrontend {
    fn report_status(&mut self, status: AgentStatus) {
        match status {
            AgentStatus::Running { container_name } => self.prepare_container_log(&container_name),
            AgentStatus::Building | AgentStatus::Pulling | AgentStatus::Starting => tracing::info!(
                task = %self.task,
                run_id = %self.run_id,
                status = ?status,
                "squad agent lifecycle transition"
            ),
            AgentStatus::Stopping | AgentStatus::Exited(_) | AgentStatus::Failed(_) => {
                tracing::info!(
                    task = %self.task,
                    run_id = %self.run_id,
                    status = ?status,
                    "squad agent lifecycle transition"
                )
            }
        }
    }

    fn report_progress(&mut self, _progress: AgentProgress) {}

    /// Every squad agent gets a PTY even when nobody is attached. This makes
    /// its process the same interactive agent TUI a later attach reconnects
    /// to, while the fixed 120x40 size is large enough for supported CLIs and
    /// does not depend on a daemon-owned terminal.
    fn take_io(&mut self) -> AgentIo {
        let (stdout_tx, stdout_rx) = tokio::sync::mpsc::unbounded_channel();
        let (stderr_tx, stderr_rx) = tokio::sync::mpsc::unbounded_channel();
        let (stdin_tx, stdin_rx) = tokio::sync::mpsc::unbounded_channel();
        let log_file = self.pending_log_files.pop_front();
        spawn_file_drain(log_file.clone(), stdout_rx);
        spawn_file_drain(log_file, stderr_rx);
        AgentIo {
            stdout: stdout_tx,
            stderr: stderr_tx,
            stdin_tx,
            stdin_rx,
            resize: Some(tokio::sync::mpsc::unbounded_channel().1),
            initial_size: Some((120, 40)),
        }
    }
}

fn spawn_file_drain(
    log_file: Option<SharedSquadRunLog>,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
) {
    tokio::spawn(async move {
        while let Some(bytes) = rx.recv().await {
            // The PTY bridge delivers one merged stream through stdout. Keep
            // the same shared file for stderr too, which also preserves a
            // faithful interleaving should a runtime ever take the piped path.
            if let Some(log) = &log_file {
                log.write_bytes(&bytes);
            }
        }
    });
}

/// The container-side sink `UnattendedFrontend` hands to Layer 1 when an
/// engine asks for one through [`HasAgentFrontend`].
///
/// It writes what `UnattendedFrontend` itself writes — a `<container>.log`
/// under the run directory, opened when the container reports `Running` and
/// fed by the same drain tasks — but as a standalone object, because
/// `container_frontend` must hand back an owned `Box<dyn AgentFrontend>`
/// rather than a borrow of the frontend.
///
/// The squad workflow path does not use it: `exec workflow` binds
/// `UnattendedFrontend`'s own `AgentFrontend` impl through the shared handle.
/// It exists for the image-setup calls (`AgentEngine::ensure_available`) that
/// the `AgentLaunchFrontend` bound covers.
struct UnattendedContainerSink {
    logs: SquadRunLogs,
    task: String,
    run_id: RunId,
    pending: Option<SharedSquadRunLog>,
}

impl UserMessageSink for UnattendedContainerSink {
    fn write_message(&mut self, msg: UserMessage) {
        tracing::info!(task = %self.task, run_id = %self.run_id, text = %msg.text, "squad container message");
    }
    fn replay_queued(&mut self) {}
}

#[async_trait]
impl AgentFrontend for UnattendedContainerSink {
    fn report_status(&mut self, status: AgentStatus) {
        if let AgentStatus::Running { container_name } = &status {
            match self.logs.open_container_log(container_name) {
                Ok(log) => self.pending = Some(log),
                Err(error) => tracing::error!(
                    task = %self.task,
                    run_id = %self.run_id,
                    container = %container_name,
                    error = %error,
                    "squad failed to open per-container log"
                ),
            }
        }
        tracing::info!(
            task = %self.task,
            run_id = %self.run_id,
            status = ?status,
            "squad container lifecycle transition"
        );
    }

    fn report_progress(&mut self, _progress: AgentProgress) {}

    fn take_io(&mut self) -> AgentIo {
        let (stdout_tx, stdout_rx) = tokio::sync::mpsc::unbounded_channel();
        let (stderr_tx, stderr_rx) = tokio::sync::mpsc::unbounded_channel();
        let (stdin_tx, stdin_rx) = tokio::sync::mpsc::unbounded_channel();
        let log_file = self.pending.take();
        spawn_file_drain(log_file.clone(), stdout_rx);
        spawn_file_drain(log_file, stderr_rx);
        AgentIo {
            stdout: stdout_tx,
            stderr: stderr_tx,
            stdin_tx,
            stdin_rx,
            resize: None,
            initial_size: None,
        }
    }
}

impl crate::command::commands::agent_setup::HasAgentFrontend for UnattendedFrontend {
    fn container_frontend(&mut self) -> Box<dyn AgentFrontend> {
        Box::new(UnattendedContainerSink {
            logs: self.logs.clone(),
            task: self.task.clone(),
            run_id: self.run_id.clone(),
            pending: None,
        })
    }
}

/// Nothing is attached to an unattended squad run, so there is no host stdio
/// to gate and no stuck dialog to colour.
impl crate::command::commands::agent_setup::AgentLaunchFrontend for UnattendedFrontend {
    fn set_pty_active(&mut self, _active: bool) {}
}

impl WorkflowFrontend for UnattendedFrontend {
    fn show_workflow_control_board(
        &mut self,
        _state: &WorkflowState,
        available: &AvailableActions,
    ) -> Result<NextAction, EngineError> {
        Ok(self.headless.workflow_next_action(available))
    }

    fn yolo_countdown_tick(
        &mut self,
        _step_name: &str,
        _remaining: Duration,
        _total: Duration,
    ) -> Result<YoloTickOutcome, EngineError> {
        Ok(self.headless.yolo_tick())
    }

    fn report_step_status(&mut self, step: &WorkflowStep, status: WorkflowStepStatus) {
        match status {
            // The engine reports `Running` before it launches the container,
            // so the next `AgentStatus::Running` is this step's.
            WorkflowStepStatus::Running => {
                self.current_step = Some(step.name.clone());
            }
            // WI 0112 Part 5: a step container that exited non-zero on its
            // own is an error line that names its log. (A yolo-countdown kill
            // never arrives here as `Failed`: the engine marks that step
            // succeeded and moves on.)
            WorkflowStepStatus::Failed { exit_code } => {
                let log_path = self
                    .step_logs
                    .get(&step.name)
                    .map(|p| p.display().to_string())
                    .unwrap_or_default();
                tracing::error!(
                    task = %self.task,
                    run_id = %self.run_id,
                    step = %step.name,
                    exit_code,
                    log_path = %log_path,
                    "squad workflow step failed"
                );
                return;
            }
            _ => {}
        }
        tracing::info!(
            task = %self.task,
            run_id = %self.run_id,
            step = %step.name,
            ?status,
            "squad workflow step lifecycle transition"
        );
    }

    fn report_step_output(&mut self, _step: &WorkflowStep, _output: StepOutput) {}

    // ── setup / teardown steps (WI 0112 Part 5) ──────────────────────────

    fn on_phase_step_started(&mut self, kind: PhaseKind, description: &str) {
        let seen = match kind {
            PhaseKind::Setup => {
                self.setup_steps_seen += 1;
                self.setup_steps_seen
            }
            PhaseKind::Teardown => {
                self.teardown_steps_seen += 1;
                self.teardown_steps_seen
            }
        };
        self.begin_phase_step(kind.label(), seen, description);
    }
    fn on_phase_step_output(&mut self, _kind: PhaseKind, line: &str) {
        self.phase_step_line(line);
    }
    fn on_phase_step_completed(&mut self, kind: PhaseKind, description: &str) {
        self.phase_step_completed(kind.label(), description);
    }
    fn on_phase_step_failed(
        &mut self,
        kind: PhaseKind,
        description: &str,
        exit_code: i32,
        stderr: &str,
    ) {
        self.phase_step_failed(kind.label(), description, exit_code, stderr);
    }
    fn on_phase_step_fixing(&mut self, kind: PhaseKind, description: &str, attempt: u32, of: u32) {
        self.phase_step_fixing(kind.label(), description, attempt, of);
    }

    fn report_workflow_completed(&mut self, outcome: &WorkflowOutcome) {
        tracing::info!(task = %self.task, run_id = %self.run_id, ?outcome, "squad workflow completed");
    }

    fn confirm_resume(&mut self, _mismatch: &ResumeMismatch) -> Result<bool, EngineError> {
        Ok(self.headless.confirm_resume())
    }

    // `supports_interactive_recovery` keeps its `false` default: nobody can
    // choose, so a failed step gets the engine's one automatic retry and then
    // ends the run (WI-0115 §3); the scheduler records the failure and backs
    // the task off.
}

/// Every answer below delegates to [`HeadlessDefaults::squad`], the Layer 2
/// profile that owns the squad daemon's headless policy: never touch the
/// user's working tree or branches, and start each scheduled evaluation
/// fresh. See `src/command/headless.rs` for the table and for how it differs
/// from the API's and the CLI's.
impl MountScopeFrontend for UnattendedFrontend {
    fn ask_mount_scope(
        &mut self,
        _git_root: &Path,
        _cwd: &Path,
    ) -> Result<MountScopeDecision, CommandError> {
        Ok(self.headless.mount_scope())
    }
}

impl AgentSetupFrontend for UnattendedFrontend {
    fn ask_agent_setup(
        &mut self,
        _requested: &AgentName,
        _default: &AgentName,
        default_available: bool,
        _image_only: bool,
    ) -> Result<AgentSetupDecision, CommandError> {
        Ok(self.headless.agent_setup(default_available))
    }

    fn record_fallback(&mut self, requested: &AgentName, fallback: &AgentName) {
        tracing::warn!(
            task = %self.task,
            run_id = %self.run_id,
            requested = requested.as_str(),
            fallback = fallback.as_str(),
            "squad fell back to a different agent"
        );
    }
}

impl AgentAuthFrontend for UnattendedFrontend {
    fn ask_agent_auth_consent(
        &mut self,
        _agent: &AgentName,
        _env_var_names: &[&str],
    ) -> Result<AgentAuthDecision, CommandError> {
        Ok(self.headless.agent_auth_consent())
    }
}

impl WorktreeLifecycleFrontend for UnattendedFrontend {
    fn ask_pre_worktree_uncommitted_files(
        &mut self,
        _files: &[String],
        suggested_message: &str,
    ) -> Result<PreWorktreeDecision, CommandError> {
        Ok(self
            .headless
            .pre_worktree_uncommitted_files(suggested_message))
    }

    fn ask_existing_worktree(
        &mut self,
        _path: &Path,
        _branch: &str,
    ) -> Result<ExistingWorktreeDecision, CommandError> {
        Ok(self.headless.existing_worktree())
    }

    fn report_worktree_created(&mut self, path: &Path, branch: &str) {
        tracing::info!(task = %self.task, run_id = %self.run_id, path = %path.display(), branch, "squad worktree created");
    }

    fn ask_post_workflow_action(
        &mut self,
        prompt: &PostWorkflowWorktreePrompt,
    ) -> Result<PostWorkflowWorktreeAction, CommandError> {
        Ok(self.headless.post_workflow_action(prompt))
    }

    fn ask_worktree_commit_before_merge(
        &mut self,
        _branch: &str,
        _files: &[String],
        suggested_message: &str,
    ) -> Result<Option<String>, CommandError> {
        Ok(self
            .headless
            .worktree_commit_before_merge(suggested_message))
    }

    fn ask_merge_mode(&mut self, _branch: &str) -> Result<WorktreeMergeMode, CommandError> {
        Ok(self.headless.merge_mode())
    }

    fn confirm_worktree_cleanup(
        &mut self,
        _branch: &str,
        _path: &Path,
    ) -> Result<bool, CommandError> {
        Ok(self.headless.confirm_worktree_cleanup())
    }

    fn report_merge_conflict(&mut self, branch: &str, wt: &Path, _root: &Path) {
        tracing::warn!(
            task = %self.task,
            run_id = %self.run_id,
            branch,
            worktree = %wt.display(),
            "squad workflow left a merge conflict"
        );
    }

    fn report_worktree_discarded(&mut self, branch: &str) {
        tracing::info!(task = %self.task, run_id = %self.run_id, branch, "squad worktree discarded");
    }

    fn report_worktree_kept(&mut self, path: &Path, branch: &str) {
        tracing::info!(task = %self.task, run_id = %self.run_id, path = %path.display(), branch, "squad worktree kept");
    }
}

impl ExecWorkflowCommandFrontend for UnattendedFrontend {
    fn report_workflow_summary(&mut self, summary: &WorkflowSummary) {
        tracing::info!(
            task = %self.task,
            run_id = %self.run_id,
            completed = summary.steps_completed,
            failed = summary.steps_failed,
            "squad workflow summary"
        );
    }

    fn ask_workflow_resume(
        &mut self,
        prompt: &WorkflowResumePrompt,
    ) -> Result<WorkflowResumeDecision, CommandError> {
        Ok(self.headless.workflow_resume(prompt))
    }

    fn notify_dynamic_workflow_resume_unavailable(
        &mut self,
        work_item: u32,
        reason: &str,
    ) -> Result<(), CommandError> {
        tracing::info!(
            task = %self.task,
            run_id = %self.run_id,
            work_item,
            reason,
            "cannot resume the previous dynamic workflow"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    // ── WI 0112 Part 5: setup/teardown step logs and failure lines ──────

    fn run_frontend(tmp: &Path) -> UnattendedFrontend {
        UnattendedFrontend::for_run(
            "task",
            &RunId("run-0112".into()),
            tmp,
            "workflow",
            MountScopeDecision::MountGitRoot,
        )
        .unwrap()
    }

    fn step(name: &str) -> WorkflowStep {
        WorkflowStep {
            name: name.to_string(),
            depends_on: Vec::new(),
            prompt_template: String::new(),
            agent: None,
            model: None,
            overlays: None,
            abort_on_failure: false,
        }
    }

    // The slug and filename rules moved to `SquadRunLogs` (Layer 0) in WI
    // 0113 F-02 and are tested there; what stays here is that the frontend
    // writes the right *content* through them.

    #[test]
    fn a_setup_step_writes_its_output_to_a_numbered_file_and_logs_the_path() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("setup-1-clone-repo-git-example.log");
        let log = captured_tracing(|| {
            let mut frontend = run_frontend(tmp.path());
            frontend.on_phase_step_started(PhaseKind::Setup, "clone_repo git@example");
            frontend.on_phase_step_output(PhaseKind::Setup, "Cloning into 'example'...");
            frontend.on_phase_step_output(PhaseKind::Setup, "done.");
            frontend.on_phase_step_completed(PhaseKind::Setup, "clone_repo git@example");
        });
        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(
            contents.starts_with("# setup step 1: clone_repo git@example\n"),
            "{contents}"
        );
        assert!(
            contents.contains("Cloning into 'example'...\ndone.\n"),
            "{contents}"
        );
        assert!(log.contains("squad setup step started"), "{log}");
        assert!(log.contains("squad setup step succeeded"), "{log}");
        assert!(log.contains(&path.display().to_string()), "{log}");
        assert!(!log.contains("ERROR"), "{log}");
    }

    #[test]
    fn a_failed_setup_step_appends_the_error_and_names_the_file_in_an_error_line() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("setup-1-clone-repo.log");
        let log = captured_tracing(|| {
            let mut frontend = run_frontend(tmp.path());
            frontend.on_phase_step_started(PhaseKind::Setup, "clone_repo");
            frontend.on_phase_step_output(PhaseKind::Setup, "Cloning...");
            frontend.on_phase_step_failed(
                PhaseKind::Setup,
                "clone_repo",
                128,
                "fatal: not a git repository\n",
            );
        });
        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(
            contents.ends_with("# failed (exit 128):\nfatal: not a git repository\n"),
            "{contents}"
        );
        let error_line = log
            .lines()
            .find(|l| l.contains("squad setup step failed"))
            .unwrap_or_else(|| panic!("no failure line in {log}"));
        assert!(error_line.contains("ERROR"), "{error_line}");
        assert!(error_line.contains("exit_code=128"), "{error_line}");
        assert!(
            error_line.contains(&format!("log_path={}", path.display())),
            "{error_line}"
        );
        assert!(
            error_line.contains("fatal: not a git repository"),
            "{error_line}"
        );
    }

    #[test]
    fn identical_steps_are_numbered_and_teardown_has_its_own_counter() {
        let tmp = tempfile::tempdir().unwrap();
        let mut frontend = run_frontend(tmp.path());
        frontend.on_phase_step_started(PhaseKind::Setup, "run_shell");
        frontend.on_phase_step_completed(PhaseKind::Setup, "run_shell");
        frontend.on_phase_step_started(PhaseKind::Setup, "run_shell");
        frontend.on_phase_step_completed(PhaseKind::Setup, "run_shell");
        frontend.on_phase_step_started(PhaseKind::Teardown, "create_pr");
        frontend.on_phase_step_completed(PhaseKind::Teardown, "create_pr");
        assert!(tmp.path().join("setup-1-run-shell.log").exists());
        assert!(tmp.path().join("setup-2-run-shell.log").exists());
        assert!(tmp.path().join("teardown-1-create-pr.log").exists());
    }

    #[test]
    fn remediation_output_lands_in_the_same_file_under_a_separator() {
        let tmp = tempfile::tempdir().unwrap();
        let mut frontend = run_frontend(tmp.path());
        frontend.on_phase_step_started(PhaseKind::Setup, "run_shell");
        frontend.on_phase_step_output(PhaseKind::Setup, "first try");
        frontend.on_phase_step_fixing(PhaseKind::Setup, "run_shell", 1, 2);
        frontend.on_phase_step_output(PhaseKind::Setup, "second try");
        frontend.on_phase_step_completed(PhaseKind::Setup, "run_shell");
        let contents = std::fs::read_to_string(tmp.path().join("setup-1-run-shell.log")).unwrap();
        let first = contents.find("first try").unwrap();
        let sep = contents
            .find("# on_failure remediation attempt 1/2")
            .unwrap();
        let second = contents.find("second try").unwrap();
        assert!(first < sep && sep < second, "{contents}");
        assert!(
            !tmp.path().join("setup-2-run-shell.log").exists(),
            "no second file"
        );
    }

    #[test]
    fn a_failed_agent_step_is_an_error_line_naming_its_container_log() {
        let tmp = tempfile::tempdir().unwrap();
        let container = "awman-squad-task-abcdef01";
        let log = captured_tracing(|| {
            let mut frontend = run_frontend(tmp.path());
            frontend.report_step_status(&step("build"), WorkflowStepStatus::Running);
            frontend.report_status(AgentStatus::Running {
                container_name: container.to_string(),
            });
            frontend
                .report_step_status(&step("build"), WorkflowStepStatus::Failed { exit_code: 2 });
            frontend.report_step_status(&step("test"), WorkflowStepStatus::Running);
            frontend.report_step_status(&step("test"), WorkflowStepStatus::Succeeded);
        });
        let failed = log
            .lines()
            .find(|l| l.contains("squad workflow step failed"))
            .unwrap_or_else(|| panic!("no failure line in {log}"));
        assert!(failed.contains("ERROR"), "{failed}");
        assert!(failed.contains("step=build"), "{failed}");
        assert!(failed.contains("exit_code=2"), "{failed}");
        assert!(
            failed.contains(&format!(
                "log_path={}",
                tmp.path().join(format!("{container}.log")).display()
            )),
            "{failed}"
        );
        let succeeded = log
            .lines()
            .find(|l| l.contains("step=test") && l.contains("Succeeded"))
            .unwrap_or_else(|| panic!("no success line in {log}"));
        assert!(succeeded.contains("INFO"), "{succeeded}");
    }

    #[test]
    fn dropping_the_frontend_mid_step_flushes_the_open_step_log() {
        let tmp = tempfile::tempdir().unwrap();
        {
            let mut frontend = run_frontend(tmp.path());
            frontend.on_phase_step_started(PhaseKind::Teardown, "push_branch");
            frontend.on_phase_step_output(PhaseKind::Teardown, "pushing...");
        }
        let contents =
            std::fs::read_to_string(tmp.path().join("teardown-1-push-branch.log")).unwrap();
        assert!(contents.contains("pushing..."), "{contents}");
    }

    /// Install a global subscriber that records nothing, once per test binary.
    ///
    /// Its only job is to exist. `tracing` caches each callsite's `Interest`
    /// process-wide the first time that callsite is reached, and with no
    /// global subscriber installed a callsite first reached by a thread with
    /// no scoped subscriber is cached as *never* interesting. From then on the
    /// `info!` short-circuits on every thread — including one that later
    /// installs a capture subscriber, which then reads back an empty log. The
    /// suite has plenty of tests that drive this frontend without capturing,
    /// so which happens first is a race, and it only shows up when siblings
    /// run alongside: `captured_tracing` on its own always wins the race.
    ///
    /// Answering [`Interest::sometimes`] keeps every callsite dynamic, so each
    /// event is resolved against whatever subscriber the *current thread* has
    /// — the capture sink here, or this no-op everywhere else.
    fn keep_callsites_dynamic() {
        use tracing::span::{Attributes, Id, Record};
        use tracing::subscriber::Interest;
        use tracing::{Event, Metadata};

        struct Discard;

        impl tracing::Subscriber for Discard {
            fn register_callsite(&self, _: &Metadata<'_>) -> Interest {
                Interest::sometimes()
            }
            fn enabled(&self, _: &Metadata<'_>) -> bool {
                false
            }
            fn new_span(&self, _: &Attributes<'_>) -> Id {
                Id::from_u64(1)
            }
            fn record(&self, _: &Id, _: &Record<'_>) {}
            fn record_follows_from(&self, _: &Id, _: &Id) {}
            fn event(&self, _: &Event<'_>) {}
            fn enter(&self, _: &Id) {}
            fn exit(&self, _: &Id) {}
        }

        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            // Nothing else in the crate sets a global default; if that ever
            // changes, that subscriber wins and this is a no-op.
            let _ = tracing::subscriber::set_global_default(Discard);
        });
    }

    /// Collect everything written to `tracing` while `body` runs, as text.
    fn captured_tracing(body: impl FnOnce()) -> String {
        #[derive(Clone, Default)]
        struct Sink(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for Sink {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Sink {
            type Writer = Sink;
            fn make_writer(&'a self) -> Self::Writer {
                self.clone()
            }
        }

        let sink = Sink::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(sink.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::INFO)
            .finish();
        keep_callsites_dynamic();
        tracing::subscriber::with_default(subscriber, body);
        let bytes = sink.0.lock().unwrap().clone();
        String::from_utf8(bytes).unwrap()
    }

    /// WI 0106 §6c: for a task bound to a repository, the daemon's own log
    /// must record where the run's worktree lives and what happened to it when
    /// the workflow ended. These are the callbacks the shared worktree
    /// lifecycle fires; nothing about squad skips them, and each line carries
    /// the task, the run id, and the path/branch needed to go find it.
    #[test]
    fn worktree_creation_and_disposition_are_recorded_in_the_daemon_log() {
        let created = captured_tracing(|| {
            let mut frontend = UnattendedFrontend::new("nightly".into());
            frontend.report_worktree_created(Path::new("/wt/nightly"), "awman/squad-nightly");
        });
        assert!(created.contains("squad worktree created"), "{created}");
        assert!(created.contains("/wt/nightly"), "{created}");
        assert!(created.contains("awman/squad-nightly"), "{created}");
        assert!(created.contains("INFO"), "{created}");

        let kept = captured_tracing(|| {
            let mut frontend = UnattendedFrontend::new("nightly".into());
            frontend.report_worktree_kept(Path::new("/wt/nightly"), "awman/squad-nightly");
        });
        assert!(kept.contains("squad worktree kept"), "{kept}");
        assert!(kept.contains("/wt/nightly"), "{kept}");

        let discarded = captured_tracing(|| {
            let mut frontend = UnattendedFrontend::new("nightly".into());
            frontend.report_worktree_discarded("awman/squad-nightly");
        });
        assert!(
            discarded.contains("squad worktree discarded"),
            "{discarded}"
        );
        assert!(discarded.contains("awman/squad-nightly"), "{discarded}");

        let conflict = captured_tracing(|| {
            let mut frontend = UnattendedFrontend::new("nightly".into());
            frontend.report_merge_conflict(
                "awman/squad-nightly",
                Path::new("/wt/nightly"),
                Path::new("/repo"),
            );
        });
        assert!(
            conflict.contains("squad workflow left a merge conflict"),
            "{conflict}"
        );
        assert!(conflict.contains("WARN"), "{conflict}");
    }

    #[test]
    fn the_workflow_frontend_answers_the_mount_scope_question_with_the_captured_scope() {
        let mut frontend = UnattendedFrontend::with_mount_scope(
            "c".into(),
            MountScopeDecision::MountCurrentDirOnly,
        );
        let decision = frontend
            .ask_mount_scope(Path::new("/repo"), Path::new("/repo/sub"))
            .unwrap();
        assert!(
            matches!(decision, MountScopeDecision::MountCurrentDirOnly),
            "an unattended run must never widen the captured mount scope"
        );
    }

    /// The leader's scope comes from Layer 2, exactly like the workflow's.
    ///
    /// `leader_frontend` used to pass `MountScopeDecision::MountGitRoot` from
    /// here, so which directory a squad *leader* mounted was chosen by a
    /// frontend literal — a decision left in `src/frontend/squad/`, which Q4
    /// forbids, and one `every_profile_answers_its_recorded_table` could not
    /// see. This asserts only the forwarding; *what* the scope should be is
    /// `SquadEvaluator`'s, via `mount_scope_decision(task.mount_scope)`.
    #[test]
    fn a_leader_run_forwards_the_scope_it_is_given() {
        let tmp = tempfile::tempdir().unwrap();
        let mut frontend = UnattendedFrontend::for_run(
            "task",
            &RunId("run-0114".into()),
            tmp.path(),
            "leader",
            MountScopeDecision::MountCurrentDirOnly,
        )
        .unwrap();

        let decision = frontend
            .ask_mount_scope(Path::new("/repo"), Path::new("/repo/sub"))
            .unwrap();
        assert!(
            matches!(decision, MountScopeDecision::MountCurrentDirOnly),
            "a leader run must answer with the scope Layer 2 handed it, \
             not one of its own"
        );
    }

    /// Every `ask_*` answer is the squad profile's, not this frontend's.
    ///
    /// *What* those answers are — never widen a mount scope, never merge,
    /// never delete a worktree, start over rather than resume — is the one
    /// comparable table in `command::headless`, pinned there by
    /// `every_profile_answers_its_recorded_table`. Asserting the values again
    /// here would be a decision asserted in a frontend test (F-13, Tenet 2),
    /// and would let the two drift. So this asserts only the delegation: each
    /// body returns exactly what the profile it was built with returns.
    #[test]
    fn every_answer_is_the_squad_profiles_answer() {
        let profile = HeadlessDefaults::squad(MountScopeDecision::MountCurrentDirOnly);
        let mut frontend = UnattendedFrontend::new("c/leader".into());
        let resume_prompt = WorkflowResumePrompt::new(
            "wf".into(),
            None,
            None,
            false,
            1,
            3,
            vec![
                crate::command::commands::exec_workflow::WorkflowResumeStep {
                    name: "b".into(),
                    role: "the step that failed".into(),
                },
            ],
        );
        let worktree_prompt = PostWorkflowWorktreePrompt {
            branch: "awman/squad".into(),
            target_branch: "main".into(),
            had_error: false,
            title: "t".into(),
            body: "b".into(),
            merge_label: "m".into(),
            discard_label: "d".into(),
            keep_label: "k".into(),
        };

        assert_eq!(
            frontend.ask_workflow_resume(&resume_prompt).unwrap(),
            profile.workflow_resume(&resume_prompt)
        );
        assert_eq!(
            frontend.ask_post_workflow_action(&worktree_prompt).unwrap(),
            profile.post_workflow_action(&worktree_prompt)
        );
        assert_eq!(
            frontend
                .confirm_worktree_cleanup("b", Path::new("/w"))
                .unwrap(),
            profile.confirm_worktree_cleanup()
        );
        assert_eq!(frontend.ask_merge_mode("b").unwrap(), profile.merge_mode());
        assert_eq!(
            frontend
                .ask_pre_worktree_uncommitted_files(&[], "")
                .unwrap(),
            profile.pre_worktree_uncommitted_files("")
        );
        assert_eq!(
            frontend
                .ask_existing_worktree(Path::new("/w"), "b")
                .unwrap(),
            profile.existing_worktree()
        );
    }

    /// A resize channel and an initial terminal size are what make the engine
    /// take the PTY path rather than piping the agent. Squad runs every agent
    /// PTY-backed whether or not anyone is attached (WI 0106 §3c): that is
    /// what makes the running agent a real interactive TUI process, and it is
    /// the prerequisite for `attach` connecting to the actual agent rather
    /// than a shell. The daemon owns no real terminal, so it supplies a fixed
    /// spacious default instead of measuring one.
    #[tokio::test]
    async fn agent_io_is_pty_backed_even_when_nobody_is_attached() {
        let mut frontend = UnattendedFrontend::new("c/leader".into());
        let io = frontend.take_io();
        assert!(
            io.resize.is_some(),
            "a PTY-backed agent needs a resize channel"
        );
        assert!(
            io.initial_size.is_some(),
            "an unattended PTY still needs a terminal size to allocate"
        );
    }

    #[tokio::test]
    async fn leader_and_workflow_container_output_is_written_to_each_run_log() {
        let tmp = tempfile::tempdir().unwrap();
        let run_id = RunId("run-0106".into());

        for (label, container) in [
            ("leader", "awman-squad-task-11111111"),
            ("workflow", "awman-squad-task-22222222"),
        ] {
            let run_dir = tmp.path().join(label);
            std::fs::create_dir(&run_dir).unwrap();
            let mut frontend = UnattendedFrontend::for_run(
                "task",
                &run_id,
                &run_dir,
                label,
                MountScopeDecision::MountGitRoot,
            )
            .unwrap();
            frontend.report_status(AgentStatus::Running {
                container_name: container.to_string(),
            });
            let io = frontend.take_io();
            io.stdout.send(b"stdout from agent\n".to_vec()).unwrap();
            io.stderr.send(b"stderr from agent\n".to_vec()).unwrap();
            drop(io);

            let log_path = run_dir.join(format!("{container}.log"));
            let mut contents = String::new();
            for _ in 0..40 {
                contents = std::fs::read_to_string(&log_path).unwrap_or_default();
                if contents.contains("stdout from agent") && contents.contains("stderr from agent")
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            assert!(
                contents.contains("stdout from agent"),
                "{label}: {contents:?}"
            );
            assert!(
                contents.contains("stderr from agent"),
                "{label}: {contents:?}"
            );
        }
    }
}
