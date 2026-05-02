//! `WorkflowFrontend` impl for the CLI.
//!
//! Per WI 0069 §1, the CLI prompts on stdin (when it is a TTY) and falls
//! back to the safe non-interactive defaults from §7u otherwise. The
//! prompt presents only the actions in `AvailableActions` whose `can_*`
//! flags are true; excluded actions are skipped (with their
//! `*_unavailable_reason` printed as a parenthetical note).

use std::time::Duration;

use crate::data::workflow_definition::WorkflowStep;
use crate::data::workflow_state::WorkflowState;
use crate::engine::container::instance::ContainerExitInfo;
use crate::engine::error::EngineError;
use crate::engine::workflow::actions::{
    AvailableActions, NextAction, ResumeMismatch, StepFailureChoice, StepOutput,
    WorkflowOutcome, WorkflowStepStatus, YoloTickOutcome,
};
use crate::engine::workflow::frontend::WorkflowFrontend;

use crate::frontend::cli::command_frontend::CliFrontend;
use crate::frontend::cli::output::stdin_is_tty;

impl WorkflowFrontend for CliFrontend {
    fn user_choose_next_action(
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
            "b" | "B" if available.can_cancel_to_previous_step => {
                NextAction::CancelToPreviousStep
            }
            "p" | "P" if available.can_pause => NextAction::Pause,
            "a" | "A" if available.can_abort => NextAction::Abort,
            "f" | "F" if available.can_finish_workflow => NextAction::FinishWorkflow,
            _ => NextAction::Pause,
        })
    }

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

    fn report_step_status(&mut self, step: &WorkflowStep, status: WorkflowStepStatus) {
        let _ = (step, status);
    }

    fn report_step_output(&mut self, _step: &WorkflowStep, _output: StepOutput) {}

    fn report_step_stuck(&mut self, _step: &WorkflowStep) {}

    fn report_step_unstuck(&mut self, _step: &WorkflowStep) {}

    fn yolo_countdown_tick(
        &mut self,
        _remaining: Duration,
    ) -> Result<YoloTickOutcome, EngineError> {
        Ok(YoloTickOutcome::Continue)
    }

    fn report_workflow_completed(&mut self, _outcome: &WorkflowOutcome) {}
}
