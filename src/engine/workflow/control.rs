//! The Workflow Control Board: what it offers, and what each answer does.
//!
//! Split out of `workflow/mod.rs` by WI 0114 F-51. A child module of
//! `workflow`, so it reaches `WorkflowEngine`'s private fields exactly as the
//! code did before the move; methods it defines are `pub(super)` so the
//! other halves of the engine can still call them.

use super::*;

impl WorkflowEngine {
    /// Compose the failure-scoped [`AvailableActions`] for the Workflow
    /// Control Board (WI-0115 §1).
    ///
    /// The failed step's container is already dead, so "continue in the current
    /// container" is never offered and Esc means Pause rather than Dismiss.
    /// `can_finish_workflow` is forced off: there is no "Enter to finish
    /// workflow" on a failure board — Ctrl-C aborts instead.
    pub(super) fn compute_failure_actions(
        &self,
        step_name: &str,
        exit_code: i32,
    ) -> Result<AvailableActions, EngineError> {
        // `last_exit_info` tracks the most recent container to exit, which in a
        // parallel group need not be the one that failed — a later-exiting peer
        // overwrites it. Only trust it when its code matches this failure.
        let exit = self
            .last_exit_info
            .clone()
            .filter(|e| e.exit_code == exit_code);
        let signal = exit.as_ref().and_then(|e| e.signal);

        let mut detail_lines = Vec::new();
        if let Some(sig) = signal {
            detail_lines.push(format!("Container terminated by signal {sig}"));
        }
        detail_lines.push(format!("Exit code: {exit_code}"));
        if let Some(e) = &exit {
            let secs = e
                .ended_at
                .signed_duration_since(e.started_at)
                .num_seconds()
                .max(0);
            detail_lines.push(format!("Ran for {secs}s"));
        }

        // The step LaunchNext will start: the first step that becomes ready
        // once the failed step is treated as skipped.
        let mut completed_if_skipped = self.state.completed_steps.clone();
        completed_if_skipped.insert(step_name.to_string());
        let next_step = self
            .dag
            .ready_steps(&completed_if_skipped)
            .into_iter()
            .next();
        let previous_step = self.previous_step_name();

        Ok(AvailableActions {
            can_launch_next: next_step.is_some(),
            can_restart_current_step: true,
            can_cancel_to_previous_step: previous_step.is_some(),
            can_pause: true,
            can_abort: true,
            can_finish_workflow: false,
            can_dismiss: false,
            cancel_to_previous_unavailable_reason: previous_step
                .is_none()
                .then(|| "this is the first step".to_string()),
            continue_unavailable_reason: Some("the failed step's container has exited".into()),
            finish_workflow_unavailable_reason: Some(
                "a step failed; choose a recovery action or Ctrl-C to cancel".into(),
            ),
            launch_next_label: next_step
                .as_ref()
                .map(|n| format!("Skip to '{n}' (new container)")),
            focused_step: Some(step_name.to_string()),
            // A failure board is never the simple-advance case.
            simple_advance: None,
            step_failure: Some(StepFailureContext {
                step_name: step_name.to_string(),
                exit_code,
                signal,
                detail_lines,
                previous_step,
                next_step,
            }),
            ..Default::default()
        })
    }

    /// Decide what happens after a non-`abort_on_failure` step failure.
    ///
    /// Interactive frontends (CLI on a TTY, TUI) get the Workflow Control Board
    /// with the failure attached and drive the recovery themselves. Unattended
    /// frontends (squad daemon, API server, `--non-interactive`) get one yolo
    /// countdown and one automatic retry; a second failure of the same step
    /// fails the workflow (WI-0115 §1, §3).
    pub(super) async fn handle_step_failure(
        &mut self,
        step_name: &str,
        exit_code: i32,
    ) -> Result<IterationOutcome, EngineError> {
        if self.frontend.supports_interactive_recovery() {
            self.handle_step_failure_interactive(step_name, exit_code)
        } else {
            self.handle_step_failure_unattended(step_name, exit_code)
                .await
        }
    }

    /// Interactive recovery loop. Repeats until the user picks an action that
    /// either resolves the failure or ends the workflow — a Dismiss (or any
    /// action that is not meaningful on a failure board) re-presents it, since
    /// leaving a failed step unanswered has nowhere to go.
    pub(super) fn handle_step_failure_interactive(
        &mut self,
        step_name: &str,
        exit_code: i32,
    ) -> Result<IterationOutcome, EngineError> {
        self.msg_error(format!(
            "Step '{step_name}' failed (exit {exit_code}); choose how to recover"
        ));
        loop {
            let available = self.compute_failure_actions(step_name, exit_code)?;
            // Each arm below narrates its own recovery, so `log_wcb_action`'s
            // generic between-steps copy would only double up here.
            let action = self
                .frontend
                .show_workflow_control_board(&self.state, &available)?;
            match action {
                NextAction::RestartCurrentStep => {
                    self.msg_info(format!("Restarting failed step '{step_name}'"));
                    self.state.set_status(step_name, StepState::Pending);
                    self.persist()?;
                    return Ok(IterationOutcome::Continue);
                }
                NextAction::CancelToPreviousStep => {
                    let Some(prev) = available
                        .step_failure
                        .as_ref()
                        .and_then(|f| f.previous_step.clone())
                    else {
                        continue;
                    };
                    self.msg_info(format!(
                        "Cancelling failed step '{step_name}', returning to '{prev}'"
                    ));
                    self.state.set_status(step_name, StepState::Pending);
                    self.state.set_status(&prev, StepState::Pending);
                    self.persist()?;
                    return Ok(IterationOutcome::Continue);
                }
                NextAction::LaunchNext => {
                    let Some(next) = available
                        .step_failure
                        .as_ref()
                        .and_then(|f| f.next_step.clone())
                    else {
                        continue;
                    };
                    self.msg_warning(format!(
                        "Skipping failed step '{step_name}'; starting '{next}' in a new container"
                    ));
                    self.state.set_status(step_name, StepState::Skipped);
                    self.persist()?;
                    return Ok(IterationOutcome::Continue);
                }
                NextAction::Abort => return Ok(IterationOutcome::Ended(self.handle_abort()?)),
                NextAction::Pause => {
                    self.msg_info("Workflow paused");
                    self.persist()?;
                    let paused = WorkflowOutcome::Paused;
                    self.frontend.report_workflow_completed(&paused);
                    return Ok(IterationOutcome::Ended(paused));
                }
                // Dismiss / Continue-in-container / Finish are all meaningless
                // on a dead container: re-present the board.
                _ => continue,
            }
        }
    }

    /// Unattended recovery: a 60s yolo countdown (reported through the same
    /// frontend hooks a stuck-step countdown uses) followed by exactly one
    /// automatic retry of the failed step. A second failure of the same step
    /// ends the workflow as `Failed`.
    pub(super) async fn handle_step_failure_unattended(
        &mut self,
        step_name: &str,
        exit_code: i32,
    ) -> Result<IterationOutcome, EngineError> {
        if self.auto_retried_steps.contains(step_name) {
            self.msg_error(format!(
                "Step '{step_name}' failed again after its automatic retry (exit {exit_code}); \
                 failing workflow",
            ));
            for s in &self.workflow.steps {
                if !self.state.completed_steps.contains(&s.name) {
                    self.state.set_status(&s.name, StepState::Cancelled);
                }
            }
            self.state.set_status(
                step_name,
                StepState::Failed {
                    exit_code,
                    error_message: Some(format!(
                        "failed twice (exit {exit_code}); automatic retry exhausted"
                    )),
                },
            );
            self.persist()?;
            let failed = WorkflowOutcome::Failed {
                last_step: step_name.to_string(),
                exit_code,
            };
            self.frontend.report_workflow_completed(&failed);
            return Ok(IterationOutcome::Ended(failed));
        }

        self.msg_warning(format!(
            "Step '{step_name}' failed (exit {exit_code}); retrying once in {}s",
            timing::YOLO_COUNTDOWN_DURATION.as_secs(),
        ));
        match self.run_failure_retry_countdown(step_name).await? {
            YoloTickOutcome::Cancel => {
                self.msg_warning(format!(
                    "Retry countdown for step '{step_name}' cancelled; failing workflow",
                ));
                for s in &self.workflow.steps {
                    if !self.state.completed_steps.contains(&s.name) {
                        self.state.set_status(&s.name, StepState::Cancelled);
                    }
                }
                self.state.set_status(
                    step_name,
                    StepState::Failed {
                        exit_code,
                        error_message: Some("retry countdown cancelled".into()),
                    },
                );
                self.persist()?;
                let failed = WorkflowOutcome::Failed {
                    last_step: step_name.to_string(),
                    exit_code,
                };
                self.frontend.report_workflow_completed(&failed);
                Ok(IterationOutcome::Ended(failed))
            }
            _ => {
                self.auto_retried_steps.insert(step_name.to_string());
                self.msg_info(format!(
                    "Retrying failed step '{step_name}' (attempt 2 of 2)"
                ));
                self.state.set_status(step_name, StepState::Pending);
                self.persist()?;
                Ok(IterationOutcome::Continue)
            }
        }
    }

    /// Tick the retry countdown for a failed step. Unlike the mid-step yolo
    /// countdown there is no container left to recover, so the only outcomes
    /// are "expired / advance now" (retry) and "cancelled" (fail).
    pub(super) async fn run_failure_retry_countdown(
        &mut self,
        step_name: &str,
    ) -> Result<YoloTickOutcome, EngineError> {
        let total = timing::YOLO_COUNTDOWN_DURATION;
        let start = Instant::now();
        self.frontend
            .yolo_countdown_started(step_name, CountdownKind::FailureRetry);
        let outcome = loop {
            let elapsed = start.elapsed();
            let remaining = total.saturating_sub(elapsed);
            match self
                .frontend
                .yolo_countdown_tick(step_name, remaining, total)?
            {
                YoloTickOutcome::AdvanceNow => break YoloTickOutcome::AdvanceNow,
                YoloTickOutcome::Cancel => break YoloTickOutcome::Cancel,
                YoloTickOutcome::Continue => {}
            }
            if remaining.is_zero() {
                break YoloTickOutcome::Continue;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        self.frontend.yolo_countdown_finished(step_name);
        Ok(outcome)
    }

    /// Present the post-failure recovery board for a non-abort step failure
    /// that surfaced after a parallel group drained.
    pub(super) async fn handle_group_step_failure(
        &mut self,
        step_name: &str,
        exit_code: i32,
    ) -> Result<IterationOutcome, EngineError> {
        // Scope the board to the failed step: its peers have already exited,
        // so the previous/next names must be computed relative to it.
        self.current_step_name = Some(step_name.to_string());
        self.handle_step_failure(step_name, exit_code).await
    }

    pub(super) fn handle_mid_step_control_board(
        &mut self,
        step_name: &str,
        cancel_handle: &Option<crate::engine::agent_runtime::execution::CancelHandle>,
        wait_rx: &mut tokio::sync::oneshot::Receiver<(
            AgentExecution,
            Result<AgentExitInfo, EngineError>,
        )>,
    ) -> Result<MidStepOutcome, EngineError> {
        let available = self.compute_available_actions()?;
        let action = self
            .frontend
            .show_workflow_control_board(&self.state, &available)?;

        self.log_wcb_action(&action);

        let already_finished = match wait_rx.try_recv() {
            Ok((exec_back, exit_result)) => {
                self.set_focused_execution(exec_back);
                Some(exit_result)
            }
            Err(_) => None,
        };

        match action {
            NextAction::Dismiss | NextAction::RetryFailedStep { .. } => {
                if let Some(exit_result) = already_finished {
                    return Ok(MidStepOutcome::StepCompleted(
                        self.finalize_step(step_name, exit_result?)?,
                    ));
                }
                Ok(MidStepOutcome::Continue)
            }
            NextAction::ContinueInCurrentContainer { prompt } => {
                // Direct field access keeps the borrow of `active_steps`
                // disjoint from `agent_factory` (the helper would borrow all of
                // `self`).
                if let Some(exec) = self.active_steps.first().and_then(|s| s.execution.as_ref()) {
                    let _ = self.agent_factory.inject_prompt(exec, &prompt);
                }
                if let Some(exit_result) = already_finished {
                    return Ok(MidStepOutcome::StepCompleted(
                        self.finalize_step(step_name, exit_result?)?,
                    ));
                }
                Ok(MidStepOutcome::Continue)
            }
            NextAction::Pause => {
                if already_finished.is_none() {
                    self.kill_current_container(cancel_handle);
                }
                self.state.set_status(step_name, StepState::Pending);
                self.persist()?;
                let outcome = WorkflowOutcome::Paused;
                self.frontend.report_workflow_completed(&outcome);
                Ok(MidStepOutcome::WorkflowEnded(outcome))
            }
            NextAction::Abort => {
                if already_finished.is_none() {
                    self.kill_current_container(cancel_handle);
                }
                for s in &self.workflow.steps {
                    if !self.state.completed_steps.contains(&s.name) {
                        self.state.set_status(&s.name, StepState::Cancelled);
                    }
                }
                self.persist()?;
                let outcome = WorkflowOutcome::Aborted;
                self.frontend.report_workflow_completed(&outcome);
                Ok(MidStepOutcome::WorkflowEnded(outcome))
            }
            NextAction::FinishWorkflow => {
                if !self.is_last_step() {
                    return Err(EngineError::InvalidAdvanceAction(
                        "FinishWorkflow only valid on the last step".into(),
                    ));
                }
                if already_finished.is_none() {
                    self.kill_current_container(cancel_handle);
                }
                for s in &self.workflow.steps {
                    if !self.state.completed_steps.contains(&s.name) {
                        self.state.set_status(&s.name, StepState::Skipped);
                    }
                }
                self.persist()?;
                let outcome = WorkflowOutcome::Completed;
                self.frontend.report_workflow_completed(&outcome);
                Ok(MidStepOutcome::WorkflowEnded(outcome))
            }
            NextAction::LaunchNext => {
                if already_finished.is_none() {
                    self.kill_current_container(cancel_handle);
                }
                self.state.set_status(step_name, StepState::Succeeded);
                self.persist()?;
                Ok(MidStepOutcome::LoopContinue)
            }
            NextAction::RestartCurrentStep => {
                if already_finished.is_none() {
                    self.kill_current_container(cancel_handle);
                }
                self.state.set_status(step_name, StepState::Pending);
                self.persist()?;
                Ok(MidStepOutcome::LoopContinue)
            }
            NextAction::CancelToPreviousStep => {
                if already_finished.is_none() {
                    self.kill_current_container(cancel_handle);
                }
                if let Some(prev) = self.previous_step_name() {
                    self.state.set_status(step_name, StepState::Cancelled);
                    self.state.set_status(&prev, StepState::Pending);
                    self.persist()?;
                }
                Ok(MidStepOutcome::LoopContinue)
            }
        }
    }

    /// Execute a top-level action from the WCB (used after yolo auto-advance
    /// on the last step, and in run_to_completion inter-step transitions).
    pub(super) fn execute_top_level_action(
        &mut self,
        action: NextAction,
    ) -> Result<InterruptibleStepResult, EngineError> {
        match action {
            NextAction::Dismiss | NextAction::LaunchNext | NextAction::RetryFailedStep { .. } => {
                Ok(InterruptibleStepResult::LoopContinue)
            }
            NextAction::FinishWorkflow => {
                let wo = self.handle_finish_workflow()?;
                Ok(InterruptibleStepResult::WorkflowEnded(wo))
            }
            NextAction::Pause => {
                self.persist()?;
                let outcome = WorkflowOutcome::Paused;
                self.frontend.report_workflow_completed(&outcome);
                Ok(InterruptibleStepResult::WorkflowEnded(outcome))
            }
            NextAction::Abort => {
                let wo = self.handle_abort()?;
                Ok(InterruptibleStepResult::WorkflowEnded(wo))
            }
            NextAction::RestartCurrentStep => {
                if let Some(name) = self.current_step_name.clone() {
                    self.state.set_status(&name, StepState::Pending);
                    self.persist()?;
                }
                Ok(InterruptibleStepResult::LoopContinue)
            }
            NextAction::CancelToPreviousStep => {
                self.handle_cancel_to_previous()?;
                Ok(InterruptibleStepResult::LoopContinue)
            }
            NextAction::ContinueInCurrentContainer { prompt } => {
                self.handle_continue_in_current_container(&prompt)?;
                Ok(InterruptibleStepResult::LoopContinue)
            }
        }
    }

    pub(super) fn handle_finish_workflow(&mut self) -> Result<WorkflowOutcome, EngineError> {
        if !self.is_last_step() {
            return Err(EngineError::InvalidAdvanceAction(
                "FinishWorkflow only valid on the last step".into(),
            ));
        }
        let skipped: Vec<String> = self
            .workflow
            .steps
            .iter()
            .filter(|s| !self.state.completed_steps.contains(&s.name))
            .map(|s| s.name.clone())
            .collect();
        for name in &skipped {
            self.state.set_status(name, StepState::Skipped);
        }
        if !skipped.is_empty() {
            self.msg_info(format!("Skipping remaining steps: {}", skipped.join(", "),));
        }
        self.persist()?;
        self.msg_success(format!("Workflow '{}' completed", self.state.workflow_name,));
        let outcome = WorkflowOutcome::Completed;
        self.frontend.report_workflow_completed(&outcome);
        Ok(outcome)
    }

    pub(super) fn handle_abort(&mut self) -> Result<WorkflowOutcome, EngineError> {
        self.msg_warning("Workflow aborted");
        for s in &self.workflow.steps {
            if !self.state.completed_steps.contains(&s.name) {
                self.state.set_status(&s.name, StepState::Cancelled);
            }
        }
        self.persist()?;
        let outcome = WorkflowOutcome::Aborted;
        self.frontend.report_workflow_completed(&outcome);
        Ok(outcome)
    }

    pub(super) fn log_wcb_action(&mut self, action: &NextAction) {
        let step = self.current_step_name.as_deref().unwrap_or("unknown");
        match action {
            NextAction::Dismiss => {}
            NextAction::LaunchNext => {
                self.msg_info("Advancing to next step");
            }
            NextAction::ContinueInCurrentContainer { .. } => {
                self.msg_info(format!(
                    "Continuing in current container for next step (from '{}')",
                    step,
                ));
            }
            NextAction::RestartCurrentStep => {
                self.msg_info(format!("Restarting step '{}'", step));
            }
            NextAction::CancelToPreviousStep => {
                self.msg_info(format!("Cancelling step '{}', returning to previous", step,));
            }
            NextAction::FinishWorkflow => {
                self.msg_info("Finishing workflow");
            }
            NextAction::Pause => {
                self.msg_info("Workflow paused");
            }
            NextAction::Abort => {
                self.msg_warning("Workflow aborted");
            }
            NextAction::RetryFailedStep { step_name } => {
                self.msg_info(format!("Retrying failed step '{step_name}'"));
            }
        }
    }

    pub(super) fn handle_cancel_to_previous(&mut self) -> Result<(), EngineError> {
        let prev = self.previous_step_name();
        match prev {
            Some(prev) => {
                if let Some(curr) = self.current_step_name.clone() {
                    self.state.set_status(&curr, StepState::Cancelled);
                }
                self.state.set_status(&prev, StepState::Pending);
                self.persist()?;
                Ok(())
            }
            None => Err(EngineError::InvalidAdvanceAction(
                "no previous step to cancel to".into(),
            )),
        }
    }

    pub(super) fn handle_continue_in_current_container(
        &mut self,
        prompt: &str,
    ) -> Result<(), EngineError> {
        let next_step = match self.next_ready_step()? {
            Some(s) => s,
            None => {
                return Err(EngineError::InvalidAdvanceAction(
                    "ContinueInCurrentContainer: no next step is ready".into(),
                ))
            }
        };
        let next_agent = self.resolve_agent(&next_step)?;
        let next_model = self.resolve_model(&next_step);
        let agent_ok = self
            .current_step_agent
            .as_ref()
            .map(|a| *a == next_agent)
            .unwrap_or(false);
        let model_ok = self.current_step_model == next_model;
        if !agent_ok || !model_ok {
            return Err(EngineError::InvalidAdvanceAction(
                "ContinueInCurrentContainer requires the same agent and model \
                 for the current and next steps"
                    .into(),
            ));
        }
        match self.active_steps.first().and_then(|s| s.execution.as_ref()) {
            Some(exec) => match self.agent_factory.inject_prompt(exec, prompt)? {
                Some(()) => {
                    self.state.set_status(&next_step.name, StepState::Succeeded);
                    self.current_step_name = Some(next_step.name.clone());
                    self.persist()?;
                    Ok(())
                }
                None => Err(EngineError::InvalidAdvanceAction(
                    "container backend does not support prompt injection; \
                         use LaunchNext to start a fresh container"
                        .into(),
                )),
            },
            None => Err(EngineError::InvalidAdvanceAction(
                "no container execution is available to inject into".into(),
            )),
        }
    }

    pub fn compute_available_actions(&self) -> Result<AvailableActions, EngineError> {
        let has_execution = self.focused_execution().is_some();
        let mut a = AvailableActions {
            can_launch_next: !self.state.is_complete(),
            can_restart_current_step: self.current_step_name.is_some(),
            can_pause: true,
            can_abort: true,
            can_finish_workflow: self.is_last_step(),
            can_dismiss: has_execution || self.current_step_name.is_some(),
            ..Default::default()
        };
        if let Some(next) = self.next_ready_step()? {
            let next_agent = self.resolve_agent(&next)?;
            let next_model = self.resolve_model(&next);
            let ok = match (&self.current_step_agent, &self.current_step_model) {
                (Some(curr_a), curr_m) => *curr_a == next_agent && *curr_m == next_model,
                _ => false,
            };
            if ok && has_execution {
                a.can_continue_in_current_container = true;
                a.continue_prompt = Some(next.prompt_template.clone());
            } else {
                a.continue_unavailable_reason = Some(if self.current_step_agent.is_none() {
                    "no current container".into()
                } else {
                    "next step targets a different agent or model".into()
                });
            }
        }
        if self.previous_step_name().is_some() {
            a.can_cancel_to_previous_step = true;
        } else {
            a.cancel_to_previous_unavailable_reason = Some("this is the first step".into());
        }
        if !a.can_finish_workflow {
            a.finish_workflow_unavailable_reason =
                Some("FinishWorkflow is only valid on the last step".into());
        }

        // Workflow Control Board scoping for parallel groups (WI-0096 §10).
        // The board's actions apply to the focused container; peers keep
        // running. `active_steps` counts live containers; the focused step is
        // the first entry, so peers = len - 1.
        let peers_running = self.active_steps.len().saturating_sub(1);
        a.parallel_peer_count = self.active_steps.len();
        a.parallel_peers_running = peers_running;
        if peers_running > 0 {
            // Restart still targets only the focused container; surface the
            // scoping note so frontends can explain it.
            a.can_restart_current_step = false;
            a.restart_unavailable_reason =
                Some("Restart applies only to the focused container. Switch with Ctrl-S.".into());
            a.can_cancel_to_previous_step = false;
            a.cancel_to_previous_unavailable_reason =
                Some("Cannot go back while other agents in this group are still running.".into());
            a.can_finish_workflow = false;
            a.finish_workflow_unavailable_reason =
                Some("Cannot finish while other agents in this group are still running.".into());
        }

        a.focused_step = self.running_step_name();
        a.simple_advance = self.simple_advance(&a);
        Ok(a)
    }

    /// The step currently in `Running` state, if exactly one is.
    ///
    /// The name a board is about between steps. Frontends used to scan
    /// `step_states` for this themselves (F-18).
    pub(super) fn running_step_name(&self) -> Option<String> {
        self.state
            .step_states
            .iter()
            .find(|(_, s)| matches!(s, StepState::Running { .. }))
            .map(|(name, _)| name.clone())
    }

    /// Whether this board is the plain "advance to the next step?" case: the
    /// engine can launch something, nothing is running to dismiss back to,
    /// no step has failed, and exactly one step is still pending.
    ///
    /// Reproduces, unchanged, the scan the TUI ran over `step_states` before
    /// F-18 moved the judgement here.
    pub(super) fn simple_advance(&self, actions: &AvailableActions) -> Option<SimpleAdvance> {
        if !actions.can_launch_next || actions.can_dismiss {
            return None;
        }
        if self
            .state
            .step_states
            .values()
            .any(|s| matches!(s, StepState::Failed { .. }))
        {
            return None;
        }
        let mut pending = self
            .state
            .step_states
            .iter()
            .filter(|(_, s)| matches!(s, StepState::Pending));
        let (next_step, _) = pending.next()?;
        if pending.next().is_some() {
            return None;
        }
        Some(SimpleAdvance {
            completed_step: actions.focused_step_label().to_string(),
            next_step: next_step.clone(),
        })
    }
}
