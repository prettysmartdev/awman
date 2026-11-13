//! One step at a time: launching it, waiting on it, and reacting to it
//! going quiet or exiting non-zero.
//!
//! Split out of `workflow/mod.rs` by WI 0114 F-51. A child module of
//! `workflow`, so it reaches `WorkflowEngine`'s private fields exactly as the
//! code did before the move; methods it defines are `pub(super)` so the
//! other halves of the engine can still call them.

use super::*;

/// Resolved inputs for one launch. Scheduling and frontend notifications stay
/// with the single-step or parallel caller.
pub(super) struct PreparedStepLaunch {
    pub(super) runtime: WorkflowRuntimeContext,
    pub(super) attempt: u32,
}

/// A spawned execution whose container identity has been persisted.
pub(super) struct StepLaunch {
    pub(super) execution: AgentExecution,
    pub(super) agent: AgentName,
    pub(super) model: Option<String>,
}

impl WorkflowEngine {
    pub(super) fn prepare_step_launch(
        &mut self,
        step: &WorkflowStep,
    ) -> Result<PreparedStepLaunch, EngineError> {
        let agent = self.resolve_agent(step)?;
        let model = self.resolve_model(step);
        tracing::info!(
            step = %self.redaction.redact(&step.name),
            agent = %agent.as_str(),
            model = ?model,
            "workflow_engine resolved step parameters"
        );
        let attempt = self.state.record_step_launch(&step.name);
        let runtime = WorkflowRuntimeContext {
            step_agent: agent,
            step_model: model,
            git_root: self.session.git_root().to_path_buf(),
            session_id: self.session.id(),
            workflow_invocation_id: self.state.invocation_id,
            workflow_step_info: self.build_workflow_step_info(&step.name),
            source: self.main_step_source(&step.name, attempt),
        };
        Ok(PreparedStepLaunch { runtime, attempt })
    }

    /// Persist both sides of the factory call. A failed spawn has no live
    /// container; record Failed and attempt to save it before returning the
    /// factory error. A failed save is logged and resume handles the prior
    /// interrupted Running checkpoint.
    pub(super) fn spawn_prepared_step(
        &mut self,
        step: &WorkflowStep,
        prepared: PreparedStepLaunch,
    ) -> Result<StepLaunch, EngineError> {
        self.state
            .set_status(&step.name, StepState::Running { container_id: None });
        self.persist()?;
        let execution = match self.agent_factory.execution_for_step(
            step,
            &self.session,
            &prepared.runtime,
        ) {
            Ok(execution) => execution,
            Err(error) => {
                self.state.set_status(
                    &step.name,
                    StepState::Failed {
                        exit_code: -1,
                        error_message: Some(error.to_string()),
                    },
                );
                if let Err(save_error) = self.persist() {
                    tracing::error!(step = %self.redaction.redact(&step.name), error = %self.redaction.redact(&save_error.to_string()),
                            spawn_error = %self.redaction.redact(&error.to_string()), "could not persist failed workflow launch");
                }
                return Err(error);
            }
        };
        self.state.set_status(
            &step.name,
            StepState::Running {
                container_id: Some(execution.handle().id.clone()),
            },
        );
        self.state
            .record_step_container(&step.name, &execution.handle().name);
        self.persist()?;
        let agent = prepared.runtime.step_agent;
        let model = prepared.runtime.step_model;
        self.last_launched_identity = Some((agent.clone(), model.clone()));
        Ok(StepLaunch {
            execution,
            agent,
            model,
        })
    }

    /// The attribution of a main step's container (WI 0137 §5): its index
    /// in `WorkflowState.steps`, stable across attempts, and the 1-based
    /// attempt this launch is.
    pub(super) fn main_step_source(&self, step_name: &str, step_attempt: u32) -> EventSource {
        EventSource {
            phase: EventSourcePhase::Main,
            step_name: Some(step_name.to_string()),
            step_index: self.state.step_index(step_name),
            container: None,
            step_attempt: Some(step_attempt),
        }
    }

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
            // A cancel that lands while the step was failing wins: no auth
            // retry, no abort_on_failure, no unattended retry.
            if self.cancel_requested() {
                return Ok(IterationOutcome::Ended(self.cancel_run()?));
            }

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
        }

        // run_to_completion enforces final review in every interactive mode,
        // including natural exits and resumed runs with all steps complete.
        if !workflow_just_completed {
            let available = self.compute_available_actions()?;
            let action = self
                .frontend
                .show_workflow_control_board(&self.state, &available)?;
            self.log_wcb_action(&action);
            match action {
                NextAction::Dismiss
                | NextAction::LaunchNext
                | NextAction::RetryFailedStep { .. } => return Ok(IterationOutcome::Continue),
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

        let prepared = self.prepare_step_launch(&step)?;
        self.frontend.report_step_interactive_launch(
            &step,
            prepared.runtime.step_agent.as_str(),
            prepared.runtime.step_model.as_deref(),
        );
        self.frontend
            .report_step_launching(&step.name, prepared.attempt);
        self.frontend
            .report_step_status(&step, WorkflowStepStatus::Running);
        let launch = match self.spawn_prepared_step(&step, prepared) {
            Ok(launch) => launch,
            Err(error) => {
                if matches!(
                    self.state.status_of(&step.name),
                    Some(StepState::Failed { .. })
                ) {
                    self.frontend
                        .report_step_status(&step, WorkflowStepStatus::Failed { exit_code: -1 });
                }
                return Err(error);
            }
        };
        let single_step::StepLaunch {
            execution,
            agent: resolved_agent,
            model: resolved_model,
        } = launch;

        let container_name = execution.handle().name.clone();
        let handle = execution.handle().clone();
        let output_tail = execution.output_tail();
        let program_status = execution.program_status_tracker();
        self.active_steps = vec![ActiveParallelStep {
            step_name: step.name.clone(),
            execution: Some(execution),
            cancel_handle: None,
            container_name,
            handle,
            output_tail,
            awman_killed: false,
            stuck: false,
            yolo_deadline: None,
            countdown_kind: CountdownKind::StuckStep,
            output_watch: None,
            last_report: None,
            reported_problem: None,
            program_status,
            agent: resolved_agent.clone(),
            model: resolved_model.clone(),
            launch_id: 0,
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
        // WI 0141: the agent's final status (its tracker at exit) is
        // settled before the exit code is read, on every finalising path.
        self.settle_reported_status(step_name, None);
        self.last_exit_info = Some(exit.clone());
        // The step's container has actually terminated (wait() resolved) —
        // tell the frontend so it can tear down any live container UI.
        self.frontend.report_container_exited(exit.exit_code);

        // On a genuine (non-awman-kill) container failure, persist the buffered
        // output tail so the user can debug what went wrong.
        self.maybe_dump_step_failure(step_name, exit.exit_code);

        // WI 0141: a problem the agent reported outranks the exit code — a
        // container awman killed after `blocked`/`error` (KILLED_EXIT_CODE,
        // otherwise read as an engine advance) or one that exited by itself,
        // even with 0, is Failed with the agent's reason. After `done` the
        // exit code stands.
        let (status, step_state) = if let Some(problem) = self.reported_problem_of(step_name) {
            (
                WorkflowStepStatus::Failed {
                    exit_code: exit.exit_code,
                },
                StepState::Failed {
                    exit_code: exit.exit_code,
                    error_message: Some(program_status::reported_failure_reason(&problem)),
                },
            )
        } else if exit.exit_code == 0 {
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

        // Cloned so the select below borrows nothing of `self` for it.
        let cancel = self.cancel.clone();

        loop {
            tokio::select! {
                biased;
                result = &mut wait_rx => {
                    let (exec_back, exit_result) = result
                        .map_err(|_| EngineError::Other("step wait task dropped unexpectedly".into()))?;
                    // A report that was ready together with the exit is
                    // applied before finalising (the select is biased).
                    self.settle_reported_status(&step_name, stuck_rx.as_mut());
                    self.set_focused_execution(exec_back);
                    return Ok(InterruptibleStepResult::StepCompleted(
                        self.finalize_step(&step_name, exit_result?)?
                    ));
                }
                // After `wait_rx` (the loop is biased): a container that
                // already exited is finalised first.
                _ = cancel.cancelled() => {
                    self.kill_current_container(&cancel_handle);
                    return Ok(InterruptibleStepResult::WorkflowEnded(self.cancel_run()?));
                }
                Some(event) = Self::recv_stuck(&mut stuck_rx) => {
                    match event {
                        // The agent's own OSC 7501 status (WI 0141); see
                        // `program_status` for the state table.
                        StuckEvent::Reported(status) => {
                            if let Some(r) = self.handle_step_reported(
                                &step_name,
                                status,
                                &cancel_handle,
                                &mut wait_rx,
                                &mut stuck_rx,
                            ).await? {
                                return Ok(r);
                            }
                        }
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
        if self.yolo && !self.is_final_review_step(step_name) {
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
                // The agent started reporting during a silence countdown: its
                // report decides from here on.
                MidStepYoloResult::Reported(status) => {
                    self.handle_step_reported(step_name, status, cancel_handle, wait_rx, stuck_rx)
                        .await
                }
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
        Ok(
            match self
                .drive_yolo_countdown(step_name, wait_rx, stuck_rx)
                .await?
            {
                MidStepYoloResult::StepCompleted((exec_back, exit_result)) => {
                    self.set_focused_execution(exec_back);
                    MidStepYoloResult::StepCompleted(self.finalize_step(step_name, exit_result?)?)
                }
                MidStepYoloResult::Advanced => MidStepYoloResult::Advanced,
                MidStepYoloResult::Cancelled => MidStepYoloResult::Cancelled,
                MidStepYoloResult::ShowControlBoard => MidStepYoloResult::ShowControlBoard,
                MidStepYoloResult::Recovered => MidStepYoloResult::Recovered,
                MidStepYoloResult::Reported(status) => MidStepYoloResult::Reported(status),
            },
        )
    }

    /// The stuck-container yolo countdown shared by workflow steps and
    /// `on_failure` remediation agents: tick the frontend and stop on expiry,
    /// Esc, recovery, the container exiting (its wait result is handed back
    /// untouched) or Ctrl-W. Calls
    /// `yolo_countdown_started` at the beginning and `yolo_countdown_finished`
    /// before returning.
    pub(super) async fn drive_yolo_countdown<T>(
        &mut self,
        step_name: &str,
        wait_rx: &mut tokio::sync::oneshot::Receiver<T>,
        stuck_rx: &mut Option<tokio::sync::broadcast::Receiver<StuckEvent>>,
    ) -> Result<MidStepYoloResult<T>, EngineError> {
        self.msg_info(format!(
            "Starting yolo countdown for step '{}' ({}s)",
            step_name,
            timing::yolo_countdown_duration().as_secs(),
        ));
        self.frontend
            .yolo_countdown_started(step_name, CountdownKind::StuckStep);
        let total = timing::yolo_countdown_duration();
        let start = std::time::Instant::now();

        loop {
            // An external cancel ends the countdown; the caller's own select
            // then observes the token and stops the container.
            if self.cancel_requested() {
                self.frontend.yolo_countdown_finished(step_name);
                return Ok(MidStepYoloResult::Cancelled);
            }

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
                        // The agent reports its own status now, so silence
                        // no longer decides (WI 0141). Hand the report back.
                        Ok(StuckEvent::Reported(status)) => {
                            self.frontend.yolo_countdown_finished(step_name);
                            return Ok(MidStepYoloResult::Reported(status));
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
                    let result = result
                        .map_err(|_| EngineError::Other("step wait task dropped unexpectedly".into()))?;
                    self.frontend.yolo_countdown_finished(step_name);
                    return Ok(MidStepYoloResult::StepCompleted(result));
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
                        StuckEvent::Reported(status) => {
                            self.frontend.yolo_countdown_finished(step_name);
                            return Ok(MidStepYoloResult::Reported(status));
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
