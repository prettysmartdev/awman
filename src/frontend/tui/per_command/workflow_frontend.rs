//! `WorkflowFrontend` impl for the TUI.

use std::time::Duration;

use crate::data::workflow_definition::WorkflowStep;
use crate::data::workflow_state::WorkflowState;
use crate::engine::container::instance::ContainerExitInfo;
use crate::engine::error::EngineError;
use crate::engine::message::UserMessageSink;
use crate::engine::workflow::actions::{
    AvailableActions, NextAction, ResumeMismatch, StepFailureChoice, StepOutput, WorkflowOutcome,
    WorkflowStepStatus, YoloTickOutcome,
};
use crate::engine::workflow::frontend::WorkflowFrontend;
use crate::frontend::tui::command_frontend::TuiCommandFrontend;
use crate::frontend::tui::dialogs::{
    DialogRequest, DialogResponse, WorkflowControlBoardState, WorkflowStepErrorState,
};

impl WorkflowFrontend for TuiCommandFrontend {
    fn user_choose_next_action(
        &mut self,
        state: &WorkflowState,
        available: &AvailableActions,
    ) -> Result<NextAction, EngineError> {
        // Use the engine-reported current step (or the first ready next step
        // if nothing is currently running).
        let step_name = state
            .step_states
            .iter()
            .find(|(_, s)| matches!(s, crate::data::workflow_state::StepState::Running { .. }))
            .map(|(name, _)| name.clone())
            .unwrap_or_else(|| "current step".to_string());
        let response = self
            .ask_dialog(DialogRequest::WorkflowControlBoard(
                WorkflowControlBoardState {
                    step_name,
                    can_launch_next: available.can_launch_next,
                    can_continue_current: available.can_continue_in_current_container,
                    can_restart: available.can_restart_current_step,
                    can_go_back: available.can_cancel_to_previous_step,
                    can_finish: available.can_finish_workflow,
                    continue_unavailable_reason: available.continue_unavailable_reason.clone(),
                    cancel_to_previous_unavailable_reason: available
                        .cancel_to_previous_unavailable_reason
                        .clone(),
                    finish_workflow_unavailable_reason: available
                        .finish_workflow_unavailable_reason
                        .clone(),
                },
            ))
            .map_err(|e| EngineError::Other(e.to_string()))?;
        Ok(match response {
            DialogResponse::Char('>') => NextAction::LaunchNext,
            DialogResponse::Char('v') => {
                let prompt = available.continue_prompt.clone().unwrap_or_default();
                NextAction::ContinueInCurrentContainer { prompt }
            }
            DialogResponse::Char('^') => NextAction::RestartCurrentStep,
            DialogResponse::Char('<') => NextAction::CancelToPreviousStep,
            DialogResponse::Char('f') => NextAction::FinishWorkflow,
            DialogResponse::Char('a') => NextAction::Abort,
            // Esc on the board pauses the workflow but keeps state for resume.
            DialogResponse::Dismissed => NextAction::Pause,
            _ => NextAction::Pause,
        })
    }

    fn confirm_resume(&mut self, mismatch: &ResumeMismatch) -> Result<bool, EngineError> {
        let response = self
            .ask_dialog(DialogRequest::YesNo {
                title: "Resume workflow?".into(),
                body: format!(
                    "Workflow '{}' has changed since last run.\n{}\n\nResume anyway?",
                    mismatch.workflow_name, mismatch.message
                ),
            })
            .map_err(|e| EngineError::Other(e.to_string()))?;
        Ok(matches!(
            response,
            DialogResponse::Yes | DialogResponse::Char('y')
        ))
    }

    fn user_choose_after_step_failure(
        &mut self,
        step: &WorkflowStep,
        exit: &ContainerExitInfo,
    ) -> Result<StepFailureChoice, EngineError> {
        // Build a few helpful lines from the actual exit info instead of the
        // old stub "Step failed" string. Old amux only had `exit_code`; the
        // new info also carries `signal` and timing.
        let mut error_lines = Vec::new();
        if let Some(sig) = exit.signal {
            error_lines.push(format!("Container exited from signal {}", sig));
        }
        error_lines.push(format!("Exit code: {}", exit.exit_code));
        let duration =
            exit.ended_at.signed_duration_since(exit.started_at).num_seconds().max(0);
        error_lines.push(format!("Ran for {}s", duration));

        let response = self
            .ask_dialog(DialogRequest::WorkflowStepError(WorkflowStepErrorState {
                step_name: step.name.clone(),
                error_lines,
            }))
            .map_err(|e| EngineError::Other(e.to_string()))?;
        Ok(match response {
            DialogResponse::Char('r') | DialogResponse::Char('1') => StepFailureChoice::Retry,
            DialogResponse::Char('a') => StepFailureChoice::Abort,
            _ => StepFailureChoice::Pause,
        })
    }

    fn report_step_interactive_launch(
        &mut self,
        _step: &WorkflowStep,
        agent: &str,
        _model: Option<&str>,
    ) {
        // Signal the TUI event loop to reset the vt100 parser for the new step.
        self.pty_reset_flag
            .store(true, std::sync::atomic::Ordering::Relaxed);
        // Update container agent display name for the new step.
        self.messages.info(format!(
            "Launching agent '{}' in new container...",
            agent
        ));
    }

    fn report_step_status(&mut self, step: &WorkflowStep, status: WorkflowStepStatus) {
        self.messages
            .info(format!("workflow step '{}': {:?}", step.name, status));
        // Update the shared workflow_view so the strip reflects the new status.
        if let Ok(mut guard) = self.workflow_view.lock() {
            if let Some(view) = guard.as_mut() {
                let status_str = workflow_status_str(&status);
                if let Some(s) = view.steps.iter_mut().find(|s| s.name == step.name) {
                    s.status = status_str.to_string();
                }
                view.current_step =
                    if matches!(status, WorkflowStepStatus::Running) {
                        Some(step.name.clone())
                    } else if view
                        .current_step
                        .as_deref()
                        .map(|cur| cur == step.name.as_str())
                        .unwrap_or(false)
                    {
                        // Step finished — clear current_step so the strip
                        // doesn't keep highlighting a now-Done step.
                        None
                    } else {
                        view.current_step.clone()
                    };
            }
        }
    }

    fn report_step_output(&mut self, _step: &WorkflowStep, _output: StepOutput) {
        // Output goes through ContainerFrontend, not here.
    }

    fn report_step_stuck(&mut self, step: &WorkflowStep) {
        self.messages.warning(format!(
            "Step '{}' appears stuck (no output for 30s)",
            step.name
        ));
    }

    fn report_step_unstuck(&mut self, step: &WorkflowStep) {
        self.messages
            .info(format!("Step '{}' resumed producing output", step.name));
    }

    fn yolo_countdown_tick(
        &mut self,
        remaining: Duration,
    ) -> Result<YoloTickOutcome, EngineError> {
        // Don't spawn a new dialog on every 100ms tick (the engine ticks at
        // 10Hz). Instead, poke a shared state struct that the renderer reads
        // and shows as a non-modal overlay. The engine cancels via PTY
        // activity (via report_step_unstuck → that path also resets the
        // ticker on the engine side).
        let step_name = self
            .workflow_view
            .lock()
            .ok()
            .and_then(|g| g.as_ref().and_then(|v| v.current_step.clone()))
            .unwrap_or_else(|| "current step".to_string());
        if let Ok(mut guard) = self.yolo_state.lock() {
            *guard = Some(crate::frontend::tui::tabs::YoloState {
                step_name,
                remaining_secs: remaining.as_secs(),
            });
        }
        // Keep the countdown going. The TUI cancels via the keymap-level
        // path (Esc) which sets a sentinel on yolo_state that we check here.
        // If the user pressed Esc, the renderer-side handler clears
        // `yolo_state` and we propagate Cancel.
        let still_active = self
            .yolo_state
            .lock()
            .ok()
            .map(|g| g.is_some())
            .unwrap_or(false);
        Ok(if still_active {
            YoloTickOutcome::Continue
        } else {
            YoloTickOutcome::Cancel
        })
    }

    fn report_workflow_completed(&mut self, outcome: &WorkflowOutcome) {
        // Clear the yolo overlay so it doesn't stick around after completion.
        if let Ok(mut g) = self.yolo_state.lock() {
            *g = None;
        }
        match outcome {
            WorkflowOutcome::Completed => {
                self.messages.success("Workflow completed successfully")
            }
            WorkflowOutcome::Paused => self.messages.info("Workflow paused"),
            WorkflowOutcome::Aborted => self.messages.warning("Workflow aborted"),
            WorkflowOutcome::Failed {
                last_step,
                exit_code,
            } => {
                self.messages.error_msg(format!(
                    "Workflow failed at step '{}' (exit {})",
                    last_step, exit_code
                ));
            }
        }
    }

    fn report_workflow_progress(
        &mut self,
        steps: &[crate::engine::workflow::actions::WorkflowStepProgressInfo],
    ) {
        // First snapshot of the workflow → seed workflow_view. Subsequent
        // calls overwrite step statuses (engine sends progress whenever the
        // shape of the workflow changes / before each step).
        if let Ok(mut guard) = self.workflow_view.lock() {
            let view = guard.get_or_insert_with(|| {
                crate::frontend::tui::tabs::WorkflowViewState::default()
            });
            // Re-build the step list from scratch so renames/reorders apply.
            let prev_disabled = view.auto_disabled.clone();
            view.steps = steps
                .iter()
                .map(|s| crate::frontend::tui::tabs::WorkflowStepView {
                    name: s.name.clone(),
                    status: workflow_status_str(&s.status).to_string(),
                    agent: Some(s.agent.clone()),
                    model: s.model.clone(),
                    depends_on: Vec::new(), // Engine doesn't expose this here yet
                })
                .collect();
            view.current_step = steps
                .iter()
                .find(|s| matches!(s.status, WorkflowStepStatus::Running))
                .map(|s| s.name.clone());
            view.auto_disabled = prev_disabled;
        }
    }
}

/// Map a `WorkflowStepStatus` to the lower-case string used in
/// `WorkflowStepView.status` (the renderer matches on it).
fn workflow_status_str(status: &WorkflowStepStatus) -> &'static str {
    match status {
        WorkflowStepStatus::Pending => "pending",
        WorkflowStepStatus::Running => "running",
        WorkflowStepStatus::Succeeded => "done",
        WorkflowStepStatus::Failed { .. } => "error",
        WorkflowStepStatus::Cancelled => "cancelled",
        WorkflowStepStatus::Skipped => "skipped",
    }
}

#[cfg(test)]
mod tests {
    use crate::engine::container::instance::ContainerExitInfo;
    use crate::engine::workflow::actions::StepFailureChoice;
    use crate::engine::workflow::frontend::WorkflowFrontend;
    use crate::frontend::tui::command_frontend::TuiCommandFrontend;
    use crate::frontend::tui::dialogs::{DialogRequest, DialogResponse};

    fn make_frontend() -> (
        TuiCommandFrontend,
        std::sync::mpsc::Receiver<DialogRequest>,
        std::sync::mpsc::Sender<DialogResponse>,
    ) {
        let (req_tx, req_rx) = std::sync::mpsc::channel::<DialogRequest>();
        let (resp_tx, resp_rx) = std::sync::mpsc::channel::<DialogResponse>();
        let (stdout_tx, _stdout_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let (stdin_tx, stdin_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let (_resize_tx, resize_rx) =
            tokio::sync::mpsc::unbounded_channel::<(u16, u16)>();
        let container_io = crate::engine::container::frontend::ContainerIo {
            stdout: stdout_tx,
            stdin_tx,
            stdin_rx,
            resize: resize_rx,
            initial_size: (80, 24),
        };
        let status_log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let parsed = crate::command::dispatch::parsed_input::ParsedCommandBoxInput {
            path: vec!["workflow".into()],
            flags: Default::default(),
            arguments: Default::default(),
        };
        let workflow_view = std::sync::Arc::new(std::sync::Mutex::new(None));
        let yolo_state = std::sync::Arc::new(std::sync::Mutex::new(None));
        let pty_reset_flag = std::sync::Arc::new(
            std::sync::atomic::AtomicBool::new(false),
        );
        let frontend = TuiCommandFrontend::new(
            parsed,
            status_log,
            req_tx,
            resp_rx,
            container_io,
            workflow_view,
            yolo_state,
            pty_reset_flag,
        );
        (frontend, req_rx, resp_tx)
    }

    fn dummy_step() -> crate::data::workflow_definition::WorkflowStep {
        crate::data::workflow_definition::WorkflowStep {
            name: "test-step".into(),
            depends_on: vec![],
            prompt_template: "do the thing".into(),
            agent: None,
            model: None,
        }
    }

    fn dummy_exit_info() -> ContainerExitInfo {
        ContainerExitInfo {
            exit_code: 1,
            signal: None,
            started_at: chrono::Utc::now(),
            ended_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn user_choose_after_step_failure_r_retries() {
        let (mut frontend, req_rx, resp_tx) = make_frontend();
        let step = dummy_step();
        let exit = dummy_exit_info();
        let handle = std::thread::spawn(move || {
            let _req = req_rx.recv().unwrap();
            resp_tx.send(DialogResponse::Char('r')).unwrap();
        });
        let result = frontend.user_choose_after_step_failure(&step, &exit).unwrap();
        handle.join().unwrap();
        assert_eq!(result, StepFailureChoice::Retry);
    }

    #[test]
    fn user_choose_after_step_failure_1_retries() {
        let (mut frontend, req_rx, resp_tx) = make_frontend();
        let step = dummy_step();
        let exit = dummy_exit_info();
        let handle = std::thread::spawn(move || {
            let _req = req_rx.recv().unwrap();
            resp_tx.send(DialogResponse::Char('1')).unwrap();
        });
        let result = frontend.user_choose_after_step_failure(&step, &exit).unwrap();
        handle.join().unwrap();
        assert_eq!(result, StepFailureChoice::Retry);
    }

    #[test]
    fn user_choose_after_step_failure_a_aborts() {
        let (mut frontend, req_rx, resp_tx) = make_frontend();
        let step = dummy_step();
        let exit = dummy_exit_info();
        let handle = std::thread::spawn(move || {
            let _req = req_rx.recv().unwrap();
            resp_tx.send(DialogResponse::Char('a')).unwrap();
        });
        let result = frontend.user_choose_after_step_failure(&step, &exit).unwrap();
        handle.join().unwrap();
        assert_eq!(result, StepFailureChoice::Abort);
    }

    #[test]
    fn user_choose_after_step_failure_dismissed_pauses() {
        let (mut frontend, req_rx, resp_tx) = make_frontend();
        let step = dummy_step();
        let exit = dummy_exit_info();
        let handle = std::thread::spawn(move || {
            let _req = req_rx.recv().unwrap();
            resp_tx.send(DialogResponse::Dismissed).unwrap();
        });
        let result = frontend.user_choose_after_step_failure(&step, &exit).unwrap();
        handle.join().unwrap();
        assert_eq!(result, StepFailureChoice::Pause);
    }

    #[test]
    fn confirm_resume_yes_returns_true() {
        use crate::engine::workflow::actions::ResumeMismatch;
        let (mut frontend, req_rx, resp_tx) = make_frontend();
        let mismatch = ResumeMismatch {
            workflow_name: "test-wf".into(),
            saved_hash: "abc".into(),
            current_hash: "def".into(),
            message: "Steps changed".into(),
        };
        let handle = std::thread::spawn(move || {
            let _req = req_rx.recv().unwrap();
            resp_tx.send(DialogResponse::Yes).unwrap();
        });
        let result = frontend.confirm_resume(&mismatch).unwrap();
        handle.join().unwrap();
        assert!(result);
    }

    #[test]
    fn confirm_resume_no_returns_false() {
        use crate::engine::workflow::actions::ResumeMismatch;
        let (mut frontend, req_rx, resp_tx) = make_frontend();
        let mismatch = ResumeMismatch {
            workflow_name: "test-wf".into(),
            saved_hash: "abc".into(),
            current_hash: "def".into(),
            message: "Steps changed".into(),
        };
        let handle = std::thread::spawn(move || {
            let _req = req_rx.recv().unwrap();
            resp_tx.send(DialogResponse::No).unwrap();
        });
        let result = frontend.confirm_resume(&mismatch).unwrap();
        handle.join().unwrap();
        assert!(!result);
    }
}
