//! What a workflow does when a step's agent reports its own status over
//! OSC 7501 (WI 0141).
//!
//! The container bridges parse the reports and the `StuckDetector` turns
//! them into [`StuckEvent::Reported`] events; silence `Stuck` is never sent
//! for an agent that reports. This module is the single place that decides
//! what each reported state means for a step:
//!
//! | state | yolo | non-yolo (user present) | no user |
//! |---|---|---|---|
//! | `working` / cleared | cancel countdown | nothing (frontend may close an auto-opened board) | cancel countdown |
//! | `done`, `idle` | [`CountdownKind::AgentDone`] → kill + advance | lightweight step confirm | as yolo |
//! | `blocked` | pause, "needs you" alert, no countdown | control board | [`CountdownKind::AgentBlocked`] → kill + fail |
//! | `error` | [`CountdownKind::AgentError`] → kill + fail | control board | as yolo |
//!
//! A problem (`blocked`, `error`) never advances a step in any mode. A step
//! killed after a problem report, or one that exits by itself after one, is
//! finalised as Failed whatever its exit code ([`super::single_step`]'s
//! `finalize_step`), so the ordinary failure paths (`on_failure`, the
//! unattended one-retry `FailureRetry` countdown, the recovery board) apply.
//!
//! [`respond_to_report`] is pure and shared with the squad launcher and the
//! dynamic-workflow leader, so all three apply the same table.
//!
//! [`StuckEvent::Reported`]: crate::engine::agent_runtime::execution::StuckEvent::Reported

use std::sync::Arc;
use std::time::{Duration, Instant};

use super::*;
use crate::data::program_status::{EffectiveStatus, ProgramState};

/// What the engine does with one reported status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportedResponse {
    /// `working`, or the agent cleared its records: cancel any running agent
    /// countdown and forget a recorded problem. The step keeps running.
    Resume,
    /// Start this countdown. [`CountdownKind::AgentDone`] expiry kills the
    /// container and advances; `AgentBlocked` / `AgentError` expiry kills it
    /// and fails the step.
    Countdown(CountdownKind),
    /// Non-yolo with a user: open the lightweight "advance to the next step?"
    /// confirmation, as if the step had completed.
    ConfirmAdvance,
    /// Non-yolo with a user: open the Workflow Control Board now, carrying
    /// the agent's kind and message. Advancing stays an explicit choice.
    ProblemBoard,
    /// Yolo with a user, agent `blocked`: pause — no countdown, no advance —
    /// and alert the user that the agent needs them.
    NeedsYou,
}

/// The state table above. `status == None` means the agent cleared every
/// record. `yolo` is `--yolo`; `attended` is
/// [`WorkflowFrontend::supports_interactive_recovery`] (a human can answer).
pub fn respond_to_report(
    status: Option<&EffectiveStatus>,
    yolo: bool,
    attended: bool,
) -> ReportedResponse {
    let Some(status) = status else {
        return ReportedResponse::Resume;
    };
    match status.state {
        ProgramState::Working => ReportedResponse::Resume,
        ProgramState::Done | ProgramState::Idle => {
            if yolo || !attended {
                ReportedResponse::Countdown(CountdownKind::AgentDone)
            } else {
                ReportedResponse::ConfirmAdvance
            }
        }
        ProgramState::Blocked => {
            if !attended {
                ReportedResponse::Countdown(CountdownKind::AgentBlocked)
            } else if yolo {
                ReportedResponse::NeedsYou
            } else {
                ReportedResponse::ProblemBoard
            }
        }
        ProgramState::Error => {
            if yolo || !attended {
                ReportedResponse::Countdown(CountdownKind::AgentError)
            } else {
                ReportedResponse::ProblemBoard
            }
        }
    }
}

/// Every `Reported` status still queued on `rx`, oldest first. Other events
/// are dropped: the container has exited, so silence no longer matters.
pub(crate) fn drain_pending_reports(
    rx: &mut tokio::sync::broadcast::Receiver<StuckEvent>,
) -> Vec<Option<EffectiveStatus>> {
    use tokio::sync::broadcast::error::TryRecvError;
    let mut reports = Vec::new();
    loop {
        match rx.try_recv() {
            Ok(StuckEvent::Reported(status)) => reports.push(status),
            Ok(_) | Err(TryRecvError::Lagged(_)) => {}
            Err(TryRecvError::Empty) | Err(TryRecvError::Closed) => return reports,
        }
    }
}

/// The tracker's effective status at exit, when the agent reported at all
/// this execution (`None` = never reported: nothing to settle).
pub(crate) fn final_tracker_status(
    tracker: &crate::engine::agent_runtime::program_status::SharedProgramStatusTracker,
) -> Option<Option<EffectiveStatus>> {
    tracker.lock().ok()?.exit_status()
}

/// Whether `status` is a problem the step must never advance past.
pub fn is_reported_problem(status: Option<&EffectiveStatus>) -> bool {
    status.is_some_and(|s| s.state.is_problem())
}

/// How long a countdown of `kind` runs: [`timing::agent_done_countdown_duration`]
/// for `AgentDone`, the normal yolo duration for everything else.
pub fn countdown_duration(kind: CountdownKind) -> Duration {
    match kind {
        CountdownKind::AgentDone => timing::agent_done_countdown_duration(),
        CountdownKind::StuckStep
        | CountdownKind::FailureRetry
        | CountdownKind::AgentBlocked
        | CountdownKind::AgentError => timing::yolo_countdown_duration(),
    }
}

/// Whether expiry of a countdown of `kind` fails the step (rather than
/// advancing it or retrying it).
pub fn countdown_fails_step(kind: CountdownKind) -> bool {
    matches!(
        kind,
        CountdownKind::AgentBlocked | CountdownKind::AgentError
    )
}

/// Longest agent message the engine quotes in a status line. The text is
/// agent-controlled; frontends truncate further to fit.
const MAX_QUOTED_MSG_CHARS: usize = 200;

/// A short, single-line description of a reported status for engine
/// messages and `StepState::Failed::error_message`: `"error: build failed"`,
/// `"blocked (permission): allow rm?"`. The agent's `msg` has already had
/// control and invisible characters rejected or stripped by the parser; it
/// is only ever quoted, never interpreted.
pub fn describe_status(status: &EffectiveStatus) -> String {
    let mut out = status.state.as_str().to_string();
    if let Some(kind) = status.kind {
        out.push_str(&format!(" ({})", kind.as_str()));
    }
    if let Some(msg) = status
        .msg
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty())
    {
        let quoted: String = msg.chars().take(MAX_QUOTED_MSG_CHARS).collect();
        let ellipsis = if msg.chars().count() > MAX_QUOTED_MSG_CHARS {
            "…"
        } else {
            ""
        };
        out.push_str(&format!(": {quoted}{ellipsis}"));
    }
    out
}

/// The reason recorded on a step failed because of a reported problem.
pub fn reported_failure_reason(status: &EffectiveStatus) -> String {
    format!("agent reported {}", describe_status(status))
}

/// How long after an `AgentError` countdown starts the engine waits before
/// treating new output as the agent recovering. The agent usually prints its
/// error text right next to the report; that must not count as recovery.
const OUTPUT_SETTLE: Duration = Duration::from_secs(1);

/// Watches a step's output tail during an `AgentError` countdown: "any output
/// in the meantime cancels it". A reporting agent never gets silence
/// `Unstuck` events, so output is observed through the tail the bridge feeds.
pub(super) struct OutputWatch {
    tail: Arc<OutputTail>,
    baseline: String,
    clock: crate::data::clock::SharedClock,
    started: Duration,
}

impl OutputWatch {
    pub(super) fn new(tail: Arc<OutputTail>, clock: crate::data::clock::SharedClock) -> Self {
        let baseline = tail.snapshot_text();
        let started = clock.monotonic();
        Self {
            tail,
            baseline,
            clock,
            started,
        }
    }

    /// Whether the agent has produced output since the watch began (after the
    /// settle window; output during it moves the baseline instead).
    pub(super) fn produced_output(&mut self) -> bool {
        let now = self.tail.snapshot_text();
        if now == self.baseline {
            return false;
        }
        if self.clock.monotonic().saturating_sub(self.started) < OUTPUT_SETTLE {
            self.baseline = now;
            return false;
        }
        true
    }
}

/// How an agent countdown ([`WorkflowEngine::drive_agent_countdown`]) ended.
pub(super) enum AgentCountdownEnd<T> {
    /// The container exited first; its wait result, untouched. An exit always
    /// beats a pending countdown, so a `done` step never advances twice.
    Exited(T),
    /// The countdown ran out, or the user chose to act now.
    Expired,
    /// The user cancelled the countdown (Esc), or an external cancel arrived.
    Cancelled,
    /// The user pressed Ctrl-W: show the control board instead.
    ShowControlBoard,
    /// Output arrived during an `AgentError` countdown: the agent recovered.
    Resumed,
    /// A new report that calls for a different response (`working`, a clear,
    /// a problem during `done`, ...). The caller handles it as a fresh report.
    Superseded(Option<EffectiveStatus>),
}

impl WorkflowEngine {
    /// The response this engine gives to `status`.
    pub(super) fn reported_response(&self, status: Option<&EffectiveStatus>) -> ReportedResponse {
        respond_to_report(
            status,
            self.yolo,
            self.frontend.supports_interactive_recovery(),
        )
    }

    /// Record a report on the step's slot and tell the frontend. A problem is
    /// kept as the slot's `reported_problem` (the finaliser fails the step on
    /// it); anything else clears it.
    pub(super) fn note_reported_status(
        &mut self,
        step_name: &str,
        status: Option<&EffectiveStatus>,
    ) {
        if let Some(slot) = self
            .active_steps
            .iter_mut()
            .find(|s| s.step_name == step_name)
        {
            slot.last_report = status.cloned();
            slot.reported_problem = status.filter(|s| s.state.is_problem()).cloned();
        }
        self.frontend.report_agent_status(step_name, status);
    }

    /// Settle a step's final reported status as its container exits, before
    /// it is finalised. Reports still queued on `pending` (the exit and the
    /// report were ready together; the biased select took the exit) are
    /// applied in order, then the execution's own tracker — authoritative,
    /// it also holds a report the detector had not published yet. Only a
    /// change is recorded; no countdown starts for an exited process.
    pub(super) fn settle_reported_status(
        &mut self,
        step_name: &str,
        pending: Option<&mut tokio::sync::broadcast::Receiver<StuckEvent>>,
    ) {
        for status in pending.map(drain_pending_reports).unwrap_or_default() {
            self.note_if_changed(step_name, status);
        }
        let snapshot = self
            .active_steps
            .iter()
            .find(|s| s.step_name == step_name)
            .and_then(|s| s.program_status.as_ref())
            .and_then(final_tracker_status);
        if let Some(status) = snapshot {
            self.note_if_changed(step_name, status);
        }
    }

    /// `note_reported_status` unless `status` is already the slot's last
    /// report.
    fn note_if_changed(&mut self, step_name: &str, status: Option<EffectiveStatus>) {
        let unchanged = self
            .active_steps
            .iter()
            .find(|s| s.step_name == step_name)
            .is_some_and(|s| s.last_report == status);
        if !unchanged {
            self.note_reported_status(step_name, status.as_ref());
        }
    }

    /// The problem recorded on `step_name`'s slot, if any.
    pub(super) fn reported_problem_of(&self, step_name: &str) -> Option<EffectiveStatus> {
        self.active_steps
            .iter()
            .find(|s| s.step_name == step_name)
            .and_then(|s| s.reported_problem.clone())
    }

    /// The engine's one-line narration when a report arrives.
    pub(super) fn narrate_report(
        &mut self,
        step_name: &str,
        status: &EffectiveStatus,
        response: ReportedResponse,
    ) {
        let what = describe_status(status);
        match response {
            ReportedResponse::Resume => {}
            ReportedResponse::NeedsYou => {
                let mut text = format!(
                    "Step '{step_name}' needs you: agent reported {what}. The workflow is paused \
                     until you answer in the container or use the control board (Ctrl-W)"
                );
                if status.kind == Some(crate::data::program_status::BlockedKind::Permission) {
                    text.push_str("; yolo should have skipped permission prompts");
                }
                self.msg_warning(text);
            }
            ReportedResponse::ConfirmAdvance => {
                self.msg_info(format!("Step '{step_name}': agent reported {what}"));
            }
            ReportedResponse::ProblemBoard => {
                self.msg_warning(format!("Step '{step_name}': agent reported {what}"));
            }
            ReportedResponse::Countdown(kind) => {
                let secs = countdown_duration(kind).as_secs();
                let text = match kind {
                    CountdownKind::AgentDone => {
                        format!("Step '{step_name}': agent reported {what}; advancing in {secs}s")
                    }
                    _ => format!(
                        "Step '{step_name}': agent reported {what}; failing step in {secs}s"
                    ),
                };
                if countdown_fails_step(kind) {
                    self.msg_warning(text);
                } else {
                    self.msg_info(text);
                }
            }
        }
    }

    /// Board actions for a board opened because of a report: the ordinary
    /// mid-step board, carrying the report. For `done`/`idle`
    /// (`ConfirmAdvance`) it is also marked as the lightweight confirm when a
    /// next step exists; on the last step the full board offers Finish.
    pub(super) fn reported_board_actions(
        &self,
        step_name: &str,
        status: Option<&EffectiveStatus>,
        response: ReportedResponse,
    ) -> Result<AvailableActions, EngineError> {
        let mut available = self.compute_available_actions()?;
        available.reported_status = status.cloned();
        available.simple_advance = None;
        if response == ReportedResponse::ConfirmAdvance && available.can_launch_next {
            let failed = self
                .state
                .step_states
                .values()
                .any(|s| matches!(s, StepState::Failed { .. }));
            if !failed {
                if let Some(next) = self.next_step_after(step_name)? {
                    available.simple_advance = Some(SimpleAdvance {
                        completed_step: step_name.to_string(),
                        next_step: next,
                    });
                }
            }
        }
        Ok(available)
    }

    /// The step that becomes ready once `step_name` succeeds.
    fn next_step_after(&self, step_name: &str) -> Result<Option<String>, EngineError> {
        let mut completed = self.state.completed_steps.clone();
        completed.insert(step_name.to_string());
        Ok(self.dag.ready_steps(&completed).into_iter().next())
    }

    /// A step's agent reported a status while the single-step loop waits on
    /// it. Applies the table in the module docs. Returns `None` to keep
    /// waiting on the step, or the result to return from
    /// `step_once_interruptible`.
    pub(super) async fn handle_step_reported(
        &mut self,
        step_name: &str,
        status: Option<EffectiveStatus>,
        cancel_handle: &Option<CancelHandle>,
        wait_rx: &mut tokio::sync::oneshot::Receiver<(
            AgentExecution,
            Result<AgentExitInfo, EngineError>,
        )>,
        stuck_rx: &mut Option<tokio::sync::broadcast::Receiver<StuckEvent>>,
    ) -> Result<Option<InterruptibleStepResult>, EngineError> {
        let mut status = status;
        loop {
            self.note_reported_status(step_name, status.as_ref());
            let response = self.reported_response(status.as_ref());
            if let Some(s) = &status {
                self.narrate_report(step_name, s, response);
            }
            match response {
                ReportedResponse::Resume | ReportedResponse::NeedsYou => return Ok(None),
                ReportedResponse::ConfirmAdvance | ReportedResponse::ProblemBoard => {
                    let available =
                        self.reported_board_actions(step_name, status.as_ref(), response)?;
                    let mid = self.handle_mid_step_board_with(
                        step_name,
                        cancel_handle,
                        wait_rx,
                        available,
                    )?;
                    return Ok(mid_step_result(mid));
                }
                ReportedResponse::Countdown(kind) => {
                    let tail = self
                        .active_steps
                        .first()
                        .and_then(|s| s.output_tail.clone());
                    match self
                        .drive_agent_countdown(step_name, kind, wait_rx, stuck_rx, tail)
                        .await?
                    {
                        AgentCountdownEnd::Exited((exec_back, exit_result)) => {
                            self.settle_reported_status(step_name, stuck_rx.as_mut());
                            self.set_focused_execution(exec_back);
                            return Ok(Some(InterruptibleStepResult::StepCompleted(
                                self.finalize_step(step_name, exit_result?)?,
                            )));
                        }
                        AgentCountdownEnd::Expired => {
                            return self
                                .agent_countdown_expired(step_name, kind, cancel_handle, wait_rx)
                                .await
                                .map(Some);
                        }
                        AgentCountdownEnd::Cancelled => return Ok(None),
                        AgentCountdownEnd::Resumed => {
                            // Output cancels the countdown only: the error stays
                            // recorded until the agent reports a new status, so
                            // an exit without one still fails the step.
                            self.msg_info(format!(
                                "Step '{step_name}' produced output after reporting an error; \
                                 cancelling countdown"
                            ));
                            return Ok(None);
                        }
                        AgentCountdownEnd::ShowControlBoard => {
                            let mid = self.handle_mid_step_control_board(
                                step_name,
                                cancel_handle,
                                wait_rx,
                            )?;
                            return Ok(mid_step_result(mid));
                        }
                        AgentCountdownEnd::Superseded(next) => status = next,
                    }
                }
            }
        }
    }

    /// An agent countdown expired on the single-step path. `AgentDone` kills
    /// the container and advances — on the last step that completes the
    /// workflow (no `is_last_step()` review, unlike the silence path). A
    /// problem countdown kills the container and waits for it, so
    /// `finalize_step` records the step as Failed with the agent's reason.
    async fn agent_countdown_expired(
        &mut self,
        step_name: &str,
        kind: CountdownKind,
        cancel_handle: &Option<CancelHandle>,
        wait_rx: &mut tokio::sync::oneshot::Receiver<(
            AgentExecution,
            Result<AgentExitInfo, EngineError>,
        )>,
    ) -> Result<InterruptibleStepResult, EngineError> {
        if !countdown_fails_step(kind) {
            self.msg_info(format!(
                "Agent reported done; advancing past step '{step_name}'"
            ));
            self.kill_current_container(cancel_handle);
            self.state.set_status(step_name, StepState::Succeeded);
            self.persist()?;
            let step = self.find_step(step_name)?;
            self.frontend
                .report_step_status(&step, WorkflowStepStatus::Succeeded);
            let progress = self.workflow_progress_info();
            self.frontend.report_workflow_progress(&progress);
            return Ok(InterruptibleStepResult::LoopContinue);
        }

        self.msg_warning(format!(
            "Step '{step_name}' did not recover from its reported problem; stopping its container"
        ));
        // Killed by awman: no failure log. The exit is real, so the
        // finaliser reports it rather than `kill_current_container`.
        self.mark_focused_killed();
        if let Some(ch) = cancel_handle {
            let _ = ch.cancel();
        }
        let exit = match tokio::time::timeout(KILL_WAIT_LIMIT, &mut *wait_rx).await {
            Ok(Ok((exec_back, result))) => {
                self.set_focused_execution(exec_back);
                result?
            }
            // The wait never resolved: record the kill without its real code.
            Ok(Err(_)) | Err(_) => {
                let now = chrono::Utc::now();
                AgentExitInfo {
                    exit_code: KILLED_EXIT_CODE,
                    signal: None,
                    started_at: now,
                    ended_at: now,
                }
            }
        };
        Ok(InterruptibleStepResult::StepCompleted(
            self.finalize_step(step_name, exit)?,
        ))
    }

    /// Run one agent countdown through the `yolo_countdown_*` hooks: tick the
    /// frontend, and stop on expiry, Esc, Ctrl-W, the container exiting (its
    /// wait result is handed back untouched), a report that needs a different
    /// response, or — for `AgentError` — fresh output. Shared by workflow
    /// steps and `on_failure` remediation agents.
    pub(super) async fn drive_agent_countdown<T>(
        &mut self,
        step_name: &str,
        kind: CountdownKind,
        wait_rx: &mut tokio::sync::oneshot::Receiver<T>,
        stuck_rx: &mut Option<tokio::sync::broadcast::Receiver<StuckEvent>>,
        output_tail: Option<Arc<OutputTail>>,
    ) -> Result<AgentCountdownEnd<T>, EngineError> {
        let total = countdown_duration(kind);
        let mut start = Instant::now();
        let mut output = (kind == CountdownKind::AgentError)
            .then_some(output_tail)
            .flatten()
            .map(|tail| OutputWatch::new(tail, Arc::clone(&self.clock)));
        // Active-tab suppression (WI 0141): while the user is typing or
        // scrolling on this agent's tab, an `AgentDone` countdown does not
        // start; the loop below still handles exits, reports and cancel.
        // The full countdown starts once the user is quiet.
        let mut deferred =
            kind == CountdownKind::AgentDone && self.frontend.user_interacting_with(step_name);
        if !deferred {
            self.frontend.yolo_countdown_started(step_name, kind);
        }
        let end = loop {
            if self.cancel_requested() {
                break AgentCountdownEnd::Cancelled;
            }
            // Drain queued events first so a `working` that lands at the
            // instant of expiry still wins (same rule as the silence path).
            if let Some(end) = self.drain_countdown_events(step_name, kind, stuck_rx) {
                break end;
            }
            if output.as_mut().is_some_and(OutputWatch::produced_output) {
                break AgentCountdownEnd::Resumed;
            }
            if deferred && !self.frontend.user_interacting_with(step_name) {
                deferred = false;
                start = Instant::now();
                self.frontend.yolo_countdown_started(step_name, kind);
            }
            if deferred {
                tokio::select! {
                    biased;
                    result = &mut *wait_rx => {
                        let result = result.map_err(|_| {
                            EngineError::Other("step wait task dropped unexpectedly".into())
                        })?;
                        // Never shown, so there is nothing to finish.
                        return Ok(AgentCountdownEnd::Exited(result));
                    }
                    Some(event) = Self::recv_stuck(stuck_rx) => {
                        if let Some(end) = self.countdown_event(step_name, kind, event) {
                            return Ok(end);
                        }
                    }
                    Some(req) = Self::recv_engine(&mut self.engine_rx) => {
                        if let EngineRequest::OpenControlBoard { .. } = req {
                            return Ok(AgentCountdownEnd::ShowControlBoard);
                        }
                    }
                    _ = tokio::time::sleep(Duration::from_millis(100)) => {}
                }
                continue;
            }
            let remaining = total.saturating_sub(start.elapsed());
            match self
                .frontend
                .yolo_countdown_tick(step_name, remaining, total)?
            {
                YoloTickOutcome::AdvanceNow => break AgentCountdownEnd::Expired,
                YoloTickOutcome::Cancel => {
                    self.msg_info(format!("Countdown cancelled for step '{step_name}'"));
                    break AgentCountdownEnd::Cancelled;
                }
                YoloTickOutcome::Continue => {}
            }
            if remaining.is_zero() {
                break AgentCountdownEnd::Expired;
            }
            tokio::select! {
                biased;
                result = &mut *wait_rx => {
                    let result = result
                        .map_err(|_| EngineError::Other("step wait task dropped unexpectedly".into()))?;
                    break AgentCountdownEnd::Exited(result);
                }
                Some(event) = Self::recv_stuck(stuck_rx) => {
                    if let Some(end) = self.countdown_event(step_name, kind, event) {
                        break end;
                    }
                }
                Some(req) = Self::recv_engine(&mut self.engine_rx) => {
                    match req {
                        EngineRequest::OpenControlBoard { .. } => {
                            break AgentCountdownEnd::ShowControlBoard;
                        }
                        EngineRequest::StepUnstuck { .. } if kind == CountdownKind::AgentError => {
                            break AgentCountdownEnd::Resumed;
                        }
                        EngineRequest::StepUnstuck { .. } | EngineRequest::StepStuck { .. } => {}
                    }
                }
                _ = tokio::time::sleep(Duration::from_millis(100)) => {}
            }
        };
        self.frontend.yolo_countdown_finished(step_name);
        Ok(end)
    }

    fn drain_countdown_events<T>(
        &mut self,
        step_name: &str,
        kind: CountdownKind,
        stuck_rx: &mut Option<tokio::sync::broadcast::Receiver<StuckEvent>>,
    ) -> Option<AgentCountdownEnd<T>> {
        use tokio::sync::broadcast::error::TryRecvError;
        let rx = stuck_rx.as_mut()?;
        loop {
            match rx.try_recv() {
                Ok(event) => {
                    if let Some(end) = self.countdown_event(step_name, kind, event) {
                        return Some(end);
                    }
                }
                Err(TryRecvError::Lagged(_)) => continue,
                Err(TryRecvError::Empty) | Err(TryRecvError::Closed) => return None,
            }
        }
    }

    /// What one event means to a running countdown of `kind`: a report with
    /// the same response keeps the countdown and its deadline, but its new
    /// data (a different blocked kind, an updated message) is still recorded
    /// and published; any other report ends the countdown for the caller to
    /// handle. Silence events only matter to `AgentError` (output after
    /// silence is "any output").
    fn countdown_event<T>(
        &mut self,
        step_name: &str,
        kind: CountdownKind,
        event: StuckEvent,
    ) -> Option<AgentCountdownEnd<T>> {
        match event {
            StuckEvent::Reported(status) => {
                if self.reported_response(status.as_ref()) == ReportedResponse::Countdown(kind) {
                    self.note_if_changed(step_name, status);
                    None
                } else {
                    Some(AgentCountdownEnd::Superseded(status))
                }
            }
            StuckEvent::Unstuck if kind == CountdownKind::AgentError => {
                Some(AgentCountdownEnd::Resumed)
            }
            StuckEvent::Unstuck | StuckEvent::Stuck | StuckEvent::StartupGraceExpired => None,
        }
    }
}

/// Upper bound on waiting for a container the engine just killed after a
/// problem countdown. Real runtimes resolve at once; this only stops a
/// misbehaving backend from hanging the workflow.
const KILL_WAIT_LIMIT: Duration = Duration::from_secs(30);

/// Map a mid-step board outcome onto the step loop's return value.
pub(super) fn mid_step_result(mid: MidStepOutcome) -> Option<InterruptibleStepResult> {
    match mid {
        MidStepOutcome::Continue => None,
        MidStepOutcome::StepCompleted(o) => Some(InterruptibleStepResult::StepCompleted(o)),
        MidStepOutcome::WorkflowEnded(wo) => Some(InterruptibleStepResult::WorkflowEnded(wo)),
        MidStepOutcome::LoopContinue => Some(InterruptibleStepResult::LoopContinue),
    }
}
