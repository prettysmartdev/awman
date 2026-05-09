//! `WorkflowFrontend` trait — defined by Layer 1, implemented by Layer 3.

use std::time::Duration;

use crate::data::workflow_definition::WorkflowStep;
use crate::data::workflow_state::WorkflowState;
use crate::engine::container::instance::ContainerExitInfo;
use crate::engine::error::EngineError;
use crate::engine::message::UserMessageSink;
use crate::engine::workflow::actions::{
    AvailableActions, NextAction, ResumeMismatch, StepFailureChoice, StepOutput, WorkflowOutcome,
    WorkflowStepProgressInfo, WorkflowStepStatus, YoloTickOutcome,
};

/// Per-workflow frontend the engine uses for every Q&A and status report.
///
/// The engine treats CLI, TUI, and headless implementations identically; the
/// engine never knows which is on the other side.
pub trait WorkflowFrontend: UserMessageSink + Send {
    fn user_choose_next_action(
        &mut self,
        state: &WorkflowState,
        available: &AvailableActions,
    ) -> Result<NextAction, EngineError>;

    fn confirm_resume(&mut self, mismatch: &ResumeMismatch) -> Result<bool, EngineError>;

    /// Called after a step transitions to `Failed`. Default behaviors:
    ///   - Retry → engine reverts the step to Pending and re-runs.
    ///   - Pause → engine persists state and returns from `step_once`.
    ///   - Abort → engine marks remaining steps Cancelled and returns.
    fn user_choose_after_step_failure(
        &mut self,
        step: &WorkflowStep,
        exit: &ContainerExitInfo,
    ) -> Result<StepFailureChoice, EngineError>;

    fn report_step_status(&mut self, step: &WorkflowStep, status: WorkflowStepStatus);

    fn report_step_output(&mut self, step: &WorkflowStep, output: StepOutput);

    /// Called once when stuck-detection fires for the current step. The engine
    /// continues running the step; the frontend SHOULD render a stuck indicator.
    fn report_step_stuck(&mut self, step: &WorkflowStep);

    /// Called once when stuck-detection clears.
    fn report_step_unstuck(&mut self, step: &WorkflowStep);

    /// Called repeatedly while a yolo countdown is ticking down.
    fn yolo_countdown_tick(&mut self, remaining: Duration) -> Result<YoloTickOutcome, EngineError>;

    /// Reset the yolo-initialized flag so a new countdown starts fresh.
    /// Called at the beginning of each mid-step yolo countdown.
    fn reset_yolo_initialized(&mut self) {}

    /// Clear the shared yolo state after a countdown finishes (advanced,
    /// cancelled, or step completed). Prevents stale state from being
    /// rendered.
    fn clear_yolo_state(&mut self) {}

    fn report_workflow_completed(&mut self, outcome: &WorkflowOutcome);

    /// Called by the engine before each step runs and before any yolo countdown
    /// or user-input prompt. The engine controls the call ordering; the frontend
    /// renders the table. Default implementation is a no-op (e.g. for tests).
    fn report_workflow_progress(&mut self, _steps: &[WorkflowStepProgressInfo]) {}

    /// Called by the engine after resolving the step's agent/model but before
    /// the container launches. When stdin is a TTY the CLI frontend prints the
    /// interactive-mode ASCII banner. Default implementation is a no-op.
    fn report_step_interactive_launch(
        &mut self,
        _step: &WorkflowStep,
        _agent: &str,
        _model: Option<&str>,
    ) {
    }

    /// Whether the given step should auto-advance (yolo countdown). Returns
    /// `true` by default so CLI/headless frontends always auto-advance. The
    /// TUI overrides this to respect the per-step `[d]` toggle.
    fn should_auto_advance(&self, _step_name: &str) -> bool {
        true
    }

    /// Called by the engine after creating the control-board channel. The
    /// frontend stores the sender so the TUI event loop can open the WCB
    /// mid-step. Default is a no-op (CLI/headless don't need this).
    fn set_control_board_sender(
        &mut self,
        _tx: tokio::sync::mpsc::UnboundedSender<crate::engine::workflow::ControlBoardRequest>,
    ) {
    }
}
