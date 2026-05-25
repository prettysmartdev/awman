//! `WorkflowFrontend` impl for the CLI.
//!
//! The CLI prompts on stdin (when it is a TTY) and falls back to the safe
//! non-interactive defaults otherwise. The
//! prompt presents only the actions in `AvailableActions` whose `can_*`
//! flags are true; excluded actions are skipped (with their
//! `*_unavailable_reason` printed as a parenthetical note).

use std::time::Duration;

use crate::data::workflow_definition::WorkflowStep;
use crate::data::workflow_state::WorkflowState;
use crate::engine::container::instance::ContainerExitInfo;
use crate::engine::error::EngineError;
use crate::engine::workflow::actions::{
    AvailableActions, NextAction, ResumeMismatch, StepFailureChoice, StepOutput, WorkflowOutcome,
    WorkflowStepProgressInfo, WorkflowStepStatus, YoloTickOutcome,
};
use crate::engine::workflow::frontend::WorkflowFrontend;

use crate::frontend::cli::command_frontend::CliFrontend;

impl WorkflowFrontend for CliFrontend {
    fn show_workflow_control_board(
        &mut self,
        _state: &WorkflowState,
        available: &AvailableActions,
    ) -> Result<NextAction, EngineError> {
        if self.non_interactive {
            return Ok(NextAction::LaunchNext);
        }
        eprintln!("awman: workflow paused — choose next action:");
        if available.can_launch_next {
            eprintln!("  [n] Launch next step (new container)");
        }
        if available.can_continue_in_current_container {
            eprintln!("  [c] Continue in current container");
        } else if let Some(reason) = &available.continue_unavailable_reason {
            eprintln!("  (continue unavailable: {reason})");
        }
        if available.can_restart_current_step {
            eprintln!("  [r] Restart current step");
        }
        if available.can_cancel_to_previous_step {
            eprintln!("  [b] Back to previous step");
        } else if let Some(reason) = &available.cancel_to_previous_unavailable_reason {
            eprintln!("  (back unavailable: {reason})");
        }
        if available.can_pause {
            eprintln!("  [p] Pause workflow");
        }
        if available.can_abort {
            eprintln!("  [a] Abort workflow");
        }
        if available.can_finish_workflow {
            eprintln!("  [f] Finish workflow");
        } else if let Some(reason) = &available.finish_workflow_unavailable_reason {
            eprintln!("  (finish unavailable: {reason})");
        }
        let mut buf = String::new();
        if std::io::stdin().read_line(&mut buf).is_err() {
            return Ok(NextAction::Pause);
        }
        Ok(match buf.trim() {
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
        })
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
                let msg = format!(" yolo: auto-advancing in {}s ", secs);
                if let Ok((_, rows)) = crossterm::terminal::size() {
                    let mut out = std::io::stdout().lock();
                    let _ = write!(out, "\x1b7\x1b[{};1H\x1b[2K\x1b[7m{}\x1b[0m\x1b8", rows, msg);
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
                "\r\x1b[2K  yolo: auto-advancing in {:2}s  [n] now  [a] abort  [p] pause",
                secs
            );
            let _ = std::io::stderr().flush();
            self.last_sink_message_time = Some(std::time::Instant::now());
        }

        if self.non_interactive {
            return Ok(YoloTickOutcome::Continue);
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
                // Drop the raw mode guard before any status output is printed,
                // restoring cooked mode for the next step or workflow summary.
                self.raw_mode_guard.take();
            }
            _ => {}
        }
    }

    fn report_step_output(&mut self, _step: &WorkflowStep, _output: StepOutput) {}

    fn confirm_resume(&mut self, _mismatch: &ResumeMismatch) -> Result<bool, EngineError> {
        if self.non_interactive {
            return Ok(false);
        }
        eprintln!("awman: workflow file changed since last run; resume anyway? [y/n]");
        let mut buf = String::new();
        if std::io::stdin().read_line(&mut buf).is_err() {
            return Ok(false);
        }
        Ok(matches!(buf.trim(), "y" | "Y"))
    }

    fn user_choose_after_step_failure(
        &mut self,
        step: &WorkflowStep,
        exit: &ContainerExitInfo,
    ) -> Result<StepFailureChoice, EngineError> {
        if self.non_interactive {
            return Ok(StepFailureChoice::Pause);
        }
        let signal_str = exit
            .signal
            .map(|s| s.to_string())
            .unwrap_or_else(|| "—".to_string());
        eprintln!(
            "awman: step '{}' failed (exit {}, signal {signal_str}). [r]etry / [p]ause / [a]bort?",
            step.name, exit.exit_code,
        );
        let mut buf = String::new();
        if std::io::stdin().read_line(&mut buf).is_err() {
            return Ok(StepFailureChoice::Pause);
        }
        Ok(match buf.trim() {
            "r" | "R" => StepFailureChoice::Retry,
            "a" | "A" => StepFailureChoice::Abort,
            _ => StepFailureChoice::Pause,
        })
    }

    fn report_workflow_completed(&mut self, outcome: &WorkflowOutcome) {
        let msg = match outcome {
            WorkflowOutcome::Completed => "workflow completed successfully.",
            WorkflowOutcome::Paused => "workflow paused.",
            WorkflowOutcome::Aborted => "workflow aborted.",
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

    fn on_setup_step_started(&mut self, description: &str) {
        eprintln!("awman: setup: {description}");
    }

    fn on_setup_step_output(&mut self, line: &str) {
        eprintln!("  {line}");
    }

    fn on_setup_step_completed(&mut self, description: &str) {
        eprintln!("awman: setup: {description} [ok]");
    }

    fn on_setup_step_failed(&mut self, description: &str, exit_code: i32, stderr: &str) {
        eprintln!("awman: setup: {description} [failed exit={exit_code}]");
        if !stderr.is_empty() {
            for line in stderr.lines() {
                eprintln!("  {line}");
            }
        }
    }

    fn on_teardown_step_started(&mut self, description: &str) {
        eprintln!("awman: teardown: {description}");
    }

    fn on_teardown_step_output(&mut self, line: &str) {
        eprintln!("  {line}");
    }

    fn on_teardown_step_completed(&mut self, description: &str) {
        eprintln!("awman: teardown: {description} [ok]");
    }

    fn on_teardown_step_failed(&mut self, description: &str, exit_code: i32, stderr: &str) {
        eprintln!("awman: teardown: {description} [failed exit={exit_code}]");
        if !stderr.is_empty() {
            for line in stderr.lines() {
                eprintln!("  {line}");
            }
        }
    }

    fn report_step_interactive_launch(
        &mut self,
        _step: &WorkflowStep,
        agent: &str,
        _model: Option<&str>,
    ) {
        if self.non_interactive {
            return;
        }
        eprintln!();
        eprintln!("╔══════════════════════════════════════════════════════════════╗");
        eprintln!("║                                                              ║");
        eprintln!("║     ╦╔╗╔╔╦╗╔═╗╦═╗╔═╗╔═╗╔╦╗╦╦  ╦╔═╗  ╔╦╗╔═╗╔╦╗╔═╗             ║");
        eprintln!("║     ║║║║ ║ ║╣ ╠╦╝╠═╣║   ║ ║╚╗╔╝║╣   ║║║║ ║ ║║║╣              ║");
        eprintln!("║     ╩╝╚╝ ╩ ╚═╝╩╚═╩ ╩╚═╝ ╩ ╩ ╚╝ ╚═╝  ╩ ╩╚═╝═╩╝╚═╝             ║");
        eprintln!("║                                                              ║");
        let label = format!("║  Agent '{}' is launching in INTERACTIVE mode.", agent);
        let pad = 64usize.saturating_sub(label.chars().count() + 1);
        eprintln!("{}{}║", label, " ".repeat(pad));
        eprintln!("║  You will need to quit the agent (Ctrl+C or exit)            ║");
        eprintln!("║  when its work is complete.                                  ║");
        eprintln!("║                                                              ║");
        eprintln!("╚══════════════════════════════════════════════════════════════╝");
        eprintln!();
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crate::command::dispatch::catalogue::CommandCatalogue;
    use crate::data::workflow_definition::WorkflowStep;
    use crate::engine::workflow::actions::WorkflowStepStatus;
    use crate::engine::workflow::frontend::WorkflowFrontend;
    use crate::frontend::cli::command_frontend::{CliFrontend, RawModeGuard};

    fn make_step(name: &str) -> WorkflowStep {
        WorkflowStep {
            name: name.to_string(),
            depends_on: vec![],
            prompt_template: "test prompt".to_string(),
            agent: None,
            model: None,
        }
    }

    /// Build a non-interactive `CliFrontend` suitable for unit tests.
    /// In the test environment stdin is never a TTY, so `non_interactive` is
    /// always set to `true` by `CliFrontend::new`.
    fn make_frontend() -> CliFrontend {
        let cmd = CommandCatalogue::get().build_clap_command();
        let m = cmd
            .try_get_matches_from(["awman", "exec", "workflow", "wf.toml"])
            .unwrap();
        CliFrontend::new(m)
    }

    // ── raw-mode guard lifecycle ──────────────────────────────────────────────

    /// Terminal status (Succeeded) drops the guard before the call returns,
    /// restoring cooked mode so the next step's output prints cleanly.
    #[test]
    fn raw_mode_guard_dropped_on_step_succeeded() {
        let mut fe = make_frontend();
        // Inject a guard directly (bypasses enable_raw_mode — safe in tests).
        fe.raw_mode_guard = Some(RawModeGuard);
        assert!(fe.raw_mode_guard.is_some());

        fe.report_step_status(&make_step("s"), WorkflowStepStatus::Succeeded);

        assert!(
            fe.raw_mode_guard.is_none(),
            "guard must be dropped when the step Succeeds"
        );
    }

    #[test]
    fn raw_mode_guard_dropped_on_step_failed() {
        let mut fe = make_frontend();
        fe.raw_mode_guard = Some(RawModeGuard);

        fe.report_step_status(&make_step("s"), WorkflowStepStatus::Failed { exit_code: 1 });

        assert!(fe.raw_mode_guard.is_none(), "guard must be dropped on Failed");
    }

    #[test]
    fn raw_mode_guard_dropped_on_step_cancelled() {
        let mut fe = make_frontend();
        fe.raw_mode_guard = Some(RawModeGuard);

        fe.report_step_status(&make_step("s"), WorkflowStepStatus::Cancelled);

        assert!(fe.raw_mode_guard.is_none(), "guard must be dropped on Cancelled");
    }

    /// On a terminal status, the stdin-reader-shutdown flag must be flipped
    /// before the guard drops, so the reader thread wakes from `poll(2)` and
    /// exits instead of racing the next step's reader for `/dev/stdin`.
    #[test]
    fn stdin_reader_shutdown_flag_set_on_terminal_status() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        let mut fe = make_frontend();
        let flag = Arc::new(AtomicBool::new(false));
        fe.stdin_reader_shutdown = Some(flag.clone());

        fe.report_step_status(&make_step("s"), WorkflowStepStatus::Succeeded);

        assert!(
            flag.load(Ordering::Relaxed),
            "shutdown flag must be set so the poll-based stdin reader exits"
        );
        assert!(
            fe.stdin_reader_shutdown.is_none(),
            "frontend must drop its handle to the flag once signaled"
        );
    }

    /// Non-terminal statuses must leave the shutdown flag alone — the reader
    /// is still needed while the step is running.
    #[test]
    fn stdin_reader_shutdown_flag_untouched_on_running_status() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        let mut fe = make_frontend();
        let flag = Arc::new(AtomicBool::new(false));
        fe.stdin_reader_shutdown = Some(flag.clone());

        fe.report_step_status(&make_step("s"), WorkflowStepStatus::Running);

        assert!(!flag.load(Ordering::Relaxed));
        assert!(fe.stdin_reader_shutdown.is_some());
    }

    /// Non-terminal statuses must leave the guard intact so raw mode is not
    /// prematurely disabled while the container is still running.
    #[test]
    fn raw_mode_guard_retained_on_running_status() {
        let mut fe = make_frontend();
        fe.raw_mode_guard = Some(RawModeGuard);

        fe.report_step_status(&make_step("s"), WorkflowStepStatus::Running);

        assert!(
            fe.raw_mode_guard.is_some(),
            "guard must NOT be dropped while the step is Running"
        );
    }

    #[test]
    fn raw_mode_guard_retained_on_pending_status() {
        let mut fe = make_frontend();
        fe.raw_mode_guard = Some(RawModeGuard);

        fe.report_step_status(&make_step("s"), WorkflowStepStatus::Pending);

        assert!(
            fe.raw_mode_guard.is_some(),
            "guard must NOT be dropped for Pending status"
        );
    }

    // ── yolo countdown message throttle ──────────────────────────────────────

    /// The first `yolo_countdown_tick` call (no `last_sink_message_time` yet)
    /// must set the timestamp, indicating a message was emitted.
    #[tokio::test]
    async fn yolo_countdown_first_tick_sets_throttle_timestamp() {
        let mut fe = make_frontend();
        assert!(fe.last_sink_message_time.is_none());

        fe.yolo_countdown_tick("step", Duration::from_secs(60), Duration::from_secs(60))
            .unwrap();

        assert!(
            fe.last_sink_message_time.is_some(),
            "first tick must set last_sink_message_time"
        );
    }

    /// A second rapid tick (well within the 10-second window) must NOT update
    /// `last_sink_message_time`, proving the message was suppressed.
    #[tokio::test]
    async fn yolo_countdown_rapid_second_tick_is_suppressed() {
        let mut fe = make_frontend();

        fe.yolo_countdown_tick("step", Duration::from_secs(60), Duration::from_secs(60))
            .unwrap();
        let first_time = fe.last_sink_message_time.unwrap();

        // Immediately call again — should be throttled.
        fe.yolo_countdown_tick("step", Duration::from_secs(59), Duration::from_secs(60))
            .unwrap();
        let second_time = fe.last_sink_message_time.unwrap();

        assert_eq!(
            first_time, second_time,
            "rapid second tick must not update last_sink_message_time"
        );
    }

    /// Once 10+ seconds have elapsed (simulated by rewinding `last_sink_message_time`),
    /// the next tick must emit and update the timestamp.
    #[tokio::test]
    async fn yolo_countdown_tick_emits_after_throttle_window_elapses() {
        let mut fe = make_frontend();

        fe.yolo_countdown_tick("step", Duration::from_secs(60), Duration::from_secs(60))
            .unwrap();

        // Simulate 11 seconds having passed by rewinding the timestamp.
        fe.last_sink_message_time =
            Some(std::time::Instant::now() - Duration::from_secs(11));
        let rewound = fe.last_sink_message_time.unwrap();

        fe.yolo_countdown_tick("step", Duration::from_secs(58), Duration::from_secs(60))
            .unwrap();

        let updated = fe.last_sink_message_time.unwrap();
        assert!(
            updated > rewound,
            "tick after throttle window must refresh last_sink_message_time"
        );
    }

    /// `yolo_countdown_finished` resets `last_sink_message_time` to `None`
    /// so the next countdown's first tick always emits.
    #[test]
    fn yolo_countdown_finished_resets_throttle_timestamp() {
        let mut fe = make_frontend();
        fe.last_sink_message_time = Some(std::time::Instant::now());

        fe.yolo_countdown_finished("step");

        assert!(
            fe.last_sink_message_time.is_none(),
            "yolo_countdown_finished must reset last_sink_message_time to None"
        );
    }
}
