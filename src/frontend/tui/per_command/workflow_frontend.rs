//! `WorkflowFrontend` impl for the TUI.

use std::time::Duration;

use crate::data::workflow_definition::WorkflowStep;
use crate::data::workflow_state::WorkflowState;
use crate::engine::container::instance::ContainerExitInfo;
use crate::engine::error::EngineError;
use crate::engine::message::UserMessageSink;
use crate::engine::workflow::actions::{
    AvailableActions, NextAction, ResumeMismatch, StepFailureChoice, StepOutput, WorkflowOutcome,
    WorkflowStepProgressInfo, WorkflowStepStatus, YoloTickOutcome,
};
use crate::engine::workflow::frontend::WorkflowFrontend;
use crate::engine::workflow::EngineRequest;
use crate::frontend::tui::command_frontend::TuiCommandFrontend;
use crate::frontend::tui::dialogs::{
    DialogRequest, DialogResponse, WorkflowControlBoardState, WorkflowStepErrorState,
};

impl WorkflowFrontend for TuiCommandFrontend {
    fn show_workflow_control_board(
        &mut self,
        state: &WorkflowState,
        available: &AvailableActions,
    ) -> Result<NextAction, EngineError> {
        let step_name = state
            .step_states
            .iter()
            .find(|(_, s)| matches!(s, crate::data::workflow_state::StepState::Running { .. }))
            .map(|(name, _)| name.clone())
            .unwrap_or_else(|| "current step".to_string());

        // Lightweight step confirm for the simple "advance to next step?" case.
        let has_failures = state
            .step_states
            .values()
            .any(|s| matches!(s, crate::data::workflow_state::StepState::Failed { .. }));
        if available.can_launch_next && !has_failures && !available.can_dismiss {
            let mut pending = state
                .step_states
                .iter()
                .filter(|(_, s)| matches!(s, crate::data::workflow_state::StepState::Pending));
            let first_pending = pending.next().map(|(name, _)| name.clone());
            let is_single = first_pending.is_some() && pending.next().is_none();
            if let Some(next_name) = first_pending.filter(|_| is_single) {
                let response = self
                    .ask_dialog(DialogRequest::WorkflowStepConfirm(
                        crate::frontend::tui::dialogs::WorkflowStepConfirmState {
                            completed_step: step_name.clone(),
                            next_step: next_name,
                        },
                    ))
                    .map_err(|e| EngineError::Other(e.to_string()))?;
                return Ok(match response {
                    DialogResponse::Char('>') => NextAction::LaunchNext,
                    DialogResponse::Char('W') => {
                        let response2 = self
                            .ask_dialog(DialogRequest::WorkflowControlBoard(
                                WorkflowControlBoardState {
                                    step_name: step_name.clone(),
                                    can_launch_next: available.can_launch_next,
                                    can_continue_current: available
                                        .can_continue_in_current_container,
                                    can_restart: available.can_restart_current_step,
                                    can_go_back: available.can_cancel_to_previous_step,
                                    can_finish: available.can_finish_workflow,
                                    continue_unavailable_reason: available
                                        .continue_unavailable_reason
                                        .clone(),
                                    cancel_to_previous_unavailable_reason: available
                                        .cancel_to_previous_unavailable_reason
                                        .clone(),
                                    finish_workflow_unavailable_reason: available
                                        .finish_workflow_unavailable_reason
                                        .clone(),
                                    can_dismiss: available.can_dismiss,
                                },
                            ))
                            .map_err(|e| EngineError::Other(e.to_string()))?;
                        wcb_response_to_action(response2, available)
                    }
                    DialogResponse::Dismissed => NextAction::Pause,
                    _ => NextAction::Pause,
                });
            }
        }

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
                    can_dismiss: available.can_dismiss,
                },
            ))
            .map_err(|e| EngineError::Other(e.to_string()))?;
        Ok(wcb_response_to_action(response, available))
    }

    fn yolo_countdown_tick(
        &mut self,
        step_name: &str,
        remaining: Duration,
        _total: Duration,
    ) -> Result<YoloTickOutcome, EngineError> {
        if self
            .yolo_cancel_flag
            .swap(false, std::sync::atomic::Ordering::Relaxed)
        {
            if let Ok(mut guard) = self.yolo_state.lock() {
                *guard = None;
            }
            return Ok(YoloTickOutcome::Cancel);
        }
        if let Ok(mut guard) = self.yolo_state.lock() {
            *guard = Some(crate::frontend::tui::tabs::YoloState {
                step_name: step_name.to_string(),
                remaining_secs: remaining.as_secs(),
            });
        }
        Ok(YoloTickOutcome::Continue)
    }

    fn yolo_countdown_started(&mut self, _step_name: &str) {
        // State is set by yolo_countdown_tick; nothing extra needed.
    }

    fn yolo_countdown_finished(&mut self, _step_name: &str) {
        if let Ok(mut guard) = self.yolo_state.lock() {
            *guard = None;
        }
    }

    fn report_step_status(&mut self, step: &WorkflowStep, status: WorkflowStepStatus) {
        self.messages
            .info(format!("workflow step '{}': {:?}", step.name, status));
        if let Ok(mut guard) = self.workflow_view.lock() {
            if let Some(view) = guard.as_mut() {
                let status_str = workflow_status_str(&status);
                if let Some(s) = view.steps.iter_mut().find(|s| s.name == step.name) {
                    s.status = status_str.to_string();
                }
                view.current_step = if matches!(status, WorkflowStepStatus::Running) {
                    Some(step.name.clone())
                } else if view
                    .current_step
                    .as_deref()
                    .map(|cur| cur == step.name.as_str())
                    .unwrap_or(false)
                {
                    None
                } else {
                    view.current_step.clone()
                };
            }
        }
    }

    fn report_step_output(&mut self, _step: &WorkflowStep, _output: StepOutput) {}

    fn report_workflow_completed(&mut self, outcome: &WorkflowOutcome) {
        if let Ok(mut g) = self.yolo_state.lock() {
            *g = None;
        }
        match outcome {
            WorkflowOutcome::Completed => self.messages.success("Workflow completed successfully"),
            WorkflowOutcome::CompletedTeardownFailed => {
                self.messages
                    .warning("Workflow completed but teardown failed");
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

    fn report_workflow_progress(&mut self, steps: &[WorkflowStepProgressInfo]) {
        if let Ok(mut guard) = self.workflow_view.lock() {
            let view =
                guard.get_or_insert_with(crate::frontend::tui::tabs::WorkflowViewState::default);
            view.steps = steps
                .iter()
                .map(|s| crate::frontend::tui::tabs::WorkflowStepView {
                    name: s.name.clone(),
                    status: workflow_status_str(&s.status).to_string(),
                    agent: Some(s.agent.clone()),
                    model: s.model.clone(),
                    depends_on: s.depends_on.clone(),
                })
                .collect();
            view.current_step = steps
                .iter()
                .find(|s| matches!(s.status, WorkflowStepStatus::Running))
                .map(|s| s.name.clone());
        }
    }

    fn report_step_interactive_launch(
        &mut self,
        _step: &WorkflowStep,
        agent: &str,
        _model: Option<&str>,
    ) {
        self.pty_reset_flag
            .store(true, std::sync::atomic::Ordering::Relaxed);

        if let Ok(mut guard) = self.yolo_state.lock() {
            *guard = None;
        }

        self.recreate_container_io();

        if let Ok(mut name) = self.container_name_shared.lock() {
            *name = None;
        }

        self.messages
            .info(format!("Launching agent '{}' in new container...", agent));
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
        let mut error_lines = Vec::new();
        if let Some(sig) = exit.signal {
            error_lines.push(format!("Container exited from signal {}", sig));
        }
        error_lines.push(format!("Exit code: {}", exit.exit_code));
        let duration = exit
            .ended_at
            .signed_duration_since(exit.started_at)
            .num_seconds()
            .max(0);
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

    fn on_setup_step_started(&mut self, description: &str) {
        self.messages.info(format!("setup: {description}"));
    }

    fn on_setup_step_output(&mut self, line: &str) {
        self.messages.info(format!("  {line}"));
    }

    fn on_setup_step_completed(&mut self, description: &str) {
        self.messages.success(format!("setup: {description}"));
    }

    fn on_setup_step_failed(&mut self, description: &str, exit_code: i32, stderr: &str) {
        let msg = if stderr.is_empty() {
            format!("setup failed: {description} (exit {exit_code})")
        } else {
            format!("setup failed: {description} (exit {exit_code}): {stderr}")
        };
        self.messages.error_msg(msg);
    }

    fn on_teardown_step_started(&mut self, description: &str) {
        self.messages.info(format!("teardown: {description}"));
    }

    fn on_teardown_step_output(&mut self, line: &str) {
        self.messages.info(format!("  {line}"));
    }

    fn on_teardown_step_completed(&mut self, description: &str) {
        self.messages.success(format!("teardown: {description}"));
    }

    fn on_teardown_step_failed(&mut self, description: &str, exit_code: i32, stderr: &str) {
        let msg = if stderr.is_empty() {
            format!("teardown failed: {description} (exit {exit_code})")
        } else {
            format!("teardown failed: {description} (exit {exit_code}): {stderr}")
        };
        self.messages.error_msg(msg);
    }

    fn set_engine_sender(&mut self, tx: tokio::sync::mpsc::UnboundedSender<EngineRequest>) {
        if let Ok(mut guard) = self.engine_tx_shared.lock() {
            *guard = Some(tx);
        }
    }

    fn set_stuck_sender(
        &mut self,
        sender: std::sync::Arc<
            tokio::sync::broadcast::Sender<crate::engine::container::instance::StuckEvent>,
        >,
    ) {
        if let Ok(mut guard) = self.stuck_sender_shared.lock() {
            *guard = Some(sender);
        }
    }
}

/// Map a WCB dialog response to a `NextAction`.
fn wcb_response_to_action(response: DialogResponse, available: &AvailableActions) -> NextAction {
    match response {
        DialogResponse::Char('>') => NextAction::LaunchNext,
        DialogResponse::Char('v') => {
            let prompt = available.continue_prompt.clone().unwrap_or_default();
            NextAction::ContinueInCurrentContainer { prompt }
        }
        DialogResponse::Char('^') => NextAction::RestartCurrentStep,
        DialogResponse::Char('<') => NextAction::CancelToPreviousStep,
        DialogResponse::Char('f') if available.can_finish_workflow => NextAction::FinishWorkflow,
        DialogResponse::Char('a') => NextAction::Abort,
        DialogResponse::Char('p') if available.can_dismiss => NextAction::Pause,
        DialogResponse::Dismissed if available.can_dismiss => NextAction::Dismiss,
        DialogResponse::Dismissed => NextAction::Pause,
        _ => NextAction::Pause,
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
    use std::time::Duration;

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
        let (_resize_tx, resize_rx) = tokio::sync::mpsc::unbounded_channel::<(u16, u16)>();
        let (stderr_tx, _stderr_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let container_io = crate::engine::container::frontend::ContainerIo {
            stdout: stdout_tx,
            stderr: stderr_tx,
            stdin_tx,
            stdin_rx,
            resize: Some(resize_rx),
            initial_size: Some((80, 24)),
        };
        let status_log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let parsed = crate::command::dispatch::parsed_input::ParsedCommandBoxInput {
            path: vec!["workflow".into()],
            flags: Default::default(),
            arguments: Default::default(),
        };
        let workflow_view = std::sync::Arc::new(std::sync::Mutex::new(None));
        let yolo_state = std::sync::Arc::new(std::sync::Mutex::new(None));
        let yolo_cancel_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let pty_reset_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stdin_tx_shared = std::sync::Arc::new(std::sync::Mutex::new(None));
        let resize_tx_shared = std::sync::Arc::new(std::sync::Mutex::new(None));
        let engine_tx_shared = std::sync::Arc::new(std::sync::Mutex::new(None));
        let stuck_sender_shared = std::sync::Arc::new(std::sync::Mutex::new(None));
        let frontend = TuiCommandFrontend::new(
            parsed,
            status_log,
            req_tx,
            resp_rx,
            container_io,
            workflow_view,
            yolo_state,
            yolo_cancel_flag,
            pty_reset_flag,
            std::sync::Arc::new(std::sync::Mutex::new(None)),
            stdin_tx_shared,
            resize_tx_shared,
            engine_tx_shared,
            stuck_sender_shared,
            std::sync::Arc::new(std::sync::Mutex::new(None)),
            std::sync::Arc::new(std::sync::Mutex::new(None)),
            std::sync::Arc::new(std::sync::Mutex::new(
                crate::command::commands::status::StatusCommandTuiContext::default(),
            )),
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
            overlays: None,
            abort_on_failure: false,
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
        let result = frontend
            .user_choose_after_step_failure(&step, &exit)
            .unwrap();
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
        let result = frontend
            .user_choose_after_step_failure(&step, &exit)
            .unwrap();
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
        let result = frontend
            .user_choose_after_step_failure(&step, &exit)
            .unwrap();
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
        let result = frontend
            .user_choose_after_step_failure(&step, &exit)
            .unwrap();
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

    // ─── Simple advance / parallel fan-out dialog routing ────────────────────

    fn make_workflow_state_one_pending() -> crate::data::workflow_state::WorkflowState {
        use crate::data::workflow_definition::WorkflowStep;
        crate::data::workflow_state::WorkflowState::new(
            "wf".into(),
            &[
                WorkflowStep {
                    name: "build".into(),
                    depends_on: vec![],
                    prompt_template: "".into(),
                    agent: None,
                    model: None,
                    overlays: None,
                    abort_on_failure: false,
                },
                WorkflowStep {
                    name: "test".into(),
                    depends_on: vec!["build".into()],
                    prompt_template: "".into(),
                    agent: None,
                    model: None,
                    overlays: None,
                    abort_on_failure: false,
                },
            ],
            "hash".into(),
            None,
        )
    }

    fn make_workflow_state_two_pending() -> crate::data::workflow_state::WorkflowState {
        use crate::data::workflow_definition::WorkflowStep;
        crate::data::workflow_state::WorkflowState::new(
            "wf".into(),
            &[
                WorkflowStep {
                    name: "build".into(),
                    depends_on: vec![],
                    prompt_template: "".into(),
                    agent: None,
                    model: None,
                    overlays: None,
                    abort_on_failure: false,
                },
                WorkflowStep {
                    name: "test-a".into(),
                    depends_on: vec!["build".into()],
                    prompt_template: "".into(),
                    agent: None,
                    model: None,
                    overlays: None,
                    abort_on_failure: false,
                },
                WorkflowStep {
                    name: "test-b".into(),
                    depends_on: vec!["build".into()],
                    prompt_template: "".into(),
                    agent: None,
                    model: None,
                    overlays: None,
                    abort_on_failure: false,
                },
            ],
            "hash".into(),
            None,
        )
    }

    fn make_available_launch_next() -> crate::engine::workflow::actions::AvailableActions {
        crate::engine::workflow::actions::AvailableActions {
            can_launch_next: true,
            ..Default::default()
        }
    }

    #[test]
    fn simple_advance_shows_lightweight_dialog() {
        use crate::data::workflow_state::StepState;
        use crate::engine::workflow::frontend::WorkflowFrontend;

        let (mut frontend, req_rx, resp_tx) = make_frontend();

        let mut state = make_workflow_state_one_pending();
        state.set_status("build", StepState::Succeeded);
        let available = make_available_launch_next();

        let handle = std::thread::spawn(move || {
            let req = req_rx.recv().unwrap();
            assert!(
                matches!(
                    req,
                    crate::frontend::tui::dialogs::DialogRequest::WorkflowStepConfirm(_)
                ),
                "single-pending-step should show WorkflowStepConfirm, got {:?}",
                req
            );
            resp_tx.send(DialogResponse::Char('>')).unwrap();
        });

        let result = frontend
            .show_workflow_control_board(&state, &available)
            .unwrap();
        handle.join().unwrap();
        assert_eq!(
            result,
            crate::engine::workflow::actions::NextAction::LaunchNext,
        );
    }

    #[test]
    fn parallel_fan_out_falls_through_to_wcb() {
        use crate::data::workflow_state::StepState;
        use crate::engine::workflow::frontend::WorkflowFrontend;

        let (mut frontend, req_rx, resp_tx) = make_frontend();

        let mut state = make_workflow_state_two_pending();
        state.set_status("build", StepState::Succeeded);
        let available = make_available_launch_next();

        let handle = std::thread::spawn(move || {
            let req = req_rx.recv().unwrap();
            assert!(
                matches!(
                    req,
                    crate::frontend::tui::dialogs::DialogRequest::WorkflowControlBoard(_)
                ),
                "two pending steps should show WorkflowControlBoard, got {:?}",
                req
            );
            resp_tx.send(DialogResponse::Char('>')).unwrap();
        });

        let result = frontend
            .show_workflow_control_board(&state, &available)
            .unwrap();
        handle.join().unwrap();
        assert_eq!(
            result,
            crate::engine::workflow::actions::NextAction::LaunchNext,
        );
    }

    // ─── Yolo countdown tick tests ──────────────────────────────────────────

    #[test]
    fn yolo_countdown_tick_returns_continue_by_default() {
        use crate::engine::workflow::actions::YoloTickOutcome;
        use crate::engine::workflow::frontend::WorkflowFrontend;

        let (mut frontend, _req_rx, _resp_tx) = make_frontend();
        let result = frontend
            .yolo_countdown_tick("build", Duration::from_secs(30), Duration::from_secs(60))
            .unwrap();
        assert_eq!(result, YoloTickOutcome::Continue);
    }

    #[test]
    fn yolo_countdown_tick_returns_cancel_when_flag_set() {
        use crate::engine::workflow::actions::YoloTickOutcome;
        use crate::engine::workflow::frontend::WorkflowFrontend;

        let (mut frontend, _req_rx, _resp_tx) = make_frontend();
        frontend
            .yolo_cancel_flag
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let result = frontend
            .yolo_countdown_tick("build", Duration::from_secs(30), Duration::from_secs(60))
            .unwrap();
        assert_eq!(result, YoloTickOutcome::Cancel);
    }

    #[test]
    fn yolo_cancel_flag_resets_after_consumption() {
        use crate::engine::workflow::actions::YoloTickOutcome;
        use crate::engine::workflow::frontend::WorkflowFrontend;

        let (mut frontend, _req_rx, _resp_tx) = make_frontend();
        frontend
            .yolo_cancel_flag
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let _ = frontend
            .yolo_countdown_tick("build", Duration::from_secs(30), Duration::from_secs(60))
            .unwrap();
        let result = frontend
            .yolo_countdown_tick("build", Duration::from_secs(29), Duration::from_secs(60))
            .unwrap();
        assert_eq!(result, YoloTickOutcome::Continue);
    }

    #[test]
    fn yolo_countdown_tick_updates_shared_state() {
        use crate::engine::workflow::frontend::WorkflowFrontend;

        let (mut frontend, _req_rx, _resp_tx) = make_frontend();
        let _ = frontend
            .yolo_countdown_tick("build", Duration::from_secs(42), Duration::from_secs(60))
            .unwrap();
        let guard = frontend.yolo_state.lock().unwrap();
        let state = guard.as_ref().expect("yolo_state must be Some");
        assert_eq!(state.step_name, "build");
        assert_eq!(state.remaining_secs, 42);
    }

    #[test]
    fn yolo_countdown_cancel_clears_shared_state() {
        use crate::engine::workflow::frontend::WorkflowFrontend;

        let (mut frontend, _req_rx, _resp_tx) = make_frontend();
        // First set some state
        let _ = frontend
            .yolo_countdown_tick("build", Duration::from_secs(30), Duration::from_secs(60))
            .unwrap();
        assert!(frontend.yolo_state.lock().unwrap().is_some());

        // Cancel clears state
        frontend
            .yolo_cancel_flag
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let _ = frontend
            .yolo_countdown_tick("build", Duration::from_secs(29), Duration::from_secs(60))
            .unwrap();
        assert!(frontend.yolo_state.lock().unwrap().is_none());
    }

    #[test]
    fn yolo_countdown_finished_clears_shared_state() {
        use crate::engine::workflow::frontend::WorkflowFrontend;

        let (mut frontend, _req_rx, _resp_tx) = make_frontend();
        let _ = frontend
            .yolo_countdown_tick("build", Duration::from_secs(10), Duration::from_secs(60))
            .unwrap();
        assert!(frontend.yolo_state.lock().unwrap().is_some());
        frontend.yolo_countdown_finished("build");
        assert!(frontend.yolo_state.lock().unwrap().is_none());
    }

    #[test]
    fn set_engine_sender_stores_tx() {
        use crate::engine::workflow::frontend::WorkflowFrontend;
        use crate::engine::workflow::EngineRequest;

        let (mut frontend, _req_rx, _resp_tx) = make_frontend();
        assert!(frontend.engine_tx_shared.lock().unwrap().is_none());

        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<EngineRequest>();
        frontend.set_engine_sender(tx);
        assert!(frontend.engine_tx_shared.lock().unwrap().is_some());
    }
}
