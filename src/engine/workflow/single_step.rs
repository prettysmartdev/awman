//! One step at a time: launching it, waiting on it, and reacting to it
//! going quiet or exiting non-zero.
//!
//! Split out of `workflow/mod.rs` by WI 0114 F-51. A child module of
//! `workflow`, so it reaches `WorkflowEngine`'s private fields exactly as the
//! code did before the move; methods it defines are `pub(super)` so the
//! other halves of the engine can still call them.

use super::*;

impl WorkflowEngine {
    /// One iteration of the sequential (single-step) path: launch the first
    /// ready step, drive its interactive lifecycle (mid-step WCB, yolo, stuck),
    /// handle failure, then present the inter-step Workflow Control Board.
    ///
    /// This is the pre-WI-0096 `run_to_completion` loop body, preserved
    /// verbatim so single-step and `max_concurrent == 1` workflows behave
    /// exactly as before.
    pub(super) async fn run_single_step_iteration(
        &mut self,
    ) -> Result<IterationOutcome, EngineError> {
        let interruptible_result = self.step_once_interruptible().await?;
        let outcome = match interruptible_result {
            InterruptibleStepResult::StepCompleted(o) => o,
            InterruptibleStepResult::WorkflowEnded(wo) => return Ok(IterationOutcome::Ended(wo)),
            InterruptibleStepResult::LoopContinue => return Ok(IterationOutcome::Continue),
        };

        if let WorkflowStepStatus::Failed { exit_code } = outcome.status {
            let progress = self.workflow_progress_info();
            self.frontend.report_workflow_progress(&progress);

            if self.recover_auth_failure(&outcome.step_name)? {
                self.state
                    .set_status(&outcome.step_name, StepState::Pending);
                self.persist()?;
                return Ok(IterationOutcome::Continue);
            }

            let step = self.find_step(&outcome.step_name)?;

            if step.abort_on_failure {
                self.msg_warning(format!(
                    "Step '{}' failed (abort_on_failure); aborting workflow",
                    outcome.step_name,
                ));
                self.abort_on_failure_triggered = true;
                for s in &self.workflow.steps {
                    if !self.state.completed_steps.contains(&s.name) {
                        self.state.set_status(&s.name, StepState::Cancelled);
                    }
                }
                self.persist()?;
                let aborted = WorkflowOutcome::Aborted;
                self.frontend.report_workflow_completed(&aborted);
                return Ok(IterationOutcome::Ended(aborted));
            }

            return self
                .handle_step_failure(&outcome.step_name, exit_code)
                .await;
        }

        // Step succeeded. Decide what to do next.
        let workflow_just_completed = self.state.is_complete();

        if !workflow_just_completed {
            let progress = self.workflow_progress_info();
            self.frontend.report_workflow_progress(&progress);

            if self.yolo {
                return Ok(IterationOutcome::Continue);
            }
        } else if self.yolo {
            // Last step in yolo mode: always require explicit user
            // confirmation before ending the workflow so the user can
            // review the final step's output.
            let progress = self.workflow_progress_info();
            self.frontend.report_workflow_progress(&progress);
        }

        if !workflow_just_completed || self.yolo {
            let available = self.compute_available_actions()?;
            let action = self
                .frontend
                .show_workflow_control_board(&self.state, &available)?;
            self.log_wcb_action(&action);
            match action {
                NextAction::Dismiss | NextAction::LaunchNext => {
                    return Ok(IterationOutcome::Continue)
                }
                NextAction::ContinueInCurrentContainer { prompt } => {
                    self.handle_continue_in_current_container(&prompt)?;
                    return Ok(IterationOutcome::Continue);
                }
                NextAction::RestartCurrentStep => {
                    if let Some(name) = self.current_step_name.clone() {
                        self.state.set_status(&name, StepState::Pending);
                        self.persist()?;
                    }
                    return Ok(IterationOutcome::Continue);
                }
                NextAction::CancelToPreviousStep => {
                    self.handle_cancel_to_previous()?;
                    return Ok(IterationOutcome::Continue);
                }
                NextAction::FinishWorkflow => {
                    return Ok(IterationOutcome::Ended(self.handle_finish_workflow()?));
                }
                NextAction::Pause => {
                    self.persist()?;
                    let outcome = WorkflowOutcome::Paused;
                    self.frontend.report_workflow_completed(&outcome);
                    return Ok(IterationOutcome::Ended(outcome));
                }
                NextAction::Abort => {
                    return Ok(IterationOutcome::Ended(self.handle_abort()?));
                }
            }
        }

        Ok(IterationOutcome::Continue)
    }

    // ── Parallel group execution (WI-0096 §2) ───────────────────────────────

    /// Advance exactly one step, reporting status through the frontend.
    pub async fn step_once(&mut self) -> Result<StepOutcome, EngineError> {
        let step_name = self.launch_step().await?;
        let exit = {
            let exec = self
                .active_steps
                .first_mut()
                .and_then(|s| s.execution.as_mut())
                .expect("launch_step stored execution");
            exec.wait().await?
        };
        self.finalize_step(&step_name, exit)
    }

    pub(super) async fn launch_step(&mut self) -> Result<String, EngineError> {
        let ready = self.state.next_ready(&self.dag);
        let step_name = ready
            .first()
            .cloned()
            .ok_or_else(|| EngineError::InvalidAdvanceAction("no ready steps remaining".into()))?;
        let step = self.find_step(&step_name)?;

        let resolved_agent = self.resolve_agent(&step)?;
        let resolved_model = self.resolve_model(&step);
        tracing::info!(
            step = %step.name,
            agent = %resolved_agent.as_str(),
            model = ?resolved_model,
            "workflow_engine resolved step parameters"
        );

        let workflow_step_info = self.build_workflow_step_info(&step.name);
        let runtime = WorkflowRuntimeContext {
            step_agent: resolved_agent.clone(),
            step_model: resolved_model.clone(),
            git_root: self.session.git_root().to_path_buf(),
            session_id: self.session.id(),
            workflow_invocation_id: self.state.invocation_id,
            workflow_step_info,
        };

        self.frontend.report_step_interactive_launch(
            &step,
            resolved_agent.as_str(),
            resolved_model.as_deref(),
        );

        self.state
            .set_status(&step.name, StepState::Running { container_id: None });
        self.frontend
            .report_step_status(&step, WorkflowStepStatus::Running);
        self.persist()?;

        let execution = self
            .agent_factory
            .execution_for_step(&step, &self.session, &runtime)?;

        self.state.set_status(
            &step.name,
            StepState::Running {
                container_id: Some(execution.handle().id.clone()),
            },
        );
        self.persist()?;

        let container_name = execution.handle().name.clone();
        let output_tail = execution.output_tail();
        self.active_steps = vec![ActiveParallelStep {
            step_name: step.name.clone(),
            execution: Some(execution),
            cancel_handle: None,
            container_name,
            output_tail,
            awman_killed: false,
            stuck: false,
            yolo_deadline: None,
            agent: resolved_agent.clone(),
            model: resolved_model.clone(),
        }];
        self.current_step_name = Some(step.name.clone());
        self.current_step_agent = Some(resolved_agent);
        self.current_step_model = resolved_model;
        Ok(step.name)
    }

    pub(super) fn finalize_step(
        &mut self,
        step_name: &str,
        exit: AgentExitInfo,
    ) -> Result<StepOutcome, EngineError> {
        self.last_exit_info = Some(exit.clone());
        // The step's container has actually terminated (wait() resolved) —
        // tell the frontend so it can tear down any live container UI.
        self.frontend.report_container_exited(exit.exit_code);

        // On a genuine (non-awman-kill) container failure, persist the buffered
        // output tail so the user can debug what went wrong.
        self.maybe_dump_step_failure(step_name, exit.exit_code);

        let (status, step_state) = if exit.exit_code == 0 {
            (WorkflowStepStatus::Succeeded, StepState::Succeeded)
        } else {
            (
                WorkflowStepStatus::Failed {
                    exit_code: exit.exit_code,
                },
                StepState::Failed {
                    exit_code: exit.exit_code,
                    error_message: None,
                },
            )
        };
        let step = self.find_step(step_name)?;
        self.state.set_status(step_name, step_state);
        self.frontend.report_step_status(&step, status.clone());
        self.persist()?;

        let remaining = self
            .workflow
            .steps
            .iter()
            .filter(|s| !self.state.completed_steps.contains(&s.name))
            .count();
        Ok(StepOutcome {
            step_name: step_name.to_string(),
            status,
            remaining,
        })
    }

    /// Like `step_once`, but processes `EngineRequest` messages (Ctrl-W)
    /// and container stuck events while the step container runs.
    pub(super) async fn step_once_interruptible(
        &mut self,
    ) -> Result<InterruptibleStepResult, EngineError> {
        let step_name = self.launch_step().await?;

        let cancel_handle = self.focused_execution().and_then(|e| e.cancel_handle());

        // Subscribe to stuck/unstuck events from the container's io_bridge.
        let mut stuck_rx = self.focused_execution().map(|e| e.subscribe_stuck());

        // Publish the stuck sender to the frontend (TUI uses it for tab coloring).
        if let Some(exec) = self.focused_execution() {
            self.frontend
                .attach_engine(EngineHandles::stuck(exec.stuck_sender()));
        }

        let mut exec = self
            .take_focused_execution()
            .expect("launch_step stored execution");
        let (wait_tx, mut wait_rx) =
            tokio::sync::oneshot::channel::<(AgentExecution, Result<AgentExitInfo, EngineError>)>();
        tokio::spawn(async move {
            let result = exec.wait().await;
            let _ = wait_tx.send((exec, result));
        });

        loop {
            tokio::select! {
                biased;
                result = &mut wait_rx => {
                    let (exec_back, exit_result) = result
                        .map_err(|_| EngineError::Other("step wait task dropped unexpectedly".into()))?;
                    self.set_focused_execution(exec_back);
                    return Ok(InterruptibleStepResult::StepCompleted(
                        self.finalize_step(&step_name, exit_result?)?
                    ));
                }
                Some(event) = Self::recv_stuck(&mut stuck_rx) => {
                    match event {
                        StuckEvent::Stuck => {
                            let result = self.handle_step_stuck(
                                &step_name,
                                &cancel_handle,
                                &mut wait_rx,
                                &mut stuck_rx,
                            ).await?;
                            match result {
                                None => continue,
                                Some(r) => return Ok(r),
                            }
                        }
                        StuckEvent::Unstuck => {
                            // Not inside a yolo countdown — nothing to cancel.
                        }
                        StuckEvent::StartupGraceExpired => {
                            // Container produced no output during its grace
                            // window. The bridge already invoked the cancel
                            // callback to kill it; surface a warning and let
                            // wait_rx resolve naturally so finalize_step
                            // records the failure. This is an awman-initiated
                            // kill, so mark the slot to suppress a failure log.
                            self.mark_focused_killed();
                            self.msg_warning(format!(
                                "Step '{}' produced no output before its startup grace expired; killing container",
                                step_name,
                            ));
                        }
                    }
                }
                Some(req) = Self::recv_engine(&mut self.engine_rx) => {
                    match req {
                        EngineRequest::OpenControlBoard { .. } => {
                            let mid = self.handle_mid_step_control_board(
                                &step_name,
                                &cancel_handle,
                                &mut wait_rx,
                            )?;
                            match mid {
                                MidStepOutcome::Continue => continue,
                                MidStepOutcome::StepCompleted(o) => {
                                    return Ok(InterruptibleStepResult::StepCompleted(o));
                                }
                                MidStepOutcome::WorkflowEnded(wo) => {
                                    return Ok(InterruptibleStepResult::WorkflowEnded(wo));
                                }
                                MidStepOutcome::LoopContinue => {
                                    return Ok(InterruptibleStepResult::LoopContinue);
                                }
                            }
                        }
                        EngineRequest::StepStuck { .. } => {
                            let result = self.handle_step_stuck(
                                &step_name,
                                &cancel_handle,
                                &mut wait_rx,
                                &mut stuck_rx,
                            ).await?;
                            match result {
                                None => continue,
                                Some(r) => return Ok(r),
                            }
                        }
                        EngineRequest::StepUnstuck { .. } => {
                            // Not inside a yolo countdown — nothing to cancel.
                        }
                    }
                }
            }
        }
    }

    /// Receive from the engine channel, or pend forever if None.
    pub(super) async fn recv_engine(
        rx: &mut Option<tokio::sync::mpsc::UnboundedReceiver<EngineRequest>>,
    ) -> Option<EngineRequest> {
        match rx {
            Some(rx) => rx.recv().await,
            None => std::future::pending().await,
        }
    }

    /// Receive from the stuck broadcast channel, or pend forever if None.
    pub(super) async fn recv_stuck(
        rx: &mut Option<tokio::sync::broadcast::Receiver<StuckEvent>>,
    ) -> Option<StuckEvent> {
        match rx {
            Some(rx) => rx.recv().await.ok(),
            None => std::future::pending().await,
        }
    }

    /// Kill the current step's container and immediately tell the frontend
    /// it is gone. Used by every engine-initiated kill (yolo auto-advance,
    /// WCB advance/restart/back/pause/abort/finish). Steps whose container
    /// exits on its own are reported via `finalize_step` instead.
    pub(super) fn kill_current_container(
        &mut self,
        cancel_handle: &Option<crate::engine::agent_runtime::execution::CancelHandle>,
    ) {
        // Mark before cancelling so that if the container's wait future later
        // reaches finalize_step, the exit is treated as an expected kill and no
        // failure log is written.
        self.mark_focused_killed();
        if let Some(ch) = cancel_handle {
            let _ = ch.cancel();
            self.frontend.report_container_exited(KILLED_EXIT_CODE);
        }
    }

    /// Handle a stuck event (from broadcast channel or EngineRequest).
    /// Returns `None` to continue the select loop, or `Some(result)` to return.
    pub(super) async fn handle_step_stuck(
        &mut self,
        step_name: &str,
        cancel_handle: &Option<crate::engine::agent_runtime::execution::CancelHandle>,
        wait_rx: &mut tokio::sync::oneshot::Receiver<(
            AgentExecution,
            Result<AgentExitInfo, EngineError>,
        )>,
        stuck_rx: &mut Option<tokio::sync::broadcast::Receiver<StuckEvent>>,
    ) -> Result<Option<InterruptibleStepResult>, EngineError> {
        self.msg_warning(format!("Step '{}' appears stuck (no output)", step_name,));
        if self.yolo && !self.is_last_step() {
            let yolo_result = self
                .run_mid_step_yolo_countdown(step_name, cancel_handle, wait_rx, stuck_rx)
                .await?;
            match yolo_result {
                MidStepYoloResult::StepCompleted(o) => {
                    Ok(Some(InterruptibleStepResult::StepCompleted(o)))
                }
                MidStepYoloResult::ShowControlBoard => {
                    let mid =
                        self.handle_mid_step_control_board(step_name, cancel_handle, wait_rx)?;
                    Ok(match mid {
                        MidStepOutcome::Continue => None,
                        MidStepOutcome::StepCompleted(o) => {
                            Some(InterruptibleStepResult::StepCompleted(o))
                        }
                        MidStepOutcome::WorkflowEnded(wo) => {
                            Some(InterruptibleStepResult::WorkflowEnded(wo))
                        }
                        MidStepOutcome::LoopContinue => Some(InterruptibleStepResult::LoopContinue),
                    })
                }
                MidStepYoloResult::Cancelled | MidStepYoloResult::Recovered => Ok(None),
                MidStepYoloResult::Advanced => {
                    self.msg_info(format!("Yolo auto-advancing past step '{}'", step_name,));
                    self.kill_current_container(cancel_handle);
                    self.state.set_status(step_name, StepState::Succeeded);
                    self.persist()?;
                    let step = self.find_step(step_name)?;
                    self.frontend
                        .report_step_status(&step, WorkflowStepStatus::Succeeded);
                    let progress = self.workflow_progress_info();
                    self.frontend.report_workflow_progress(&progress);

                    if self.is_last_step() {
                        let available = self.compute_available_actions()?;
                        let action = self
                            .frontend
                            .show_workflow_control_board(&self.state, &available)?;
                        return Ok(Some(self.execute_top_level_action(action)?));
                    }

                    Ok(Some(InterruptibleStepResult::LoopContinue))
                }
            }
        } else {
            let mid = self.handle_mid_step_control_board(step_name, cancel_handle, wait_rx)?;
            Ok(match mid {
                MidStepOutcome::Continue => None,
                MidStepOutcome::StepCompleted(o) => Some(InterruptibleStepResult::StepCompleted(o)),
                MidStepOutcome::WorkflowEnded(wo) => {
                    Some(InterruptibleStepResult::WorkflowEnded(wo))
                }
                MidStepOutcome::LoopContinue => Some(InterruptibleStepResult::LoopContinue),
            })
        }
    }

    /// Run a mid-step yolo countdown. The step container keeps running while
    /// the countdown ticks. The engine calls `yolo_countdown_started` at the
    /// beginning and `yolo_countdown_finished` before returning.
    pub(super) async fn run_mid_step_yolo_countdown(
        &mut self,
        step_name: &str,
        _cancel_handle: &Option<crate::engine::agent_runtime::execution::CancelHandle>,
        wait_rx: &mut tokio::sync::oneshot::Receiver<(
            AgentExecution,
            Result<AgentExitInfo, EngineError>,
        )>,
        stuck_rx: &mut Option<tokio::sync::broadcast::Receiver<StuckEvent>>,
    ) -> Result<MidStepYoloResult, EngineError> {
        self.msg_info(format!(
            "Starting yolo countdown for step '{}' ({}s)",
            step_name,
            timing::YOLO_COUNTDOWN_DURATION.as_secs(),
        ));
        self.frontend
            .yolo_countdown_started(step_name, CountdownKind::StuckStep);
        let total = timing::YOLO_COUNTDOWN_DURATION;
        let start = std::time::Instant::now();

        loop {
            // Drain any pending stuck events first. Without this, an `Unstuck`
            // event that lands at almost the same instant as countdown expiry
            // can be passed over by the `remaining.is_zero()` check below —
            // the loop would return `Advanced` (and mark the step Succeeded)
            // even though the container just produced fresh output. Draining
            // here guarantees Unstuck wins the race.
            if let Some(rx) = stuck_rx.as_mut() {
                loop {
                    match rx.try_recv() {
                        Ok(StuckEvent::Unstuck) => {
                            self.msg_info(format!(
                                "Step '{}' recovered, cancelling countdown (timers reset)",
                                step_name,
                            ));
                            self.frontend.yolo_countdown_finished(step_name);
                            return Ok(MidStepYoloResult::Recovered);
                        }
                        Ok(StuckEvent::StartupGraceExpired) => {
                            self.msg_warning(format!(
                                "Step '{}' produced no output before its startup grace expired; cancelling countdown",
                                step_name,
                            ));
                            self.frontend.yolo_countdown_finished(step_name);
                            return Ok(MidStepYoloResult::Recovered);
                        }
                        Ok(StuckEvent::Stuck) => continue,
                        Err(tokio::sync::broadcast::error::TryRecvError::Empty) => break,
                        // Lagged: a message was dropped because the channel
                        // buffer (16) was exceeded. Loop again so we keep
                        // draining whatever's still in the queue.
                        Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => continue,
                        Err(tokio::sync::broadcast::error::TryRecvError::Closed) => break,
                    }
                }
            }

            let elapsed = start.elapsed();
            let remaining = if elapsed >= total {
                std::time::Duration::ZERO
            } else {
                total - elapsed
            };

            match self
                .frontend
                .yolo_countdown_tick(step_name, remaining, total)?
            {
                YoloTickOutcome::AdvanceNow => {
                    self.frontend.yolo_countdown_finished(step_name);
                    return Ok(MidStepYoloResult::Advanced);
                }
                YoloTickOutcome::Cancel => {
                    self.msg_info(format!("Yolo countdown cancelled for step '{}'", step_name,));
                    self.frontend.yolo_countdown_finished(step_name);
                    return Ok(MidStepYoloResult::Cancelled);
                }
                YoloTickOutcome::Continue => {}
            }

            if remaining.is_zero() {
                self.frontend.yolo_countdown_finished(step_name);
                return Ok(MidStepYoloResult::Advanced);
            }

            tokio::select! {
                biased;
                result = &mut *wait_rx => {
                    let (exec_back, exit_result) = result
                        .map_err(|_| EngineError::Other("step wait task dropped unexpectedly".into()))?;
                    self.set_focused_execution(exec_back);
                    self.frontend.yolo_countdown_finished(step_name);
                    return Ok(MidStepYoloResult::StepCompleted(
                        self.finalize_step(step_name, exit_result?)?
                    ));
                }
                Some(event) = Self::recv_stuck(stuck_rx) => {
                    match event {
                        StuckEvent::Unstuck => {
                            self.msg_info(format!(
                                "Step '{}' recovered, cancelling countdown (timers reset)",
                                step_name,
                            ));
                            self.frontend.yolo_countdown_finished(step_name);
                            return Ok(MidStepYoloResult::Recovered);
                        }
                        StuckEvent::Stuck => {
                            // Already counting down; ignore duplicate.
                        }
                        StuckEvent::StartupGraceExpired => {
                            // The container never produced its first byte
                            // before grace ran out, so the bridge already
                            // killed it. Tear down the countdown; wait_rx
                            // will resolve and finalize_step records the
                            // failure.
                            self.msg_warning(format!(
                                "Step '{}' produced no output before its startup grace expired; cancelling countdown",
                                step_name,
                            ));
                            self.frontend.yolo_countdown_finished(step_name);
                            return Ok(MidStepYoloResult::Recovered);
                        }
                    }
                }
                Some(req) = Self::recv_engine(&mut self.engine_rx) => {
                    match req {
                        EngineRequest::OpenControlBoard { .. } => {
                            self.frontend.yolo_countdown_finished(step_name);
                            return Ok(MidStepYoloResult::ShowControlBoard);
                        }
                        EngineRequest::StepUnstuck { .. } => {
                            self.msg_info(format!(
                                "Step '{}' recovered (engine request), cancelling countdown",
                                step_name,
                            ));
                            self.frontend.yolo_countdown_finished(step_name);
                            return Ok(MidStepYoloResult::Recovered);
                        }
                        EngineRequest::StepStuck { .. } => {
                            // Already counting down; ignore duplicate.
                        }
                    }
                }
                _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {}
            }
        }
    }
}
