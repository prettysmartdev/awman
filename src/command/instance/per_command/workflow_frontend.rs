//! `WorkflowFrontend` impl for the TUI.

use std::time::Duration;

use crate::command::instance::feeds::{ContainerSlotEvent, StepViewStatus, WorkflowStepKind};
use crate::command::instance::frontend::InstanceCommandFrontend;
use crate::data::dialog::{DialogRequest, DialogResponse, WorkflowControlBoardState};
use crate::data::message::UserMessageSink;
use crate::data::program_status::{EffectiveStatus, ProgramState};
use crate::data::prompt::Prompt;
use crate::data::workflow_definition::WorkflowStep;
use crate::data::workflow_state::{PhaseKind, WorkflowState};
use crate::engine::error::EngineError;
use crate::engine::workflow::actions::{
    AvailableActions, CountdownKind, NextAction, ParallelGroupDecision, ResumeMismatch, StepOutput,
    WorkflowOutcome, WorkflowStepProgressInfo, WorkflowStepStatus, YoloTickOutcome,
};
use crate::engine::workflow::frontend::WorkflowFrontend;

impl WorkflowFrontend for InstanceCommandFrontend {
    fn set_container_source(
        &mut self,
        source: Option<crate::engine::workflow::frontend::ContainerSource>,
    ) {
        if let Some(recorder) = &self.worker_events {
            recorder.container_source(source);
        }
    }
    fn report_pull_request_created(&mut self, url: &str) {
        if let Some(recorder) = &self.worker_events {
            recorder.writer.record_pull_request(url);
        }
    }
    fn report_workflow_state(&mut self, state: &WorkflowState) {
        if let Some(recorder) = &self.worker_events {
            recorder.state(state);
        }
    }
    fn show_workflow_control_board(
        &mut self,
        _state: &WorkflowState,
        available: &AvailableActions,
    ) -> Result<NextAction, EngineError> {
        if let Some(defaults) = self.cli_defaults() {
            return Ok(defaults.workflow_next_action(available));
        }
        // Which step the board is about, and whether this is the plain
        // "advance to the next step?" case, are the engine's answers (F-18).
        // The TUI used to scan `WorkflowState::step_states` for both.
        let step_name = available.focused_step_label().to_string();
        let reported_done = available
            .reported_status
            .as_ref()
            .is_some_and(|s| matches!(s.state, ProgramState::Done | ProgramState::Idle));
        // Active-tab suppression: an idle/done report on the tab the user is
        // typing in must not pop the confirmation over their keystrokes.
        if reported_done {
            let seen_done = !self.agent_resumed();
            self.wait_for_quiet_user();
            // The agent resumed while the board was held back: it is stale,
            // so it is never shown (the step keeps running).
            let working = self
                .signals
                .status()
                .is_some_and(|s| s.state == ProgramState::Working);
            if available.can_dismiss && (working || (seen_done && self.agent_resumed())) {
                return Ok(NextAction::Dismiss);
            }
        }
        let from_report = available.reported_status.is_some();
        let presentation = self.presentation.clone();
        let ask = |request: DialogRequest| {
            if from_report {
                self.ask_report_dialog(request)
            } else {
                self.ask_dialog(request)
            }
        };

        if let Some(advance) = &available.simple_advance {
            let response = ask(DialogRequest::WorkflowStepConfirm(
                crate::data::dialog::WorkflowStepConfirmState {
                    completed_step: advance.completed_step.clone(),
                    next_step: advance.next_step.clone(),
                },
            ))
            .map_err(|e| EngineError::Other(e.to_string()))?;
            return Ok(match response {
                DialogResponse::Char('>') => NextAction::LaunchNext,
                DialogResponse::Char('W') => {
                    let response2 = self
                        .ask_dialog(DialogRequest::WorkflowControlBoard(
                            control_board_state_with(
                                &step_name,
                                available,
                                self.presentation.as_ref(),
                            ),
                        ))
                        .map_err(|e| EngineError::Other(e.to_string()))?;
                    wcb_response_to_action(response2, available)
                }
                // Esc on a board opened by the agent's own done/idle report
                // keeps the agent running instead of stopping it.
                DialogResponse::Dismissed if reported_done && available.can_dismiss => {
                    NextAction::Dismiss
                }
                DialogResponse::Dismissed => NextAction::Pause,
                _ => NextAction::Pause,
            });
        }

        let response = ask(DialogRequest::WorkflowControlBoard(
            control_board_state_with(&step_name, available, presentation.as_ref()),
        ))
        .map_err(|e| EngineError::Other(e.to_string()))?;
        Ok(wcb_response_to_action(response, available))
    }

    /// The engine's follow-up about a running parallel group. Its wording,
    /// choices and dismissal answer are all `prompt`'s; this only draws it.
    fn ask_parallel_group(
        &mut self,
        prompt: &Prompt<ParallelGroupDecision>,
    ) -> Result<ParallelGroupDecision, EngineError> {
        self.pick_from_keyed_prompt(prompt)
            .map_err(|e| EngineError::Other(e.to_string()))
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
            *guard = Some(crate::command::instance::feeds::YoloState {
                step_name: step_name.to_string(),
                remaining_secs: remaining.as_secs(),
                kind: self.countdown_kind,
            });
        }
        Ok(YoloTickOutcome::Continue)
    }

    fn yolo_countdown_started(&mut self, _step_name: &str, kind: CountdownKind) {
        // State is set by yolo_countdown_tick; remember why it runs so the
        // dialog can say so.
        self.countdown_kind = kind;
    }

    /// Active-tab suppression is an input to the engine, which holds an
    /// `AgentDone` countdown back while this is true (and keeps processing
    /// exits and reports). Never blocks.
    fn user_interacting_with(&self, _step_name: &str) -> bool {
        self.should_defer_for_user()
    }

    fn report_agent_status(&mut self, step_name: &str, status: Option<&EffectiveStatus>) {
        let label = status
            .filter(|s| matches!(s.state, ProgramState::Blocked | ProgramState::Error))
            .map(|s| self.presentation.status_short_label(s));
        let step = step_name.to_string();
        self.workflow.update_view(|guard| {
            if let Some(view) = guard.as_mut() {
                match label {
                    Some(label) => {
                        view.reported.insert(step, label);
                    }
                    None => {
                        view.reported.remove(&step);
                    }
                }
            }
        });
    }

    fn yolo_countdown_finished(&mut self, _step_name: &str) {
        if let Ok(mut guard) = self.yolo_state.lock() {
            *guard = None;
        }
    }

    fn report_step_status(&mut self, step: &WorkflowStep, status: WorkflowStepStatus) {
        self.messages
            .info(format!("workflow step '{}': {:?}", step.name, status));
        self.workflow.update_view(|guard| {
            if let Some(view) = guard.as_mut() {
                let view_status = workflow_step_view_status(&status);
                if let Some(s) = view.steps.iter_mut().find(|s| s.name == step.name) {
                    s.status = view_status;
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
        });
    }

    fn report_step_output(&mut self, _step: &WorkflowStep, _output: StepOutput) {}

    fn report_workflow_completed(&mut self, outcome: &WorkflowOutcome) {
        if let Some(recorder) = &self.worker_events {
            recorder.completed(outcome);
        }
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
            WorkflowOutcome::Cancelled => self.messages.warning("Workflow cancelled"),
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
        self.workflow.update_view(|guard| {
            let view = guard
                .get_or_insert_with(crate::command::instance::feeds::WorkflowViewState::default);
            let main_steps = steps.iter().map(|s| {
                // Only surface agent/model on the strip when the step itself
                // declares one — otherwise it inherits the project defaults
                // and gets no label (WI: workflow-strip agent/model labels).
                let (agent, model) = if s.has_step_override {
                    (Some(s.agent.clone()), s.model.clone())
                } else {
                    (None, None)
                };
                crate::command::instance::feeds::WorkflowStepView {
                    name: s.name.clone(),
                    status: workflow_step_view_status(&s.status),
                    agent,
                    model,
                    depends_on: s.depends_on.clone(),
                    kind: crate::command::instance::feeds::WorkflowStepKind::Agent,
                }
            });
            // This callback only knows about the main phase, so replacing
            // `view.steps` wholesale would wipe out any setup/teardown
            // pseudo-steps `on_setup_step_*`/`on_teardown_step_*` already
            // tracked there — splice the new main steps back between them.
            use crate::command::instance::feeds::WorkflowStepKind;
            let setup: Vec<_> = view
                .steps
                .iter()
                .filter(|s| s.kind == WorkflowStepKind::Setup)
                .cloned()
                .collect();
            let teardown: Vec<_> = view
                .steps
                .iter()
                .filter(|s| s.kind == WorkflowStepKind::Teardown)
                .cloned()
                .collect();
            view.steps = setup
                .into_iter()
                .chain(main_steps)
                .chain(teardown)
                .collect();
            view.current_step = steps
                .iter()
                .find(|s| matches!(s.status, WorkflowStepStatus::Running))
                .map(|s| s.name.clone());
            view.max_concurrent = steps.first().and_then(|s| s.max_concurrent);
        });
    }

    fn report_step_interactive_launch(
        &mut self,
        step: &WorkflowStep,
        agent: &str,
        _model: Option<&str>,
    ) {
        if let Ok(mut guard) = self.yolo_state.lock() {
            *guard = None;
        }
        // An `on_failure` agent takes the window over from a streamed phase
        // step; a retry of that step readies it again.
        self.phase_stream_open = false;

        if self.parallel_group_active {
            // Parallel steps get their own fresh slot (with a fresh parser)
            // via the Launched event — no PTY reset, which would wipe the
            // currently-focused group slot's live content.
            self.recreate_parallel_container_io(&step.name);
        } else {
            self.recreate_container_io();
        }

        self.messages
            .info(format!("Launching agent '{}' in new container...", agent));
    }

    fn report_container_exited(&mut self, exit_code: i32) {
        self.container.exited(exit_code);
    }

    fn confirm_resume(&mut self, mismatch: &ResumeMismatch) -> Result<bool, EngineError> {
        if let Some(defaults) = self.cli_defaults() {
            return Ok(defaults.confirm_resume());
        }
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

    /// A TUI always has a user in front of it.
    fn supports_interactive_recovery(&self) -> bool {
        !self.is_headless_cli()
    }

    fn on_phase_step_started(&mut self, kind: PhaseKind, description: &str) {
        self.messages
            .info(format!("{}: {description}", kind.label()));
        self.phase_stream_title = Some(format!("[{}] {description}", kind.label()));
        self.phase_stream_open = false;
        upsert_phase_step(
            &self.workflow,
            step_kind(kind),
            description,
            StepViewStatus::Running,
        );
    }

    /// Only steps on the filtered background stream (those holding a token
    /// grant) report output line by line; a PTY step's output is already in
    /// the container window. The lines arrive redacted and are written into
    /// the container window the same way a PTY step's bytes are, so phase
    /// step output never lands in the outer status log.
    fn on_phase_step_output(&mut self, _kind: PhaseKind, line: &str) {
        if !self.phase_stream_open {
            self.phase_stream_open = true;
            self.launch_sequential_container(self.phase_stream_title.clone());
        }
        let _ = self.stdout_tx.send(format!("{line}\r\n").into_bytes());
    }

    fn on_phase_step_completed(&mut self, kind: PhaseKind, description: &str) {
        self.close_phase_stream(0);
        self.messages
            .success(format!("{}: {description}", kind.label()));
        upsert_phase_step(
            &self.workflow,
            step_kind(kind),
            description,
            StepViewStatus::Done,
        );
    }

    fn on_phase_step_failed(
        &mut self,
        kind: PhaseKind,
        description: &str,
        exit_code: i32,
        stderr: &str,
    ) {
        self.close_phase_stream(exit_code);
        let label = kind.label();
        let msg = if stderr.is_empty() {
            format!("{label} failed: {description} (exit {exit_code})")
        } else {
            format!("{label} failed: {description} (exit {exit_code}): {stderr}")
        };
        self.messages.error_msg(msg);
        upsert_phase_step(
            &self.workflow,
            step_kind(kind),
            description,
            StepViewStatus::Error,
        );
    }

    /// The TUI routes Ctrl-W through the engine's request channel and colours
    /// the tab from the stuck channel, so it keeps both. Per-parallel-step
    /// handovers (`handles.step.is_some()`) are ignored here exactly as the
    /// old `set_parallel_step_*` no-op defaults ignored them: the parallel
    /// view has its own wiring.
    fn attach_engine(&mut self, handles: crate::engine::workflow::frontend::EngineHandles) {
        self.container.attach_engine(handles);
    }

    // ── Parallel group callbacks (WI-0096) ───────────────────────────────
    // These publish lifecycle events into the shared queue; the TUI event
    // loop drains it and maintains `Tab::container_slots`. Layer discipline:
    // the frontend never inspects `active_steps` or makes scheduling
    // decisions — it only reacts to these engine callbacks.

    fn report_parallel_group_started(&mut self, _step_names: &[String]) {
        self.parallel_group_active = true;
        self.pending_step_slot_io.clear();
        // Tell the TUI to stash the sequential backbone slot while the
        // group's per-step slots own the display.
        self.push_container_slot_event(ContainerSlotEvent::GroupStarted);
    }

    fn report_parallel_step_launched(&mut self, step_name: &str, agent: &str, model: Option<&str>) {
        let io = self.pending_step_slot_io.remove(step_name);
        self.push_container_slot_event(ContainerSlotEvent::Launched {
            step_name: step_name.to_string(),
            agent: agent.to_string(),
            model: model.map(|m| m.to_string()),
            io,
        });
    }

    fn report_parallel_step_dequeued(&mut self, step_name: &str, agent: &str, model: Option<&str>) {
        // A queued step took a freed slot — same visual as a launch.
        let io = self.pending_step_slot_io.remove(step_name);
        self.push_container_slot_event(ContainerSlotEvent::Launched {
            step_name: step_name.to_string(),
            agent: agent.to_string(),
            model: model.map(|m| m.to_string()),
            io,
        });
    }

    fn report_parallel_step_container(&mut self, step_name: &str, container_name: &str) {
        self.push_container_slot_event(ContainerSlotEvent::ContainerName {
            step_name: step_name.to_string(),
            container_name: container_name.to_string(),
        });
    }

    fn report_parallel_step_exited(&mut self, step_name: &str, _exit_code: i32) {
        self.push_container_slot_event(ContainerSlotEvent::Exited {
            step_name: step_name.to_string(),
        });
    }

    fn report_parallel_group_finished(&mut self) {
        self.parallel_group_active = false;
        self.pending_step_slot_io.clear();
        self.push_container_slot_event(ContainerSlotEvent::GroupFinished);
    }

    fn report_parallel_step_stuck(&mut self, step_name: &str) {
        self.push_container_slot_event(ContainerSlotEvent::Stuck {
            step_name: step_name.to_string(),
        });
    }

    fn report_parallel_step_unstuck(&mut self, step_name: &str) {
        self.push_container_slot_event(ContainerSlotEvent::Unstuck {
            step_name: step_name.to_string(),
        });
    }

    fn parallel_step_countdown_started(&mut self, step_name: &str, kind: CountdownKind) {
        self.push_container_slot_event(ContainerSlotEvent::CountdownKind {
            step_name: step_name.to_string(),
            kind,
        });
        self.parallel_step_yolo_countdown_started(step_name);
    }

    fn parallel_step_yolo_countdown_started(&mut self, step_name: &str) {
        let cancel_flag: crate::command::instance::feeds::SharedYoloCancelFlag =
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.pending_parallel_yolo_cancel
            .insert(step_name.to_string(), cancel_flag.clone());
        self.push_container_slot_event(ContainerSlotEvent::YoloStarted {
            step_name: step_name.to_string(),
            cancel_flag,
        });
    }

    fn parallel_step_yolo_countdown_tick(
        &mut self,
        step_name: &str,
        remaining: Duration,
        _total: Duration,
    ) -> Result<YoloTickOutcome, EngineError> {
        if let Some(flag) = self.pending_parallel_yolo_cancel.get(step_name) {
            if flag.swap(false, std::sync::atomic::Ordering::Relaxed) {
                return Ok(YoloTickOutcome::Cancel);
            }
        }
        self.push_container_slot_event(ContainerSlotEvent::YoloTick {
            step_name: step_name.to_string(),
            remaining_secs: remaining.as_secs(),
        });
        Ok(YoloTickOutcome::Continue)
    }

    fn parallel_step_yolo_countdown_finished(&mut self, step_name: &str) {
        self.pending_parallel_yolo_cancel.remove(step_name);
        self.push_container_slot_event(ContainerSlotEvent::YoloFinished {
            step_name: step_name.to_string(),
        });
    }
}

/// Map a WCB dialog response to a `NextAction`.
/// Project an engine [`AvailableActions`] onto the TUI dialog state. One place
/// so the escalated-from-step-confirm board and the direct board never drift.
impl InstanceCommandFrontend {
    /// Close the container window a streamed phase step readied, the same
    /// way a container exit closes a PTY step's window.
    fn close_phase_stream(&mut self, exit_code: i32) {
        if std::mem::take(&mut self.phase_stream_open) {
            self.container.exited(exit_code);
        }
    }
}

fn control_board_state_with(
    step_name: &str,
    available: &AvailableActions,
    presentation: &dyn crate::command::instance::frontend::InstancePresentation,
) -> WorkflowControlBoardState {
    // The agent's own words about why this board is open lead the detail lines.
    let mut failure_lines: Vec<String> = available
        .reported_status
        .as_ref()
        .map(|s| vec![presentation.status_line(s)])
        .unwrap_or_default();
    failure_lines.extend(
        available
            .step_failure
            .as_ref()
            .map(|f| f.detail_lines.clone())
            .unwrap_or_default(),
    );
    WorkflowControlBoardState {
        step_name: step_name.to_string(),
        focused_step_name: step_name.to_string(),
        can_launch_next: available.can_launch_next,
        can_continue_current: available.can_continue_in_current_container,
        can_restart: available.can_restart_current_step,
        can_go_back: available.can_cancel_to_previous_step,
        can_finish: available.can_finish_workflow,
        continue_unavailable_reason: available.continue_unavailable_reason.clone(),
        cancel_to_previous_unavailable_reason: available
            .cancel_to_previous_unavailable_reason
            .clone(),
        finish_workflow_unavailable_reason: available.finish_workflow_unavailable_reason.clone(),
        restart_unavailable_reason: available.restart_unavailable_reason.clone(),
        can_dismiss: available.can_dismiss,
        launch_next_label: available.launch_next_label.clone(),
        parallel_peer_count: available.parallel_peer_count,
        parallel_peers_running: available.parallel_peers_running,
        failure_lines,
        retry_failed_step: available.retry_failed_step.clone(),
        in_parallel_group: available.acts_on_parallel_group,
    }
}

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
        DialogResponse::Char('r') if available.retry_failed_step.is_some() => {
            NextAction::RetryFailedStep {
                step_name: available.retry_failed_step.clone().unwrap_or_default(),
            }
        }
        DialogResponse::Char('p') if available.can_dismiss => NextAction::Pause,
        DialogResponse::Dismissed if available.can_dismiss => NextAction::Dismiss,
        DialogResponse::Dismissed => NextAction::Pause,
        _ => NextAction::Pause,
    }
}

/// Map a `WorkflowStepStatus` to the lower-case string used in
/// `WorkflowStepView.status` (the renderer matches on it).
/// How an engine step status reads in the Workflow Overview. The third such
/// conversion, alongside `StepViewStatus::of_step_state` /
/// `of_phase_step_status`; all three are exhaustive (WI 0114 F-22).
fn workflow_step_view_status(status: &WorkflowStepStatus) -> StepViewStatus {
    match status {
        WorkflowStepStatus::Pending => StepViewStatus::Pending,
        WorkflowStepStatus::Running => StepViewStatus::Running,
        WorkflowStepStatus::Succeeded => StepViewStatus::Done,
        WorkflowStepStatus::Failed { .. } => StepViewStatus::Error,
        WorkflowStepStatus::Cancelled => StepViewStatus::Cancelled,
        WorkflowStepStatus::Skipped => StepViewStatus::Skipped,
    }
}

/// The workflow-view row kind for a phase. The view has a third kind
/// (`Agent`) for ordinary steps, so it is not the same enum as `PhaseKind`.
fn step_kind(kind: PhaseKind) -> WorkflowStepKind {
    match kind {
        PhaseKind::Setup => WorkflowStepKind::Setup,
        PhaseKind::Teardown => WorkflowStepKind::Teardown,
    }
}

/// Insert or update a setup/teardown pseudo-step in the live Workflow
/// Overview, so a locally-attached run shows the same `[setup]`/`[teardown]`
/// column a squad/remote run gets from `workflow_state_to_view_state` — the
/// engine's `on_phase_step_*` hooks (unlike `report_workflow_progress`) carry
/// only a description, not the full ordered plan, so entries appear as they
/// start rather than as `pending` ahead of time.
///
/// The hooks carry no stable id, so a step in flight is found by matching
/// `(kind, description)`; setup/teardown steps run strictly sequentially, so
/// the most recent match (searched from the end) is always the current one.
/// A new setup entry is inserted after any earlier setup entries, ahead of
/// the main/teardown steps already tracked; a new teardown entry is appended,
/// since teardown can only start once every main step is done.
fn upsert_phase_step(
    workflow_view: &crate::command::instance::feeds::SharedWorkflowPresentation,
    kind: WorkflowStepKind,
    description: &str,
    status: StepViewStatus,
) {
    use crate::command::instance::feeds::{WorkflowStepView, WorkflowViewState};

    workflow_view.update_view(|guard| {
        let view = guard.get_or_insert_with(WorkflowViewState::default);
        if let Some(existing) = view
            .steps
            .iter_mut()
            .rev()
            .find(|s| s.kind == kind && s.name == description)
        {
            existing.status = status;
            return;
        }
        let step = WorkflowStepView {
            name: description.to_string(),
            status,
            agent: None,
            model: None,
            depends_on: Vec::new(),
            kind,
        };
        match kind {
            WorkflowStepKind::Setup => {
                let pos = view
                    .steps
                    .iter()
                    .position(|s| s.kind != WorkflowStepKind::Setup)
                    .unwrap_or(view.steps.len());
                view.steps.insert(pos, step);
            }
            WorkflowStepKind::Teardown | WorkflowStepKind::Agent => view.steps.push(step),
        }
    });
}
