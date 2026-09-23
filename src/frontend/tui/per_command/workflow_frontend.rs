//! `WorkflowFrontend` impl for the TUI.

use std::time::Duration;

use crate::data::message::UserMessageSink;
use crate::data::workflow_definition::WorkflowStep;
use crate::data::workflow_state::{PhaseKind, WorkflowState};
use crate::engine::error::EngineError;
use crate::engine::workflow::actions::{
    AvailableActions, CountdownKind, NextAction, ResumeMismatch, StepOutput, WorkflowOutcome,
    WorkflowStepProgressInfo, WorkflowStepStatus, YoloTickOutcome,
};
use crate::engine::workflow::frontend::WorkflowFrontend;
use crate::frontend::tui::command_frontend::TuiCommandFrontend;
use crate::frontend::tui::dialogs::{DialogRequest, DialogResponse, WorkflowControlBoardState};
use crate::frontend::tui::tabs::{ContainerSlotEvent, StepViewStatus, WorkflowStepKind};

impl WorkflowFrontend for TuiCommandFrontend {
    fn show_workflow_control_board(
        &mut self,
        _state: &WorkflowState,
        available: &AvailableActions,
    ) -> Result<NextAction, EngineError> {
        // Which step the board is about, and whether this is the plain
        // "advance to the next step?" case, are the engine's answers (F-18).
        // The TUI used to scan `WorkflowState::step_states` for both.
        let step_name = available.focused_step_label().to_string();

        if let Some(advance) = &available.simple_advance {
            let response = self
                .ask_dialog(DialogRequest::WorkflowStepConfirm(
                    crate::frontend::tui::dialogs::WorkflowStepConfirmState {
                        completed_step: advance.completed_step.clone(),
                        next_step: advance.next_step.clone(),
                    },
                ))
                .map_err(|e| EngineError::Other(e.to_string()))?;
            return Ok(match response {
                DialogResponse::Char('>') => NextAction::LaunchNext,
                DialogResponse::Char('W') => {
                    let response2 = self
                        .ask_dialog(DialogRequest::WorkflowControlBoard(control_board_state(
                            &step_name, available,
                        )))
                        .map_err(|e| EngineError::Other(e.to_string()))?;
                    wcb_response_to_action(response2, available)
                }
                DialogResponse::Dismissed => NextAction::Pause,
                _ => NextAction::Pause,
            });
        }

        let response = self
            .ask_dialog(DialogRequest::WorkflowControlBoard(control_board_state(
                &step_name, available,
            )))
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

    fn yolo_countdown_started(&mut self, _step_name: &str, _kind: CountdownKind) {
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
            let main_steps = steps.iter().map(|s| {
                // Only surface agent/model on the strip when the step itself
                // declares one — otherwise it inherits the project defaults
                // and gets no label (WI: workflow-strip agent/model labels).
                let (agent, model) = if s.has_step_override {
                    (Some(s.agent.clone()), s.model.clone())
                } else {
                    (None, None)
                };
                crate::frontend::tui::tabs::WorkflowStepView {
                    name: s.name.clone(),
                    status: workflow_step_view_status(&s.status),
                    agent,
                    model,
                    depends_on: s.depends_on.clone(),
                    kind: crate::frontend::tui::tabs::WorkflowStepKind::Agent,
                }
            });
            // This callback only knows about the main phase, so replacing
            // `view.steps` wholesale would wipe out any setup/teardown
            // pseudo-steps `on_setup_step_*`/`on_teardown_step_*` already
            // tracked there — splice the new main steps back between them.
            use crate::frontend::tui::tabs::WorkflowStepKind;
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
        }
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

        if self.parallel_group_active {
            // Parallel steps get their own fresh slot (with a fresh parser)
            // via the Launched event — no PTY reset, which would wipe the
            // currently-focused group slot's live content.
            self.recreate_parallel_container_io(&step.name);
        } else {
            // Sequential steps reuse the backbone slot; flag its parser for
            // a reset so the previous step's terminal content is cleared.
            self.pty_reset_flag
                .store(true, std::sync::atomic::Ordering::Relaxed);
            self.recreate_container_io();
        }

        if let Ok(mut name) = self.container_name_shared.lock() {
            *name = None;
        }

        self.messages
            .info(format!("Launching agent '{}' in new container...", agent));
    }

    fn report_container_exited(&mut self, exit_code: i32) {
        // Publish the exit so the TUI event loop closes the container window
        // (leaving the summary bar). The engine only calls this on real
        // container death — never for stuck states or during a yolo countdown.
        if let Ok(mut guard) = self.container_exit_shared.lock() {
            *guard = Some(exit_code);
        }
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

    /// A TUI always has a user in front of it.
    fn supports_interactive_recovery(&self) -> bool {
        true
    }

    fn on_phase_step_started(&mut self, kind: PhaseKind, description: &str) {
        self.messages
            .info(format!("{}: {description}", kind.label()));
        upsert_phase_step(
            &self.workflow_view,
            step_kind(kind),
            description,
            StepViewStatus::Running,
        );
    }

    fn on_phase_step_output(&mut self, _kind: PhaseKind, line: &str) {
        self.messages.info(format!("  {line}"));
    }

    fn on_phase_step_completed(&mut self, kind: PhaseKind, description: &str) {
        self.messages
            .success(format!("{}: {description}", kind.label()));
        upsert_phase_step(
            &self.workflow_view,
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
        let label = kind.label();
        let msg = if stderr.is_empty() {
            format!("{label} failed: {description} (exit {exit_code})")
        } else {
            format!("{label} failed: {description} (exit {exit_code}): {stderr}")
        };
        self.messages.error_msg(msg);
        upsert_phase_step(
            &self.workflow_view,
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
        if handles.step.is_some() {
            return;
        }
        if let Some(tx) = handles.requests {
            if let Ok(mut guard) = self.engine_tx_shared.lock() {
                *guard = Some(tx);
            }
        }
        if let Some(sender) = handles.stuck {
            if let Ok(mut guard) = self.stuck_sender_shared.lock() {
                *guard = Some(sender);
            }
        }
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

    fn parallel_step_yolo_countdown_started(&mut self, step_name: &str) {
        let cancel_flag: crate::frontend::tui::tabs::SharedYoloCancelFlag =
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
fn control_board_state(step_name: &str, available: &AvailableActions) -> WorkflowControlBoardState {
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
        failure_lines: available
            .step_failure
            .as_ref()
            .map(|f| f.detail_lines.clone())
            .unwrap_or_default(),
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
    workflow_view: &crate::frontend::tui::tabs::SharedWorkflowViewState,
    kind: WorkflowStepKind,
    description: &str,
    status: StepViewStatus,
) {
    use crate::frontend::tui::tabs::{WorkflowStepView, WorkflowViewState};

    let Ok(mut guard) = workflow_view.lock() else {
        return;
    };
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
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crate::data::workflow_state::PhaseKind;
    use crate::engine::workflow::frontend::WorkflowFrontend;
    use crate::frontend::tui::command_frontend::TuiCommandFrontend;
    use crate::frontend::tui::dialogs::{DialogRequest, DialogResponse};
    use crate::frontend::tui::tabs::StepViewStatus;

    fn make_frontend() -> (
        TuiCommandFrontend,
        std::sync::mpsc::Receiver<DialogRequest>,
        std::sync::mpsc::Sender<DialogResponse>,
    ) {
        crate::frontend::tui::tests::test_command_frontend(&["workflow"], Default::default())
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

    /// The TUI renders the lightweight confirm when — and only when — the
    /// engine says the board is the simple-advance case. *Whether* it is is
    /// the engine's judgement, tested in `engine::workflow` (F-18); this is a
    /// rendering assertion.
    #[test]
    fn a_simple_advance_board_renders_the_lightweight_dialog() {
        use crate::engine::workflow::actions::SimpleAdvance;
        use crate::engine::workflow::frontend::WorkflowFrontend;

        let (mut frontend, req_rx, resp_tx) = make_frontend();

        let state = make_workflow_state_one_pending();
        let available = crate::engine::workflow::actions::AvailableActions {
            can_launch_next: true,
            simple_advance: Some(SimpleAdvance {
                completed_step: "build".into(),
                next_step: "test".into(),
            }),
            ..Default::default()
        };

        let handle = std::thread::spawn(move || {
            let req = req_rx.recv().unwrap();
            match req {
                crate::frontend::tui::dialogs::DialogRequest::WorkflowStepConfirm(state) => {
                    assert_eq!(state.completed_step, "build");
                    assert_eq!(state.next_step, "test");
                }
                other => panic!("expected WorkflowStepConfirm, got {other:?}"),
            }
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

    /// No `simple_advance` means the full control board, whatever the state
    /// looks like — the frontend does not second-guess the engine.
    #[test]
    fn a_board_without_simple_advance_renders_the_control_board() {
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
                "no simple_advance should show WorkflowControlBoard, got {:?}",
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
    fn report_parallel_step_container_publishes_container_name_event() {
        use crate::frontend::tui::tabs::ContainerSlotEvent;

        let (mut frontend, _req_rx, _resp_tx) = make_frontend();
        frontend.report_parallel_step_container("build", "awman-build-99");

        let mut q = frontend.container_slot_events.lock().unwrap();
        let event = q.pop_front().expect("an event must be queued");
        match event {
            ContainerSlotEvent::ContainerName {
                step_name,
                container_name,
            } => {
                assert_eq!(step_name, "build");
                assert_eq!(container_name, "awman-build-99");
            }
            _ => panic!("expected ContainerName event"),
        }
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

    // ─── Parallel-group yolo countdown tests ────────────────────────────────

    #[test]
    fn parallel_yolo_started_publishes_yolo_started_event_with_cancel_flag() {
        use crate::frontend::tui::tabs::ContainerSlotEvent;

        let (mut frontend, _req_rx, _resp_tx) = make_frontend();
        frontend.parallel_step_yolo_countdown_started("build");

        assert!(
            frontend.pending_parallel_yolo_cancel.contains_key("build"),
            "a cancel flag must be stashed for later ticks"
        );

        let mut q = frontend.container_slot_events.lock().unwrap();
        let event = q.pop_front().expect("an event must be queued");
        match event {
            ContainerSlotEvent::YoloStarted {
                step_name,
                cancel_flag,
            } => {
                assert_eq!(step_name, "build");
                assert!(
                    std::sync::Arc::ptr_eq(
                        &cancel_flag,
                        frontend.pending_parallel_yolo_cancel.get("build").unwrap()
                    ),
                    "the event must carry the SAME Arc stashed for tick checks"
                );
            }
            _ => panic!("expected YoloStarted event"),
        }
    }

    #[test]
    fn parallel_yolo_tick_publishes_yolo_tick_event_by_default() {
        use crate::engine::workflow::actions::YoloTickOutcome;
        use crate::frontend::tui::tabs::ContainerSlotEvent;

        let (mut frontend, _req_rx, _resp_tx) = make_frontend();
        frontend.parallel_step_yolo_countdown_started("build");
        // Drain the YoloStarted event pushed above so this test only
        // inspects the tick's own event.
        frontend.container_slot_events.lock().unwrap().clear();

        let result = frontend
            .parallel_step_yolo_countdown_tick(
                "build",
                Duration::from_secs(17),
                Duration::from_secs(60),
            )
            .unwrap();
        assert_eq!(result, YoloTickOutcome::Continue);

        let mut q = frontend.container_slot_events.lock().unwrap();
        let event = q.pop_front().expect("an event must be queued");
        match event {
            ContainerSlotEvent::YoloTick {
                step_name,
                remaining_secs,
            } => {
                assert_eq!(step_name, "build");
                assert_eq!(remaining_secs, 17);
            }
            _ => panic!("expected YoloTick event"),
        }
    }

    #[test]
    fn parallel_yolo_tick_returns_cancel_when_slots_own_flag_is_set() {
        use crate::engine::workflow::actions::YoloTickOutcome;

        let (mut frontend, _req_rx, _resp_tx) = make_frontend();
        frontend.parallel_step_yolo_countdown_started("build");
        frontend
            .pending_parallel_yolo_cancel
            .get("build")
            .unwrap()
            .store(true, std::sync::atomic::Ordering::Relaxed);

        let result = frontend
            .parallel_step_yolo_countdown_tick(
                "build",
                Duration::from_secs(30),
                Duration::from_secs(60),
            )
            .unwrap();
        assert_eq!(result, YoloTickOutcome::Cancel);
    }

    #[test]
    fn parallel_yolo_tick_for_unrelated_step_does_not_affect_others() {
        use crate::engine::workflow::actions::YoloTickOutcome;

        let (mut frontend, _req_rx, _resp_tx) = make_frontend();
        frontend.parallel_step_yolo_countdown_started("build");
        frontend.parallel_step_yolo_countdown_started("test");
        frontend
            .pending_parallel_yolo_cancel
            .get("build")
            .unwrap()
            .store(true, std::sync::atomic::Ordering::Relaxed);

        // "test"'s countdown must keep ticking even though "build" was
        // cancelled — independent per-step, per WI-0096 §9.
        let result = frontend
            .parallel_step_yolo_countdown_tick(
                "test",
                Duration::from_secs(30),
                Duration::from_secs(60),
            )
            .unwrap();
        assert_eq!(result, YoloTickOutcome::Continue);
    }

    #[test]
    fn parallel_yolo_finished_removes_cancel_flag_and_publishes_event() {
        use crate::frontend::tui::tabs::ContainerSlotEvent;

        let (mut frontend, _req_rx, _resp_tx) = make_frontend();
        frontend.parallel_step_yolo_countdown_started("build");
        frontend.container_slot_events.lock().unwrap().clear();

        frontend.parallel_step_yolo_countdown_finished("build");
        assert!(!frontend.pending_parallel_yolo_cancel.contains_key("build"));

        let mut q = frontend.container_slot_events.lock().unwrap();
        let event = q.pop_front().expect("an event must be queued");
        match event {
            ContainerSlotEvent::YoloFinished { step_name } => {
                assert_eq!(step_name, "build");
            }
            _ => panic!("expected YoloFinished event"),
        }
    }

    #[test]
    fn report_container_exited_publishes_exit_code() {
        use crate::engine::workflow::frontend::WorkflowFrontend;

        let (mut frontend, _req_rx, _resp_tx) = make_frontend();
        assert!(frontend.container_exit_shared.lock().unwrap().is_none());

        frontend.report_container_exited(137);
        assert_eq!(*frontend.container_exit_shared.lock().unwrap(), Some(137));
    }

    #[test]
    fn attach_engine_stores_the_request_sender() {
        use crate::engine::workflow::frontend::{EngineHandles, WorkflowFrontend};
        use crate::engine::workflow::EngineRequest;

        let (mut frontend, _req_rx, _resp_tx) = make_frontend();
        assert!(frontend.engine_tx_shared.lock().unwrap().is_none());

        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<EngineRequest>();
        frontend.attach_engine(EngineHandles::requests(tx));
        assert!(frontend.engine_tx_shared.lock().unwrap().is_some());
    }

    /// A per-parallel-step handover must not overwrite the workflow-level
    /// channels — the old `set_parallel_step_*` defaults were no-ops here and
    /// the single `attach_engine` has to keep that distinction.
    #[test]
    fn attach_engine_ignores_a_per_step_handover() {
        use crate::engine::agent_runtime::execution::StuckEvent;
        use crate::engine::workflow::frontend::{EngineHandles, WorkflowFrontend};

        let (mut frontend, _req_rx, _resp_tx) = make_frontend();
        let (sender, _rx) = tokio::sync::broadcast::channel::<StuckEvent>(4);
        frontend.attach_engine(EngineHandles::step_stuck("a", std::sync::Arc::new(sender)));
        assert!(
            frontend.stuck_sender_shared.lock().unwrap().is_none(),
            "a parallel step's stuck channel must not land in the tab-level slot"
        );
    }

    // take_io self-heals when the slot is empty. Launch paths that skip
    // report_step_interactive_launch (historically, on_failure remediation
    // agents) must get fresh channels instead of panicking the command task.
    #[test]
    fn take_io_self_heals_when_slot_already_consumed() {
        use crate::engine::agent_runtime::frontend::AgentFrontend;

        let (mut frontend, _req_rx, _resp_tx) = make_frontend();

        let first = frontend.take_io();
        assert!(first.initial_size.is_some());

        // Second call without an intervening report_step_interactive_launch:
        // must rebuild channels, not panic.
        let second = frontend.take_io();
        assert!(second.initial_size.is_some());

        // The rebuilt channels publish fresh stdin/resize senders for the
        // TUI event loop to swap to.
        assert!(frontend.stdin_tx_shared.lock().unwrap().is_some());
        assert!(frontend.resize_tx_shared.lock().unwrap().is_some());
    }

    // ── setup/teardown steps in the Workflow Overview (local execution) ──────

    #[test]
    fn setup_and_teardown_steps_land_around_the_main_steps_in_the_workflow_view() {
        use crate::engine::workflow::actions::{WorkflowStepProgressInfo, WorkflowStepStatus};
        use crate::frontend::tui::tabs::WorkflowStepKind;

        let (mut frontend, _req_rx, _resp_tx) = make_frontend();

        frontend.on_phase_step_started(PhaseKind::Setup, "clone repo");
        frontend.on_phase_step_completed(PhaseKind::Setup, "clone repo");

        frontend.report_workflow_progress(&[WorkflowStepProgressInfo {
            name: "build".into(),
            agent: "claude".into(),
            model: None,
            has_step_override: false,
            status: WorkflowStepStatus::Running,
            depends_on: vec![],
            max_concurrent: None,
        }]);

        frontend.on_phase_step_started(PhaseKind::Teardown, "clean up");

        let guard = frontend.workflow_view.lock().unwrap();
        let view = guard.as_ref().expect("workflow view must be seeded");
        let steps: Vec<(WorkflowStepKind, &str, StepViewStatus)> = view
            .steps
            .iter()
            .map(|s| (s.kind, s.name.as_str(), s.status))
            .collect();
        assert_eq!(
            steps,
            vec![
                (WorkflowStepKind::Setup, "clone repo", StepViewStatus::Done),
                (WorkflowStepKind::Agent, "build", StepViewStatus::Running),
                (
                    WorkflowStepKind::Teardown,
                    "clean up",
                    StepViewStatus::Running
                ),
            ],
            "setup must stay ahead of the main steps and teardown behind them, \
             surviving report_workflow_progress's replace of the main-step list"
        );
    }

    #[test]
    fn a_failed_setup_step_is_reflected_in_the_workflow_view() {
        use crate::frontend::tui::tabs::WorkflowStepKind;

        let (mut frontend, _req_rx, _resp_tx) = make_frontend();

        frontend.on_phase_step_started(PhaseKind::Setup, "install deps");
        frontend.on_phase_step_failed(PhaseKind::Setup, "install deps", 1, "boom");

        let guard = frontend.workflow_view.lock().unwrap();
        let view = guard.as_ref().expect("workflow view must be seeded");
        assert_eq!(
            view.steps.len(),
            1,
            "the started step is updated in place, not duplicated"
        );
        assert_eq!(view.steps[0].kind, WorkflowStepKind::Setup);
        assert_eq!(view.steps[0].status, StepViewStatus::Error);
    }

    // ── WI-0115 §1: the failure board's dialog state ────────────────────

    #[test]
    fn control_board_state_carries_the_failure_detail_lines() {
        use crate::engine::workflow::actions::{AvailableActions, StepFailureContext};

        let available = AvailableActions {
            can_restart_current_step: true,
            can_abort: true,
            step_failure: Some(StepFailureContext {
                step_name: "implement".into(),
                exit_code: 1,
                signal: None,
                detail_lines: vec!["Exit code: 1".into(), "Ran for 12s".into()],
                previous_step: Some("design".into()),
                next_step: Some("review".into()),
            }),
            ..Default::default()
        };

        let state = super::control_board_state("implement", &available);
        assert_eq!(state.step_name, "implement");
        assert_eq!(state.failure_lines, vec!["Exit code: 1", "Ran for 12s"]);
        assert!(!state.can_finish);
        assert!(!state.can_dismiss);
    }

    #[test]
    fn control_board_state_has_no_failure_lines_between_steps() {
        use crate::engine::workflow::actions::AvailableActions;

        let available = AvailableActions {
            can_launch_next: true,
            ..Default::default()
        };
        let state = super::control_board_state("implement", &available);
        assert!(state.failure_lines.is_empty());
    }
}
