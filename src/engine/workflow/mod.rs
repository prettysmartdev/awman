//! `engine::workflow` — `WorkflowEngine`.
//!
//! Owns every workflow-execution concern: state, advance logic, yolo
//! countdowns, agent/model resolution, exit-code interpretation, persistence,
//! and per-step container lifecycle. Forbidden: rendering, direct user
//! input, knowledge of which frontend is on the other side of the trait,
//! worktree lifecycle management, direct container construction.
//!
//! The engine is the single source of truth for ALL workflow state.
//! No workflow execution state lives in the frontend — zero, none.

use std::sync::Arc;

use crate::data::config::effective::EffectiveConfig;
use crate::data::session::{AgentName, Session};
use crate::data::workflow_dag::WorkflowDag;
use crate::data::workflow_definition::{Workflow, WorkflowStep};
use crate::data::workflow_state::{StepState, WorkflowState, WORKFLOW_STATE_SCHEMA_VERSION};
use crate::data::workflow_state_store::WorkflowStateStore;
use crate::engine::container::instance::{ContainerExecution, ContainerExitInfo, StuckEvent};
use crate::engine::container::ContainerExec;
use crate::engine::error::EngineError;
use crate::engine::git::GitEngine;
use crate::engine::overlay::OverlayEngine;
use crate::engine::workflow::actions::{
    AvailableActions, NextAction, ResumeMismatch, StepFailureChoice, StepOutcome, WorkflowOutcome,
    WorkflowStepProgressInfo, WorkflowStepStatus, YoloTickOutcome,
};
use crate::engine::workflow::factory::{ContainerExecutionFactory, WorkflowRuntimeContext};
use crate::engine::workflow::frontend::WorkflowFrontend;

pub mod actions;
pub mod factory;
pub mod frontend;
pub mod step_commands;
pub mod timing;

/// Result of a mid-step yolo countdown (step is still running while
/// the countdown ticks).
enum MidStepYoloResult {
    /// Step completed while the countdown was ticking.
    StepCompleted(StepOutcome),
    /// Countdown expired or user pressed AdvanceNow.
    Advanced,
    /// User pressed Esc: cancel the countdown.
    Cancelled,
    /// User pressed Ctrl-W: show the WCB instead.
    ShowControlBoard,
    /// Container recovered (StepUnstuck received).
    Recovered,
}

/// Result of mid-step control board interaction.
enum MidStepOutcome {
    /// User dismissed the dialog — resume waiting on the step.
    Continue,
    /// Step completed while dialog was open; outcome is ready.
    StepCompleted(StepOutcome),
    /// User chose a workflow-level action (pause/abort/finish).
    WorkflowEnded(WorkflowOutcome),
    /// User chose an action that re-enters the loop (restart/advance/etc).
    LoopContinue,
}

/// Result of `step_once_interruptible`.
enum InterruptibleStepResult {
    /// Step completed (naturally or while dialog was open).
    StepCompleted(StepOutcome),
    /// Mid-step action ended the workflow.
    WorkflowEnded(WorkflowOutcome),
    /// Mid-step action requires the outer loop to continue (restart/advance).
    LoopContinue,
}

pub use actions::{
    StepOutput, StepOutputKind, WorkflowOutcome as Outcome, WorkflowStepStatus as Status,
};
pub use factory::{ContainerExecutionFactory as Factory, WorkflowRuntimeContext as RuntimeContext};
pub use frontend::WorkflowFrontend as Frontend;

/// Request sent from the TUI event loop (via per-tab channel) to the engine.
///
/// The frontend detects stuck/unstuck state and routes user actions;
/// the engine decides the response.
#[derive(Debug, Clone)]
pub enum EngineRequest {
    /// User pressed Ctrl-W. Engine should show the WCB.
    OpenControlBoard,
    /// Frontend detected that the current step's container is stuck
    /// (no PTY output for STUCK_TIMEOUT). Engine responds: if --yolo,
    /// start yolo countdown; if not --yolo, open WCB.
    StepStuck,
    /// Frontend detected that the container is no longer stuck (new
    /// PTY output arrived). Engine cancels any active yolo countdown.
    StepUnstuck,
}

pub struct WorkflowEngine {
    session: Session,
    workflow: Workflow,
    dag: WorkflowDag,
    state: WorkflowState,
    state_store: WorkflowStateStore,
    effective_config: EffectiveConfig,
    frontend: Box<dyn WorkflowFrontend>,
    container_factory: Box<dyn ContainerExecutionFactory>,
    git_engine: Arc<GitEngine>,
    overlay_engine: Arc<OverlayEngine>,
    current_execution: Option<ContainerExecution>,
    current_step_name: Option<String>,
    current_step_agent: Option<AgentName>,
    current_step_model: Option<String>,
    work_item_context: Option<crate::data::workflow_prompt_template::WorkItemContext>,
    yolo: bool,
    abort_on_failure_triggered: bool,
    last_exit_info: Option<ContainerExitInfo>,
    engine_rx: Option<tokio::sync::mpsc::UnboundedReceiver<EngineRequest>>,
}

impl WorkflowEngine {
    fn msg_info(&mut self, text: impl Into<String>) {
        self.frontend
            .write_message(crate::engine::message::UserMessage {
                level: crate::engine::message::MessageLevel::Info,
                text: text.into(),
            });
    }
    fn msg_warning(&mut self, text: impl Into<String>) {
        self.frontend
            .write_message(crate::engine::message::UserMessage {
                level: crate::engine::message::MessageLevel::Warning,
                text: text.into(),
            });
    }
    fn msg_success(&mut self, text: impl Into<String>) {
        self.frontend
            .write_message(crate::engine::message::UserMessage {
                level: crate::engine::message::MessageLevel::Success,
                text: text.into(),
            });
    }

    pub fn new(
        session: &Session,
        workflow: Workflow,
        work_item_context: Option<crate::data::workflow_prompt_template::WorkItemContext>,
        mut frontend: Box<dyn WorkflowFrontend>,
        container_factory: Box<dyn ContainerExecutionFactory>,
        git_engine: Arc<GitEngine>,
        overlay_engine: Arc<OverlayEngine>,
    ) -> Result<Self, EngineError> {
        let dag = WorkflowDag::build(&workflow.steps).map_err(EngineError::Data)?;
        let workflow_hash = compute_workflow_hash(&workflow);
        let work_item_number = work_item_context.as_ref().map(|c| c.number);
        let state = WorkflowState::new(
            workflow_name_for(&workflow),
            &workflow.steps,
            workflow_hash,
            work_item_number,
        );
        let state_store = WorkflowStateStore::new(session);
        let effective_config = session.effective_config();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        frontend.set_engine_sender(tx);
        Ok(Self {
            session: session.clone(),
            workflow,
            dag,
            state,
            state_store,
            effective_config,
            frontend,
            container_factory,
            git_engine,
            overlay_engine,
            current_execution: None,
            current_step_name: None,
            current_step_agent: None,
            current_step_model: None,
            work_item_context,
            yolo: false,
            abort_on_failure_triggered: false,
            last_exit_info: None,
            engine_rx: Some(rx),
        })
    }

    pub fn abort_on_failure_triggered(&self) -> bool {
        self.abort_on_failure_triggered
    }

    pub fn set_yolo(&mut self, yolo: bool) {
        self.yolo = yolo;
    }

    /// Resume from persisted state. Calls `confirm_resume` on the frontend if
    /// the workflow hash has drifted.
    pub async fn resume(
        session: &Session,
        workflow: Workflow,
        work_item_context: Option<crate::data::workflow_prompt_template::WorkItemContext>,
        mut frontend: Box<dyn WorkflowFrontend>,
        container_factory: Box<dyn ContainerExecutionFactory>,
        git_engine: Arc<GitEngine>,
        overlay_engine: Arc<OverlayEngine>,
    ) -> Result<Self, EngineError> {
        let dag = WorkflowDag::build(&workflow.steps).map_err(EngineError::Data)?;
        let store = WorkflowStateStore::new(session);
        let workflow_name = workflow_name_for(&workflow);
        let work_item_number = work_item_context.as_ref().map(|c| c.number);
        let saved = store.load(work_item_number, &workflow_name)?;

        let workflow_hash = compute_workflow_hash(&workflow);
        let mut state = match saved {
            Some(saved) => {
                if saved.schema_version > WORKFLOW_STATE_SCHEMA_VERSION {
                    return Err(EngineError::UnsupportedWorkflowSchemaVersion {
                        found: saved.schema_version,
                        supported: WORKFLOW_STATE_SCHEMA_VERSION,
                    });
                }
                if saved.workflow_hash != workflow_hash {
                    let mismatch = ResumeMismatch {
                        workflow_name: workflow_name.clone(),
                        saved_hash: saved.workflow_hash.clone(),
                        current_hash: workflow_hash.clone(),
                        message: "workflow source has changed since the saved run".into(),
                    };
                    if !frontend.confirm_resume(&mismatch)? {
                        return Err(EngineError::WorkflowResumeIncompatible(
                            "user declined to resume against drifted workflow".into(),
                        ));
                    }
                }
                saved
            }
            None => WorkflowState::new(
                workflow_name,
                &workflow.steps,
                workflow_hash,
                work_item_number,
            ),
        };

        let interrupted = state.interrupted_running_steps();
        if !interrupted.is_empty() {
            frontend.write_message(crate::engine::message::UserMessage {
                level: crate::engine::message::MessageLevel::Warning,
                text: format!(
                    "Interrupted steps detected (prior crash?): {}. Resetting to Pending.",
                    interrupted.join(", "),
                ),
            });
            for name in &interrupted {
                state.set_status(name, StepState::Pending);
            }
        }

        let effective_config = session.effective_config();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        frontend.set_engine_sender(tx);
        Ok(Self {
            session: session.clone(),
            workflow,
            dag,
            state,
            state_store: store,
            effective_config,
            frontend,
            container_factory,
            git_engine,
            overlay_engine,
            current_execution: None,
            current_step_name: None,
            current_step_agent: None,
            current_step_model: None,
            work_item_context,
            yolo: false,
            abort_on_failure_triggered: false,
            last_exit_info: None,
            engine_rx: Some(rx),
        })
    }

    pub fn state(&self) -> &WorkflowState {
        &self.state
    }

    /// Drive every step until the workflow finishes, the user pauses, or a
    /// step fails terminally.
    pub async fn run_to_completion(&mut self) -> Result<WorkflowOutcome, EngineError> {
        let completed_count = self.state.completed_steps.len();
        let total_count = self.workflow.steps.len();
        if completed_count > 0 {
            self.msg_info(format!(
                "Resuming workflow '{}' ({}/{} steps completed)",
                self.state.workflow_name, completed_count, total_count,
            ));
        } else {
            self.msg_info(format!(
                "Starting workflow '{}' ({} steps)",
                self.state.workflow_name, total_count,
            ));
        }

        let initial_progress = self.workflow_progress_info();
        self.frontend.report_workflow_progress(&initial_progress);

        loop {
            if self.state.is_complete() {
                let progress = self.workflow_progress_info();
                self.frontend.report_workflow_progress(&progress);
                self.msg_success(format!(
                    "Workflow '{}' completed successfully",
                    self.state.workflow_name,
                ));
                let outcome = WorkflowOutcome::Completed;
                self.frontend.report_workflow_completed(&outcome);
                return Ok(outcome);
            }

            let interruptible_result = self.step_once_interruptible().await?;
            let outcome = match interruptible_result {
                InterruptibleStepResult::StepCompleted(o) => o,
                InterruptibleStepResult::WorkflowEnded(wo) => return Ok(wo),
                InterruptibleStepResult::LoopContinue => continue,
            };

            if let WorkflowStepStatus::Failed { exit_code } = outcome.status {
                let progress = self.workflow_progress_info();
                self.frontend.report_workflow_progress(&progress);

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
                    return Ok(aborted);
                }

                let exit_info = self
                    .last_exit_info
                    .clone()
                    .unwrap_or_else(|| ContainerExitInfo {
                        exit_code,
                        signal: None,
                        started_at: chrono::Utc::now(),
                        ended_at: chrono::Utc::now(),
                    });
                let choice = self
                    .frontend
                    .user_choose_after_step_failure(&step, &exit_info)?;
                match choice {
                    StepFailureChoice::Retry => {
                        self.msg_info(format!("Retrying step '{}'", outcome.step_name,));
                        self.state
                            .set_status(&outcome.step_name, StepState::Pending);
                        self.persist()?;
                        continue;
                    }
                    StepFailureChoice::Pause => {
                        self.msg_info("Workflow paused");
                        self.persist()?;
                        let paused = WorkflowOutcome::Paused;
                        self.frontend.report_workflow_completed(&paused);
                        return Ok(paused);
                    }
                    StepFailureChoice::Abort => {
                        self.msg_warning("Workflow aborted");
                        for s in &self.workflow.steps {
                            if !self.state.completed_steps.contains(&s.name) {
                                self.state.set_status(&s.name, StepState::Cancelled);
                            }
                        }
                        self.persist()?;
                        let aborted = WorkflowOutcome::Aborted;
                        self.frontend.report_workflow_completed(&aborted);
                        return Ok(aborted);
                    }
                }
            }

            // Step succeeded. Decide what to do next.
            let workflow_just_completed = self.state.is_complete();

            if !workflow_just_completed {
                let progress = self.workflow_progress_info();
                self.frontend.report_workflow_progress(&progress);

                if self.yolo {
                    continue;
                }
            } else if self.yolo {
                // Last step in yolo mode: always require explicit user
                // confirmation before ending the workflow so the user can
                // review the final step's output.
                let progress = self.workflow_progress_info();
                self.frontend.report_workflow_progress(&progress);
            }

            if !workflow_just_completed || self.yolo {
                let available = self.compute_available_actions()?;
                let action = self
                    .frontend
                    .show_workflow_control_board(&self.state, &available)?;
                self.log_wcb_action(&action);
                match action {
                    NextAction::Dismiss | NextAction::LaunchNext => continue,
                    NextAction::ContinueInCurrentContainer { prompt } => {
                        self.handle_continue_in_current_container(&prompt)?;
                        continue;
                    }
                    NextAction::RestartCurrentStep => {
                        if let Some(name) = self.current_step_name.clone() {
                            self.state.set_status(&name, StepState::Pending);
                            self.persist()?;
                        }
                        continue;
                    }
                    NextAction::CancelToPreviousStep => {
                        self.handle_cancel_to_previous()?;
                        continue;
                    }
                    NextAction::FinishWorkflow => {
                        return self.handle_finish_workflow();
                    }
                    NextAction::Pause => {
                        self.persist()?;
                        let outcome = WorkflowOutcome::Paused;
                        self.frontend.report_workflow_completed(&outcome);
                        return Ok(outcome);
                    }
                    NextAction::Abort => {
                        return self.handle_abort();
                    }
                }
            }
        }
    }

    /// Advance exactly one step, reporting status through the frontend.
    pub async fn step_once(&mut self) -> Result<StepOutcome, EngineError> {
        let step_name = self.launch_step().await?;
        let exit = {
            let exec = self
                .current_execution
                .as_mut()
                .expect("launch_step stored execution");
            exec.wait().await?
        };
        self.finalize_step(&step_name, exit)
    }

    async fn launch_step(&mut self) -> Result<String, EngineError> {
        let ready = self.state.next_ready(&self.dag);
        let step_name = ready
            .first()
            .cloned()
            .ok_or_else(|| EngineError::InvalidAdvanceAction("no ready steps remaining".into()))?;
        let step = self.find_step(&step_name)?;

        let resolved_agent = self.resolve_agent(&step)?;
        let resolved_model = self.resolve_model(&step);
        tracing::info!(
            step = %step.name,
            agent = %resolved_agent.as_str(),
            model = ?resolved_model,
            "workflow_engine resolved step parameters"
        );

        let runtime = WorkflowRuntimeContext {
            step_agent: resolved_agent.clone(),
            step_model: resolved_model.clone(),
            git_root: self.session.git_root().to_path_buf(),
            session_id: self.session.id(),
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

        let execution =
            self.container_factory
                .execution_for_step(&step, &self.session, &runtime)?;

        self.state.set_status(
            &step.name,
            StepState::Running {
                container_id: Some(execution.handle().id.clone()),
            },
        );
        self.persist()?;

        self.current_execution = Some(execution);
        self.current_step_name = Some(step.name.clone());
        self.current_step_agent = Some(resolved_agent);
        self.current_step_model = resolved_model;
        Ok(step.name)
    }

    fn finalize_step(
        &mut self,
        step_name: &str,
        exit: ContainerExitInfo,
    ) -> Result<StepOutcome, EngineError> {
        self.last_exit_info = Some(exit.clone());

        let (status, step_state) = if exit.exit_code == 0 {
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
    async fn step_once_interruptible(&mut self) -> Result<InterruptibleStepResult, EngineError> {
        let step_name = self.launch_step().await?;

        let cancel_handle = self
            .current_execution
            .as_ref()
            .and_then(|e| e.cancel_handle());

        // Subscribe to stuck/unstuck events from the container's io_bridge.
        let mut stuck_rx = self.current_execution.as_ref().map(|e| e.subscribe_stuck());

        // Publish the stuck sender to the frontend (TUI uses it for tab coloring).
        if let Some(exec) = self.current_execution.as_ref() {
            self.frontend.set_stuck_sender(exec.stuck_sender());
        }

        let mut exec = self
            .current_execution
            .take()
            .expect("launch_step stored execution");
        let (wait_tx, mut wait_rx) = tokio::sync::oneshot::channel::<(
            ContainerExecution,
            Result<ContainerExitInfo, EngineError>,
        )>();
        tokio::spawn(async move {
            let result = exec.wait().await;
            let _ = wait_tx.send((exec, result));
        });

        loop {
            tokio::select! {
                biased;
                result = &mut wait_rx => {
                    let (exec_back, exit_result) = result
                        .map_err(|_| EngineError::Other("step wait task dropped unexpectedly".into()))?;
                    self.current_execution = Some(exec_back);
                    return Ok(InterruptibleStepResult::StepCompleted(
                        self.finalize_step(&step_name, exit_result?)?
                    ));
                }
                Some(event) = Self::recv_stuck(&mut stuck_rx) => {
                    match event {
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
                            // records the failure.
                            self.msg_warning(format!(
                                "Step '{}' produced no output before its startup grace expired; killing container",
                                step_name,
                            ));
                        }
                    }
                }
                Some(req) = Self::recv_engine(&mut self.engine_rx) => {
                    match req {
                        EngineRequest::OpenControlBoard => {
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
                        EngineRequest::StepStuck => {
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
                        EngineRequest::StepUnstuck => {
                            // Not inside a yolo countdown — nothing to cancel.
                        }
                    }
                }
            }
        }
    }

    /// Receive from the engine channel, or pend forever if None.
    async fn recv_engine(
        rx: &mut Option<tokio::sync::mpsc::UnboundedReceiver<EngineRequest>>,
    ) -> Option<EngineRequest> {
        match rx {
            Some(rx) => rx.recv().await,
            None => std::future::pending().await,
        }
    }

    /// Receive from the stuck broadcast channel, or pend forever if None.
    async fn recv_stuck(
        rx: &mut Option<tokio::sync::broadcast::Receiver<StuckEvent>>,
    ) -> Option<StuckEvent> {
        match rx {
            Some(rx) => rx.recv().await.ok(),
            None => std::future::pending().await,
        }
    }

    fn handle_mid_step_control_board(
        &mut self,
        step_name: &str,
        cancel_handle: &Option<crate::engine::container::instance::CancelHandle>,
        wait_rx: &mut tokio::sync::oneshot::Receiver<(
            ContainerExecution,
            Result<ContainerExitInfo, EngineError>,
        )>,
    ) -> Result<MidStepOutcome, EngineError> {
        let available = self.compute_available_actions()?;
        let action = self
            .frontend
            .show_workflow_control_board(&self.state, &available)?;

        self.log_wcb_action(&action);

        let already_finished = match wait_rx.try_recv() {
            Ok((exec_back, exit_result)) => {
                self.current_execution = Some(exec_back);
                Some(exit_result)
            }
            Err(_) => None,
        };

        match action {
            NextAction::Dismiss => {
                if let Some(exit_result) = already_finished {
                    return Ok(MidStepOutcome::StepCompleted(
                        self.finalize_step(step_name, exit_result?)?,
                    ));
                }
                Ok(MidStepOutcome::Continue)
            }
            NextAction::ContinueInCurrentContainer { prompt } => {
                if let Some(exec) = self.current_execution.as_ref() {
                    let _ = self.container_factory.inject_prompt(exec, &prompt);
                }
                if let Some(exit_result) = already_finished {
                    return Ok(MidStepOutcome::StepCompleted(
                        self.finalize_step(step_name, exit_result?)?,
                    ));
                }
                Ok(MidStepOutcome::Continue)
            }
            NextAction::Pause => {
                if already_finished.is_none() {
                    if let Some(ch) = cancel_handle {
                        let _ = ch.cancel();
                    }
                }
                self.state.set_status(step_name, StepState::Pending);
                self.persist()?;
                let outcome = WorkflowOutcome::Paused;
                self.frontend.report_workflow_completed(&outcome);
                Ok(MidStepOutcome::WorkflowEnded(outcome))
            }
            NextAction::Abort => {
                if already_finished.is_none() {
                    if let Some(ch) = cancel_handle {
                        let _ = ch.cancel();
                    }
                }
                for s in &self.workflow.steps {
                    if !self.state.completed_steps.contains(&s.name) {
                        self.state.set_status(&s.name, StepState::Cancelled);
                    }
                }
                self.persist()?;
                let outcome = WorkflowOutcome::Aborted;
                self.frontend.report_workflow_completed(&outcome);
                Ok(MidStepOutcome::WorkflowEnded(outcome))
            }
            NextAction::FinishWorkflow => {
                if !self.is_last_step() {
                    return Err(EngineError::InvalidAdvanceAction(
                        "FinishWorkflow only valid on the last step".into(),
                    ));
                }
                if already_finished.is_none() {
                    if let Some(ch) = cancel_handle {
                        let _ = ch.cancel();
                    }
                }
                for s in &self.workflow.steps {
                    if !self.state.completed_steps.contains(&s.name) {
                        self.state.set_status(&s.name, StepState::Skipped);
                    }
                }
                self.persist()?;
                let outcome = WorkflowOutcome::Completed;
                self.frontend.report_workflow_completed(&outcome);
                Ok(MidStepOutcome::WorkflowEnded(outcome))
            }
            NextAction::LaunchNext => {
                if already_finished.is_none() {
                    if let Some(ch) = cancel_handle {
                        let _ = ch.cancel();
                    }
                }
                self.state.set_status(step_name, StepState::Succeeded);
                self.persist()?;
                Ok(MidStepOutcome::LoopContinue)
            }
            NextAction::RestartCurrentStep => {
                if already_finished.is_none() {
                    if let Some(ch) = cancel_handle {
                        let _ = ch.cancel();
                    }
                }
                self.state.set_status(step_name, StepState::Pending);
                self.persist()?;
                Ok(MidStepOutcome::LoopContinue)
            }
            NextAction::CancelToPreviousStep => {
                if already_finished.is_none() {
                    if let Some(ch) = cancel_handle {
                        let _ = ch.cancel();
                    }
                }
                if let Some(prev) = self.previous_step_name() {
                    self.state.set_status(step_name, StepState::Cancelled);
                    self.state.set_status(&prev, StepState::Pending);
                    self.persist()?;
                }
                Ok(MidStepOutcome::LoopContinue)
            }
        }
    }

    /// Handle a stuck event (from broadcast channel or EngineRequest).
    /// Returns `None` to continue the select loop, or `Some(result)` to return.
    async fn handle_step_stuck(
        &mut self,
        step_name: &str,
        cancel_handle: &Option<crate::engine::container::instance::CancelHandle>,
        wait_rx: &mut tokio::sync::oneshot::Receiver<(
            ContainerExecution,
            Result<ContainerExitInfo, EngineError>,
        )>,
        stuck_rx: &mut Option<tokio::sync::broadcast::Receiver<StuckEvent>>,
    ) -> Result<Option<InterruptibleStepResult>, EngineError> {
        self.msg_warning(format!("Step '{}' appears stuck (no output)", step_name,));
        if self.yolo && !self.is_last_step() {
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
                MidStepYoloResult::Advanced => {
                    self.msg_info(format!("Yolo auto-advancing past step '{}'", step_name,));
                    if let Some(ch) = cancel_handle {
                        let _ = ch.cancel();
                    }
                    self.state.set_status(step_name, StepState::Succeeded);
                    self.persist()?;
                    let step = self.find_step(step_name)?;
                    self.frontend
                        .report_step_status(&step, WorkflowStepStatus::Succeeded);
                    let progress = self.workflow_progress_info();
                    self.frontend.report_workflow_progress(&progress);

                    if self.is_last_step() {
                        let available = self.compute_available_actions()?;
                        let action = self
                            .frontend
                            .show_workflow_control_board(&self.state, &available)?;
                        return Ok(Some(self.execute_top_level_action(action)?));
                    }

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
    async fn run_mid_step_yolo_countdown(
        &mut self,
        step_name: &str,
        _cancel_handle: &Option<crate::engine::container::instance::CancelHandle>,
        wait_rx: &mut tokio::sync::oneshot::Receiver<(
            ContainerExecution,
            Result<ContainerExitInfo, EngineError>,
        )>,
        stuck_rx: &mut Option<tokio::sync::broadcast::Receiver<StuckEvent>>,
    ) -> Result<MidStepYoloResult, EngineError> {
        self.msg_info(format!(
            "Starting yolo countdown for step '{}' ({}s)",
            step_name,
            timing::YOLO_COUNTDOWN_DURATION.as_secs(),
        ));
        self.frontend.yolo_countdown_started(step_name);
        let total = timing::YOLO_COUNTDOWN_DURATION;
        let start = std::time::Instant::now();

        loop {
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
                    let (exec_back, exit_result) = result
                        .map_err(|_| EngineError::Other("step wait task dropped unexpectedly".into()))?;
                    self.current_execution = Some(exec_back);
                    self.frontend.yolo_countdown_finished(step_name);
                    return Ok(MidStepYoloResult::StepCompleted(
                        self.finalize_step(step_name, exit_result?)?
                    ));
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
                        EngineRequest::OpenControlBoard => {
                            self.frontend.yolo_countdown_finished(step_name);
                            return Ok(MidStepYoloResult::ShowControlBoard);
                        }
                        EngineRequest::StepUnstuck => {
                            self.msg_info(format!(
                                "Step '{}' recovered (engine request), cancelling countdown",
                                step_name,
                            ));
                            self.frontend.yolo_countdown_finished(step_name);
                            return Ok(MidStepYoloResult::Recovered);
                        }
                        EngineRequest::StepStuck => {
                            // Already counting down; ignore duplicate.
                        }
                    }
                }
                _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {}
            }
        }
    }

    /// Execute a top-level action from the WCB (used after yolo auto-advance
    /// on the last step, and in run_to_completion inter-step transitions).
    fn execute_top_level_action(
        &mut self,
        action: NextAction,
    ) -> Result<InterruptibleStepResult, EngineError> {
        match action {
            NextAction::Dismiss | NextAction::LaunchNext => {
                Ok(InterruptibleStepResult::LoopContinue)
            }
            NextAction::FinishWorkflow => {
                let wo = self.handle_finish_workflow()?;
                Ok(InterruptibleStepResult::WorkflowEnded(wo))
            }
            NextAction::Pause => {
                self.persist()?;
                let outcome = WorkflowOutcome::Paused;
                self.frontend.report_workflow_completed(&outcome);
                Ok(InterruptibleStepResult::WorkflowEnded(outcome))
            }
            NextAction::Abort => {
                let wo = self.handle_abort()?;
                Ok(InterruptibleStepResult::WorkflowEnded(wo))
            }
            NextAction::RestartCurrentStep => {
                if let Some(name) = self.current_step_name.clone() {
                    self.state.set_status(&name, StepState::Pending);
                    self.persist()?;
                }
                Ok(InterruptibleStepResult::LoopContinue)
            }
            NextAction::CancelToPreviousStep => {
                self.handle_cancel_to_previous()?;
                Ok(InterruptibleStepResult::LoopContinue)
            }
            NextAction::ContinueInCurrentContainer { prompt } => {
                self.handle_continue_in_current_container(&prompt)?;
                Ok(InterruptibleStepResult::LoopContinue)
            }
        }
    }

    fn handle_finish_workflow(&mut self) -> Result<WorkflowOutcome, EngineError> {
        if !self.is_last_step() {
            return Err(EngineError::InvalidAdvanceAction(
                "FinishWorkflow only valid on the last step".into(),
            ));
        }
        let skipped: Vec<String> = self
            .workflow
            .steps
            .iter()
            .filter(|s| !self.state.completed_steps.contains(&s.name))
            .map(|s| s.name.clone())
            .collect();
        for name in &skipped {
            self.state.set_status(name, StepState::Skipped);
        }
        if !skipped.is_empty() {
            self.msg_info(format!("Skipping remaining steps: {}", skipped.join(", "),));
        }
        self.persist()?;
        self.msg_success(format!("Workflow '{}' completed", self.state.workflow_name,));
        let outcome = WorkflowOutcome::Completed;
        self.frontend.report_workflow_completed(&outcome);
        Ok(outcome)
    }

    fn handle_abort(&mut self) -> Result<WorkflowOutcome, EngineError> {
        self.msg_warning("Workflow aborted");
        for s in &self.workflow.steps {
            if !self.state.completed_steps.contains(&s.name) {
                self.state.set_status(&s.name, StepState::Cancelled);
            }
        }
        self.persist()?;
        let outcome = WorkflowOutcome::Aborted;
        self.frontend.report_workflow_completed(&outcome);
        Ok(outcome)
    }

    fn log_wcb_action(&mut self, action: &NextAction) {
        let step = self.current_step_name.as_deref().unwrap_or("unknown");
        match action {
            NextAction::Dismiss => {}
            NextAction::LaunchNext => {
                self.msg_info("Advancing to next step");
            }
            NextAction::ContinueInCurrentContainer { .. } => {
                self.msg_info(format!(
                    "Continuing in current container for next step (from '{}')",
                    step,
                ));
            }
            NextAction::RestartCurrentStep => {
                self.msg_info(format!("Restarting step '{}'", step));
            }
            NextAction::CancelToPreviousStep => {
                self.msg_info(format!("Cancelling step '{}', returning to previous", step,));
            }
            NextAction::FinishWorkflow => {
                self.msg_info("Finishing workflow");
            }
            NextAction::Pause => {
                self.msg_info("Workflow paused");
            }
            NextAction::Abort => {
                self.msg_warning("Workflow aborted");
            }
        }
    }

    fn handle_cancel_to_previous(&mut self) -> Result<(), EngineError> {
        let prev = self.previous_step_name();
        match prev {
            Some(prev) => {
                if let Some(curr) = self.current_step_name.clone() {
                    self.state.set_status(&curr, StepState::Cancelled);
                }
                self.state.set_status(&prev, StepState::Pending);
                self.persist()?;
                Ok(())
            }
            None => Err(EngineError::InvalidAdvanceAction(
                "no previous step to cancel to".into(),
            )),
        }
    }

    fn handle_continue_in_current_container(&mut self, prompt: &str) -> Result<(), EngineError> {
        let next_step = match self.next_ready_step()? {
            Some(s) => s,
            None => {
                return Err(EngineError::InvalidAdvanceAction(
                    "ContinueInCurrentContainer: no next step is ready".into(),
                ))
            }
        };
        let next_agent = self.resolve_agent(&next_step)?;
        let next_model = self.resolve_model(&next_step);
        let agent_ok = self
            .current_step_agent
            .as_ref()
            .map(|a| *a == next_agent)
            .unwrap_or(false);
        let model_ok = self.current_step_model == next_model;
        if !agent_ok || !model_ok {
            return Err(EngineError::InvalidAdvanceAction(
                "ContinueInCurrentContainer requires the same agent and model \
                 for the current and next steps"
                    .into(),
            ));
        }
        match &self.current_execution {
            Some(exec) => match self.container_factory.inject_prompt(exec, prompt)? {
                Some(()) => {
                    self.state.set_status(&next_step.name, StepState::Succeeded);
                    self.current_step_name = Some(next_step.name.clone());
                    self.persist()?;
                    Ok(())
                }
                None => Err(EngineError::InvalidAdvanceAction(
                    "container backend does not support prompt injection; \
                         use LaunchNext to start a fresh container"
                        .into(),
                )),
            },
            None => Err(EngineError::InvalidAdvanceAction(
                "no container execution is available to inject into".into(),
            )),
        }
    }

    pub fn compute_available_actions(&self) -> Result<AvailableActions, EngineError> {
        let mut a = AvailableActions {
            can_launch_next: !self.state.is_complete(),
            can_restart_current_step: self.current_step_name.is_some(),
            can_pause: true,
            can_abort: true,
            can_finish_workflow: self.is_last_step(),
            can_dismiss: self.current_execution.is_some() || self.current_step_name.is_some(),
            ..Default::default()
        };
        if let Some(next) = self.next_ready_step()? {
            let next_agent = self.resolve_agent(&next)?;
            let next_model = self.resolve_model(&next);
            let ok = match (&self.current_step_agent, &self.current_step_model) {
                (Some(curr_a), curr_m) => *curr_a == next_agent && *curr_m == next_model,
                _ => false,
            };
            if ok && self.current_execution.is_some() {
                a.can_continue_in_current_container = true;
                a.continue_prompt = Some(next.prompt_template.clone());
            } else {
                a.continue_unavailable_reason = Some(if self.current_step_agent.is_none() {
                    "no current container".into()
                } else {
                    "next step targets a different agent or model".into()
                });
            }
        }
        if self.previous_step_name().is_some() {
            a.can_cancel_to_previous_step = true;
        } else {
            a.cancel_to_previous_unavailable_reason = Some("this is the first step".into());
        }
        if !a.can_finish_workflow {
            a.finish_workflow_unavailable_reason =
                Some("FinishWorkflow is only valid on the last step".into());
        }
        Ok(a)
    }

    pub fn next_ready_steps(&self) -> Result<Vec<WorkflowStep>, EngineError> {
        self.state
            .next_ready(&self.dag)
            .into_iter()
            .map(|name| self.find_step(&name))
            .collect()
    }

    fn next_ready_step(&self) -> Result<Option<WorkflowStep>, EngineError> {
        match self.state.next_ready(&self.dag).into_iter().next() {
            Some(name) => Ok(Some(self.find_step(&name)?)),
            None => Ok(None),
        }
    }

    fn previous_step_name(&self) -> Option<String> {
        let curr = self.current_step_name.as_ref()?;
        let order = self.dag.topological_order();
        let idx = order.iter().position(|n| n == curr)?;
        if idx == 0 {
            None
        } else {
            Some(order[idx - 1].clone())
        }
    }

    fn is_last_step(&self) -> bool {
        let curr = match self.current_step_name.as_ref() {
            Some(c) => c,
            None => return false,
        };
        let order = self.dag.topological_order();
        order.last().map(|s| s == curr).unwrap_or(false)
    }

    fn find_step(&self, name: &str) -> Result<WorkflowStep, EngineError> {
        self.workflow
            .steps
            .iter()
            .find(|s| s.name == name)
            .cloned()
            .ok_or_else(|| EngineError::Other(format!("step '{name}' not found in workflow")))
    }

    fn workflow_progress_info(&self) -> Vec<WorkflowStepProgressInfo> {
        use crate::data::workflow_state::StepState;
        self.workflow
            .steps
            .iter()
            .map(|step| {
                let agent = self
                    .resolve_agent(step)
                    .map(|a| a.as_str().to_string())
                    .unwrap_or_else(|_| "?".to_string());
                let model = self.resolve_model(step);
                let status = match self.state.status_of(&step.name) {
                    None | Some(StepState::Pending) => WorkflowStepStatus::Pending,
                    Some(StepState::Running { .. }) => WorkflowStepStatus::Running,
                    Some(StepState::Succeeded) => WorkflowStepStatus::Succeeded,
                    Some(StepState::Failed { exit_code, .. }) => WorkflowStepStatus::Failed {
                        exit_code: *exit_code,
                    },
                    Some(StepState::Cancelled) => WorkflowStepStatus::Cancelled,
                    Some(StepState::Skipped) => WorkflowStepStatus::Skipped,
                };
                WorkflowStepProgressInfo {
                    name: step.name.clone(),
                    agent,
                    model,
                    status,
                    depends_on: step.depends_on.clone(),
                }
            })
            .collect()
    }

    fn resolve_agent(&self, step: &WorkflowStep) -> Result<AgentName, EngineError> {
        if let Some(name) = step.agent.as_deref() {
            return AgentName::new(name).map_err(EngineError::Data);
        }
        if let Some(name) = self.workflow.agent.as_deref() {
            return AgentName::new(name).map_err(EngineError::Data);
        }
        if let Some(name) = self.effective_config.agent() {
            return AgentName::new(&name).map_err(EngineError::Data);
        }
        Err(EngineError::Other(
            "no agent resolved for step (no step, workflow, or config default)".into(),
        ))
    }

    fn resolve_model(&self, step: &WorkflowStep) -> Option<String> {
        if let Some(m) = step.model.as_deref() {
            return Some(m.to_string());
        }
        if let Some(m) = self.workflow.model.as_ref() {
            return Some(m.clone());
        }
        self.effective_config.model()
    }

    fn persist(&self) -> Result<(), EngineError> {
        self.state_store
            .save(&self.state)
            .map_err(EngineError::Data)?;
        Ok(())
    }

    /// Run setup phase steps inside the provided background container.
    /// Returns `Ok(())` on success, `Err` if any step fails (remaining steps
    /// are skipped).
    /// Run the setup phase, asking the caller for a fresh container per step.
    ///
    /// `container_for_step(idx)` is invoked once per step and must return a
    /// container with that step's overlays/env applied — and only that step's.
    /// The returned container is dropped when the step finishes, which kills
    /// the container via `BackgroundContainer::drop`. This is what gives each
    /// step its own isolated resource set (WI-0082): two teardown entries
    /// declaring `overlays = ["ssh()"]` and `overlays = ["env(GITHUB_TOKEN)"]`
    /// must NOT each see both.
    pub fn run_setup<F>(
        &mut self,
        steps: &[crate::data::workflow_definition::SetupStep],
        abort_flags: &[bool],
        mut container_for_step: F,
    ) -> Result<(), EngineError>
    where
        F: FnMut(usize) -> Result<Box<dyn ContainerExec>, EngineError>,
    {
        use crate::data::workflow_state::{PhaseStepState, PhaseStepStatus, WorkflowPhase};
        use crate::engine::workflow::step_commands::{
            setup_step_description, setup_step_to_shell, substitute_setup_step,
        };

        let wi_ctx = self.work_item_context.as_ref();
        let steps: Vec<_> = steps
            .iter()
            .map(|s| substitute_setup_step(s, wi_ctx))
            .collect();

        self.state.current_phase = WorkflowPhase::Setup;
        self.state.setup_step_states = steps
            .iter()
            .map(|s| PhaseStepState {
                description: setup_step_description(s),
                status: PhaseStepStatus::Pending,
            })
            .collect();
        self.persist()?;

        for (idx, step) in steps.iter().enumerate() {
            let desc = setup_step_description(step);
            let (command, env) = setup_step_to_shell(step);
            let abort = abort_flags.get(idx).copied().unwrap_or(false);

            self.state.setup_step_states[idx].status = PhaseStepStatus::Running;
            self.persist()?;

            self.frontend.on_setup_step_started(&desc);

            let container = container_for_step(idx)?;
            let result = container.exec_streaming(&command, env.as_ref(), &mut |line| {
                self.frontend.on_setup_step_output(line)
            })?;

            if result.exit_code != 0 {
                self.state.setup_step_states[idx].status = PhaseStepStatus::Failed {
                    error: result.stderr.clone(),
                };
                self.persist()?;
                self.frontend
                    .on_setup_step_failed(&desc, result.exit_code, &result.stderr);
                if abort {
                    self.abort_on_failure_triggered = true;
                    return Err(EngineError::Container(format!(
                        "setup step '{}' failed with exit code {} (abort_on_failure)",
                        desc, result.exit_code
                    )));
                }
                continue;
            }

            self.state.setup_step_states[idx].status = PhaseStepStatus::Succeeded;
            self.persist()?;
            self.frontend.on_setup_step_completed(&desc);
            // `container` drops here, killing the per-step BackgroundContainer.
        }

        self.state.setup_completed = true;
        self.state.current_phase = WorkflowPhase::Main;
        self.persist()?;
        Ok(())
    }

    /// Run teardown phase steps, asking the caller for a fresh container per step.
    ///
    /// Skips all steps and returns `Ok(())` if `!teardown_on_failure && !workflow_succeeded`.
    /// Failing teardown steps are logged but do not abort the remaining steps (best-effort).
    /// See [`run_setup`] for the rationale behind per-step containers.
    ///
    /// When the per-step container factory itself fails (e.g. an overlay won't
    /// resolve, or the runtime can't start the container), the engine records
    /// that step as `Failed`, surfaces the error to the frontend, and proceeds
    /// to the next step — matching the best-effort semantics already used for
    /// non-zero exit codes.
    /// Returns `(teardown_aborted, any_step_failed)`:
    /// - `teardown_aborted`: true if an `abort_on_failure` step failed
    /// - `any_step_failed`: true if any teardown step failed (regardless of abort flag)
    pub fn run_teardown<F>(
        &mut self,
        steps: &[crate::data::workflow_definition::TeardownStep],
        abort_flags: &[bool],
        workflow_succeeded: bool,
        teardown_on_failure: bool,
        mut container_for_step: F,
    ) -> Result<(bool, bool), EngineError>
    where
        F: FnMut(usize) -> Result<Box<dyn ContainerExec>, EngineError>,
    {
        use crate::data::workflow_state::{PhaseStepState, PhaseStepStatus, WorkflowPhase};
        use crate::engine::workflow::step_commands::{
            substitute_teardown_step, teardown_step_description, teardown_step_to_shell,
        };

        if !teardown_on_failure && !workflow_succeeded {
            return Ok((false, false));
        }

        let wi_ctx = self.work_item_context.as_ref();
        let steps: Vec<_> = steps
            .iter()
            .map(|s| substitute_teardown_step(s, wi_ctx))
            .collect();

        self.state.current_phase = WorkflowPhase::Teardown;
        self.state.teardown_step_states = steps
            .iter()
            .map(|s| PhaseStepState {
                description: teardown_step_description(s),
                status: PhaseStepStatus::Pending,
            })
            .collect();
        self.persist()?;

        let mut teardown_aborted = false;
        let mut any_step_failed = false;
        for (idx, step) in steps.iter().enumerate() {
            let desc = teardown_step_description(step);
            let (command, env) = teardown_step_to_shell(step);
            let abort = abort_flags.get(idx).copied().unwrap_or(false);

            self.state.teardown_step_states[idx].status = PhaseStepStatus::Running;
            self.persist()?;

            self.frontend.on_teardown_step_started(&desc);

            let container = match container_for_step(idx) {
                Ok(c) => c,
                Err(e) => {
                    let msg = e.to_string();
                    self.state.teardown_step_states[idx].status =
                        PhaseStepStatus::Failed { error: msg.clone() };
                    self.persist()?;
                    self.frontend.on_teardown_step_failed(&desc, 1, &msg);
                    any_step_failed = true;
                    if abort {
                        teardown_aborted = true;
                        break;
                    }
                    continue;
                }
            };
            let result = container.exec_streaming(&command, env.as_ref(), &mut |line| {
                self.frontend.on_teardown_step_output(line)
            })?;

            if result.exit_code != 0 {
                self.state.teardown_step_states[idx].status = PhaseStepStatus::Failed {
                    error: result.stderr.clone(),
                };
                self.persist()?;
                self.frontend
                    .on_teardown_step_failed(&desc, result.exit_code, &result.stderr);
                any_step_failed = true;
                if abort {
                    teardown_aborted = true;
                    break;
                }
            } else {
                self.state.teardown_step_states[idx].status = PhaseStepStatus::Succeeded;
                self.persist()?;
                self.frontend.on_teardown_step_completed(&desc);
            }
            // `container` drops here, killing the per-step BackgroundContainer.
        }

        self.state.teardown_completed = true;
        self.state.current_phase = WorkflowPhase::Done;
        self.persist()?;
        Ok((teardown_aborted, any_step_failed))
    }

    /// Mark the workflow as fully finished. Called by the orchestrator after
    /// the main phase completes when no teardown phase will run (so the state
    /// reflects completion rather than lingering in `Main`).
    pub fn mark_done(&mut self) -> Result<(), EngineError> {
        use crate::data::workflow_state::WorkflowPhase;
        self.state.current_phase = WorkflowPhase::Done;
        self.persist()?;
        Ok(())
    }
}

/// Hash a workflow's steps + title to detect drift.
fn compute_workflow_hash(workflow: &Workflow) -> String {
    let json = serde_json::to_string(workflow).unwrap_or_default();
    let h = ring::digest::digest(&ring::digest::SHA256, json.as_bytes());
    let mut s = String::with_capacity(64);
    for b in h.as_ref() {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
    }
    s
}

pub fn workflow_name_for(workflow: &Workflow) -> String {
    workflow.title.as_deref().unwrap_or("workflow").to_string()
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use chrono::Utc;

    use super::*;
    use crate::data::session::{ContainerHandle, SessionOpenOptions, StaticGitRootResolver};
    use crate::data::workflow_definition::{Workflow, WorkflowStep};
    use crate::data::workflow_state_store::WorkflowStateStore;
    use crate::engine::container::instance::{ContainerExecution, ContainerExitInfo};
    use crate::engine::overlay::OverlayEngine;

    // ── Fake implementations ─────────────────────────────────────────────────

    struct FakeWorkflowFrontend {
        actions: Mutex<VecDeque<NextAction>>,
        step_statuses: Mutex<Vec<(String, WorkflowStepStatus)>>,
        completed: Mutex<Option<WorkflowOutcome>>,
        confirm_resume_response: bool,
        failure_choice: StepFailureChoice,
    }

    impl FakeWorkflowFrontend {
        fn new(actions: impl IntoIterator<Item = NextAction>) -> Self {
            Self {
                actions: Mutex::new(actions.into_iter().collect()),
                step_statuses: Mutex::new(Vec::new()),
                completed: Mutex::new(None),
                confirm_resume_response: true,
                failure_choice: StepFailureChoice::Abort,
            }
        }

        fn with_confirm_resume(mut self, response: bool) -> Self {
            self.confirm_resume_response = response;
            self
        }

        fn step_statuses(&self) -> Vec<(String, WorkflowStepStatus)> {
            self.step_statuses.lock().unwrap().clone()
        }

        fn completed_outcome(&self) -> Option<WorkflowOutcome> {
            self.completed.lock().unwrap().clone()
        }
    }

    impl crate::engine::message::UserMessageSink for FakeWorkflowFrontend {
        fn write_message(&mut self, _msg: crate::engine::message::UserMessage) {}
        fn replay_queued(&mut self) {}
    }

    impl WorkflowFrontend for FakeWorkflowFrontend {
        fn show_workflow_control_board(
            &mut self,
            _state: &WorkflowState,
            _available: &AvailableActions,
        ) -> Result<NextAction, EngineError> {
            let action = self
                .actions
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(NextAction::LaunchNext);
            Ok(action)
        }

        fn confirm_resume(&mut self, _mismatch: &ResumeMismatch) -> Result<bool, EngineError> {
            Ok(self.confirm_resume_response)
        }

        fn user_choose_after_step_failure(
            &mut self,
            _step: &WorkflowStep,
            _exit: &ContainerExitInfo,
        ) -> Result<StepFailureChoice, EngineError> {
            Ok(self.failure_choice.clone())
        }

        fn report_step_status(&mut self, step: &WorkflowStep, status: WorkflowStepStatus) {
            self.step_statuses
                .lock()
                .unwrap()
                .push((step.name.clone(), status));
        }

        fn yolo_countdown_tick(
            &mut self,
            _step_name: &str,
            _remaining: Duration,
            _total: Duration,
        ) -> Result<YoloTickOutcome, EngineError> {
            Ok(YoloTickOutcome::Cancel)
        }

        fn report_workflow_completed(&mut self, outcome: &WorkflowOutcome) {
            *self.completed.lock().unwrap() = Some(outcome.clone());
        }
    }

    struct FakeContainerExecutionFactory {
        exit_codes: Mutex<VecDeque<i32>>,
        pub execution_call_count: AtomicUsize,
        pub inject_call_count: AtomicUsize,
        pub recorded_contexts: Mutex<Vec<WorkflowRuntimeContext>>,
        inject_result: Option<()>,
    }

    impl FakeContainerExecutionFactory {
        fn new(exit_codes: impl IntoIterator<Item = i32>) -> Self {
            Self {
                exit_codes: Mutex::new(exit_codes.into_iter().collect()),
                execution_call_count: AtomicUsize::new(0),
                inject_call_count: AtomicUsize::new(0),
                recorded_contexts: Mutex::new(Vec::new()),
                inject_result: None,
            }
        }

        fn always_success() -> Self {
            Self::new(std::iter::repeat_n(0, 100))
        }

        fn with_inject_support(exit_codes: impl IntoIterator<Item = i32>) -> Self {
            Self {
                inject_result: Some(()),
                ..Self::new(exit_codes)
            }
        }
    }

    impl ContainerExecutionFactory for FakeContainerExecutionFactory {
        fn execution_for_step(
            &self,
            _step: &WorkflowStep,
            _session: &Session,
            runtime: &WorkflowRuntimeContext,
        ) -> Result<ContainerExecution, EngineError> {
            self.execution_call_count.fetch_add(1, Ordering::Relaxed);
            self.recorded_contexts.lock().unwrap().push(runtime.clone());
            let code = self.exit_codes.lock().unwrap().pop_front().unwrap_or(0);
            let now = Utc::now();
            let info = ContainerExitInfo {
                exit_code: code,
                signal: None,
                started_at: now,
                ended_at: now,
            };
            let handle = ContainerHandle {
                id: format!("fake-{}", self.execution_call_count.load(Ordering::Relaxed)),
                image_tag: "fake-image:latest".into(),
                name: "fake-container".into(),
                started_at: now,
            };
            Ok(ContainerExecution::finished(handle, info))
        }

        fn inject_prompt(
            &self,
            _execution: &ContainerExecution,
            _prompt: &str,
        ) -> Result<Option<()>, EngineError> {
            self.inject_call_count.fetch_add(1, Ordering::Relaxed);
            Ok(self.inject_result)
        }
    }

    // ── Helpers ──────────────────────────────────────────────────────────────

    fn make_session(tmp: &tempfile::TempDir) -> Session {
        let resolver = StaticGitRootResolver::new(tmp.path());
        Session::open(
            tmp.path().to_path_buf(),
            &resolver,
            SessionOpenOptions::default(),
        )
        .unwrap()
    }

    fn make_step(name: &str, deps: &[&str], agent: Option<&str>) -> WorkflowStep {
        WorkflowStep {
            name: name.to_string(),
            depends_on: deps.iter().map(|s| s.to_string()).collect(),
            prompt_template: "do something".to_string(),
            agent: agent.map(|s| s.to_string()),
            model: None,
            overlays: None,
            abort_on_failure: false,
        }
    }

    fn make_workflow(
        title: Option<&str>,
        wf_agent: Option<&str>,
        steps: Vec<WorkflowStep>,
    ) -> Workflow {
        Workflow {
            title: title.map(|s| s.to_string()),
            steps,
            agent: wf_agent.map(|s| s.to_string()),
            model: None,
            setup: Vec::new(),
            teardown: Vec::new(),
            teardown_on_failure: false,
        }
    }

    fn make_engine(
        session: &Session,
        workflow: Workflow,
        factory: FakeContainerExecutionFactory,
        actions: impl IntoIterator<Item = NextAction>,
    ) -> WorkflowEngine {
        make_engine_with_frontend(
            session,
            workflow,
            factory,
            FakeWorkflowFrontend::new(actions),
        )
    }

    fn make_engine_with_frontend(
        session: &Session,
        workflow: Workflow,
        factory: FakeContainerExecutionFactory,
        frontend: FakeWorkflowFrontend,
    ) -> WorkflowEngine {
        let overlay = OverlayEngine::with_auth_resolver(
            crate::data::fs::auth_paths::AuthPathResolver::at_home(session.git_root()),
        );
        WorkflowEngine::new(
            session,
            workflow,
            None,
            Box::new(frontend),
            Box::new(factory),
            Arc::new(GitEngine::new()),
            Arc::new(overlay),
        )
        .unwrap()
    }

    // ── WorkflowEngine tests ─────────────────────────────────────────────────

    #[tokio::test]
    async fn step_once_advances_one_step_and_persists() {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session(&tmp);
        let workflow = make_workflow(
            Some("my-wf"),
            Some("claude"),
            vec![make_step("a", &[], None), make_step("b", &["a"], None)],
        );
        let factory = FakeContainerExecutionFactory::always_success();
        let mut engine = make_engine(&session, workflow, factory, []);

        let outcome = engine.step_once().await.unwrap();
        assert_eq!(outcome.step_name, "a");
        assert!(matches!(outcome.status, WorkflowStepStatus::Succeeded));
        assert_eq!(outcome.remaining, 1);

        assert!(matches!(
            engine.state().status_of("a"),
            Some(StepState::Succeeded)
        ));
        assert!(matches!(
            engine.state().status_of("b"),
            Some(StepState::Pending)
        ));

        let store = WorkflowStateStore::at_git_root(tmp.path());
        let saved = store.load(None, "my-wf").unwrap();
        assert!(saved.is_some());
    }

    #[tokio::test]
    async fn run_to_completion_runs_all_steps() {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session(&tmp);
        let workflow = make_workflow(
            Some("wf-all"),
            Some("claude"),
            vec![make_step("a", &[], None), make_step("b", &["a"], None)],
        );
        let factory = FakeContainerExecutionFactory::always_success();
        let frontend = FakeWorkflowFrontend::new([NextAction::LaunchNext]);
        let overlay = OverlayEngine::with_auth_resolver(
            crate::data::fs::auth_paths::AuthPathResolver::at_home(session.git_root()),
        );
        let mut engine = WorkflowEngine::new(
            &session,
            workflow,
            None,
            Box::new(frontend),
            Box::new(factory),
            Arc::new(GitEngine::new()),
            Arc::new(overlay),
        )
        .unwrap();

        let result = engine.run_to_completion().await.unwrap();
        assert_eq!(result, WorkflowOutcome::Completed);
    }

    #[tokio::test]
    async fn run_to_completion_runs_all_parallel_steps() {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session(&tmp);
        let workflow = make_workflow(
            Some("wf-parallel"),
            Some("claude"),
            vec![
                make_step("a", &[], None),
                make_step("b", &["a"], None),
                make_step("c", &["a"], None),
            ],
        );
        let factory = FakeContainerExecutionFactory::always_success();
        let mut engine = make_engine(
            &session,
            workflow,
            factory,
            [NextAction::LaunchNext, NextAction::LaunchNext],
        );

        let result = engine.run_to_completion().await.unwrap();
        assert_eq!(result, WorkflowOutcome::Completed);
    }

    #[tokio::test]
    async fn run_to_completion_parallel_fan_in() {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session(&tmp);
        let workflow = make_workflow(
            Some("wf-fan-in"),
            Some("claude"),
            vec![
                make_step("a", &[], None),
                make_step("b", &["a"], None),
                make_step("c", &["a"], None),
                make_step("d", &["b", "c"], None),
            ],
        );
        let factory = FakeContainerExecutionFactory::always_success();
        let mut engine = make_engine(
            &session,
            workflow,
            factory,
            [
                NextAction::LaunchNext,
                NextAction::LaunchNext,
                NextAction::LaunchNext,
            ],
        );

        let result = engine.run_to_completion().await.unwrap();
        assert_eq!(result, WorkflowOutcome::Completed);
    }

    #[tokio::test]
    async fn non_zero_exit_code_marks_step_failed() {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session(&tmp);
        let workflow = make_workflow(
            Some("wf-fail"),
            Some("claude"),
            vec![make_step("a", &[], None)],
        );
        let factory = FakeContainerExecutionFactory::new([1]);
        let mut engine = make_engine(&session, workflow, factory, []);

        let outcome = engine.step_once().await.unwrap();
        assert!(matches!(
            outcome.status,
            WorkflowStepStatus::Failed { exit_code: 1 }
        ));
    }

    #[tokio::test]
    async fn step_failure_abort_returns_aborted() {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session(&tmp);
        let workflow = make_workflow(
            Some("wf-fail-abort"),
            Some("claude"),
            vec![make_step("a", &[], None)],
        );
        let factory = FakeContainerExecutionFactory::new([2]);
        let frontend = FakeWorkflowFrontend::new([]);
        let mut engine = make_engine_with_frontend(&session, workflow, factory, frontend);

        let result = engine.run_to_completion().await.unwrap();
        assert!(matches!(result, WorkflowOutcome::Aborted));
    }

    #[tokio::test]
    async fn step_failure_retry_reruns_step() {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session(&tmp);
        let workflow = make_workflow(
            Some("wf-fail-retry"),
            Some("claude"),
            vec![make_step("a", &[], None)],
        );
        let factory = FakeContainerExecutionFactory::new([1, 0]);
        let mut frontend = FakeWorkflowFrontend::new([]);
        frontend.failure_choice = StepFailureChoice::Retry;
        let mut engine = make_engine_with_frontend(&session, workflow, factory, frontend);

        let result = engine.run_to_completion().await.unwrap();
        assert!(matches!(result, WorkflowOutcome::Completed));
    }

    #[tokio::test]
    async fn step_failure_pause_returns_paused() {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session(&tmp);
        let workflow = make_workflow(
            Some("wf-fail-pause"),
            Some("claude"),
            vec![make_step("a", &[], None)],
        );
        let factory = FakeContainerExecutionFactory::new([1]);
        let mut frontend = FakeWorkflowFrontend::new([]);
        frontend.failure_choice = StepFailureChoice::Pause;
        let mut engine = make_engine_with_frontend(&session, workflow, factory, frontend);

        let result = engine.run_to_completion().await.unwrap();
        assert!(matches!(result, WorkflowOutcome::Paused));
    }

    #[tokio::test]
    async fn pause_persists_state_and_returns_paused() {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session(&tmp);
        let workflow = make_workflow(
            Some("wf-pause"),
            Some("claude"),
            vec![make_step("a", &[], None), make_step("b", &["a"], None)],
        );
        let factory = FakeContainerExecutionFactory::always_success();
        let mut engine = make_engine(&session, workflow, factory, [NextAction::Pause]);

        let result = engine.run_to_completion().await.unwrap();
        assert_eq!(result, WorkflowOutcome::Paused);

        let store = WorkflowStateStore::at_git_root(tmp.path());
        let saved = store.load(None, "wf-pause").unwrap();
        assert!(saved.is_some());
    }

    #[tokio::test]
    async fn resume_with_same_hash_continues_from_saved_state() {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session(&tmp);
        let wf = make_workflow(
            Some("wf-resume"),
            Some("claude"),
            vec![make_step("a", &[], None), make_step("b", &["a"], None)],
        );

        {
            let factory = FakeContainerExecutionFactory::always_success();
            let mut engine = make_engine(&session, wf.clone(), factory, [NextAction::Pause]);
            engine.run_to_completion().await.unwrap();
        }

        let factory2 = FakeContainerExecutionFactory::always_success();
        let overlay = OverlayEngine::with_auth_resolver(
            crate::data::fs::auth_paths::AuthPathResolver::at_home(session.git_root()),
        );
        let frontend = FakeWorkflowFrontend::new([]);
        let mut engine = WorkflowEngine::resume(
            &session,
            wf,
            None,
            Box::new(frontend),
            Box::new(factory2),
            Arc::new(GitEngine::new()),
            Arc::new(overlay),
        )
        .await
        .unwrap();
        let result = engine.run_to_completion().await.unwrap();
        assert_eq!(result, WorkflowOutcome::Completed);
    }

    #[tokio::test]
    async fn resume_with_drifted_hash_calls_confirm_resume_and_aborts_when_declined() {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session(&tmp);
        let wf1 = make_workflow(
            Some("wf-drift"),
            Some("claude"),
            vec![make_step("a", &[], None)],
        );

        {
            let factory = FakeContainerExecutionFactory::always_success();
            let mut engine = make_engine(&session, wf1, factory, [NextAction::Pause]);
            engine.run_to_completion().await.unwrap();
        }

        let wf2 = make_workflow(
            Some("wf-drift"),
            Some("claude"),
            vec![make_step("a", &[], None), make_step("b", &["a"], None)],
        );
        let overlay = OverlayEngine::with_auth_resolver(
            crate::data::fs::auth_paths::AuthPathResolver::at_home(session.git_root()),
        );
        let frontend = FakeWorkflowFrontend::new([]).with_confirm_resume(false);
        let result = WorkflowEngine::resume(
            &session,
            wf2,
            None,
            Box::new(frontend),
            Box::new(FakeContainerExecutionFactory::always_success()),
            Arc::new(GitEngine::new()),
            Arc::new(overlay),
        )
        .await;

        assert!(matches!(
            result,
            Err(EngineError::WorkflowResumeIncompatible(_))
        ));
    }

    #[tokio::test]
    async fn step_level_agent_overrides_workflow_level() {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session(&tmp);
        let workflow = make_workflow(
            Some("wf-agent"),
            Some("claude"),
            vec![make_step("a", &[], Some("codex"))],
        );
        let factory = FakeContainerExecutionFactory::always_success();
        let factory_arc: Arc<FakeContainerExecutionFactory> = Arc::new(factory);

        struct RecordingFactory(Arc<FakeContainerExecutionFactory>);
        impl ContainerExecutionFactory for RecordingFactory {
            fn execution_for_step(
                &self,
                step: &WorkflowStep,
                session: &Session,
                runtime: &WorkflowRuntimeContext,
            ) -> Result<ContainerExecution, EngineError> {
                self.0.execution_for_step(step, session, runtime)
            }
            fn inject_prompt(
                &self,
                e: &ContainerExecution,
                p: &str,
            ) -> Result<Option<()>, EngineError> {
                self.0.inject_prompt(e, p)
            }
        }

        let overlay = OverlayEngine::with_auth_resolver(
            crate::data::fs::auth_paths::AuthPathResolver::at_home(session.git_root()),
        );
        let mut engine = WorkflowEngine::new(
            &session,
            workflow,
            None,
            Box::new(FakeWorkflowFrontend::new([])),
            Box::new(RecordingFactory(factory_arc.clone())),
            Arc::new(GitEngine::new()),
            Arc::new(overlay),
        )
        .unwrap();

        engine.step_once().await.unwrap();
        let contexts = factory_arc.recorded_contexts.lock().unwrap().clone();
        assert_eq!(contexts.len(), 1);
        assert_eq!(contexts[0].step_agent.as_str(), "codex");
    }

    #[tokio::test]
    async fn cancel_to_previous_step_unavailable_on_first_step() {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session(&tmp);
        let workflow = make_workflow(
            Some("wf-cancel"),
            Some("claude"),
            vec![make_step("a", &[], None), make_step("b", &["a"], None)],
        );
        let factory = FakeContainerExecutionFactory::always_success();
        let mut engine = make_engine(&session, workflow, factory, []);

        engine.step_once().await.unwrap();

        let available = engine.compute_available_actions().unwrap();
        assert!(!available.can_cancel_to_previous_step);
        assert!(available.cancel_to_previous_unavailable_reason.is_some());
    }

    #[tokio::test]
    async fn yolo_mode_auto_advances_between_steps() {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session(&tmp);
        let workflow = make_workflow(
            Some("wf-yolo"),
            Some("claude"),
            vec![
                make_step("a", &[], None),
                make_step("b", &["a"], None),
                make_step("c", &["b"], None),
            ],
        );
        let factory = FakeContainerExecutionFactory::always_success();
        // No actions queued — yolo mode should auto-advance without prompting.
        let mut engine = make_engine(&session, workflow, factory, []);
        engine.set_yolo(true);

        let result = engine.run_to_completion().await.unwrap();
        assert_eq!(result, WorkflowOutcome::Completed);
    }

    // ── Blocking factory for mid-step tests ──────────────────────────────────

    use std::sync::Condvar;

    type CompletionArc = Arc<(Mutex<Option<i32>>, Condvar)>;

    struct BlockingBackend {
        cancel_flag: Arc<AtomicBool>,
        completion: CompletionArc,
    }

    impl crate::engine::container::instance::ExecutionBackend for BlockingBackend {
        fn wait_blocking(self: Box<Self>) -> Result<ContainerExitInfo, EngineError> {
            let (lock, cvar) = &*self.completion;
            loop {
                if self.cancel_flag.load(Ordering::Relaxed) {
                    let now = Utc::now();
                    return Ok(ContainerExitInfo {
                        exit_code: -1,
                        signal: None,
                        started_at: now,
                        ended_at: now,
                    });
                }
                let guard = lock.lock().unwrap();
                let (guard, _) = cvar.wait_timeout(guard, Duration::from_millis(20)).unwrap();
                if let Some(code) = *guard {
                    let now = Utc::now();
                    return Ok(ContainerExitInfo {
                        exit_code: code,
                        signal: None,
                        started_at: now,
                        ended_at: now,
                    });
                }
            }
        }

        fn cancel(&self) -> Result<(), EngineError> {
            self.cancel_flag.store(true, Ordering::Relaxed);
            let (_, cvar) = &*self.completion;
            cvar.notify_all();
            Ok(())
        }

        fn cancel_handle(&self) -> Option<crate::engine::container::instance::CancelHandle> {
            let flag = self.cancel_flag.clone();
            let completion = self.completion.clone();
            Some(crate::engine::container::instance::CancelHandle::new(
                move || {
                    flag.store(true, Ordering::Relaxed);
                    let (_, cvar) = &*completion;
                    cvar.notify_all();
                    Ok(())
                },
            ))
        }
    }

    fn make_blocking_entry() -> (Arc<AtomicBool>, CompletionArc) {
        (
            Arc::new(AtomicBool::new(false)),
            Arc::new((Mutex::new(None), Condvar::new())),
        )
    }

    fn signal_completion(c: &CompletionArc, code: i32) {
        let (lock, cvar) = &**c;
        *lock.lock().unwrap() = Some(code);
        cvar.notify_all();
    }

    struct BlockingFactory {
        execution_count: Arc<AtomicUsize>,
        inject_count: Arc<AtomicUsize>,
        inject_result: Option<()>,
        blocking_slots: Mutex<VecDeque<(Arc<AtomicBool>, CompletionArc)>>,
    }

    impl BlockingFactory {
        fn new(slots: impl IntoIterator<Item = (Arc<AtomicBool>, CompletionArc)>) -> Self {
            Self {
                execution_count: Arc::new(AtomicUsize::new(0)),
                inject_count: Arc::new(AtomicUsize::new(0)),
                inject_result: None,
                blocking_slots: Mutex::new(slots.into_iter().collect()),
            }
        }
    }

    impl ContainerExecutionFactory for BlockingFactory {
        fn execution_for_step(
            &self,
            _step: &WorkflowStep,
            _session: &Session,
            _runtime: &WorkflowRuntimeContext,
        ) -> Result<ContainerExecution, EngineError> {
            let idx = self.execution_count.fetch_add(1, Ordering::Relaxed);
            let slot = self.blocking_slots.lock().unwrap().pop_front();
            if let Some((cancel_flag, completion)) = slot {
                let backend = Box::new(BlockingBackend {
                    cancel_flag,
                    completion,
                });
                let now = Utc::now();
                let handle = ContainerHandle {
                    id: format!("blocking-{idx}"),
                    image_tag: "test:latest".into(),
                    name: "blocking-container".into(),
                    started_at: now,
                };
                let (stuck_tx, _) = tokio::sync::broadcast::channel(4);
                Ok(ContainerExecution::new(
                    handle,
                    backend,
                    std::sync::Arc::new(stuck_tx),
                ))
            } else {
                let now = Utc::now();
                let info = ContainerExitInfo {
                    exit_code: 0,
                    signal: None,
                    started_at: now,
                    ended_at: now,
                };
                let handle = ContainerHandle {
                    id: format!("instant-{idx}"),
                    image_tag: "test:latest".into(),
                    name: "instant-container".into(),
                    started_at: now,
                };
                Ok(ContainerExecution::finished(handle, info))
            }
        }

        fn inject_prompt(
            &self,
            _execution: &ContainerExecution,
            _prompt: &str,
        ) -> Result<Option<()>, EngineError> {
            self.inject_count.fetch_add(1, Ordering::Relaxed);
            Ok(self.inject_result)
        }
    }

    struct CapturingFrontend {
        actions: Mutex<VecDeque<NextAction>>,
        step_statuses: Mutex<Vec<(String, WorkflowStepStatus)>>,
        completed: Mutex<Option<WorkflowOutcome>>,
        available_log: Mutex<Vec<AvailableActions>>,
        engine_tx: Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<EngineRequest>>>>,
    }

    impl CapturingFrontend {
        fn new(
            actions: impl IntoIterator<Item = NextAction>,
            engine_tx: Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<EngineRequest>>>>,
        ) -> Self {
            Self {
                actions: Mutex::new(actions.into_iter().collect()),
                step_statuses: Mutex::new(Vec::new()),
                completed: Mutex::new(None),
                available_log: Mutex::new(Vec::new()),
                engine_tx,
            }
        }
    }

    impl crate::engine::message::UserMessageSink for CapturingFrontend {
        fn write_message(&mut self, _msg: crate::engine::message::UserMessage) {}
        fn replay_queued(&mut self) {}
    }

    impl WorkflowFrontend for CapturingFrontend {
        fn show_workflow_control_board(
            &mut self,
            _state: &WorkflowState,
            available: &AvailableActions,
        ) -> Result<NextAction, EngineError> {
            self.available_log.lock().unwrap().push(available.clone());
            let action = self
                .actions
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(NextAction::Pause);
            Ok(action)
        }

        fn confirm_resume(&mut self, _: &ResumeMismatch) -> Result<bool, EngineError> {
            Ok(true)
        }

        fn user_choose_after_step_failure(
            &mut self,
            _step: &WorkflowStep,
            _exit: &ContainerExitInfo,
        ) -> Result<StepFailureChoice, EngineError> {
            Ok(StepFailureChoice::Abort)
        }

        fn report_step_status(&mut self, step: &WorkflowStep, status: WorkflowStepStatus) {
            self.step_statuses
                .lock()
                .unwrap()
                .push((step.name.clone(), status));
        }

        fn yolo_countdown_tick(
            &mut self,
            _step_name: &str,
            _remaining: Duration,
            _total: Duration,
        ) -> Result<YoloTickOutcome, EngineError> {
            Ok(YoloTickOutcome::Cancel)
        }

        fn report_workflow_completed(&mut self, outcome: &WorkflowOutcome) {
            *self.completed.lock().unwrap() = Some(outcome.clone());
        }

        fn set_engine_sender(&mut self, tx: tokio::sync::mpsc::UnboundedSender<EngineRequest>) {
            *self.engine_tx.lock().unwrap() = Some(tx);
        }
    }

    fn make_capturing_engine(
        session: &Session,
        workflow: Workflow,
        factory: BlockingFactory,
        actions: impl IntoIterator<Item = NextAction>,
        engine_tx: Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<EngineRequest>>>>,
    ) -> WorkflowEngine {
        let overlay = OverlayEngine::with_auth_resolver(
            crate::data::fs::auth_paths::AuthPathResolver::at_home(session.git_root()),
        );
        let frontend = CapturingFrontend::new(actions, engine_tx);
        WorkflowEngine::new(
            session,
            workflow,
            None,
            Box::new(frontend),
            Box::new(factory),
            Arc::new(crate::engine::git::GitEngine::new()),
            Arc::new(overlay),
        )
        .unwrap()
    }

    // ── Mid-step control board tests ─────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn open_control_board_mid_step_does_not_cancel_container() {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session(&tmp);
        let workflow = make_workflow(
            Some("wf-mid-no-cancel"),
            Some("claude"),
            vec![make_step("a", &[], None), make_step("b", &["a"], None)],
        );

        let (cancel_flag, completion1) = make_blocking_entry();
        let engine_tx: Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<EngineRequest>>>> =
            Arc::new(Mutex::new(None));

        let factory = BlockingFactory::new([(cancel_flag.clone(), completion1.clone())]);
        let mut engine = make_capturing_engine(
            &session,
            workflow,
            factory,
            [NextAction::Dismiss, NextAction::LaunchNext],
            engine_tx.clone(),
        );

        let tx = engine_tx
            .lock()
            .unwrap()
            .clone()
            .expect("engine_tx set on construction");

        let engine_task = tokio::spawn(async move { engine.run_to_completion().await });

        tokio::time::sleep(Duration::from_millis(150)).await;
        tx.send(EngineRequest::OpenControlBoard).unwrap();
        tokio::time::sleep(Duration::from_millis(150)).await;

        assert!(
            !cancel_flag.load(Ordering::Relaxed),
            "cancel must not be called when user picks Dismiss"
        );

        signal_completion(&completion1, 0);

        let result = engine_task.await.unwrap().unwrap();
        assert_eq!(result, WorkflowOutcome::Completed);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mid_step_dismiss_resumes_waiting_on_step() {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session(&tmp);
        let workflow = make_workflow(
            Some("wf-dismiss"),
            Some("claude"),
            vec![make_step("a", &[], None), make_step("b", &["a"], None)],
        );

        let (cancel_flag, completion) = make_blocking_entry();
        let engine_tx: Arc<Mutex<Option<_>>> = Arc::new(Mutex::new(None));
        let factory = BlockingFactory::new([(cancel_flag.clone(), completion.clone())]);
        let mut engine = make_capturing_engine(
            &session,
            workflow,
            factory,
            [NextAction::Dismiss, NextAction::LaunchNext],
            engine_tx.clone(),
        );
        let tx = engine_tx.lock().unwrap().clone().unwrap();

        let engine_task = tokio::spawn(async move { engine.run_to_completion().await });

        tokio::time::sleep(Duration::from_millis(150)).await;
        tx.send(EngineRequest::OpenControlBoard).unwrap();
        tokio::time::sleep(Duration::from_millis(150)).await;

        assert!(!cancel_flag.load(Ordering::Relaxed));

        signal_completion(&completion, 0);
        let result = engine_task.await.unwrap().unwrap();
        assert_eq!(result, WorkflowOutcome::Completed);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mid_step_restart_cancels_then_re_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session(&tmp);
        let workflow = make_workflow(
            Some("wf-restart-mid"),
            Some("claude"),
            vec![make_step("a", &[], None), make_step("b", &["a"], None)],
        );

        let (cancel_flag, completion1) = make_blocking_entry();
        let engine_tx: Arc<Mutex<Option<_>>> = Arc::new(Mutex::new(None));
        let factory = BlockingFactory::new([(cancel_flag.clone(), completion1)]);
        let execution_count = factory.execution_count.clone();
        let mut engine = make_capturing_engine(
            &session,
            workflow,
            factory,
            [NextAction::RestartCurrentStep, NextAction::LaunchNext],
            engine_tx.clone(),
        );
        let tx = engine_tx.lock().unwrap().clone().unwrap();

        let engine_task = tokio::spawn(async move { engine.run_to_completion().await });

        tokio::time::sleep(Duration::from_millis(150)).await;
        tx.send(EngineRequest::OpenControlBoard).unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;

        assert!(cancel_flag.load(Ordering::Relaxed));

        let result = engine_task.await.unwrap().unwrap();
        assert_eq!(result, WorkflowOutcome::Completed);
        assert!(execution_count.load(Ordering::Relaxed) >= 2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mid_step_advance_cancels_then_marks_force_succeeded() {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session(&tmp);
        let workflow = make_workflow(
            Some("wf-advance-mid"),
            Some("claude"),
            vec![make_step("a", &[], None), make_step("b", &["a"], None)],
        );

        let (cancel_flag, completion1) = make_blocking_entry();
        let engine_tx: Arc<Mutex<Option<_>>> = Arc::new(Mutex::new(None));
        let factory = BlockingFactory::new([(cancel_flag.clone(), completion1)]);
        let execution_count = factory.execution_count.clone();
        let mut engine = make_capturing_engine(
            &session,
            workflow,
            factory,
            [NextAction::LaunchNext],
            engine_tx.clone(),
        );
        let tx = engine_tx.lock().unwrap().clone().unwrap();

        let engine_task = tokio::spawn(async move { engine.run_to_completion().await });

        tokio::time::sleep(Duration::from_millis(150)).await;
        tx.send(EngineRequest::OpenControlBoard).unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;

        assert!(cancel_flag.load(Ordering::Relaxed));

        let result = engine_task.await.unwrap().unwrap();
        assert_eq!(result, WorkflowOutcome::Completed);
        assert_eq!(execution_count.load(Ordering::Relaxed), 2);
    }

    // ── StepStuck / StepUnstuck engine tests ─────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn step_stuck_in_yolo_mode_starts_countdown() {
        // Uses a 2-step workflow so that step "a" is NOT the last step.
        // The last step never runs a yolo countdown (it shows the WCB
        // instead), so this test exercises the countdown on step "a".
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session(&tmp);
        let workflow = make_workflow(
            Some("wf-stuck-yolo"),
            Some("claude"),
            vec![make_step("a", &[], None), make_step("b", &["a"], None)],
        );

        let (cancel_flag_a, completion_a) = make_blocking_entry();
        let (_cancel_flag_b, completion_b) = make_blocking_entry();
        let engine_tx: Arc<Mutex<Option<_>>> = Arc::new(Mutex::new(None));

        // Frontend that tracks yolo lifecycle calls.
        struct YoloTrackingFrontend {
            actions: Mutex<VecDeque<NextAction>>,
            engine_tx: Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<EngineRequest>>>>,
            yolo_started: AtomicBool,
            yolo_finished: AtomicBool,
        }
        impl crate::engine::message::UserMessageSink for YoloTrackingFrontend {
            fn write_message(&mut self, _: crate::engine::message::UserMessage) {}
            fn replay_queued(&mut self) {}
        }
        impl WorkflowFrontend for YoloTrackingFrontend {
            fn show_workflow_control_board(
                &mut self,
                _: &WorkflowState,
                _: &AvailableActions,
            ) -> Result<NextAction, EngineError> {
                Ok(self
                    .actions
                    .lock()
                    .unwrap()
                    .pop_front()
                    .unwrap_or(NextAction::Pause))
            }
            fn yolo_countdown_tick(
                &mut self,
                _: &str,
                _: Duration,
                _: Duration,
            ) -> Result<YoloTickOutcome, EngineError> {
                // Cancel immediately to keep the test fast.
                Ok(YoloTickOutcome::Cancel)
            }
            fn yolo_countdown_started(&mut self, _: &str) {
                self.yolo_started.store(true, Ordering::Relaxed);
            }
            fn yolo_countdown_finished(&mut self, _: &str) {
                self.yolo_finished.store(true, Ordering::Relaxed);
            }
            fn confirm_resume(&mut self, _: &ResumeMismatch) -> Result<bool, EngineError> {
                Ok(true)
            }
            fn user_choose_after_step_failure(
                &mut self,
                _: &WorkflowStep,
                _: &ContainerExitInfo,
            ) -> Result<StepFailureChoice, EngineError> {
                Ok(StepFailureChoice::Abort)
            }
            fn report_step_status(&mut self, _: &WorkflowStep, _: WorkflowStepStatus) {}
            fn report_workflow_completed(&mut self, _: &WorkflowOutcome) {}
            fn set_engine_sender(&mut self, tx: tokio::sync::mpsc::UnboundedSender<EngineRequest>) {
                *self.engine_tx.lock().unwrap() = Some(tx);
            }
        }

        let frontend = YoloTrackingFrontend {
            // WCB is shown after last step completes in yolo mode.
            actions: Mutex::new(VecDeque::from([NextAction::FinishWorkflow])),
            engine_tx: engine_tx.clone(),
            yolo_started: AtomicBool::new(false),
            yolo_finished: AtomicBool::new(false),
        };

        let factory = BlockingFactory::new([
            (cancel_flag_a.clone(), completion_a.clone()),
            (_cancel_flag_b.clone(), completion_b.clone()),
        ]);
        let overlay = OverlayEngine::with_auth_resolver(
            crate::data::fs::auth_paths::AuthPathResolver::at_home(session.git_root()),
        );
        let mut engine = WorkflowEngine::new(
            &session,
            workflow,
            None,
            Box::new(frontend),
            Box::new(factory),
            Arc::new(GitEngine::new()),
            Arc::new(overlay),
        )
        .unwrap();
        engine.set_yolo(true);

        let tx = engine_tx.lock().unwrap().clone().unwrap();

        let engine_task = tokio::spawn(async move { engine.run_to_completion().await });

        tokio::time::sleep(Duration::from_millis(150)).await;
        tx.send(EngineRequest::StepStuck).unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Countdown was cancelled by the frontend (YoloTickOutcome::Cancel),
        // so step "a" keeps running. Complete it normally.
        signal_completion(&completion_a, 0);

        // Yolo auto-advances to step "b". Complete it.
        tokio::time::sleep(Duration::from_millis(150)).await;
        signal_completion(&completion_b, 0);

        let result = engine_task.await.unwrap().unwrap();
        assert_eq!(result, WorkflowOutcome::Completed);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn step_stuck_in_non_yolo_mode_shows_wcb() {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session(&tmp);
        let workflow = make_workflow(
            Some("wf-stuck-no-yolo"),
            Some("claude"),
            vec![make_step("a", &[], None), make_step("b", &["a"], None)],
        );

        let (cancel_flag, completion) = make_blocking_entry();
        let engine_tx: Arc<Mutex<Option<_>>> = Arc::new(Mutex::new(None));
        let factory = BlockingFactory::new([(cancel_flag.clone(), completion.clone())]);
        // When WCB opens due to stuck: Dismiss, then later LaunchNext between steps.
        let mut engine = make_capturing_engine(
            &session,
            workflow,
            factory,
            [NextAction::Dismiss, NextAction::LaunchNext],
            engine_tx.clone(),
        );
        // Not yolo mode.

        let tx = engine_tx.lock().unwrap().clone().unwrap();

        let engine_task = tokio::spawn(async move { engine.run_to_completion().await });

        tokio::time::sleep(Duration::from_millis(150)).await;
        tx.send(EngineRequest::StepStuck).unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Step still running (Dismiss was chosen).
        assert!(!cancel_flag.load(Ordering::Relaxed));

        signal_completion(&completion, 0);
        let result = engine_task.await.unwrap().unwrap();
        assert_eq!(result, WorkflowOutcome::Completed);
    }

    /// Sending `StepUnstuck` during an active yolo countdown must cancel the
    /// countdown and leave the step running — it must NOT mark the step
    /// Succeeded or advance to the next step. The container keeps running
    /// until it actually exits.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn step_unstuck_during_yolo_countdown_keeps_step_running() {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session(&tmp);
        let workflow = make_workflow(
            Some("wf-unstuck-mid-countdown"),
            Some("claude"),
            vec![make_step("a", &[], None), make_step("b", &["a"], None)],
        );

        let (cancel_flag_a, completion_a) = make_blocking_entry();
        let (_cancel_flag_b, completion_b) = make_blocking_entry();
        let engine_tx: Arc<Mutex<Option<_>>> = Arc::new(Mutex::new(None));

        // Frontend whose tick returns Continue so the countdown actually runs
        // (lets us send StepUnstuck mid-countdown). Captures step transitions
        // so the test can assert "a" was never marked Succeeded prematurely.
        struct UnstuckTestFrontend {
            actions: Mutex<VecDeque<NextAction>>,
            engine_tx: Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<EngineRequest>>>>,
            step_statuses: Mutex<Vec<(String, WorkflowStepStatus)>>,
        }
        impl crate::engine::message::UserMessageSink for UnstuckTestFrontend {
            fn write_message(&mut self, _: crate::engine::message::UserMessage) {}
            fn replay_queued(&mut self) {}
        }
        impl WorkflowFrontend for UnstuckTestFrontend {
            fn show_workflow_control_board(
                &mut self,
                _: &WorkflowState,
                _: &AvailableActions,
            ) -> Result<NextAction, EngineError> {
                Ok(self
                    .actions
                    .lock()
                    .unwrap()
                    .pop_front()
                    .unwrap_or(NextAction::Pause))
            }
            fn yolo_countdown_tick(
                &mut self,
                _: &str,
                _: Duration,
                _: Duration,
            ) -> Result<YoloTickOutcome, EngineError> {
                Ok(YoloTickOutcome::Continue)
            }
            fn confirm_resume(&mut self, _: &ResumeMismatch) -> Result<bool, EngineError> {
                Ok(true)
            }
            fn user_choose_after_step_failure(
                &mut self,
                _: &WorkflowStep,
                _: &ContainerExitInfo,
            ) -> Result<StepFailureChoice, EngineError> {
                Ok(StepFailureChoice::Abort)
            }
            fn report_step_status(&mut self, step: &WorkflowStep, status: WorkflowStepStatus) {
                self.step_statuses
                    .lock()
                    .unwrap()
                    .push((step.name.clone(), status));
            }
            fn report_workflow_completed(&mut self, _: &WorkflowOutcome) {}
            fn set_engine_sender(&mut self, tx: tokio::sync::mpsc::UnboundedSender<EngineRequest>) {
                *self.engine_tx.lock().unwrap() = Some(tx);
            }
        }
        let frontend = UnstuckTestFrontend {
            actions: Mutex::new(VecDeque::from([NextAction::FinishWorkflow])),
            engine_tx: engine_tx.clone(),
            step_statuses: Mutex::new(Vec::new()),
        };

        let factory = BlockingFactory::new([
            (cancel_flag_a.clone(), completion_a.clone()),
            (_cancel_flag_b.clone(), completion_b.clone()),
        ]);
        let overlay = OverlayEngine::with_auth_resolver(
            crate::data::fs::auth_paths::AuthPathResolver::at_home(session.git_root()),
        );
        let mut engine = WorkflowEngine::new(
            &session,
            workflow,
            None,
            Box::new(frontend),
            Box::new(factory),
            Arc::new(GitEngine::new()),
            Arc::new(overlay),
        )
        .unwrap();
        engine.set_yolo(true);

        let tx = engine_tx.lock().unwrap().clone().unwrap();

        let engine_task = tokio::spawn(async move { engine.run_to_completion().await });

        // Let the step launch.
        tokio::time::sleep(Duration::from_millis(150)).await;
        // Kick off the yolo countdown.
        tx.send(EngineRequest::StepStuck).unwrap();
        // Let the countdown run a tick or two without expiring.
        tokio::time::sleep(Duration::from_millis(200)).await;
        // Container produced output again — recovery signal.
        tx.send(EngineRequest::StepUnstuck).unwrap();
        // Wait long enough that, if the engine were mistakenly advancing the
        // step on Unstuck, step "b" would have launched. Cancel-flag-a must
        // still be false (step "a" still running, NOT cancelled by Advanced).
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            !cancel_flag_a.load(Ordering::Relaxed),
            "StepUnstuck during countdown must NOT cancel step 'a' — it must keep running"
        );

        // Now complete step "a" normally; workflow proceeds.
        signal_completion(&completion_a, 0);
        tokio::time::sleep(Duration::from_millis(150)).await;
        signal_completion(&completion_b, 0);

        let result = engine_task.await.unwrap().unwrap();
        assert_eq!(result, WorkflowOutcome::Completed);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn step_unstuck_outside_countdown_is_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session(&tmp);
        let workflow = make_workflow(
            Some("wf-unstuck"),
            Some("claude"),
            vec![make_step("a", &[], None), make_step("b", &["a"], None)],
        );

        let (_, completion) = make_blocking_entry();
        let engine_tx: Arc<Mutex<Option<_>>> = Arc::new(Mutex::new(None));
        let factory =
            BlockingFactory::new([(Arc::new(AtomicBool::new(false)), completion.clone())]);
        let mut engine = make_capturing_engine(
            &session,
            workflow,
            factory,
            [NextAction::LaunchNext],
            engine_tx.clone(),
        );

        let tx = engine_tx.lock().unwrap().clone().unwrap();

        let engine_task = tokio::spawn(async move { engine.run_to_completion().await });

        tokio::time::sleep(Duration::from_millis(100)).await;
        // Send StepUnstuck when there's no countdown — should be harmlessly ignored.
        tx.send(EngineRequest::StepUnstuck).unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;

        signal_completion(&completion, 0);
        let result = engine_task.await.unwrap().unwrap();
        assert_eq!(result, WorkflowOutcome::Completed);
    }

    // ── MockBackgroundContainer ───────────────────────────────────────────────

    struct MockBackgroundContainer {
        /// Pre-programmed results: (stdout, stderr, exit_code).
        results: Mutex<VecDeque<(String, String, i32)>>,
        /// Recorded commands (in call order).
        calls: Mutex<Vec<String>>,
        /// Number of times a fresh container was handed out — exercised by
        /// per-step-container assertions (WI-0082).
        container_handouts: Mutex<usize>,
    }

    impl MockBackgroundContainer {
        /// All execs succeed with empty output.
        fn always_success() -> Self {
            Self {
                results: Mutex::new(VecDeque::new()),
                calls: Mutex::new(Vec::new()),
                container_handouts: Mutex::new(0),
            }
        }

        /// Provide an explicit sequence of (stdout, stderr, exit_code) results.
        fn with_results(results: impl IntoIterator<Item = (String, String, i32)>) -> Self {
            Self {
                results: Mutex::new(results.into_iter().collect()),
                calls: Mutex::new(Vec::new()),
                container_handouts: Mutex::new(0),
            }
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }

        fn handouts(&self) -> usize {
            *self.container_handouts.lock().unwrap()
        }

        /// Build a factory closure for `WorkflowEngine::run_setup` /
        /// `run_teardown` that records one container handout per step and
        /// delegates exec calls back to this mock. Tests that previously
        /// passed `&mock` directly can now pass `mock.factory()`.
        fn factory<'a>(
            self: &'a Arc<Self>,
        ) -> impl FnMut(usize) -> Result<Box<dyn crate::engine::container::ContainerExec>, EngineError>
               + 'a {
            move |_idx| {
                *self.container_handouts.lock().unwrap() += 1;
                Ok(Box::new(SharedMockExec(Arc::clone(self))))
            }
        }
    }

    /// Trampoline that lets the test factory hand out fresh `Box<dyn
    /// ContainerExec>` values while keeping all recorded state in the single
    /// shared `MockBackgroundContainer`.
    struct SharedMockExec(Arc<MockBackgroundContainer>);

    impl crate::engine::container::ContainerExec for SharedMockExec {
        fn exec(
            &self,
            command: &str,
            env: Option<&std::collections::HashMap<String, String>>,
        ) -> Result<crate::engine::container::ExecOutput, crate::engine::error::EngineError>
        {
            self.0.exec(command, env)
        }
    }

    impl crate::engine::container::ContainerExec for MockBackgroundContainer {
        fn exec(
            &self,
            command: &str,
            _env: Option<&std::collections::HashMap<String, String>>,
        ) -> Result<crate::engine::container::ExecOutput, crate::engine::error::EngineError>
        {
            self.calls.lock().unwrap().push(command.to_string());
            let (stdout, stderr, exit_code) = self
                .results
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| ("".into(), "".into(), 0));
            Ok(crate::engine::container::ExecOutput {
                stdout,
                stderr,
                exit_code,
            })
        }
    }

    // ── run_setup / run_teardown unit tests ──────────────────────────────────

    fn setup_steps_sample() -> Vec<crate::data::workflow_definition::SetupStep> {
        use crate::data::workflow_definition::SetupStep;
        vec![
            SetupStep::CloneRepo {
                url: "https://example.com/repo".into(),
                branch: None,
                into: None,
            },
            SetupStep::PullBranch {
                remote: None,
                branch: None,
            },
            SetupStep::RunShell {
                command: "cargo build".into(),
                env: None,
            },
        ]
    }

    fn teardown_steps_sample() -> Vec<crate::data::workflow_definition::TeardownStep> {
        use crate::data::workflow_definition::TeardownStep;
        vec![
            TeardownStep::RunShell {
                command: "cargo test".into(),
                env: None,
            },
            TeardownStep::CommitChanges {
                message: "auto: results".into(),
                add_all: true,
            },
        ]
    }

    fn make_minimal_engine(tmp: &tempfile::TempDir) -> WorkflowEngine {
        let session = make_session(tmp);
        let workflow = make_workflow(
            Some("test-wf"),
            Some("claude"),
            vec![make_step("step-a", &[], None)],
        );
        make_engine(
            &session,
            workflow,
            FakeContainerExecutionFactory::always_success(),
            [],
        )
    }

    #[test]
    fn run_setup_executes_steps_in_order() {
        let tmp = tempfile::tempdir().unwrap();
        let mut engine = make_minimal_engine(&tmp);
        let steps = setup_steps_sample();
        let mock = Arc::new(MockBackgroundContainer::always_success());

        engine.run_setup(&steps, &[], mock.factory()).unwrap();

        let calls = mock.calls();
        assert_eq!(calls.len(), 3);
        assert!(calls[0].contains("git clone"));
        assert_eq!(calls[1], "git pull");
        assert_eq!(calls[2], "cargo build");
    }

    #[test]
    fn run_setup_uses_one_fresh_container_per_step() {
        // WI-0082 invariant: each phase step gets its own container so per-step
        // overlays do not leak across step boundaries.
        let tmp = tempfile::tempdir().unwrap();
        let mut engine = make_minimal_engine(&tmp);
        let steps = setup_steps_sample(); // 3 steps
        let mock = Arc::new(MockBackgroundContainer::always_success());

        engine.run_setup(&steps, &[], mock.factory()).unwrap();

        assert_eq!(
            mock.handouts(),
            3,
            "the factory must be invoked once per step (one container per step)",
        );
    }

    #[test]
    fn run_teardown_uses_one_fresh_container_per_step() {
        let tmp = tempfile::tempdir().unwrap();
        let mut engine = make_minimal_engine(&tmp);
        let steps = teardown_steps_sample(); // 2 steps
        let mock = Arc::new(MockBackgroundContainer::always_success());

        let (aborted, any_failed) = engine
            .run_teardown(&steps, &[], true, false, mock.factory())
            .unwrap();
        assert!(!aborted);
        assert!(!any_failed);

        assert_eq!(
            mock.handouts(),
            2,
            "teardown must request one container per step",
        );
    }

    #[test]
    fn run_setup_continues_on_failure_by_default() {
        let tmp = tempfile::tempdir().unwrap();
        let mut engine = make_minimal_engine(&tmp);
        let steps = setup_steps_sample(); // 3 steps
        let mock = Arc::new(MockBackgroundContainer::with_results([
            ("".into(), "".into(), 0),            // step 1 succeeds
            ("".into(), "build error".into(), 1), // step 2 fails
            ("".into(), "".into(), 0),            // step 3 still runs
        ]));

        let result = engine.run_setup(&steps, &[], mock.factory());

        assert!(
            result.is_ok(),
            "run_setup continues past failures when abort_on_failure=false"
        );
        assert_eq!(mock.calls().len(), 3, "all steps must be exec'd");
    }

    #[test]
    fn run_setup_aborts_on_abort_on_failure_step() {
        let tmp = tempfile::tempdir().unwrap();
        let mut engine = make_minimal_engine(&tmp);
        let steps = setup_steps_sample(); // 3 steps
        let mock = Arc::new(MockBackgroundContainer::with_results([
            ("".into(), "".into(), 0),            // step 1 succeeds
            ("".into(), "build error".into(), 1), // step 2 fails
            ("".into(), "".into(), 0),            // step 3 (never reached)
        ]));
        let abort_flags = vec![false, true, false]; // step 2 has abort_on_failure

        let result = engine.run_setup(&steps, &abort_flags, mock.factory());

        assert!(
            result.is_err(),
            "run_setup must return Err when abort_on_failure step fails"
        );
        assert_eq!(mock.calls().len(), 2, "third step must not be exec'd");
        assert!(engine.abort_on_failure_triggered());
    }

    #[test]
    fn run_teardown_skips_when_not_on_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let mut engine = make_minimal_engine(&tmp);
        let steps = teardown_steps_sample();
        let mock = Arc::new(MockBackgroundContainer::always_success());

        // teardown_on_failure = false, workflow_succeeded = false → skip all
        let (aborted, any_failed) = engine
            .run_teardown(&steps, &[], false, false, mock.factory())
            .unwrap();
        assert!(!aborted);
        assert!(!any_failed);

        assert_eq!(mock.calls().len(), 0, "no exec calls should be made");
        assert_eq!(
            mock.handouts(),
            0,
            "no containers should be requested when teardown is skipped",
        );
    }

    #[test]
    fn run_teardown_runs_when_succeeded() {
        let tmp = tempfile::tempdir().unwrap();
        let mut engine = make_minimal_engine(&tmp);
        let steps = teardown_steps_sample();
        let mock = Arc::new(MockBackgroundContainer::always_success());

        let (aborted, any_failed) = engine
            .run_teardown(&steps, &[], true, false, mock.factory())
            .unwrap();
        assert!(!aborted);
        assert!(!any_failed);

        assert_eq!(mock.calls().len(), 2, "both teardown steps must exec");
    }

    #[test]
    fn run_teardown_continues_after_step_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let mut engine = make_minimal_engine(&tmp);
        let steps = teardown_steps_sample();
        let mock = Arc::new(MockBackgroundContainer::with_results([
            ("".into(), "test failure".into(), 1), // step 1 fails
            ("".into(), "".into(), 0),             // step 2 succeeds
        ]));

        // Teardown is best-effort: returns Ok even if a step fails.
        let result = engine.run_teardown(&steps, &[], true, false, mock.factory());
        assert!(
            result.is_ok(),
            "run_teardown must return Ok despite step failure"
        );
        let (aborted, any_failed) = result.unwrap();
        assert!(!aborted, "no abort_on_failure steps were set");
        assert!(
            any_failed,
            "any_step_failed must be true when a step exits non-zero"
        );
        assert_eq!(mock.calls().len(), 2, "both steps must be exec'd");
    }

    #[test]
    fn run_teardown_aborts_on_abort_on_failure_step() {
        let tmp = tempfile::tempdir().unwrap();
        let mut engine = make_minimal_engine(&tmp);
        let steps = teardown_steps_sample();
        let mock = Arc::new(MockBackgroundContainer::with_results([
            ("".into(), "fatal".into(), 1), // step 0 fails
            ("".into(), "".into(), 0),      // step 1 would succeed
        ]));

        // abort_on_failure = true for step 0
        let result = engine.run_teardown(&steps, &[true, false], true, false, mock.factory());
        assert!(result.is_ok());
        let (aborted, any_failed) = result.unwrap();
        assert!(
            aborted,
            "run_teardown must set aborted when abort_on_failure step fails"
        );
        assert!(any_failed, "any_step_failed must also be true");
        assert_eq!(
            mock.calls().len(),
            1,
            "step 1 must be skipped after abort_on_failure step 0 fails"
        );
    }

    #[test]
    fn run_teardown_continues_after_per_step_container_factory_failure() {
        // Per-step container build failure must not abort teardown; it should
        // record the step as Failed and proceed to the next one.
        use crate::data::workflow_definition::TeardownStep;
        use crate::data::workflow_state::PhaseStepStatus;

        let tmp = tempfile::tempdir().unwrap();
        let mut engine = make_minimal_engine(&tmp);
        let steps = vec![
            TeardownStep::RunShell {
                command: "first".into(),
                env: None,
            },
            TeardownStep::RunShell {
                command: "second".into(),
                env: None,
            },
        ];

        // Factory fails on step 0 (returns Err), succeeds on step 1.
        let mock = Arc::new(MockBackgroundContainer::always_success());
        let mock_for_factory = Arc::clone(&mock);
        let factory = move |idx: usize| -> Result<
            Box<dyn crate::engine::container::ContainerExec>,
            EngineError,
        > {
            if idx == 0 {
                Err(EngineError::Other(
                    "simulated overlay resolve failure".into(),
                ))
            } else {
                *mock_for_factory.container_handouts.lock().unwrap() += 1;
                Ok(Box::new(SharedMockExec(Arc::clone(&mock_for_factory))))
            }
        };

        let result = engine.run_teardown(&steps, &[], true, false, factory);
        assert!(result.is_ok(), "factory failure must not abort teardown");
        let (_aborted, any_failed) = result.unwrap();
        assert!(
            any_failed,
            "any_step_failed must be true when factory fails"
        );

        let states = &engine.state().teardown_step_states;
        assert!(
            matches!(&states[0].status, PhaseStepStatus::Failed { error } if error.contains("simulated overlay resolve failure")),
            "step 0 must be recorded as Failed with the factory error: {:?}",
            states[0].status,
        );
        assert_eq!(
            states[1].status,
            PhaseStepStatus::Succeeded,
            "step 1 must still execute after step 0's factory failure",
        );
        assert_eq!(mock.calls().len(), 1, "only step 1 reaches exec");
    }

    #[test]
    fn run_setup_transitions_phase_to_main_on_success() {
        use crate::data::workflow_state::WorkflowPhase;

        let tmp = tempfile::tempdir().unwrap();
        let mut engine = make_minimal_engine(&tmp);
        let steps = setup_steps_sample();
        let mock = Arc::new(MockBackgroundContainer::always_success());

        engine.run_setup(&steps, &[], mock.factory()).unwrap();

        assert_eq!(
            engine.state().current_phase,
            WorkflowPhase::Main,
            "phase must be Main after successful setup"
        );
        assert!(
            engine.state().setup_completed,
            "setup_completed must be true after successful setup"
        );
    }

    #[test]
    fn run_setup_state_tracking() {
        use crate::data::workflow_state::PhaseStepStatus;

        let tmp = tempfile::tempdir().unwrap();
        let mut engine = make_minimal_engine(&tmp);

        use crate::data::workflow_definition::SetupStep;
        let steps = vec![
            SetupStep::RunShell {
                command: "step1".into(),
                env: None,
            },
            SetupStep::RunShell {
                command: "step2".into(),
                env: None,
            },
        ];
        let mock = Arc::new(MockBackgroundContainer::always_success());

        engine.run_setup(&steps, &[], mock.factory()).unwrap();

        let states = &engine.state().setup_step_states;
        assert_eq!(states.len(), 2);
        assert_eq!(states[0].status, PhaseStepStatus::Succeeded);
        assert_eq!(states[1].status, PhaseStepStatus::Succeeded);
    }

    #[test]
    fn run_teardown_state_tracking() {
        use crate::data::workflow_state::PhaseStepStatus;

        let tmp = tempfile::tempdir().unwrap();
        let mut engine = make_minimal_engine(&tmp);

        use crate::data::workflow_definition::TeardownStep;
        let steps = vec![
            TeardownStep::RunShell {
                command: "td1".into(),
                env: None,
            },
            TeardownStep::RunShell {
                command: "td2".into(),
                env: None,
            },
        ];
        let mock = Arc::new(MockBackgroundContainer::always_success());

        let (aborted, any_failed) = engine
            .run_teardown(&steps, &[], true, false, mock.factory())
            .unwrap();
        assert!(!aborted);
        assert!(!any_failed);

        let states = &engine.state().teardown_step_states;
        assert_eq!(states.len(), 2);
        assert_eq!(states[0].status, PhaseStepStatus::Succeeded);
        assert_eq!(states[1].status, PhaseStepStatus::Succeeded);
    }

    #[test]
    fn run_setup_failure_records_failed_state() {
        use crate::data::workflow_state::PhaseStepStatus;

        let tmp = tempfile::tempdir().unwrap();
        let mut engine = make_minimal_engine(&tmp);

        use crate::data::workflow_definition::SetupStep;
        let steps = vec![
            SetupStep::RunShell {
                command: "ok-step".into(),
                env: None,
            },
            SetupStep::RunShell {
                command: "bad-step".into(),
                env: None,
            },
        ];
        let mock = Arc::new(MockBackgroundContainer::with_results([
            ("".into(), "".into(), 0),
            ("".into(), "stderr content".into(), 1),
        ]));

        let result = engine.run_setup(&steps, &[], mock.factory());
        assert!(
            result.is_ok(),
            "setup continues past failures when abort_on_failure=false"
        );

        let states = &engine.state().setup_step_states;
        assert_eq!(states[0].status, PhaseStepStatus::Succeeded);
        assert!(
            matches!(&states[1].status, PhaseStepStatus::Failed { error } if error == "stderr content"),
            "failed state must capture stderr: {:?}",
            states[1].status
        );
    }

    #[test]
    fn run_teardown_transitions_phase_to_done() {
        use crate::data::workflow_state::WorkflowPhase;

        let tmp = tempfile::tempdir().unwrap();
        let mut engine = make_minimal_engine(&tmp);
        let steps = teardown_steps_sample();
        let mock = Arc::new(MockBackgroundContainer::always_success());

        let (_aborted, _any_failed) = engine
            .run_teardown(&steps, &[], true, false, mock.factory())
            .unwrap();

        assert_eq!(
            engine.state().current_phase,
            WorkflowPhase::Done,
            "phase must be Done after teardown completes"
        );
        assert!(engine.state().teardown_completed);
    }

    #[test]
    fn mark_done_sets_phase_to_done() {
        use crate::data::workflow_state::WorkflowPhase;

        let tmp = tempfile::tempdir().unwrap();
        let mut engine = make_minimal_engine(&tmp);
        assert_eq!(engine.state().current_phase, WorkflowPhase::Main);

        engine.mark_done().unwrap();
        assert_eq!(engine.state().current_phase, WorkflowPhase::Done);
    }

    #[test]
    fn run_setup_phase_persistence_verified_from_store() {
        use crate::data::workflow_state::WorkflowPhase;

        let tmp = tempfile::tempdir().unwrap();
        let mut engine = make_minimal_engine(&tmp);

        use crate::data::workflow_definition::SetupStep;
        let steps = vec![SetupStep::RunShell {
            command: "go".into(),
            env: None,
        }];
        let mock = Arc::new(MockBackgroundContainer::always_success());

        engine.run_setup(&steps, &[], mock.factory()).unwrap();

        // Verify the on-disk state was persisted with the correct phase fields.
        let store = WorkflowStateStore::at_git_root(tmp.path());
        let saved = store.load(None, "test-wf").unwrap().unwrap();
        assert_eq!(saved.current_phase, WorkflowPhase::Main);
        assert!(saved.setup_completed);
    }
}
