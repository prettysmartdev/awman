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
    DirtyMergeTargetDecision, ExistingWorktreeDecision, MergeFailure, PostWorkflowWorktreeAction,
    PostWorkflowWorktreePrompt, PreWorktreeDecision, WorktreeLifecycleFrontend, WorktreeMergeMode,
};
use crate::command::error::CommandError;
use crate::command::headless::HeadlessDefaults;
use crate::command::interaction::InteractionDecision;
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
        interaction: InteractionDecision,
    ) -> Result<Box<dyn AgentFrontend>, CommandError> {
        Ok(Box::new(UnattendedFrontend::for_run(
            task,
            run_id,
            run_log_dir,
            label,
            mount_scope,
            interaction,
        )?))
    }

    fn workflow_frontend(
        &self,
        task: &str,
        run_id: &RunId,
        run_log_dir: &Path,
        mount_scope: MountScopeDecision,
        interaction: InteractionDecision,
    ) -> Result<Box<dyn ExecWorkflowCommandFrontend>, CommandError> {
        Ok(Box::new(UnattendedFrontend::for_run(
            task,
            run_id,
            run_log_dir,
            "workflow",
            mount_scope,
            interaction,
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
    /// This run's resolved interaction (WI 0141): a PTY of
    /// `pty_size` when interactive, piped I/O when headless.
    interaction: InteractionDecision,
}

impl UnattendedFrontend {
    fn for_run(
        task: &str,
        run_id: &RunId,
        run_log_dir: &Path,
        label: &str,
        mount_scope: MountScopeDecision,
        interaction: InteractionDecision,
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
            interaction,
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

    /// An interactive squad run (`daemon.features.squad.interaction`, the
    /// default) gives every agent a PTY even when nobody is attached, so its
    /// process is the same interactive agent TUI a later attach reconnects
    /// to. Its size is `daemon.terminal.{cols,rows}` as Layer 2 read it when
    /// the run started (default 120×40); running terminals keep it. A
    /// headless run pipes the agent: no PTY, nothing to attach to, output
    /// still goes to the container log (WI 0141).
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
            resize: self
                .interaction
                .pty_size
                .map(|_| tokio::sync::mpsc::unbounded_channel().1),
            initial_size: self.interaction.pty_size,
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

    fn yolo_countdown_started(
        &mut self,
        step_name: &str,
        kind: crate::engine::workflow::actions::CountdownKind,
    ) {
        tracing::info!(
            task = %self.task,
            run_id = %self.run_id,
            step = %step_name,
            countdown = ?kind,
            "squad workflow countdown started"
        );
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

    fn existing_worktree_after_resume(
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

    fn ask_dirty_merge_target(
        &mut self,
        _target_branch: &str,
        _files: &[String],
        _suggested_message: &str,
    ) -> Result<DirtyMergeTargetDecision, CommandError> {
        Ok(self.headless.dirty_merge_target())
    }

    fn ask_launch_merge_fix_agent(
        &mut self,
        _failure: &MergeFailure,
    ) -> Result<bool, CommandError> {
        Ok(self.headless.launch_merge_fix_agent())
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

/// Nobody is attached to an unattended squad run, so a custom definition that
/// needs approval is refused, never approved.
impl crate::command::commands::agent_trust::AgentTrustFrontend for UnattendedFrontend {}

impl crate::command::commands::workflow_prerequisites::WorkflowPrerequisiteFrontend
    for UnattendedFrontend
{
    fn show_workflow_prerequisites(
        &mut self,
        report: &crate::data::workflow_prerequisites::WorkflowPrerequisiteReport,
        _next: crate::command::commands::workflow_prerequisites::PrerequisiteReportNext,
    ) -> Result<(), CommandError> {
        let summary = report.summary_line();
        match self.logs.open_step_log("workflow", 0, "prerequisites") {
            Ok(mut log) => {
                log.write_line(&format!("# {summary}"));
                for line in report.render_lines() {
                    log.write_line(&line);
                }
                let path = log.finish();
                tracing::info!(
                    task = %self.task,
                    run_id = %self.run_id,
                    summary,
                    log_path = %path.display(),
                    "squad workflow prerequisites checked"
                );
            }
            Err(error) => tracing::error!(
                task = %self.task,
                run_id = %self.run_id,
                summary,
                error = %error,
                "squad failed to open workflow prerequisite log"
            ),
        }
        Ok(())
    }
}
