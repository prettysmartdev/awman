//! Parallel groups (WI-0096): launching N containers at once, per-slot yolo
//! countdowns, and aborting or pausing the group as a whole.
//!
//! Split out of `workflow/mod.rs` by WI 0114 F-51. A child module of
//! `workflow`, so it reaches `WorkflowEngine`'s private fields exactly as the
//! code did before the move; methods it defines are `pub(super)` so the
//! other halves of the engine can still call them.

use super::*;

impl WorkflowEngine {
    /// Run a whole parallel group to completion. Launches up to
    /// `max_concurrent` of `ready` (source-file order), queuing the rest, then
    /// drives all live containers concurrently through a `FuturesUnordered`
    /// select loop — launching queued steps as slots free up, running per-step
    /// stuck detection and per-step yolo countdowns independently, and honoring
    /// `abort_on_failure` / WCB pause+abort mid-group.
    pub(super) async fn run_parallel_group(
        &mut self,
        ready: Vec<WorkflowStep>,
    ) -> Result<GroupOutcome, EngineError> {
        let group_names: Vec<String> = ready.iter().map(|s| s.name.clone()).collect();
        self.frontend.report_parallel_group_started(&group_names);
        let slot_cap = match self.max_concurrent {
            Some(n) => n.max(1),
            None => ready.len().max(1),
        };
        self.msg_info(format!(
            "Launching parallel group: {} step(s), up to {} at once",
            group_names.len(),
            slot_cap,
        ));

        let mut queue: VecDeque<WorkflowStep> = ready.into_iter().collect();
        self.active_steps.clear();

        let (stuck_tx, mut stuck_rx) =
            tokio::sync::mpsc::unbounded_channel::<(String, StuckEvent)>();
        let mut waits: ParallelWaits = FuturesUnordered::new();

        // Launch the initial batch.
        while self.active_steps.len() < slot_cap {
            match queue.pop_front() {
                Some(step) => self.launch_parallel_step(step, &mut waits, &stuck_tx, false)?,
                None => break,
            }
        }

        let total = timing::YOLO_COUNTDOWN_DURATION;
        let mut failed: Vec<(String, i32)> = Vec::new();

        while !self.active_steps.is_empty() {
            tokio::select! {
                biased;
                Some((name, result)) = waits.next() => {
                    // Guard against futures for steps already finalized out of
                    // band (yolo auto-advance kills the container but leaves its
                    // wait future pending; it resolves here later as a no-op).
                    if !self.active_steps.iter().any(|s| s.step_name == name) {
                        continue;
                    }
                    let exit = result?;
                    // Persist the buffered output on a genuine failure before the
                    // slot (which owns the tail + container name) is removed.
                    self.maybe_dump_step_failure(&name, exit.exit_code);
                    let auth_recovered = exit.exit_code != 0 && self.recover_auth_failure(&name)?;
                    self.remove_active_step(&name);
                    self.last_exit_info = Some(exit.clone());

                    let (status, step_state) = if exit.exit_code == 0 {
                        (WorkflowStepStatus::Succeeded, StepState::Succeeded)
                    } else if auth_recovered {
                        (WorkflowStepStatus::Running, StepState::Pending)
                    } else {
                        (
                            WorkflowStepStatus::Failed { exit_code: exit.exit_code },
                            StepState::Failed { exit_code: exit.exit_code, error_message: None },
                        )
                    };
                    let step = self.find_step(&name)?;
                    self.state.set_status(&name, step_state);
                    self.frontend.report_step_status(&step, status.clone());
                    self.frontend.report_parallel_step_exited(&name, exit.exit_code);
                    self.persist()?;
                    let progress = self.workflow_progress_info();
                    self.frontend.report_workflow_progress(&progress);

                    if auth_recovered {
                        // Put the same step back at the head of this parallel
                        // batch. `auth_retries_used` ensures this automatic
                        // relaunch can happen only once.
                        queue.push_front(step);
                        if self.active_steps.len() < slot_cap {
                            if let Some(next) = queue.pop_front() {
                                self.launch_parallel_step(next, &mut waits, &stuck_tx, true)?;
                            }
                        }
                    } else if let WorkflowStepStatus::Failed { exit_code } = status {
                        if step.abort_on_failure {
                            self.msg_warning(format!(
                                "Step '{}' failed (abort_on_failure); aborting parallel group",
                                name,
                            ));
                            let wo = self.abort_parallel_group()?;
                            return Ok(GroupOutcome::Ended(wo));
                        }
                        // Non-abort failure: record it, keep draining the rest
                        // of the group, but do NOT launch further queued steps.
                        // Every failure is recorded — each one gets its own
                        // recovery board once the group drains.
                        failed.push((name.clone(), exit_code));
                    } else if self.active_steps.len() < slot_cap {
                        if let Some(next) = queue.pop_front() {
                            self.launch_parallel_step(next, &mut waits, &stuck_tx, true)?;
                        }
                    }
                }
                Some((name, event)) = stuck_rx.recv() => {
                    self.handle_parallel_stuck_event(&name, event);
                }
                Some(req) = Self::recv_engine(&mut self.engine_rx) => {
                    if let Some(wo) = self.handle_parallel_engine_request(req)? {
                        return Ok(GroupOutcome::Ended(wo));
                    }
                }
                _ = tokio::time::sleep(Duration::from_millis(100)) => {
                    self.tick_parallel_yolo(&mut waits, &mut queue, slot_cap, total, &stuck_tx)?;
                }
            }
        }

        self.frontend.report_parallel_group_finished();
        Ok(GroupOutcome::Drained { failed })
    }

    /// Launch one step of a parallel group: resolve agent/model, spawn the
    /// container, wire its stuck broadcast into the fan-in channel, move its
    /// execution into a wait future, and record an `ActiveParallelStep` slot.
    pub(super) fn launch_parallel_step(
        &mut self,
        step: WorkflowStep,
        waits: &mut ParallelWaits,
        stuck_tx: &StuckFanIn,
        dequeued: bool,
    ) -> Result<(), EngineError> {
        let resolved_agent = self.resolve_agent(&step)?;
        let resolved_model = self.resolve_model(&step);
        tracing::info!(
            step = %step.name,
            agent = %resolved_agent.as_str(),
            model = ?resolved_model,
            "workflow_engine launching parallel step"
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

        let stuck_sender = execution.stuck_sender();
        let cancel_handle = execution.cancel_handle();
        let container_name = execution.handle().name.clone();
        let output_tail = execution.output_tail();

        // Publish the per-step stuck sender so the frontend can subscribe for
        // this specific container's status bar.
        self.frontend
            .attach_engine(EngineHandles::step_stuck(&step.name, stuck_sender.clone()));
        if dequeued {
            self.frontend.report_parallel_step_dequeued(
                &step.name,
                resolved_agent.as_str(),
                resolved_model.as_deref(),
            );
        } else {
            self.frontend.report_parallel_step_launched(
                &step.name,
                resolved_agent.as_str(),
                resolved_model.as_deref(),
            );
        }
        // Published after the launch/dequeue event so the frontend's slot
        // exists by the time the name arrives; drives per-container stats.
        self.frontend
            .report_parallel_step_container(&step.name, &execution.handle().name);

        // Forward this container's stuck broadcast into the unified fan-in
        // channel, tagged with the step name, so the select loop stays
        // fixed-arity regardless of how many containers are live.
        let mut rx = execution.subscribe_stuck();
        let fwd = stuck_tx.clone();
        let fwd_name = step.name.clone();
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(ev) => {
                        if fwd.send((fwd_name.clone(), ev)).is_err() {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                }
            }
        });

        let name = step.name.clone();
        waits.push(Box::pin(async move {
            let mut execution = execution;
            let r = execution.wait().await;
            (name, r)
        }));

        self.active_steps.push(ActiveParallelStep {
            step_name: step.name.clone(),
            execution: None,
            cancel_handle,
            container_name,
            output_tail,
            awman_killed: false,
            stuck: false,
            yolo_deadline: None,
            agent: resolved_agent,
            model: resolved_model,
        });
        Ok(())
    }

    pub(super) fn remove_active_step(&mut self, name: &str) {
        self.active_steps.retain(|s| s.step_name != name);
    }

    /// Delegate auth-failure recognition and refresh to the command-layer
    /// factory. Generic workflow code never knows an agent's signatures.
    ///
    /// The guard is recorded before the recovery attempt: even if the host
    /// refresh cannot advance, a matching failed step may be relaunched at most
    /// once during this workflow invocation.
    pub(super) fn recover_auth_failure(&mut self, step_name: &str) -> Result<bool, EngineError> {
        if self.auth_retries_used.contains(step_name) {
            return Ok(false);
        }
        let Some((agent, output_tail)) = self
            .active_steps
            .iter()
            .find(|s| s.step_name == step_name)
            .map(|s| {
                (
                    s.agent.clone(),
                    s.output_tail
                        .as_ref()
                        .map(|tail| tail.snapshot_text())
                        .unwrap_or_default(),
                )
            })
        else {
            return Ok(false);
        };
        if !self
            .agent_factory
            .recover_auth_failure(&agent, &output_tail)?
        {
            return Ok(false);
        }
        self.auth_retries_used.insert(step_name.to_string());
        self.msg_warning(format!(
            "Step '{step_name}' failed authentication; refreshed credentials and retrying once"
        ));
        Ok(true)
    }

    /// Handle a per-step stuck/unstuck transition inside a parallel group.
    /// Independent per slot: a noisy sibling never masks a stuck step, and a
    /// stuck step never blocks its siblings.
    pub(super) fn handle_parallel_stuck_event(&mut self, name: &str, event: StuckEvent) {
        match event {
            StuckEvent::Stuck => {
                if self.yolo {
                    let start_countdown = {
                        match self.active_steps.iter_mut().find(|s| s.step_name == name) {
                            Some(s) if s.yolo_deadline.is_none() => {
                                s.yolo_deadline =
                                    Some(Instant::now() + timing::YOLO_COUNTDOWN_DURATION);
                                true
                            }
                            _ => false,
                        }
                    };
                    if start_countdown {
                        self.msg_info(format!(
                            "Step '{}' appears stuck; starting yolo countdown",
                            name,
                        ));
                        self.frontend.parallel_step_yolo_countdown_started(name);
                    }
                } else {
                    if let Some(s) = self.active_steps.iter_mut().find(|s| s.step_name == name) {
                        s.stuck = true;
                    }
                    self.msg_warning(format!("Step '{}' appears stuck (no output)", name));
                    self.frontend.report_parallel_step_stuck(name);
                }
            }
            StuckEvent::Unstuck => {
                if let Some(s) = self.active_steps.iter_mut().find(|s| s.step_name == name) {
                    let had_countdown = s.yolo_deadline.take().is_some();
                    s.stuck = false;
                    if had_countdown {
                        self.frontend.parallel_step_yolo_countdown_finished(name);
                    }
                }
                self.frontend.report_parallel_step_unstuck(name);
            }
            StuckEvent::StartupGraceExpired => {
                // The bridge already killed the container; its wait future will
                // resolve and be finalized as a failure. Mark the slot as
                // awman-killed so the drain loop suppresses the failure log for
                // this expected kill.
                if let Some(s) = self.active_steps.iter_mut().find(|s| s.step_name == name) {
                    s.awman_killed = true;
                }
            }
        }
    }

    /// Drive every in-flight per-step yolo countdown one tick. Independent per
    /// slot — expiry kills only that container and advances the queue.
    pub(super) fn tick_parallel_yolo(
        &mut self,
        waits: &mut ParallelWaits,
        queue: &mut VecDeque<WorkflowStep>,
        slot_cap: usize,
        total: Duration,
        stuck_tx: &StuckFanIn,
    ) -> Result<(), EngineError> {
        let now = Instant::now();
        let ticking: Vec<(String, Instant)> = self
            .active_steps
            .iter()
            .filter_map(|s| s.yolo_deadline.map(|d| (s.step_name.clone(), d)))
            .collect();
        for (name, deadline) in ticking {
            let remaining = deadline.saturating_duration_since(now);
            match self
                .frontend
                .parallel_step_yolo_countdown_tick(&name, remaining, total)?
            {
                YoloTickOutcome::Cancel => {
                    if let Some(s) = self.active_steps.iter_mut().find(|s| s.step_name == name) {
                        s.yolo_deadline = None;
                    }
                    self.frontend.parallel_step_yolo_countdown_finished(&name);
                }
                YoloTickOutcome::AdvanceNow => {
                    self.yolo_advance_parallel(&name, waits, queue, slot_cap, stuck_tx)?;
                }
                YoloTickOutcome::Continue => {
                    if remaining.is_zero() {
                        self.yolo_advance_parallel(&name, waits, queue, slot_cap, stuck_tx)?;
                    }
                }
            }
        }
        Ok(())
    }

    /// Yolo countdown expired (or user forced advance) for one parallel slot:
    /// kill just that container, mark the step Succeeded, free the slot, and
    /// launch the next queued step if one is waiting.
    pub(super) fn yolo_advance_parallel(
        &mut self,
        name: &str,
        waits: &mut ParallelWaits,
        queue: &mut VecDeque<WorkflowStep>,
        slot_cap: usize,
        stuck_tx: &StuckFanIn,
    ) -> Result<(), EngineError> {
        self.msg_info(format!("Yolo auto-advancing past step '{}'", name));
        self.frontend.parallel_step_yolo_countdown_finished(name);
        if let Some(pos) = self.active_steps.iter().position(|s| s.step_name == name) {
            if let Some(ch) = &self.active_steps[pos].cancel_handle {
                let _ = ch.cancel();
            }
            self.active_steps.remove(pos);
        }
        self.frontend
            .report_parallel_step_exited(name, KILLED_EXIT_CODE);
        self.state.set_status(name, StepState::Succeeded);
        let step = self.find_step(name)?;
        self.frontend
            .report_step_status(&step, WorkflowStepStatus::Succeeded);
        self.persist()?;
        let progress = self.workflow_progress_info();
        self.frontend.report_workflow_progress(&progress);
        if self.active_steps.len() < slot_cap {
            if let Some(next) = queue.pop_front() {
                self.launch_parallel_step(next, waits, stuck_tx, true)?;
            }
        }
        Ok(())
    }

    /// Kill every live container in the current parallel group and cancel all
    /// not-yet-completed steps, then proceed with the standard abort path.
    pub(super) fn abort_parallel_group(&mut self) -> Result<WorkflowOutcome, EngineError> {
        self.abort_on_failure_triggered = true;
        let names: Vec<String> = self
            .active_steps
            .iter()
            .map(|s| s.step_name.clone())
            .collect();
        for s in &self.active_steps {
            if let Some(ch) = &s.cancel_handle {
                let _ = ch.cancel();
            }
        }
        self.active_steps.clear();
        for name in &names {
            self.frontend
                .report_parallel_step_exited(name, KILLED_EXIT_CODE);
        }
        for s in &self.workflow.steps {
            if !self.state.completed_steps.contains(&s.name) {
                self.state.set_status(&s.name, StepState::Cancelled);
            }
        }
        self.persist()?;
        self.frontend.report_parallel_group_finished();
        let aborted = WorkflowOutcome::Aborted;
        self.frontend.report_workflow_completed(&aborted);
        Ok(aborted)
    }

    /// WCB Pause during a parallel group: kill all live containers, reset the
    /// running steps to Pending so a resume replays them, and end the run.
    pub(super) fn pause_parallel_group(&mut self) -> Result<WorkflowOutcome, EngineError> {
        let names: Vec<String> = self
            .active_steps
            .iter()
            .map(|s| s.step_name.clone())
            .collect();
        for s in &self.active_steps {
            if let Some(ch) = &s.cancel_handle {
                let _ = ch.cancel();
            }
        }
        self.active_steps.clear();
        for name in &names {
            self.frontend
                .report_parallel_step_exited(name, KILLED_EXIT_CODE);
            self.state.set_status(name, StepState::Pending);
        }
        self.persist()?;
        self.frontend.report_parallel_group_finished();
        let paused = WorkflowOutcome::Paused;
        self.frontend.report_workflow_completed(&paused);
        Ok(paused)
    }

    /// Route an `EngineRequest` received while a parallel group is running.
    /// Returns `Some(outcome)` when the request ends the workflow (WCB
    /// pause/abort), `None` otherwise.
    pub(super) fn handle_parallel_engine_request(
        &mut self,
        req: EngineRequest,
    ) -> Result<Option<WorkflowOutcome>, EngineError> {
        match req {
            EngineRequest::StepStuck { step_name } => {
                self.handle_parallel_stuck_event(&step_name, StuckEvent::Stuck);
                Ok(None)
            }
            EngineRequest::StepUnstuck { step_name } => {
                self.handle_parallel_stuck_event(&step_name, StuckEvent::Unstuck);
                Ok(None)
            }
            EngineRequest::OpenControlBoard { step_name } => {
                // Scope the board to the focused container; peers keep running.
                self.focus_parallel_step(&step_name);
                let available = self.compute_available_actions()?;
                let action = self
                    .frontend
                    .show_workflow_control_board(&self.state, &available)?;
                self.log_wcb_action(&action);
                match action {
                    NextAction::Pause => Ok(Some(self.pause_parallel_group()?)),
                    NextAction::Abort => Ok(Some(self.abort_parallel_group()?)),
                    // Back / finish / restart / continue / launch-next are all
                    // scoped away while peers run (see compute_available_actions
                    // §10); treat anything else as a dismiss — the group keeps
                    // running undisturbed.
                    _ => Ok(None),
                }
            }
        }
    }

    /// Make `step_name` the focused slot (index 0) so `compute_available_actions`
    /// evaluates the board relative to it. No-op if the name is unknown.
    pub(super) fn focus_parallel_step(&mut self, step_name: &str) {
        if let Some(pos) = self
            .active_steps
            .iter()
            .position(|s| s.step_name == step_name)
        {
            self.active_steps.swap(0, pos);
            let s = &self.active_steps[0];
            self.current_step_name = Some(s.step_name.clone());
            self.current_step_agent = Some(s.agent.clone());
            self.current_step_model = s.model.clone();
        }
    }
}
