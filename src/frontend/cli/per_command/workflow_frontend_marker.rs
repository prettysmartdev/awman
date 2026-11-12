//! `WorkflowFrontend` impl for the CLI.
//!
//! The CLI prompts on stdin (when it is a TTY) and falls back to the safe
//! headless defaults otherwise. The
//! prompt presents only the actions in `AvailableActions` whose `can_*`
//! flags are true; excluded actions are skipped (with their
//! `*_unavailable_reason` printed as a parenthetical note).

use std::time::Duration;

use crate::data::workflow_definition::WorkflowStep;
use crate::data::workflow_state::{PhaseKind, WorkflowState};
use crate::engine::error::EngineError;
use crate::engine::workflow::actions::{
    AvailableActions, NextAction, ResumeMismatch, StepOutput, WorkflowOutcome,
    WorkflowStepProgressInfo, WorkflowStepStatus, YoloTickOutcome,
};
use crate::engine::workflow::frontend::WorkflowFrontend;

use crate::frontend::cli::command_frontend::CliFrontend;

impl CliFrontend {
    /// The countdown line: the long-standing "yolo: auto-advancing" wording for
    /// the silence and retry countdowns, the agent-report wording otherwise.
    fn countdown_text(&self, secs: u64) -> String {
        use crate::engine::workflow::actions::CountdownKind;
        match self.countdown_kind {
            CountdownKind::StuckStep | CountdownKind::FailureRetry => {
                format!(" yolo: auto-advancing in {secs}s ")
            }
            kind => format!(
                " {} ",
                crate::frontend::program_status_text::countdown_label(kind, secs)
            ),
        }
    }
}

impl WorkflowFrontend for CliFrontend {
    fn set_container_source(
        &mut self,
        source: Option<crate::engine::workflow::frontend::ContainerSource>,
    ) {
        if let Some(recorder) = &self.worker_events {
            recorder.container_source(source);
        }
    }
    fn report_pull_request_created(&mut self, url: &str) {
        if let Some(recorder) = &self.worker_events {
            recorder.writer.record_pull_request(url);
        }
    }
    fn report_workflow_state(&mut self, state: &WorkflowState) {
        if let Some(recorder) = &self.worker_events {
            recorder.state(state);
        }
    }
    fn show_workflow_control_board(
        &mut self,
        _state: &WorkflowState,
        available: &AvailableActions,
    ) -> Result<NextAction, EngineError> {
        if self.non_interactive {
            return Ok(self.headless.workflow_next_action(available));
        }

        // If stdio is currently bound to a container PTY (e.g. the dialog
        // was triggered by the container becoming stuck), release it before
        // printing the menu so the user's keystrokes reach `read_line`
        // instead of being forwarded into the container. The container is
        // left running; we rebind below if the user picks Resume/Continue.
        let was_bound = self.unbind_container_stdio();
        let resume_available = was_bound && available.can_dismiss;

        let menu = control_board_lines(available, resume_available);
        for line in &menu {
            eprintln!("{line}");
        }
        let mut lines_printed = menu.len();

        let mut buf = String::new();
        if std::io::stdin().read_line(&mut buf).is_err() {
            return Ok(NextAction::Pause);
        }
        // The echoed user input + Enter advances the terminal one line.
        lines_printed += 1;

        let action = match buf.trim() {
            "s" | "S" if resume_available => NextAction::Dismiss,
            "n" | "N" if available.can_launch_next => NextAction::LaunchNext,
            "c" | "C" if available.can_continue_in_current_container => {
                NextAction::ContinueInCurrentContainer {
                    prompt: available.continue_prompt.clone().unwrap_or_default(),
                }
            }
            "r" | "R" if available.can_restart_current_step => NextAction::RestartCurrentStep,
            "b" | "B" if available.can_cancel_to_previous_step => NextAction::CancelToPreviousStep,
            "p" | "P" if available.can_pause => NextAction::Pause,
            "a" | "A" if available.can_abort => NextAction::Abort,
            "f" | "F" if available.can_finish_workflow => NextAction::FinishWorkflow,
            _ => NextAction::Pause,
        };

        // If the user chose to resume the still-running container (Dismiss
        // here, or Continue which injects a prompt and keeps the container
        // alive), erase the menu we printed and rebind stdio. For any other
        // action, the engine will cancel the container or transition to a
        // new step, so leaving stdio unbound is correct.
        let resumes_container = matches!(
            &action,
            NextAction::Dismiss | NextAction::ContinueInCurrentContainer { .. }
        );
        if was_bound && resumes_container {
            erase_lines_above(lines_printed);
            self.rebind_container_stdio();
        }

        Ok(action)
    }

    fn yolo_countdown_started(
        &mut self,
        _step_name: &str,
        kind: crate::engine::workflow::actions::CountdownKind,
    ) {
        self.countdown_kind = kind;
    }

    fn yolo_countdown_tick(
        &mut self,
        _step_name: &str,
        remaining: Duration,
        _total: Duration,
    ) -> Result<YoloTickOutcome, EngineError> {
        use crate::engine::workflow::timing::YOLO_SINK_THROTTLE_INTERVAL;
        use std::io::Write as _;

        if remaining.is_zero() {
            if self.raw_mode_guard.is_some() {
                // Clear the overlay line before advancing.
                if let Ok((_, rows)) = crossterm::terminal::size() {
                    let mut out = std::io::stdout().lock();
                    let _ = write!(out, "\x1b7\x1b[{};1H\x1b[2K\x1b8", rows);
                    let _ = out.flush();
                } else {
                    // Terminal size unavailable in raw mode: fall back to
                    // stderr with explicit \r\n so the message lands on its
                    // own line.
                    let mut err = std::io::stderr().lock();
                    let _ = write!(err, "\r\n  yolo: auto-advancing to next step...\r\n");
                    let _ = err.flush();
                }
            } else {
                eprintln!("\r\x1b[2K  yolo: auto-advancing to next step...");
            }
            return Ok(YoloTickOutcome::Continue);
        }

        let should_emit = self
            .last_sink_message_time
            .map(|t| t.elapsed() >= YOLO_SINK_THROTTLE_INTERVAL)
            .unwrap_or(true);

        if self.raw_mode_guard.is_some() {
            // Raw mode: ANSI overlay on the last terminal line.
            if should_emit {
                let secs = remaining.as_secs();
                let msg = self.countdown_text(secs);
                if let Ok((_, rows)) = crossterm::terminal::size() {
                    let mut out = std::io::stdout().lock();
                    let _ = write!(
                        out,
                        "\x1b7\x1b[{};1H\x1b[2K\x1b[7m{}\x1b[0m\x1b8",
                        rows, msg
                    );
                    let _ = out.flush();
                } else {
                    // No terminal size — can't position the overlay safely.
                    // Fall back to stderr with explicit \r\n (the cooked-mode
                    // newline isn't enough in raw mode).
                    let mut err = std::io::stderr().lock();
                    let _ = write!(err, "\r\n{}\r\n", msg);
                    let _ = err.flush();
                }
                self.last_sink_message_time = Some(std::time::Instant::now());
            }
            // In raw mode, stdin goes to the container; no interactive input.
            return Ok(YoloTickOutcome::Continue);
        }

        if should_emit {
            let secs = remaining.as_secs();
            eprint!(
                "\r\x1b[2K  {}  [n] now  [a] abort  [p] pause",
                self.countdown_text(secs)
            );
            let _ = std::io::stderr().flush();
            self.last_sink_message_time = Some(std::time::Instant::now());
        }

        if self.non_interactive {
            return Ok(self.headless.yolo_tick());
        }

        if self.yolo_stdin_rx.is_none() {
            let (tx, rx) = std::sync::mpsc::channel::<String>();
            std::thread::spawn(move || {
                use std::io::BufRead as _;
                let stdin = std::io::stdin();
                for line in stdin.lock().lines() {
                    match line {
                        Ok(l) => {
                            if tx.send(l).is_err() {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
            });
            self.yolo_stdin_rx = Some(std::sync::Mutex::new(rx));
        }

        if let Some(m) = &self.yolo_stdin_rx {
            if let Ok(rx) = m.try_lock() {
                match rx.try_recv() {
                    Ok(line) => {
                        return Ok(match line.trim() {
                            "n" | "N" => YoloTickOutcome::AdvanceNow,
                            "a" | "A" | "p" | "P" => YoloTickOutcome::Cancel,
                            _ => YoloTickOutcome::Continue,
                        });
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => {}
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {}
                }
            }
        }

        Ok(YoloTickOutcome::Continue)
    }

    fn yolo_countdown_finished(&mut self, _step_name: &str) {
        self.last_sink_message_time = None;
    }

    fn report_agent_status(
        &mut self,
        _step_name: &str,
        status: Option<&crate::data::program_status::EffectiveStatus>,
    ) {
        if self.is_json_mode() {
            return;
        }
        use crate::data::message::{MessageLevel, UserMessage, UserMessageSink as _};
        if let Some(text) = self.status_gate.line_for(status) {
            let level = match status.map(|s| s.state) {
                Some(crate::data::program_status::ProgramState::Error) => MessageLevel::Error,
                Some(crate::data::program_status::ProgramState::Blocked) => MessageLevel::Warning,
                _ => MessageLevel::Info,
            };
            self.messages.write_message(UserMessage { level, text });
        }
    }

    fn report_step_status(&mut self, _step: &WorkflowStep, status: WorkflowStepStatus) {
        match status {
            WorkflowStepStatus::Succeeded
            | WorkflowStepStatus::Failed { .. }
            | WorkflowStepStatus::Cancelled => {
                // Signal the interactive stdin reader thread to exit before
                // dropping the raw mode guard. Without this, the thread
                // would block in `poll(2)` until the next keystroke and
                // race the next step's reader thread for `/dev/stdin`.
                if let Some(flag) = self.stdin_reader_shutdown.take() {
                    flag.store(true, std::sync::atomic::Ordering::Relaxed);
                }
                // Join the reader thread so the host stdin lock is fully
                // released. Worktree finalize prompts and the workflow
                // summary that follow would otherwise wait up to one poll
                // cycle (~200ms) for the lock — and any keystrokes typed
                // in that window would be sent into the now-dead container
                // channel rather than `read_line`.
                if let Some(handle) = self.stdin_reader_handle.take() {
                    let _ = handle.join();
                }
                // Drop the raw mode guard before any status output is printed,
                // restoring cooked mode for the next step or workflow summary.
                self.raw_mode_guard.take();
                // The container's stdin channel is now stale; clear it so a
                // later workflow-control-board call doesn't try to rebind
                // stdio to a dead container.
                self.container_stdin_tx = None;
            }
            _ => {}
        }
    }

    fn report_step_output(&mut self, _step: &WorkflowStep, _output: StepOutput) {}

    fn confirm_resume(&mut self, _mismatch: &ResumeMismatch) -> Result<bool, EngineError> {
        if self.non_interactive {
            return Ok(self.headless.confirm_resume());
        }
        eprintln!("awman: workflow file changed since last run; resume anyway? [y/n]");
        let mut buf = String::new();
        if std::io::stdin().read_line(&mut buf).is_err() {
            return Ok(false);
        }
        Ok(matches!(buf.trim(), "y" | "Y"))
    }

    /// A CLI on a TTY can ask; `--headless` cannot, and falls to the
    /// engine's unattended retry path instead.
    fn supports_interactive_recovery(&self) -> bool {
        !self.non_interactive
    }

    fn report_workflow_completed(&mut self, outcome: &WorkflowOutcome) {
        if let Some(recorder) = &self.worker_events {
            recorder.completed(outcome);
        }
        let msg = match outcome {
            WorkflowOutcome::Completed => "workflow completed successfully.",
            WorkflowOutcome::CompletedTeardownFailed => "workflow completed but teardown failed.",
            WorkflowOutcome::Paused => "workflow paused.",
            WorkflowOutcome::Aborted => "workflow aborted.",
            WorkflowOutcome::Cancelled => "workflow cancelled.",
            WorkflowOutcome::Failed {
                last_step,
                exit_code,
            } => {
                eprintln!(
                    "awman: workflow failed at step '{}' (exit {}).",
                    last_step, exit_code
                );
                return;
            }
        };
        eprintln!("awman: {}", msg);
    }

    fn report_workflow_progress(&mut self, steps: &[WorkflowStepProgressInfo]) {
        if steps.is_empty() {
            return;
        }
        let name_w = steps.iter().map(|s| s.name.len()).max().unwrap_or(4).max(4);
        let agent_w = steps
            .iter()
            .map(|s| s.agent.len())
            .max()
            .unwrap_or(5)
            .max(5);
        let model_w = steps
            .iter()
            .map(|s| s.model.as_deref().unwrap_or("default").len())
            .max()
            .unwrap_or(5)
            .max(5);

        let div = format!(
            "  {bar}  {bar2}  {bar3}  {bar4}",
            bar = "─".repeat(2),
            bar2 = "─".repeat(name_w),
            bar3 = "─".repeat(agent_w),
            bar4 = "─".repeat(model_w),
        );
        eprintln!();
        eprintln!(
            "  {:>2}  {:<name_w$}  {:<agent_w$}  {:<model_w$}  Status",
            "#",
            "Step",
            "Agent",
            "Model",
            name_w = name_w,
            agent_w = agent_w,
            model_w = model_w,
        );
        eprintln!("{}", div);
        for (i, step) in steps.iter().enumerate() {
            let model_str = step.model.as_deref().unwrap_or("default");
            let status_str = match &step.status {
                WorkflowStepStatus::Pending => "· Pending".to_string(),
                WorkflowStepStatus::Running => "▶ Running".to_string(),
                WorkflowStepStatus::Succeeded => "✓ Done".to_string(),
                WorkflowStepStatus::Failed { exit_code } => format!("✗ Failed ({})", exit_code),
                WorkflowStepStatus::Cancelled => "○ Cancelled".to_string(),
                WorkflowStepStatus::Skipped => "⊘ Skipped".to_string(),
            };
            eprintln!(
                "  {:>2}  {:<name_w$}  {:<agent_w$}  {:<model_w$}  {}",
                i + 1,
                step.name,
                step.agent,
                model_str,
                status_str,
                name_w = name_w,
                agent_w = agent_w,
                model_w = model_w,
            );
        }
        eprintln!("{}", div);
        eprintln!();
    }

    fn on_phase_step_started(&mut self, kind: PhaseKind, description: &str) {
        eprintln!("awman: {}: {description}", kind.label());
    }

    fn on_phase_step_output(&mut self, _kind: PhaseKind, line: &str) {
        eprintln!("  {line}");
    }

    fn on_phase_step_completed(&mut self, kind: PhaseKind, description: &str) {
        eprintln!("awman: {}: {description} [ok]", kind.label());
    }

    fn on_phase_step_failed(
        &mut self,
        kind: PhaseKind,
        description: &str,
        exit_code: i32,
        stderr: &str,
    ) {
        eprintln!(
            "awman: {}: {description} [failed exit={exit_code}]",
            kind.label()
        );
        if !stderr.is_empty() {
            for line in stderr.lines() {
                eprintln!("  {line}");
            }
        }
    }

    /// Deliberately silent (F-15, decision Q6: delete the banner entirely).
    ///
    /// The trait method survives because the TUI uses it to refill its
    /// container slot when an interactive step takes over the terminal; the
    /// CLI has no slot to refill and prints nothing. The INTERACTIVE-mode box
    /// art that used to live here is gone from every frontend.
    fn report_step_interactive_launch(
        &mut self,
        _step: &WorkflowStep,
        _agent: &str,
        _model: Option<&str>,
    ) {
    }
}

/// The Workflow Control Board menu, one terminal line per entry.
///
/// Kept separate from the printing so the board's copy — in particular the
/// failure banner (WI-0115 §1) — can be asserted on without a TTY. The caller
/// erases exactly `len()` lines when it rewinds the menu, so every returned
/// string must be one printed line.
fn control_board_lines(available: &AvailableActions, resume_available: bool) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(failure) = &available.step_failure {
        lines.push(format!("awman: step '{}' failed.", failure.step_name));
        for line in &failure.detail_lines {
            lines.push(format!("  {line}"));
        }
        lines.push("awman: choose how to recover:".to_string());
    } else {
        lines.push("awman: workflow paused — choose next action:".to_string());
    }
    if resume_available {
        lines.push("  [s] Resume final step (return to running container)".to_string());
    }
    if available.can_launch_next {
        let label = available
            .launch_next_label
            .as_deref()
            .unwrap_or("Launch next step (new container)");
        lines.push(format!("  [n] {label}"));
    }
    if available.can_continue_in_current_container {
        lines.push("  [c] Continue in current container".to_string());
    } else if let Some(reason) = &available.continue_unavailable_reason {
        lines.push(format!("  (continue unavailable: {reason})"));
    }
    if available.can_restart_current_step {
        let label = if available.step_failure.is_some() {
            "  [r] Restart failed step"
        } else {
            "  [r] Restart current step"
        };
        lines.push(label.to_string());
    }
    if available.can_cancel_to_previous_step {
        lines.push("  [b] Back to previous step".to_string());
    } else if let Some(reason) = &available.cancel_to_previous_unavailable_reason {
        lines.push(format!("  (back unavailable: {reason})"));
    }
    if available.can_pause {
        lines.push("  [p] Pause workflow".to_string());
    }
    if available.can_abort {
        lines.push("  [a] Abort workflow".to_string());
    }
    if available.can_finish_workflow {
        lines.push("  [f] Finish workflow".to_string());
    } else if let Some(reason) = &available.finish_workflow_unavailable_reason {
        lines.push(format!("  (finish unavailable: {reason})"));
    }
    lines
}

/// Erase `n` lines above the current cursor position (stderr).
///
/// Used by the workflow control board to undo its menu output when the
/// user chooses to resume the still-running container — the menu is wiped
/// so the user returns to the agent's last screen state. Writes the
/// CSI sequence `ESC[<n>F` (cursor up `n` lines, column 1) followed by
/// `ESC[J` (clear from cursor to end of screen). No-op for `n == 0`.
fn erase_lines_above(n: usize) {
    if n == 0 {
        return;
    }
    use std::io::Write;
    let mut err = std::io::stderr().lock();
    let _ = write!(err, "\x1b[{n}F\x1b[J");
    let _ = err.flush();
}
