//! `WorkflowFrontend` trait — defined by Layer 1, implemented by Layer 3.
//!
//! Engine-driven: the engine calls these methods to command the frontend.
//! The frontend is a pure I/O layer — it renders what the engine tells it
//! and collects user input when the engine asks for it.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::broadcast;

use crate::data::message::UserMessageSink;
use crate::data::workflow_definition::WorkflowStep;
use crate::data::workflow_state::WorkflowState;
use crate::engine::agent_runtime::execution::AgentExitInfo;
use crate::engine::agent_runtime::execution::StuckEvent;
use crate::engine::agent_runtime::frontend::AgentIo;
use crate::engine::error::EngineError;
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

    /// Called by the engine the moment the current container has actually
    /// terminated — either it exited on its own (agent quit, crash, startup
    /// grace kill) or the engine killed it (yolo advance, WCB action).
    ///
    /// Contract: this fires ONLY on real container death. It is never called
    /// for a stuck-but-alive container or while a yolo countdown is still
    /// running. `exit_code` is the container's exit code when known, or
    /// [`KILLED_EXIT_CODE`](crate::engine::agent_runtime::execution::KILLED_EXIT_CODE)
    /// when the engine killed it without waiting for the real code.
    fn report_container_exited(&mut self, _exit_code: i32) {}

    // === User decisions (blocking) ===

    fn confirm_resume(&mut self, mismatch: &ResumeMismatch) -> Result<bool, EngineError>;

    /// Called after a step transitions to Failed.
    fn user_choose_after_step_failure(
        &mut self,
        step: &WorkflowStep,
        exit: &AgentExitInfo,
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
    /// Step failed and an `on_failure` agent is about to run. Emitted
    /// once per remediation attempt before the agent launches; the step
    /// will be retried once the agent finishes.
    fn on_setup_step_fixing(&mut self, _description: &str, _attempt: u32, _of: u32) {}

    fn on_teardown_step_started(&mut self, _description: &str) {}
    fn on_teardown_step_output(&mut self, _line: &str) {}
    fn on_teardown_step_completed(&mut self, _description: &str) {}
    fn on_teardown_step_failed(&mut self, _description: &str, _exit_code: i32, _stderr: &str) {}
    fn on_teardown_step_fixing(&mut self, _description: &str, _attempt: u32, _of: u32) {}

    // === Parallel-group commands (WI-0096) ===
    //
    // All default no-ops. The single-step path never calls these; frontends
    // that don't render multi-container UX simply ignore them. The engine owns
    // every scheduling/concurrency decision — these callbacks are pure
    // presentation notifications.

    /// Engine is launching multiple parallel containers for this group.
    /// `step_names` is the ordered list of all steps in this parallel batch
    /// (including queued ones that are not yet running).
    fn report_parallel_group_started(&mut self, _step_names: &[String]) {}

    /// One container in a parallel group has started running.
    fn report_parallel_step_launched(
        &mut self,
        _step_name: &str,
        _agent: &str,
        _model: Option<&str>,
    ) {
    }

    /// The engine learned a parallel step's actual container name (right
    /// after the container launched). Frontends use it for per-container
    /// stats polling and status-bar display.
    fn report_parallel_step_container(&mut self, _step_name: &str, _container_name: &str) {}

    /// One container in a parallel group has exited.
    /// `evict` — the frontend should remove the status bar for this step
    /// entirely (not replace it with a grey summary bar).
    fn report_parallel_step_exited(&mut self, _step_name: &str, _exit_code: i32) {}

    /// A queued step in this parallel group has started (because a slot freed up).
    fn report_parallel_step_dequeued(
        &mut self,
        _step_name: &str,
        _agent: &str,
        _model: Option<&str>,
    ) {
    }

    /// The parallel group has fully drained; all steps completed.
    fn report_parallel_group_finished(&mut self) {}

    /// Per-step stuck notification for a parallel container.
    fn report_parallel_step_stuck(&mut self, _step_name: &str) {}
    fn report_parallel_step_unstuck(&mut self, _step_name: &str) {}

    /// Per-step yolo countdown updates.
    fn parallel_step_yolo_countdown_started(&mut self, _step_name: &str) {}
    fn parallel_step_yolo_countdown_tick(
        &mut self,
        _step_name: &str,
        _remaining: Duration,
        _total: Duration,
    ) -> Result<YoloTickOutcome, EngineError> {
        Ok(YoloTickOutcome::Continue)
    }
    fn parallel_step_yolo_countdown_finished(&mut self, _step_name: &str) {}

    /// Set per-step I/O channels. Called once per parallel step launch.
    fn set_parallel_step_io(&mut self, _step_name: &str, _io: AgentIo) {}

    /// Set per-step stuck sender (one per active parallel container).
    fn set_parallel_step_stuck_sender(
        &mut self,
        _step_name: &str,
        _sender: Arc<broadcast::Sender<StuckEvent>>,
    ) {
    }
}
