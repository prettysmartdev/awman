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
                Some((name, launch_id, result)) = waits.next() => {
                    // Guard against futures for steps already finalized out of
                    // band (yolo auto-advance kills the container but leaves its
                    // wait future pending; it resolves here later as a no-op),
                    // and for a killed container whose step was relaunched.
                    if !self
                        .active_steps
                        .iter()
                        .any(|s| s.step_name == name && s.launch_id == launch_id)
                    {
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
                    // The earliest failure still awaiting recovery is offered
                    // as a retry on any board opened while the group runs.
                    let retryable = failed.first().map(|(name, _)| name.clone());
                    match self.handle_parallel_engine_request(
                        req,
                        &group_names,
                        retryable.as_deref(),
                    )? {
                        ParallelRequestOutcome::Continue => {}
                        ParallelRequestOutcome::Ended(wo) => return Ok(GroupOutcome::Ended(wo)),
                        ParallelRequestOutcome::Rewound => return Ok(GroupOutcome::Rewound),
                        ParallelRequestOutcome::RestartStep(name) => {
                            failed.retain(|(n, _)| *n != name);
                            self.restart_parallel_step(
                                &name,
                                &mut waits,
                                &mut queue,
                                slot_cap,
                                &stuck_tx,
                            )?;
                        }
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
        let launch_id = self.next_launch_id;
        self.next_launch_id += 1;
        waits.push(Box::pin(async move {
            let mut execution = execution;
            let r = execution.wait().await;
            (name, launch_id, r)
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
            launch_id,
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

    /// Kill every live container in the group and forget its slot. Returns
    /// the names of the steps that were running.
    fn kill_active_parallel_steps(&mut self) -> Vec<String> {
        let killed: Vec<ActiveParallelStep> = self.active_steps.drain(..).collect();
        for s in &killed {
            if let Some(ch) = &s.cancel_handle {
                let _ = ch.cancel();
            }
            if s.yolo_deadline.is_some() {
                self.frontend
                    .parallel_step_yolo_countdown_finished(&s.step_name);
            }
            self.frontend
                .report_parallel_step_exited(&s.step_name, KILLED_EXIT_CODE);
        }
        killed.into_iter().map(|s| s.step_name).collect()
    }

    /// Kill every live container in the current parallel group and cancel all
    /// not-yet-completed steps, then proceed with the standard abort path.
    pub(super) fn abort_parallel_group(&mut self) -> Result<WorkflowOutcome, EngineError> {
        self.abort_on_failure_triggered = true;
        self.kill_active_parallel_steps();
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
        for name in self.kill_active_parallel_steps() {
            self.state.set_status(&name, StepState::Pending);
        }
        self.persist()?;
        self.frontend.report_parallel_group_finished();
        let paused = WorkflowOutcome::Paused;
        self.frontend.report_workflow_completed(&paused);
        Ok(paused)
    }

    /// WCB "restart one agent of the group": restart `name` in a fresh
    /// container whatever state it is in — running (its container is killed
    /// first), finished, failed, or still queued. Launches at once when a slot
    /// is free, otherwise it goes to the front of the queue.
    pub(super) fn restart_parallel_step(
        &mut self,
        name: &str,
        waits: &mut ParallelWaits,
        queue: &mut VecDeque<WorkflowStep>,
        slot_cap: usize,
        stuck_tx: &StuckFanIn,
    ) -> Result<(), EngineError> {
        if let Some(pos) = self.active_steps.iter().position(|s| s.step_name == name) {
            let slot = self.active_steps.remove(pos);
            if let Some(ch) = &slot.cancel_handle {
                let _ = ch.cancel();
            }
            if slot.yolo_deadline.is_some() {
                self.frontend.parallel_step_yolo_countdown_finished(name);
            }
            self.frontend
                .report_parallel_step_exited(name, KILLED_EXIT_CODE);
        }
        queue.retain(|s| s.name != name);

        let step = self.find_step(name)?;
        if self.active_steps.len() < slot_cap {
            self.launch_parallel_step(step, waits, stuck_tx, true)?;
        } else {
            self.state.set_status(name, StepState::Pending);
            self.frontend
                .report_step_status(&step, WorkflowStepStatus::Pending);
            self.persist()?;
            queue.push_front(step);
        }
        let progress = self.workflow_progress_info();
        self.frontend.report_workflow_progress(&progress);
        Ok(())
    }

    /// Kill the group's containers and reset every step in `names` to
    /// Pending, ending the group so the outer loop starts again from whatever
    /// is ready. Shared by "restart the whole group" and "go back".
    fn rewind_parallel_group(&mut self, names: &[String]) -> Result<(), EngineError> {
        self.kill_active_parallel_steps();
        for name in names {
            self.state.set_status(name, StepState::Pending);
            let step = self.find_step(name)?;
            self.frontend
                .report_step_status(&step, WorkflowStepStatus::Pending);
        }
        self.persist()?;
        self.frontend.report_parallel_group_finished();
        let progress = self.workflow_progress_info();
        self.frontend.report_workflow_progress(&progress);
        Ok(())
    }

    /// WCB "next" mid-group: kill the group's containers and mark every member
    /// that has not succeeded as Skipped, so the steps after the group become
    /// ready (or the workflow completes, when nothing follows it).
    fn skip_parallel_group(&mut self, group: &[String]) -> Result<(), EngineError> {
        self.kill_active_parallel_steps();
        for name in group {
            if matches!(self.state.status_of(name), Some(StepState::Succeeded)) {
                continue;
            }
            self.state.set_status(name, StepState::Skipped);
            let step = self.find_step(name)?;
            self.frontend
                .report_step_status(&step, WorkflowStepStatus::Skipped);
        }
        self.persist()?;
        self.frontend.report_parallel_group_finished();
        let progress = self.workflow_progress_info();
        self.frontend.report_workflow_progress(&progress);
        Ok(())
    }

    /// The Workflow Control Board for a board opened mid-group. On top of the
    /// focused-container scoping `compute_available_actions` applies, restart,
    /// back and next act on the whole group, so each is offered whenever it
    /// can do anything: restart and next always, back whenever the group has
    /// a step to return to. All three depend on follow-up questions, so an
    /// engine without their copy offers none of them.
    fn parallel_group_board_actions(
        &self,
        group: &[String],
        retryable: Option<&str>,
    ) -> Result<AvailableActions, EngineError> {
        let mut a = self.compute_available_actions()?;
        a.retry_failed_step = retryable.map(str::to_string);
        if self.parallel_group_prompts.is_none() {
            a.can_restart_current_step = false;
            a.can_cancel_to_previous_step = false;
            a.can_launch_next = false;
            return Ok(a);
        }
        let has_previous = !self.dependencies_of(group).is_empty();
        a.acts_on_parallel_group = true;
        a.can_restart_current_step = true;
        a.restart_unavailable_reason = None;
        a.can_cancel_to_previous_step = has_previous;
        a.cancel_to_previous_unavailable_reason =
            (!has_previous).then(|| "this parallel group is the first step".to_string());
        a.can_launch_next = true;
        Ok(a)
    }

    /// Put one of the Layer 2 follow-up questions to the frontend. Without
    /// the copy there is no question to ask, which answers "keep running".
    fn ask_parallel_group(
        &mut self,
        build: impl FnOnce(&ParallelGroupPrompts) -> Prompt<ParallelGroupDecision>,
    ) -> Result<ParallelGroupDecision, EngineError> {
        match &self.parallel_group_prompts {
            Some(prompts) => {
                let prompt = build(prompts);
                self.frontend.ask_parallel_group(&prompt)
            }
            None => Ok(ParallelGroupDecision::KeepRunning),
        }
    }

    /// A mid-group board chose restart, back or next — each acts on the whole
    /// group. Ask the follow-up questions and carry out the answer; any
    /// answer other than the expected one leaves the group running.
    fn resolve_parallel_group_action(
        &mut self,
        action: NextAction,
        group: &[String],
    ) -> Result<ParallelRequestOutcome, EngineError> {
        use ParallelGroupDecision as D;
        match action {
            NextAction::RestartCurrentStep => {
                match self.ask_parallel_group(|p| (p.restart_scope)(group))? {
                    D::RestartWholeGroup => {
                        self.msg_info("Restarting the whole parallel group");
                        self.rewind_parallel_group(group)?;
                        Ok(ParallelRequestOutcome::Rewound)
                    }
                    D::RestartOneAgent => {
                        let members: Vec<crate::engine::workflow::actions::GroupMember> = group
                            .iter()
                            .map(|n| (n.clone(), self.state.status_of(n).cloned()))
                            .collect();
                        match self.ask_parallel_group(|p| (p.restart_which)(&members))? {
                            D::RestartStep(name) if group.contains(&name) => {
                                self.msg_info(format!("Restarting step '{name}'"));
                                Ok(ParallelRequestOutcome::RestartStep(name))
                            }
                            _ => Ok(ParallelRequestOutcome::Continue),
                        }
                    }
                    _ => Ok(ParallelRequestOutcome::Continue),
                }
            }
            // Going back cancels the whole group: the group, the steps it
            // depends on, and anything already finished downstream of those
            // all run again.
            NextAction::CancelToPreviousStep => {
                let back_to = self.dependencies_of(group);
                if back_to.is_empty() {
                    return Ok(ParallelRequestOutcome::Continue);
                }
                let exit = GroupExit::Back(back_to.clone());
                if self.ask_parallel_group(|p| (p.cancel_group)(group, &exit))? != D::CancelGroup {
                    return Ok(ParallelRequestOutcome::Continue);
                }
                self.msg_info("Cancelling the parallel group and going back");
                let mut reset = back_to.clone();
                reset.extend(self.dependents_of(&back_to));
                reset.extend(group.iter().cloned());
                reset.sort();
                reset.dedup();
                self.rewind_parallel_group(&reset)?;
                Ok(ParallelRequestOutcome::Rewound)
            }
            // Moving on cancels the whole group: whatever has not finished is
            // skipped, and the steps after the group run.
            NextAction::LaunchNext => {
                let exit = GroupExit::Next(self.direct_dependents_of(group));
                if self.ask_parallel_group(|p| (p.cancel_group)(group, &exit))? != D::CancelGroup {
                    return Ok(ParallelRequestOutcome::Continue);
                }
                self.msg_info("Cancelling the parallel group and moving on");
                self.skip_parallel_group(group)?;
                Ok(ParallelRequestOutcome::Rewound)
            }
            _ => Ok(ParallelRequestOutcome::Continue),
        }
    }

    /// Route an `EngineRequest` received while a parallel group is running.
    ///
    /// `group` is every step of the running group. `retryable` names a peer
    /// that already failed in it; a board opened now offers to relaunch it
    /// (`NextAction::RetryFailedStep`) instead of leaving it until the whole
    /// group drains.
    pub(super) fn handle_parallel_engine_request(
        &mut self,
        req: EngineRequest,
        group: &[String],
        retryable: Option<&str>,
    ) -> Result<ParallelRequestOutcome, EngineError> {
        match req {
            EngineRequest::StepStuck { step_name } => {
                self.handle_parallel_stuck_event(&step_name, StuckEvent::Stuck);
                Ok(ParallelRequestOutcome::Continue)
            }
            EngineRequest::StepUnstuck { step_name } => {
                self.handle_parallel_stuck_event(&step_name, StuckEvent::Unstuck);
                Ok(ParallelRequestOutcome::Continue)
            }
            EngineRequest::OpenControlBoard { step_name } => {
                // Scope the board to the focused container; peers keep running.
                self.focus_parallel_step(&step_name);
                let available = self.parallel_group_board_actions(group, retryable)?;
                let action = self
                    .frontend
                    .show_workflow_control_board(&self.state, &available)?;
                match action {
                    NextAction::Pause => {
                        self.log_wcb_action(&action);
                        Ok(ParallelRequestOutcome::Ended(self.pause_parallel_group()?))
                    }
                    NextAction::Abort => {
                        self.log_wcb_action(&action);
                        Ok(ParallelRequestOutcome::Ended(self.abort_parallel_group()?))
                    }
                    NextAction::RetryFailedStep { ref step_name }
                        if retryable == Some(step_name.as_str()) =>
                    {
                        self.log_wcb_action(&action);
                        Ok(ParallelRequestOutcome::RestartStep(step_name.clone()))
                    }
                    // Narrated once the follow-up questions are answered.
                    NextAction::RestartCurrentStep
                    | NextAction::CancelToPreviousStep
                    | NextAction::LaunchNext
                        if available.acts_on_parallel_group =>
                    {
                        self.resolve_parallel_group_action(action, group)
                    }
                    // Finish / continue are scoped away while the group runs;
                    // treat anything else as a dismiss — the group keeps
                    // running undisturbed.
                    _ => Ok(ParallelRequestOutcome::Continue),
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
