//! Parallel groups (WI-0096): launching N containers at once, per-slot yolo
//! countdowns, and aborting or pausing the group as a whole.
//!
//! Split out of `workflow/mod.rs` by WI 0114 F-51. A child module of
//! `workflow`, so it reaches `WorkflowEngine`'s private fields exactly as the
//! code did before the move; methods it defines are `pub(super)` so the
//! other halves of the engine can still call them.

use super::*;

impl WorkflowEngine {
    pub(super) fn is_final_yolo_group(&self, ready: &[WorkflowStep]) -> bool {
        self.yolo
            && self.frontend.supports_interactive_recovery()
            && self.workflow.steps.iter().all(|step| {
                self.state.completed_steps.contains(&step.name)
                    || ready.iter().any(|candidate| candidate.name == step.name)
            })
    }

    /// Run a whole parallel group to completion. Launches up to
    /// `max_concurrent` of `ready` (source-file order), except for an
    /// interactive final yolo group, and queues the rest. Then it
    /// drives all live containers concurrently through a `FuturesUnordered`
    /// select loop — launching queued steps as slots free up, running per-step
    /// stuck detection and per-step yolo countdowns independently, and honoring
    /// `abort_on_failure` / WCB pause+abort mid-group.
    pub(super) async fn run_parallel_group(
        &mut self,
        ready: Vec<WorkflowStep>,
    ) -> Result<GroupOutcome, EngineError> {
        // Only a group containing every remaining step is a final review
        // stage. Earlier groups still free their slots on yolo expiry.
        let final_yolo_group = self.is_final_yolo_group(&ready);
        let group_names: Vec<String> = ready.iter().map(|s| s.name.clone()).collect();
        self.frontend.report_parallel_group_started(&group_names);
        // Final review needs every remaining agent visible and alive together.
        // The normal cap still governs every non-final group.
        let slot_cap = if final_yolo_group {
            ready.len().max(1)
        } else {
            match self.max_concurrent {
                Some(n) => n.max(1),
                None => ready.len().max(1),
            }
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

        let total = timing::yolo_countdown_duration();
        let mut failed: Vec<(String, i32)> = Vec::new();
        let mut review_ready = HashSet::new();
        // Final-group steps an `AgentDone` countdown advanced (WI 0141): the
        // agent said it was done, so they need no final review.
        let mut done_advanced: HashSet<String> = HashSet::new();
        let mut final_review_shown = false;
        // Cloned so the select below borrows nothing of `self` for it.
        let cancel = self.cancel.clone();

        while !self.active_steps.is_empty()
            || (final_yolo_group
                && failed.is_empty()
                && !group_names.iter().all(|n| done_advanced.contains(n)))
        {
            // A final group stays alive after its countdowns. Once every live
            // member has reached review readiness (or exited), ask once; Esc
            // returns to the live containers, and Ctrl-W can reopen the board.
            if final_yolo_group
                && !final_review_shown
                && failed.is_empty()
                && queue.is_empty()
                && self
                    .active_steps
                    .iter()
                    .all(|slot| review_ready.contains(&slot.step_name))
            {
                final_review_shown = true;
                let step_name = self
                    .active_steps
                    .first()
                    .map(|slot| slot.step_name.clone())
                    .or_else(|| group_names.last().cloned())
                    .unwrap_or_default();
                match self.handle_parallel_engine_request(
                    EngineRequest::OpenControlBoard { step_name },
                    &group_names,
                    None,
                    true,
                )? {
                    ParallelRequestOutcome::Continue => {}
                    ParallelRequestOutcome::Ended(wo) => return Ok(GroupOutcome::Ended(wo)),
                    ParallelRequestOutcome::Rewound => return Ok(GroupOutcome::Rewound),
                    ParallelRequestOutcome::RestartStep(name) => {
                        review_ready.remove(&name);
                        done_advanced.remove(&name);
                        self.restart_parallel_step(
                            &name, &mut waits, &mut queue, slot_cap, &stuck_tx,
                        )?;
                        final_review_shown = false;
                    }
                }
            }
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
                    // Cancellation also wins over a runtime wait error, while
                    // every slot still owns the handle needed to stop it.
                    if self.cancel_requested() {
                        return Ok(GroupOutcome::Ended(self.cancel_parallel_group()?));
                    }
                    let exit = result?;
                    // Persist the buffered output on a genuine failure before the
                    // slot (which owns the tail + container name) is removed.
                    self.maybe_dump_step_failure(&name, exit.exit_code);
                    // A failure that raced a cancel belongs to the cancelled
                    // run: no auth relaunch, no abort policy (which would
                    // suppress teardown). The slot is still active, so the
                    // group cancel stops and removes it with the others.
                    if exit.exit_code != 0 && self.cancel_requested() {
                        return Ok(GroupOutcome::Ended(self.cancel_parallel_group()?));
                    }
                    // WI 0141: the agent's reported problem outranks the exit
                    // code (an awman kill after the countdown, or a self-exit
                    // even with 0). It is a real failure, never an auth retry.
                    // A report still queued or debounced at exit counts: settle
                    // from the execution's tracker first.
                    self.settle_reported_status(&name, None);
                    let problem = self.reported_problem_of(&name);
                    let auth_recovered = problem.is_none()
                        && exit.exit_code != 0
                        && self.recover_auth_failure(&name)?;
                    self.remove_active_step(&name);
                    review_ready.remove(&name);
                    done_advanced.remove(&name);
                    self.last_exit_info = Some(exit.clone());

                    let (status, step_state) = if let Some(problem) = &problem {
                        (
                            WorkflowStepStatus::Failed { exit_code: exit.exit_code },
                            StepState::Failed {
                                exit_code: exit.exit_code,
                                error_message: Some(program_status::reported_failure_reason(problem)),
                            },
                        )
                    } else if exit.exit_code == 0 {
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

                    // Never launch a queued step once a cancel arrived.
                    if self.cancel_requested() {
                        return Ok(GroupOutcome::Ended(self.cancel_parallel_group()?));
                    }

                    if auth_recovered {
                        final_review_shown = false;
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
                        if final_yolo_group {
                            // Final agents may already be held alive for
                            // review, so waiting for the group to drain would
                            // strand recovery. Offer the failed peer now.
                            match self.handle_parallel_engine_request(
                                EngineRequest::OpenControlBoard {
                                    step_name: name.clone(),
                                },
                                &group_names,
                                Some(&name),
                                false,
                            )? {
                                ParallelRequestOutcome::Continue => {}
                                ParallelRequestOutcome::Ended(wo) => {
                                    return Ok(GroupOutcome::Ended(wo));
                                }
                                ParallelRequestOutcome::Rewound => {
                                    return Ok(GroupOutcome::Rewound);
                                }
                                ParallelRequestOutcome::RestartStep(restart) => {
                                    failed.retain(|(failed_name, _)| *failed_name != restart);
                                    review_ready.remove(&restart);
                                    done_advanced.remove(&restart);
                                    final_review_shown = false;
                                    self.restart_parallel_step(
                                        &restart,
                                        &mut waits,
                                        &mut queue,
                                        slot_cap,
                                        &stuck_tx,
                                    )?;
                                }
                            }
                        }
                    } else if failed.is_empty() && self.active_steps.len() < slot_cap {
                        // An unresolved failure freezes the queue (docs/05-workflows.md):
                        // peers keep running, but nothing new starts until it is retried.
                        if let Some(next) = queue.pop_front() {
                            self.launch_parallel_step(next, &mut waits, &stuck_tx, true)?;
                        }
                    }
                }
                // After `waits` (the loop is biased): a container that already
                // exited is recorded first.
                _ = cancel.cancelled() => {
                    return Ok(GroupOutcome::Ended(self.cancel_parallel_group()?));
                }
                Some((name, event)) = stuck_rx.recv() => {
                    // A non-yolo report (`done`, `blocked`, `error`) opens the
                    // board for that step at once; its report rides along in
                    // `AvailableActions::reported_status`.
                    if self.handle_parallel_stuck_event(&name, event) {
                        let retryable = failed.first().map(|(name, _)| name.clone());
                        match self.handle_parallel_engine_request(
                            EngineRequest::OpenControlBoard { step_name: name },
                            &group_names,
                            retryable.as_deref(),
                            final_yolo_group && final_review_shown && failed.is_empty() && queue.is_empty(),
                        )? {
                            ParallelRequestOutcome::Continue => {}
                            ParallelRequestOutcome::Ended(wo) => return Ok(GroupOutcome::Ended(wo)),
                            ParallelRequestOutcome::Rewound => return Ok(GroupOutcome::Rewound),
                            ParallelRequestOutcome::RestartStep(name) => {
                                failed.retain(|(n, _)| *n != name);
                                review_ready.remove(&name);
                                done_advanced.remove(&name);
                                final_review_shown = false;
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
                }
                Some(req) = Self::recv_engine(&mut self.engine_rx) => {
                    // The earliest failure still awaiting recovery is offered
                    // as a retry on any board opened while the group runs.
                    let retryable = failed.first().map(|(name, _)| name.clone());
                    match self.handle_parallel_engine_request(
                        req,
                        &group_names,
                        retryable.as_deref(),
                        final_yolo_group && final_review_shown && failed.is_empty() && queue.is_empty(),
                    )? {
                        ParallelRequestOutcome::Continue => {}
                        ParallelRequestOutcome::Ended(wo) => return Ok(GroupOutcome::Ended(wo)),
                        ParallelRequestOutcome::Rewound => return Ok(GroupOutcome::Rewound),
                        ParallelRequestOutcome::RestartStep(name) => {
                            failed.retain(|(n, _)| *n != name);
                            review_ready.remove(&name);
                            done_advanced.remove(&name);
                            final_review_shown = false;
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
                    let launch_queued = failed.is_empty();
                    let final_batch = final_yolo_group && queue.is_empty();
                    self.tick_parallel_yolo(
                        &mut waits,
                        &mut queue,
                        slot_cap,
                        launch_queued,
                        total,
                        &stuck_tx,
                        final_batch,
                        &mut review_ready,
                        &mut done_advanced,
                    )?;
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
                // A launch error exits the group. Stop its owned siblings
                // before their wait futures are dropped, including runtimes
                // whose execution Drop deliberately leaves containers alive.
                let live: Vec<String> = self
                    .active_steps
                    .iter()
                    .map(|slot| slot.step_name.clone())
                    .collect();
                self.stop_active_agents();
                for name in live {
                    self.state.set_status(&name, StepState::Pending);
                    let sibling = self.find_step(&name)?;
                    self.frontend
                        .report_step_status(&sibling, WorkflowStepStatus::Pending);
                    self.frontend
                        .report_parallel_step_exited(&name, KILLED_EXIT_CODE);
                }
                if let Err(save_error) = self.persist() {
                    tracing::error!(step = %step.name, error = %save_error,
                        "could not persist parallel launch failure cleanup");
                }
                return Err(error);
            }
        };
        let single_step::StepLaunch {
            execution,
            agent: resolved_agent,
            model: resolved_model,
        } = launch;

        let stuck_sender = execution.stuck_sender();
        let cancel_handle = execution.cancel_handle();
        let container_name = execution.handle().name.clone();
        let handle = execution.handle().clone();
        let output_tail = execution.output_tail();
        let program_status = execution.program_status_tracker();

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

    /// Handle a per-step stuck/unstuck transition or OSC 7501 report inside a
    /// parallel group. Independent per slot: a noisy sibling never masks a
    /// stuck step, a stuck step never blocks its siblings, and one agent's
    /// `done` or `error` never advances or fails another (WI 0141).
    ///
    /// Returns `true` when the engine should open the control board for
    /// `name` now: a non-yolo agent reported `done`/`idle`, `blocked` or
    /// `error` with a user present. The caller opens it, since only the group
    /// loop can act on the answer.
    pub(super) fn handle_parallel_stuck_event(&mut self, name: &str, event: StuckEvent) -> bool {
        match event {
            StuckEvent::Reported(status) => return self.handle_parallel_report(name, status),
            StuckEvent::Stuck => {
                if self.yolo {
                    let start_countdown = {
                        match self.active_steps.iter_mut().find(|s| s.step_name == name) {
                            Some(s) if s.yolo_deadline.is_none() => {
                                s.yolo_deadline =
                                    Some(Instant::now() + timing::yolo_countdown_duration());
                                s.countdown_kind = CountdownKind::StuckStep;
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
                        self.frontend
                            .parallel_step_countdown_started(name, CountdownKind::StuckStep);
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
                    // Output cancels a silence countdown, and an `AgentError`
                    // one ("any output"), but never an agent's `done` or
                    // `blocked` countdown: those end on a report.
                    let cancels = matches!(
                        s.countdown_kind,
                        CountdownKind::StuckStep | CountdownKind::AgentError
                    );
                    let had_countdown = cancels && s.yolo_deadline.is_some();
                    if had_countdown {
                        s.yolo_deadline = None;
                        s.output_watch = None;
                        if s.countdown_kind == CountdownKind::AgentError {
                            s.reported_problem = None;
                        }
                    }
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
        false
    }

    /// One parallel step's agent reported its status: the state table in
    /// [`program_status`], applied to this slot only.
    fn handle_parallel_report(
        &mut self,
        name: &str,
        status: Option<crate::data::program_status::EffectiveStatus>,
    ) -> bool {
        use program_status::ReportedResponse;
        if !self.active_steps.iter().any(|s| s.step_name == name) {
            return false;
        }
        self.note_reported_status(name, status.as_ref());
        let response = self.reported_response(status.as_ref());
        if let Some(s) = &status {
            self.narrate_report(name, s, response);
        }
        match response {
            // Working / cleared, or a problem that must never be pushed past:
            // whatever countdown was running — silence or `done` — stops.
            ReportedResponse::Resume
            | ReportedResponse::AwaitRepair
            | ReportedResponse::NeedsYou => {
                self.cancel_parallel_countdown(name);
                false
            }
            ReportedResponse::ConfirmAdvance | ReportedResponse::ProblemBoard => {
                self.cancel_parallel_countdown(name);
                true
            }
            ReportedResponse::Countdown(kind) => {
                let Some(slot) = self.active_steps.iter_mut().find(|s| s.step_name == name) else {
                    return false;
                };
                if slot.yolo_deadline.is_some() && slot.countdown_kind == kind {
                    return false;
                }
                let replaced = slot.yolo_deadline.is_some();
                slot.yolo_deadline =
                    Some(Instant::now() + program_status::countdown_duration(kind));
                slot.countdown_kind = kind;
                slot.output_watch = if kind == CountdownKind::AgentError {
                    slot.output_tail
                        .clone()
                        .map(program_status::OutputWatch::new)
                } else {
                    None
                };
                if replaced {
                    self.frontend.parallel_step_yolo_countdown_finished(name);
                }
                self.frontend.parallel_step_countdown_started(name, kind);
                false
            }
        }
    }

    /// Stop `name`'s per-step countdown, whatever it is for.
    fn cancel_parallel_countdown(&mut self, name: &str) {
        if let Some(slot) = self.active_steps.iter_mut().find(|s| s.step_name == name) {
            slot.output_watch = None;
            if slot.yolo_deadline.take().is_some() {
                self.frontend.parallel_step_yolo_countdown_finished(name);
            }
        }
    }

    /// A parallel agent-problem countdown expired: kill just
    /// that container and keep its slot. Its wait future then resolves and
    /// the group loop finalises the step as Failed on `reported_problem`, so
    /// the group's ordinary failure handling (abort_on_failure, recovery
    /// board, unattended retry) applies. Siblings are untouched.
    fn fail_parallel_step_for_report(&mut self, name: &str) {
        self.msg_warning(format!(
            "Step '{name}' did not recover from its reported problem; stopping its container"
        ));
        if let Some(slot) = self.active_steps.iter_mut().find(|s| s.step_name == name) {
            slot.yolo_deadline = None;
            slot.output_watch = None;
            slot.awman_killed = true;
            if let Some(ch) = &slot.cancel_handle {
                let _ = ch.cancel();
            }
        }
        self.frontend.parallel_step_yolo_countdown_finished(name);
    }

    /// Drive every in-flight per-step yolo countdown one tick. Independent per
    /// slot — expiry kills only that container. A silence or `AgentDone`
    /// expiry advances (and frees the slot for the queue); an `AgentBlocked`
    /// expiry fails that step instead (WI 0141).
    #[allow(clippy::too_many_arguments)] // The parallel scheduler passes its coordinated phase queues together.
    pub(super) fn tick_parallel_yolo(
        &mut self,
        waits: &mut ParallelWaits,
        queue: &mut VecDeque<WorkflowStep>,
        slot_cap: usize,
        launch_queued: bool,
        total: Duration,
        stuck_tx: &StuckFanIn,
        final_review: bool,
        review_ready: &mut HashSet<String>,
        done_advanced: &mut HashSet<String>,
    ) -> Result<(), EngineError> {
        let now = Instant::now();
        let ticking: Vec<(String, Instant, CountdownKind)> = self
            .active_steps
            .iter()
            .filter_map(|s| {
                s.yolo_deadline
                    .map(|d| (s.step_name.clone(), d, s.countdown_kind))
            })
            .collect();
        for (name, deadline, kind) in ticking {
            // "Any output" cancels an `AgentError` countdown: the agent
            // recovered by itself.
            if kind == CountdownKind::AgentError && self.parallel_output_arrived(&name) {
                // Output cancels the countdown only; the error stays recorded
                // until a new status report (an exit without one fails).
                self.msg_info(format!(
                    "Step '{name}' produced output after reporting an error; cancelling countdown"
                ));
                self.cancel_parallel_countdown(&name);
                continue;
            }
            // Active-tab suppression (WI 0141): an `AgentDone` countdown is
            // held at its full length while the user interacts with this
            // agent, so the countdown they see starts once they are quiet.
            if kind == CountdownKind::AgentDone && self.frontend.user_interacting_with(&name) {
                if let Some(slot) = self.active_steps.iter_mut().find(|s| s.step_name == name) {
                    slot.yolo_deadline = Some(now + program_status::countdown_duration(kind));
                }
                continue;
            }
            let remaining = deadline.saturating_duration_since(now);
            let kind_total = if kind == CountdownKind::StuckStep {
                total
            } else {
                program_status::countdown_duration(kind)
            };
            let expire = match self
                .frontend
                .parallel_step_yolo_countdown_tick(&name, remaining, kind_total)?
            {
                YoloTickOutcome::Cancel => {
                    self.cancel_parallel_countdown(&name);
                    false
                }
                YoloTickOutcome::AdvanceNow => true,
                YoloTickOutcome::Continue => remaining.is_zero(),
            };
            if !expire {
                continue;
            }
            if program_status::countdown_fails_step(kind) {
                self.fail_parallel_step_for_report(&name);
            } else if final_review && kind != CountdownKind::AgentDone {
                // Only a silent (non-reporting) final agent is held for
                // review; a reported `done` advances like any other step.
                self.mark_final_parallel_step_ready(&name, review_ready);
            } else {
                if kind == CountdownKind::AgentDone {
                    done_advanced.insert(name.clone());
                }
                self.yolo_advance_parallel(&name, waits, queue, slot_cap, launch_queued, stuck_tx)?;
            }
        }
        Ok(())
    }

    /// Whether `name`'s `AgentError` countdown has seen new output.
    fn parallel_output_arrived(&mut self, name: &str) -> bool {
        self.active_steps
            .iter_mut()
            .find(|s| s.step_name == name)
            .and_then(|s| s.output_watch.as_mut())
            .is_some_and(program_status::OutputWatch::produced_output)
    }

    fn mark_final_parallel_step_ready(&mut self, name: &str, ready: &mut HashSet<String>) {
        if let Some(slot) = self
            .active_steps
            .iter_mut()
            .find(|slot| slot.step_name == name)
        {
            slot.yolo_deadline = None;
            ready.insert(name.to_string());
            self.frontend.parallel_step_yolo_countdown_finished(name);
            self.msg_info(format!("Final step '{name}' is ready for review"));
        }
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
        launch_queued: bool,
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
        if launch_queued && self.active_steps.len() < slot_cap {
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

    /// An external cancel arrived while a parallel group runs: kill and stop
    /// every live container, close the group, and end the run as
    /// [`WorkflowOutcome::Cancelled`]. Unlike [`Self::abort_parallel_group`] it
    /// does not set `abort_on_failure_triggered`, so cancelled teardown runs.
    pub(super) fn cancel_parallel_group(&mut self) -> Result<WorkflowOutcome, EngineError> {
        let live: Vec<String> = self
            .active_steps
            .iter()
            .map(|s| s.step_name.clone())
            .collect();
        self.stop_active_agents();
        for name in &live {
            self.frontend
                .report_parallel_step_exited(name, KILLED_EXIT_CODE);
        }
        self.frontend.report_parallel_group_finished();
        self.cancel_run()
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

    /// The sole unfinished member can be advanced as a successful group,
    /// including while its yolo countdown is still running.
    fn can_complete_parallel_group(&self, group: &[String]) -> bool {
        let [active] = self.active_steps.as_slice() else {
            return false;
        };
        group.iter().all(|name| {
            name == &active.step_name
                || matches!(self.state.status_of(name), Some(StepState::Succeeded))
        })
    }

    fn complete_parallel_group(&mut self, group: &[String]) -> Result<(), EngineError> {
        // The next stage must not launch until the last container is stopped.
        // A cancel handle may only signal its wait task, so also use the
        // factory's synchronous stop operation before recording success.
        if let Some(slot) = self.active_steps.first() {
            if let Some(handle) = &slot.cancel_handle {
                let _ = handle.cancel();
            }
            self.agent_factory.stop_agent(&slot.handle)?;
        }
        let active = self.kill_active_parallel_steps();
        for name in active {
            self.state.set_status(&name, StepState::Succeeded);
            let step = self.find_step(&name)?;
            self.frontend
                .report_step_status(&step, WorkflowStepStatus::Succeeded);
        }
        self.persist()?;
        self.frontend.report_parallel_group_finished();
        let progress = self.workflow_progress_info();
        self.frontend.report_workflow_progress(&progress);
        debug_assert!(group
            .iter()
            .all(|name| self.state.completed_steps.contains(name)));
        Ok(())
    }

    /// The Workflow Control Board for a board opened mid-group. On top of the
    /// focused-container scoping `compute_available_actions` applies, restart,
    /// back and next act on the whole group, so each is offered whenever it
    /// can do anything: restart and next always, back whenever the group has
    /// a step to return to. Restart, back, and cancelling a group need
    /// follow-up questions; completing its sole remaining member does not.
    fn parallel_group_board_actions(
        &self,
        group: &[String],
        retryable: Option<&str>,
    ) -> Result<AvailableActions, EngineError> {
        let mut a = self.compute_available_actions()?;
        a.retry_failed_step = retryable.map(str::to_string);
        a.can_finish_workflow = false;
        a.finish_workflow_unavailable_reason =
            Some("Finish after the parallel group is ready for review.".into());
        let can_complete = self.can_complete_parallel_group(group);
        if can_complete {
            a.launch_next_label = Some("Next (complete current group)".into());
        }
        if self.parallel_group_prompts.is_none() {
            a.can_restart_current_step = false;
            a.can_cancel_to_previous_step = false;
            a.can_launch_next = can_complete;
            a.acts_on_parallel_group = can_complete;
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
    /// group. Ask the relevant follow-up questions, except when Next simply
    /// completes the sole remaining member.
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
            // Moving on completes the last active member when its peers have
            // succeeded. Otherwise it cancels and skips unfinished members.
            NextAction::LaunchNext => {
                if self.can_complete_parallel_group(group) {
                    self.msg_info("Completing the parallel group and moving on");
                    self.complete_parallel_group(group)?;
                    return Ok(ParallelRequestOutcome::Rewound);
                }
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
        final_review: bool,
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
                let mut available = self.parallel_group_board_actions(group, retryable)?;
                if final_review {
                    available.acts_on_parallel_group = true;
                    available.can_finish_workflow = true;
                    available.finish_workflow_unavailable_reason = None;
                    available.can_launch_next = false;
                    available.can_dismiss = !self.active_steps.is_empty();
                    available.simple_advance = None;
                }
                let action = self
                    .frontend
                    .show_workflow_control_board(&self.state, &available)?;
                match action {
                    NextAction::FinishWorkflow if final_review => {
                        self.log_wcb_action(&action);
                        Ok(ParallelRequestOutcome::Ended(
                            self.finish_final_parallel_group(group)?,
                        ))
                    }
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

    /// The review decision closes every surviving final-stage container
    /// before the caller can enter teardown.
    fn finish_final_parallel_group(
        &mut self,
        group: &[String],
    ) -> Result<WorkflowOutcome, EngineError> {
        let live: Vec<ActiveParallelStep> = self.active_steps.drain(..).collect();
        let mut stop_error = None;
        for slot in &live {
            if let Some(handle) = &slot.cancel_handle {
                let _ = handle.cancel();
            }
            if slot.yolo_deadline.is_some() {
                self.frontend
                    .parallel_step_yolo_countdown_finished(&slot.step_name);
            }
            if let Err(error) = self.agent_factory.stop_agent(&slot.handle) {
                tracing::error!(step = %slot.step_name, error = %error,
                    "final workflow agent could not be stopped");
                stop_error.get_or_insert(error);
            } else {
                self.frontend
                    .report_parallel_step_exited(&slot.step_name, KILLED_EXIT_CODE);
            }
        }
        if let Some(error) = stop_error {
            // Teardown may publish the worktree, so a failed stop cannot be
            // treated as successful completion.
            self.abort_on_failure_triggered = true;
            self.frontend.report_parallel_group_finished();
            return Err(error);
        }
        for name in group {
            if matches!(self.state.status_of(name), Some(StepState::Running { .. })) {
                self.state.set_status(name, StepState::Succeeded);
                let step = self.find_step(name)?;
                self.frontend
                    .report_step_status(&step, WorkflowStepStatus::Succeeded);
            }
        }
        self.persist()?;
        self.frontend.report_parallel_group_finished();
        let progress = self.workflow_progress_info();
        self.frontend.report_workflow_progress(&progress);
        self.handle_finish_workflow()
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
