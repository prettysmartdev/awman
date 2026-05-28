//! `WorkflowFrontend` trait — defined by Layer 1, implemented by Layer 3.
//!
//! Engine-driven: the engine calls these methods to command the frontend.
//! The frontend is a pure I/O layer — it renders what the engine tells it
//! and collects user input when the engine asks for it.

use std::time::Duration;

use crate::data::workflow_definition::WorkflowStep;
use crate::data::workflow_state::WorkflowState;
use crate::engine::container::instance::ContainerExitInfo;
use crate::engine::container::instance::StuckEvent;
use crate::engine::error::EngineError;
use crate::engine::message::UserMessageSink;
use crate::engine::workflow::actions::{
    AvailableActions, NextAction, ResumeMismatch, StepFailureChoice, StepOutput, WorkflowOutcome,
    WorkflowStepProgressInfo, WorkflowStepStatus, YoloTickOutcome,
};
use crate::engine::workflow::EngineRequest;

/// Per-workflow frontend the engine uses for every Q&A and status report.
///
/// The engine treats CLI, TUI, and API implementations identically; the
/// engine never knows which is on the other side.
pub trait WorkflowFrontend: UserMessageSink + Send {
    // === Engine-driven display commands (blocking) ===

    /// Engine tells frontend to show the Workflow Control Board with these
    /// actions. Frontend collects user input and returns the chosen action.
    /// This is a BLOCKING call — the engine waits for the user's choice.
    fn show_workflow_control_board(
        &mut self,
        state: &WorkflowState,
        available: &AvailableActions,
    ) -> Result<NextAction, EngineError>;

    /// Engine tells frontend to update the yolo countdown display.
    /// Called repeatedly (every ~100ms) with the remaining time.
    /// Frontend returns whether to Continue, Cancel, or AdvanceNow.
    fn yolo_countdown_tick(
        &mut self,
        step_name: &str,
        remaining: Duration,
        total: Duration,
    ) -> Result<YoloTickOutcome, EngineError>;

    /// Engine tells frontend: yolo countdown just started for this step.
    /// Frontend should show the countdown dialog (active tab) or flash
    /// the tab header yellow/purple (background tab).
    fn yolo_countdown_started(&mut self, _step_name: &str) {}

    /// Engine tells frontend: yolo countdown finished (expired, cancelled,
    /// or step recovered). Frontend dismisses dialog / resets tab style.
    fn yolo_countdown_finished(&mut self, _step_name: &str) {}

    // === Status reporting (fire-and-forget) ===

    fn report_step_status(&mut self, step: &WorkflowStep, status: WorkflowStepStatus);

    fn report_step_output(&mut self, _step: &WorkflowStep, _output: StepOutput) {}

    fn report_workflow_completed(&mut self, outcome: &WorkflowOutcome);

    /// Called by the engine before each step and before any user-input prompt.
    /// The engine controls call ordering; the frontend renders the table.
    fn report_workflow_progress(&mut self, _steps: &[WorkflowStepProgressInfo]) {}

    /// Called by the engine after resolving the step's agent/model but before
    /// the container launches.
    fn report_step_interactive_launch(
        &mut self,
        _step: &WorkflowStep,
        _agent: &str,
        _model: Option<&str>,
    ) {
    }

    // === User decisions (blocking) ===

    fn confirm_resume(&mut self, mismatch: &ResumeMismatch) -> Result<bool, EngineError>;

    /// Called after a step transitions to Failed.
    fn user_choose_after_step_failure(
        &mut self,
        step: &WorkflowStep,
        exit: &ContainerExitInfo,
    ) -> Result<StepFailureChoice, EngineError>;

    // === Channel setup ===

    /// Called by the engine after creating its EngineRequest channel.
    /// The frontend stores the sender so the TUI event loop can route
    /// Ctrl-W requests to this specific engine instance.
    fn set_engine_sender(&mut self, _tx: tokio::sync::mpsc::UnboundedSender<EngineRequest>) {}

    /// Called by the engine after launching a step's container. The stuck
    /// sender is the broadcast channel from the container's stuck detector;
    /// the TUI subscribes to it for tab-coloring. CLI/API frontends ignore it.
    fn set_stuck_sender(
        &mut self,
        _sender: std::sync::Arc<tokio::sync::broadcast::Sender<StuckEvent>>,
    ) {
    }

    // === Setup/Teardown phase output (fire-and-forget, default no-ops) ===

    fn on_setup_step_started(&mut self, _description: &str) {}
    fn on_setup_step_output(&mut self, _line: &str) {}
    fn on_setup_step_completed(&mut self, _description: &str) {}
    fn on_setup_step_failed(&mut self, _description: &str, _exit_code: i32, _stderr: &str) {}

    fn on_teardown_step_started(&mut self, _description: &str) {}
    fn on_teardown_step_output(&mut self, _line: &str) {}
    fn on_teardown_step_completed(&mut self, _description: &str) {}
    fn on_teardown_step_failed(&mut self, _description: &str, _exit_code: i32, _stderr: &str) {}
}
