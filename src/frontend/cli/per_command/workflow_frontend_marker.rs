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
use crate::frontend::cli::output::stdin_is_tty;

impl WorkflowFrontend for CliFrontend {
    fn show_workflow_control_board(
        &mut self,
        _state: &WorkflowState,
        available: &AvailableActions,
    ) -> Result<NextAction, EngineError> {
        if !stdin_is_tty() {
            return Ok(NextAction::LaunchNext);
        }
        eprintln!("amux: workflow paused — choose next action:");
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
        use std::io::Write as _;

        if remaining.is_zero() {
            eprintln!("\r\x1b[2K  yolo: auto-advancing to next step...");
            return Ok(YoloTickOutcome::Continue);
        }

        let secs = remaining.as_secs();
        eprint!(
            "\r\x1b[2K  yolo: auto-advancing in {:2}s  [n] now  [a] abort  [p] pause",
            secs
        );
        let _ = std::io::stderr().flush();

        if !stdin_is_tty() {
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

    fn report_step_status(&mut self, _step: &WorkflowStep, _status: WorkflowStepStatus) {}

    fn report_step_output(&mut self, _step: &WorkflowStep, _output: StepOutput) {}

    fn confirm_resume(&mut self, _mismatch: &ResumeMismatch) -> Result<bool, EngineError> {
        if !stdin_is_tty() {
            return Ok(false);
        }
        eprintln!("amux: workflow file changed since last run; resume anyway? [y/n]");
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
        if !stdin_is_tty() {
            return Ok(StepFailureChoice::Pause);
        }
        let signal_str = exit
            .signal
            .map(|s| s.to_string())
            .unwrap_or_else(|| "—".to_string());
        eprintln!(
            "amux: step '{}' failed (exit {}, signal {signal_str}). [r]etry / [p]ause / [a]bort?",
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
                    "amux: workflow failed at step '{}' (exit {}).",
                    last_step, exit_code
                );
                return;
            }
        };
        eprintln!("amux: {}", msg);
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

    fn report_step_interactive_launch(
        &mut self,
        _step: &WorkflowStep,
        agent: &str,
        _model: Option<&str>,
    ) {
        if !stdin_is_tty() {
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
