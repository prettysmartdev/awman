//! `CliParallelFrontend` — lightweight multi-agent chrome for the CLI
//! (WI-0096 §7).
//!
//! Wraps a [`CliFrontend`] and delegates every single-container
//! `WorkflowFrontend` method to it unchanged. The only new behavior is a
//! two-row status bar reserved at the bottom of the terminal (via crossterm
//! ANSI cursor addressing, no Ratatui) whenever more than one parallel step
//! is actually running and stdout is a TTY. When at most one step is
//! running, or stdout is not a TTY, this frontend is behaviorally identical
//! to plain `CliFrontend` passthrough.

use std::io::{IsTerminal, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use tokio::sync::broadcast;

use crate::data::message::{MessageLevel, UserMessage, UserMessageSink};
use crate::data::workflow_definition::WorkflowStep;
use crate::data::workflow_state::WorkflowState;
use crate::engine::agent_runtime::execution::{AgentExitInfo, StuckEvent};
use crate::engine::agent_runtime::frontend::AgentIo;
use crate::engine::error::EngineError;
use crate::engine::workflow::actions::{
    AvailableActions, NextAction, ResumeMismatch, StepFailureChoice, StepOutput, WorkflowOutcome,
    WorkflowStepProgressInfo, WorkflowStepStatus, YoloTickOutcome,
};
use crate::engine::workflow::frontend::WorkflowFrontend;
use crate::engine::workflow::EngineRequest;

use crate::command::commands::agent_auth::{AgentAuthDecision, AgentAuthFrontend};
use crate::command::commands::agent_setup::{AgentSetupDecision, AgentSetupFrontend};
use crate::command::commands::exec_workflow::{ExecWorkflowCommandFrontend, WorkflowSummary};
use crate::command::commands::mount_scope::{MountScopeDecision, MountScopeFrontend};
use crate::command::commands::worktree_lifecycle::{
    ExistingWorktreeDecision, PostWorkflowWorktreeAction, PostWorkflowWorktreePrompt,
    PreWorktreeDecision, WorktreeLifecycleFrontend,
};
use crate::command::error::CommandError;
use crate::data::session::AgentName;
use crate::engine::agent_runtime::frontend::{AgentFrontend, AgentProgress, AgentStatus};
use crate::frontend::cli::command_frontend::CliFrontend;

/// One currently-running (or just-dequeued) parallel step, as tracked for
/// status-bar rendering. Purely a presentation-layer mirror of what the
/// engine has reported via `WorkflowFrontend` callbacks — never read from
/// engine state directly.
struct ParallelStepInfo {
    step_name: String,
    stuck: bool,
}

struct ChromeState {
    running: Vec<ParallelStepInfo>,
    focused: usize,
}

/// Background thread that watches for Ctrl-S while the chrome is active and
/// advances the focused step. Stopped and joined whenever the chrome
/// deactivates (group finished, or the running count drops back to <= 1).
struct CtrlSWatcher {
    stop: Arc<AtomicBool>,
    handle: std::thread::JoinHandle<()>,
}

pub struct CliParallelFrontend {
    inner: CliFrontend,
    is_terminal: bool,
    state: Arc<Mutex<ChromeState>>,
    chrome_active: bool,
    watcher: Option<CtrlSWatcher>,
}

impl CliParallelFrontend {
    pub fn new(inner: CliFrontend) -> Self {
        Self {
            inner,
            is_terminal: std::io::stdout().is_terminal(),
            state: Arc::new(Mutex::new(ChromeState {
                running: Vec::new(),
                focused: 0,
            })),
            chrome_active: false,
            watcher: None,
        }
    }

    fn add_running(&mut self, step_name: &str) {
        let mut st = self.state.lock().unwrap();
        st.running.push(ParallelStepInfo {
            step_name: step_name.to_string(),
            stuck: false,
        });
    }

    fn remove_running(&mut self, step_name: &str) {
        let mut st = self.state.lock().unwrap();
        if let Some(pos) = st.running.iter().position(|s| s.step_name == step_name) {
            st.running.remove(pos);
            if st.running.is_empty() {
                st.focused = 0;
            } else if st.focused >= st.running.len() {
                st.focused = st.running.len() - 1;
            }
        }
    }

    fn set_stuck(&mut self, step_name: &str, stuck: bool) {
        let mut st = self.state.lock().unwrap();
        if let Some(entry) = st.running.iter_mut().find(|s| s.step_name == step_name) {
            entry.stuck = stuck;
        }
    }

    fn is_focused(&self, step_name: &str) -> bool {
        let st = self.state.lock().unwrap();
        st.running
            .get(st.focused)
            .map(|s| s.step_name == step_name)
            .unwrap_or(false)
    }

    /// Reconciles chrome on/off state with the current running count and
    /// redraws the status bar when the chrome stays active. Called after
    /// every add/remove/stuck-flag mutation.
    fn sync_chrome(&mut self) {
        let running_count = self.state.lock().unwrap().running.len();
        let want_active = self.is_terminal && running_count > 1;
        if want_active && !self.chrome_active {
            self.activate_chrome();
        } else if !want_active && self.chrome_active {
            self.deactivate_chrome();
        } else if self.chrome_active {
            redraw_status_bar(&self.state);
        }
    }

    fn activate_chrome(&mut self) {
        if self.chrome_active {
            return;
        }
        self.chrome_active = true;
        if let Ok((_, rows)) = crossterm::terminal::size() {
            let body_bottom = rows.saturating_sub(2).max(1);
            let mut out = std::io::stdout().lock();
            // Shrink the scrolling region so container passthrough output
            // scrolls only above the reserved chrome rows.
            let _ = write!(out, "\x1b[1;{body_bottom}r\x1b[{rows};1H");
            let _ = out.flush();
        }
        self.spawn_ctrl_s_watcher();
        redraw_status_bar(&self.state);
    }

    fn deactivate_chrome(&mut self) {
        if !self.chrome_active {
            return;
        }
        self.chrome_active = false;
        self.stop_ctrl_s_watcher();
        if let Ok((_, rows)) = crossterm::terminal::size() {
            let mut out = std::io::stdout().lock();
            // Restore the full-screen scrolling region and clear the bar.
            let _ = write!(out, "\x1b[r\x1b[{rows};1H\x1b[2K");
            let _ = out.flush();
        }
    }

    /// Spawn the background Ctrl-S watcher. Known limitation: this reads
    /// terminal input concurrently with whatever single-container stdin
    /// forwarding the active step's `AgentIo` owns (the engine does not yet
    /// call `set_parallel_step_io` per step — see the implement-engine and
    /// implement-tui notes — so there is no per-step input channel to
    /// arbitrate against). Only Ctrl-S is consumed here; every other key is
    /// left alone.
    fn spawn_ctrl_s_watcher(&mut self) {
        if self.watcher.is_some() {
            return;
        }
        let stop = Arc::new(AtomicBool::new(false));
        let stop_for_thread = Arc::clone(&stop);
        let state = Arc::clone(&self.state);
        let handle = std::thread::spawn(move || {
            while !stop_for_thread.load(Ordering::Relaxed) {
                match event::poll(Duration::from_millis(150)) {
                    Ok(true) => {
                        if let Ok(Event::Key(key)) = event::read() {
                            if key.modifiers.contains(KeyModifiers::CONTROL)
                                && matches!(key.code, KeyCode::Char('s') | KeyCode::Char('S'))
                            {
                                let mut st = state.lock().unwrap();
                                if !st.running.is_empty() {
                                    st.focused = (st.focused + 1) % st.running.len();
                                }
                                drop(st);
                                redraw_status_bar(&state);
                            }
                        }
                    }
                    Ok(false) => continue,
                    Err(_) => return,
                }
            }
        });
        self.watcher = Some(CtrlSWatcher { stop, handle });
    }

    fn stop_ctrl_s_watcher(&mut self) {
        if let Some(w) = self.watcher.take() {
            w.stop.store(true, Ordering::Relaxed);
            let _ = w.handle.join();
        }
    }
}

impl Drop for CliParallelFrontend {
    fn drop(&mut self) {
        self.deactivate_chrome();
    }
}

/// Redraws the bottom status row: `[N agents running | showing: <step> |
/// Ctrl-S: switch]`, reverse-video, restoring the cursor position
/// afterward. No-op if the chrome's row can't be located (no terminal size).
fn redraw_status_bar(state: &Arc<Mutex<ChromeState>>) {
    let (n, showing) = {
        let st = state.lock().unwrap();
        if st.running.is_empty() {
            return;
        }
        let showing = st
            .running
            .get(st.focused)
            .map(|s| {
                if s.stuck {
                    format!("\u{26a0} {}", s.step_name)
                } else {
                    s.step_name.clone()
                }
            })
            .unwrap_or_else(|| "-".to_string());
        (st.running.len(), showing)
    };
    let plural = if n == 1 { "" } else { "s" };
    let mut label = format!(" [{n} agent{plural} running | showing: {showing} | Ctrl-S: switch] ");
    let Ok((cols, rows)) = crossterm::terminal::size() else {
        return;
    };
    if label.chars().count() > cols as usize {
        label = label.chars().take(cols as usize).collect();
    }
    let mut out = std::io::stdout().lock();
    let _ = write!(out, "\x1b7\x1b[{rows};1H\x1b[2K\x1b[7m{label}\x1b[0m\x1b8");
    let _ = out.flush();
}

impl UserMessageSink for CliParallelFrontend {
    fn write_message(&mut self, msg: UserMessage) {
        self.inner.write_message(msg);
    }

    fn replay_queued(&mut self) {
        self.inner.replay_queued();
    }
}

impl AgentFrontend for CliParallelFrontend {
    fn report_status(&mut self, status: AgentStatus) {
        self.inner.report_status(status);
    }

    fn report_progress(&mut self, progress: AgentProgress) {
        self.inner.report_progress(progress);
    }

    fn take_io(&mut self) -> AgentIo {
        self.inner.take_io()
    }

    fn grace_timeout(&self) -> Duration {
        self.inner.grace_timeout()
    }

    fn stuck_timeout(&self) -> Duration {
        self.inner.stuck_timeout()
    }
}

impl MountScopeFrontend for CliParallelFrontend {
    fn ask_mount_scope(
        &mut self,
        git_root: &Path,
        cwd: &Path,
    ) -> Result<MountScopeDecision, CommandError> {
        self.inner.ask_mount_scope(git_root, cwd)
    }
}

impl AgentSetupFrontend for CliParallelFrontend {
    fn ask_agent_setup(
        &mut self,
        requested: &AgentName,
        default: &AgentName,
        default_available: bool,
        image_only: bool,
    ) -> Result<AgentSetupDecision, CommandError> {
        self.inner
            .ask_agent_setup(requested, default, default_available, image_only)
    }

    fn record_fallback(&mut self, requested: &AgentName, fallback: &AgentName) {
        self.inner.record_fallback(requested, fallback);
    }
}

impl AgentAuthFrontend for CliParallelFrontend {
    fn ask_agent_auth_consent(
        &mut self,
        agent: &AgentName,
        env_var_names: &[&str],
    ) -> Result<AgentAuthDecision, CommandError> {
        self.inner.ask_agent_auth_consent(agent, env_var_names)
    }
}

impl WorktreeLifecycleFrontend for CliParallelFrontend {
    fn ask_pre_worktree_uncommitted_files(
        &mut self,
        files: &[String],
        suggested_message: &str,
    ) -> Result<PreWorktreeDecision, CommandError> {
        self.inner
            .ask_pre_worktree_uncommitted_files(files, suggested_message)
    }

    fn ask_existing_worktree(
        &mut self,
        path: &Path,
        branch: &str,
    ) -> Result<ExistingWorktreeDecision, CommandError> {
        self.inner.ask_existing_worktree(path, branch)
    }

    fn report_worktree_created(&mut self, path: &Path, branch: &str) {
        self.inner.report_worktree_created(path, branch);
    }

    fn ask_post_workflow_action(
        &mut self,
        prompt: &PostWorkflowWorktreePrompt,
    ) -> Result<PostWorkflowWorktreeAction, CommandError> {
        self.inner.ask_post_workflow_action(prompt)
    }

    fn ask_worktree_commit_before_merge(
        &mut self,
        branch: &str,
        files: &[String],
        suggested_message: &str,
    ) -> Result<Option<String>, CommandError> {
        self.inner
            .ask_worktree_commit_before_merge(branch, files, suggested_message)
    }

    fn confirm_squash_merge(&mut self, branch: &str) -> Result<bool, CommandError> {
        self.inner.confirm_squash_merge(branch)
    }

    fn confirm_worktree_cleanup(
        &mut self,
        branch: &str,
        path: &Path,
    ) -> Result<bool, CommandError> {
        self.inner.confirm_worktree_cleanup(branch, path)
    }

    fn report_merge_conflict(&mut self, branch: &str, worktree_path: &Path, git_root: &Path) {
        self.inner
            .report_merge_conflict(branch, worktree_path, git_root);
    }

    fn report_worktree_discarded(&mut self, branch: &str) {
        self.inner.report_worktree_discarded(branch);
    }

    fn report_worktree_kept(&mut self, path: &Path, branch: &str) {
        self.inner.report_worktree_kept(path, branch);
    }
}

impl ExecWorkflowCommandFrontend for CliParallelFrontend {
    fn set_pty_active(&mut self, active: bool) {
        self.inner.set_pty_active(active);
    }

    fn report_workflow_summary(&mut self, summary: &WorkflowSummary) {
        self.inner.report_workflow_summary(summary);
    }

    fn ask_workflow_resume_or_fresh(
        &mut self,
        workflow_name: &str,
        completed_steps: usize,
        total_steps: usize,
    ) -> Result<bool, CommandError> {
        self.inner
            .ask_workflow_resume_or_fresh(workflow_name, completed_steps, total_steps)
    }
}

impl WorkflowFrontend for CliParallelFrontend {
    // === Single-container methods: delegate to the existing CLI workflow
    // frontend unchanged. ===

    fn show_workflow_control_board(
        &mut self,
        state: &WorkflowState,
        available: &AvailableActions,
    ) -> Result<NextAction, EngineError> {
        self.inner.show_workflow_control_board(state, available)
    }

    fn yolo_countdown_tick(
        &mut self,
        step_name: &str,
        remaining: Duration,
        total: Duration,
    ) -> Result<YoloTickOutcome, EngineError> {
        self.inner.yolo_countdown_tick(step_name, remaining, total)
    }

    fn yolo_countdown_started(&mut self, step_name: &str) {
        self.inner.yolo_countdown_started(step_name);
    }

    fn yolo_countdown_finished(&mut self, step_name: &str) {
        self.inner.yolo_countdown_finished(step_name);
    }

    fn report_step_status(&mut self, step: &WorkflowStep, status: WorkflowStepStatus) {
        self.inner.report_step_status(step, status);
    }

    fn report_step_output(&mut self, step: &WorkflowStep, output: StepOutput) {
        self.inner.report_step_output(step, output);
    }

    fn report_workflow_completed(&mut self, outcome: &WorkflowOutcome) {
        self.deactivate_chrome();
        self.inner.report_workflow_completed(outcome);
    }

    fn report_workflow_progress(&mut self, steps: &[WorkflowStepProgressInfo]) {
        self.inner.report_workflow_progress(steps);
    }

    fn report_step_interactive_launch(
        &mut self,
        step: &WorkflowStep,
        agent: &str,
        model: Option<&str>,
    ) {
        self.inner
            .report_step_interactive_launch(step, agent, model);
    }

    fn report_container_exited(&mut self, exit_code: i32) {
        self.inner.report_container_exited(exit_code);
    }

    fn confirm_resume(&mut self, mismatch: &ResumeMismatch) -> Result<bool, EngineError> {
        self.inner.confirm_resume(mismatch)
    }

    fn user_choose_after_step_failure(
        &mut self,
        step: &WorkflowStep,
        exit: &AgentExitInfo,
    ) -> Result<StepFailureChoice, EngineError> {
        self.inner.user_choose_after_step_failure(step, exit)
    }

    fn set_engine_sender(&mut self, tx: tokio::sync::mpsc::UnboundedSender<EngineRequest>) {
        self.inner.set_engine_sender(tx);
    }

    fn set_stuck_sender(&mut self, sender: Arc<broadcast::Sender<StuckEvent>>) {
        self.inner.set_stuck_sender(sender);
    }

    fn on_setup_step_started(&mut self, description: &str) {
        self.inner.on_setup_step_started(description);
    }

    fn on_setup_step_output(&mut self, line: &str) {
        self.inner.on_setup_step_output(line);
    }

    fn on_setup_step_completed(&mut self, description: &str) {
        self.inner.on_setup_step_completed(description);
    }

    fn on_setup_step_failed(&mut self, description: &str, exit_code: i32, stderr: &str) {
        self.inner
            .on_setup_step_failed(description, exit_code, stderr);
    }

    fn on_setup_step_fixing(&mut self, description: &str, attempt: u32, of: u32) {
        self.inner.on_setup_step_fixing(description, attempt, of);
    }

    fn on_teardown_step_started(&mut self, description: &str) {
        self.inner.on_teardown_step_started(description);
    }

    fn on_teardown_step_output(&mut self, line: &str) {
        self.inner.on_teardown_step_output(line);
    }

    fn on_teardown_step_completed(&mut self, description: &str) {
        self.inner.on_teardown_step_completed(description);
    }

    fn on_teardown_step_failed(&mut self, description: &str, exit_code: i32, stderr: &str) {
        self.inner
            .on_teardown_step_failed(description, exit_code, stderr);
    }

    fn on_teardown_step_fixing(&mut self, description: &str, attempt: u32, of: u32) {
        self.inner.on_teardown_step_fixing(description, attempt, of);
    }

    // === Parallel-group methods: the actual WI-0096 §7 chrome. ===

    fn report_parallel_group_started(&mut self, step_names: &[String]) {
        {
            let mut st = self.state.lock().unwrap();
            st.running.clear();
            st.focused = 0;
        }
        if !self.is_terminal && step_names.len() > 1 {
            self.inner.write_message(UserMessage {
                level: MessageLevel::Warning,
                text: format!(
                    "{} agents are launching in parallel, but output merging is not \
                     supported in non-interactive mode; step output will interleave \
                     without chrome",
                    step_names.len()
                ),
            });
        }
    }

    fn report_parallel_step_launched(
        &mut self,
        step_name: &str,
        _agent: &str,
        _model: Option<&str>,
    ) {
        self.add_running(step_name);
        self.sync_chrome();
    }

    fn report_parallel_step_exited(&mut self, step_name: &str, _exit_code: i32) {
        self.remove_running(step_name);
        self.sync_chrome();
    }

    fn report_parallel_step_dequeued(
        &mut self,
        step_name: &str,
        _agent: &str,
        _model: Option<&str>,
    ) {
        self.add_running(step_name);
        self.sync_chrome();
    }

    fn report_parallel_group_finished(&mut self) {
        {
            let mut st = self.state.lock().unwrap();
            st.running.clear();
            st.focused = 0;
        }
        self.deactivate_chrome();
    }

    fn report_parallel_step_stuck(&mut self, step_name: &str) {
        self.set_stuck(step_name, true);
        if self.chrome_active {
            redraw_status_bar(&self.state);
        }
    }

    fn report_parallel_step_unstuck(&mut self, step_name: &str) {
        self.set_stuck(step_name, false);
        if self.chrome_active {
            redraw_status_bar(&self.state);
        }
    }

    fn parallel_step_yolo_countdown_started(&mut self, step_name: &str) {
        if !self.chrome_active || self.is_focused(step_name) {
            self.inner.yolo_countdown_started(step_name);
        }
    }

    fn parallel_step_yolo_countdown_tick(
        &mut self,
        step_name: &str,
        remaining: Duration,
        total: Duration,
    ) -> Result<YoloTickOutcome, EngineError> {
        // Only the currently-focused step's countdown is surfaced (it's the
        // only one with visible output above the chrome); background siblings
        // keep counting down silently rather than fighting over the terminal.
        if !self.chrome_active || self.is_focused(step_name) {
            return self.inner.yolo_countdown_tick(step_name, remaining, total);
        }
        Ok(YoloTickOutcome::Continue)
    }

    fn parallel_step_yolo_countdown_finished(&mut self, step_name: &str) {
        if !self.chrome_active || self.is_focused(step_name) {
            self.inner.yolo_countdown_finished(step_name);
        }
    }

    fn set_parallel_step_io(&mut self, _step_name: &str, _io: AgentIo) {}

    fn set_parallel_step_stuck_sender(
        &mut self,
        _step_name: &str,
        _sender: Arc<broadcast::Sender<StuckEvent>>,
    ) {
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_cli_frontend() -> CliFrontend {
        let cmd = crate::command::dispatch::catalogue::CommandCatalogue::get().build_clap_command();
        let m = cmd
            .try_get_matches_from(["awman", "exec", "workflow", "wf.toml"])
            .unwrap();
        CliFrontend::new(m)
    }

    /// Non-TTY stdout: even with two parallel agents the chrome never
    /// activates — output falls back to plain passthrough (WI-0096 §7).
    #[test]
    fn chrome_stays_inactive_when_not_a_terminal() {
        let mut f = CliParallelFrontend::new(make_cli_frontend());
        f.is_terminal = false;

        f.report_parallel_group_started(&["a".to_string(), "b".to_string()]);
        f.report_parallel_step_launched("a", "claude", None);
        f.report_parallel_step_launched("b", "claude", None);

        assert!(
            !f.chrome_active,
            "no status-bar chrome without a TTY — plain passthrough"
        );
        assert_eq!(
            f.state.lock().unwrap().running.len(),
            2,
            "step tracking still works off callbacks even without chrome"
        );
    }

    /// One running agent (even on a TTY) is not multi-agent UX — no chrome.
    #[test]
    fn chrome_inactive_with_a_single_running_agent() {
        let mut f = CliParallelFrontend::new(make_cli_frontend());
        f.is_terminal = true;

        f.report_parallel_step_launched("a", "claude", None);

        assert!(
            !f.chrome_active,
            "a single running agent never activates the multi-agent chrome"
        );
    }

    /// TTY + more than one running agent draws the status-bar chrome; once the
    /// group drains back to a single agent the chrome is removed again.
    #[test]
    fn chrome_activates_with_two_agents_on_a_tty_then_deactivates_when_draining() {
        let mut f = CliParallelFrontend::new(make_cli_frontend());
        f.is_terminal = true;

        f.report_parallel_step_launched("a", "claude", None);
        f.report_parallel_step_launched("b", "claude", None);
        assert!(
            f.chrome_active,
            "two agents running on a TTY must draw the status bar row"
        );

        f.report_parallel_step_exited("a", 0);
        assert!(
            !f.chrome_active,
            "dropping back to one running agent removes the chrome"
        );
    }
}
