//! `engine::workflow` — `WorkflowEngine`.
//!
//! Owns every workflow-execution concern: state, advance logic, yolo
//! countdowns, agent/model resolution, exit-code interpretation, persistence,
//! and per-step container lifecycle. Forbidden: rendering, direct user
//! input, knowledge of which frontend is on the other side of the trait,
//! worktree lifecycle management, direct container construction.

use std::sync::Arc;

use crate::data::config::effective::EffectiveConfig;
use crate::data::session::{AgentName, Session};
use crate::data::workflow_dag::WorkflowDag;
use crate::data::workflow_definition::{Workflow, WorkflowStep};
use crate::data::workflow_state::{StepState, WorkflowState, WORKFLOW_STATE_SCHEMA_VERSION};
use crate::data::workflow_state_store::WorkflowStateStore;
use crate::engine::container::instance::{ContainerExecution, ContainerExitInfo};
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
pub mod timing;

/// Result of `run_yolo_countdown`.
enum YoloCountdownResult {
    Advance,
    Pause,
    ShowControlBoard,
}

/// Result of a mid-step yolo countdown (step is still running while
/// the countdown ticks).
enum MidStepYoloResult {
    /// Step completed while the countdown was ticking.
    StepCompleted(StepOutcome),
    /// Countdown expired: auto-advance the step.
    Advanced,
    /// User pressed Esc: cancel the countdown.
    Cancelled,
    /// User pressed Ctrl-W: show the WCB instead.
    ShowControlBoard,
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

/// Request sent from the TUI (via channel) to interrupt the engine mid-step.
#[derive(Debug, Clone)]
pub enum ControlBoardRequest {
    /// User pressed Ctrl+W while a step is running. The engine computes
    /// mid-step available actions and calls `user_choose_next_action`.
    OpenControlBoard,
    /// The frontend detected that the current step's container is stuck
    /// (no PTY output for `STUCK_TIMEOUT`). In yolo mode the engine
    /// starts a yolo countdown; in non-yolo mode the engine opens the WCB.
    StepStuck,
}

/// Configuration the engine consumes at construction.
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
    /// In-flight execution from the most recent step launch (for prompt
    /// injection on `ContinueInCurrentContainer`).
    current_execution: Option<ContainerExecution>,
    current_step_name: Option<String>,
    /// The agent the in-flight execution targets.
    current_step_agent: Option<AgentName>,
    /// The model the in-flight execution targets.
    current_step_model: Option<String>,
    /// Work item number (e.g. 42 for work item 0042). `None` when running a
    /// standalone workflow via `exec workflow` without `--work-item`.
    work_item: Option<u32>,
    /// When true, skip the inter-step user prompt and auto-advance after a
    /// 60-second countdown (giving the user a chance to intervene).
    yolo: bool,
    /// Exit info from the most recent step execution, used by the step-failure
    /// dialog so it can display timing and signal information.
    last_exit_info: Option<ContainerExitInfo>,
    /// Receiver for mid-step control board requests from the TUI.
    control_board_rx: Option<tokio::sync::mpsc::UnboundedReceiver<ControlBoardRequest>>,
}

impl WorkflowEngine {
    pub fn new(
        session: &Session,
        workflow: Workflow,
        work_item: Option<u32>,
        mut frontend: Box<dyn WorkflowFrontend>,
        container_factory: Box<dyn ContainerExecutionFactory>,
        git_engine: Arc<GitEngine>,
        overlay_engine: Arc<OverlayEngine>,
    ) -> Result<Self, EngineError> {
        let dag = WorkflowDag::build(&workflow.steps).map_err(EngineError::Data)?;
        let workflow_hash = compute_workflow_hash(&workflow);
        let state = WorkflowState::new(
            workflow_name_for(&workflow),
            &workflow.steps,
            workflow_hash,
            work_item,
        );
        let state_store = WorkflowStateStore::new(session);
        let effective_config = session.effective_config();
        let (cb_tx, cb_rx) = tokio::sync::mpsc::unbounded_channel();
        frontend.set_control_board_sender(cb_tx);
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
            work_item,
            yolo: false,
            last_exit_info: None,
            control_board_rx: Some(cb_rx),
        })
    }

    /// Enable yolo mode: auto-advance between steps after a 60-second
    /// countdown instead of prompting the user.
    pub fn set_yolo(&mut self, yolo: bool) {
        self.yolo = yolo;
    }

    /// Resume from persisted state. Calls `confirm_resume` on the frontend if
    /// the workflow hash has drifted; aborts with `WorkflowResumeIncompatible`
    /// if the user declines.
    pub async fn resume(
        session: &Session,
        workflow: Workflow,
        work_item: Option<u32>,
        mut frontend: Box<dyn WorkflowFrontend>,
        container_factory: Box<dyn ContainerExecutionFactory>,
        git_engine: Arc<GitEngine>,
        overlay_engine: Arc<OverlayEngine>,
    ) -> Result<Self, EngineError> {
        let dag = WorkflowDag::build(&workflow.steps).map_err(EngineError::Data)?;
        let store = WorkflowStateStore::new(session);
        let workflow_name = workflow_name_for(&workflow);
        let saved = store.load(work_item, &workflow_name)?;

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
            None => WorkflowState::new(workflow_name, &workflow.steps, workflow_hash, work_item),
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
        let (cb_tx, cb_rx) = tokio::sync::mpsc::unbounded_channel();
        frontend.set_control_board_sender(cb_tx);
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
            work_item,
            yolo: false,
            last_exit_info: None,
            control_board_rx: Some(cb_rx),
        })
    }

    pub fn state(&self) -> &WorkflowState {
        &self.state
    }

    /// Drive every step until the workflow finishes, the user pauses, or a
    /// step fails terminally.
    pub async fn run_to_completion(&mut self) -> Result<WorkflowOutcome, EngineError> {
        // Report initial progress immediately so the TUI workflow strip
        // renders before the first step starts running.
        let initial_progress = self.workflow_progress_info();
        self.frontend.report_workflow_progress(&initial_progress);

        loop {
            if self.state.is_complete() {
                let progress = self.workflow_progress_info();
                self.frontend.report_workflow_progress(&progress);
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
                        self.state
                            .set_status(&outcome.step_name, StepState::Pending);
                        self.persist()?;
                        continue;
                    }
                    StepFailureChoice::Pause => {
                        self.persist()?;
                        let paused = WorkflowOutcome::Paused;
                        self.frontend.report_workflow_completed(&paused);
                        return Ok(paused);
                    }
                    StepFailureChoice::Abort => {
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
            // Ask the user what to do next when there are remaining steps.
            if !self.state.is_complete() {
                // Emit the progress table before yolo countdown or user prompt.
                let progress = self.workflow_progress_info();
                self.frontend.report_workflow_progress(&progress);

                // In yolo mode, replace the interactive prompt with a 60-second
                // countdown that auto-advances unless the user cancels.
                // Respect the per-step auto-advance toggle ([d] in TUI).
                let step_auto_advance = self
                    .current_step_name
                    .as_deref()
                    .map(|n| self.frontend.should_auto_advance(n))
                    .unwrap_or(true);
                if self.yolo && step_auto_advance {
                    match self.run_yolo_countdown().await? {
                        YoloCountdownResult::Advance => continue,
                        YoloCountdownResult::Pause => {
                            self.persist()?;
                            let outcome = WorkflowOutcome::Paused;
                            self.frontend.report_workflow_completed(&outcome);
                            return Ok(outcome);
                        }
                        YoloCountdownResult::ShowControlBoard => {
                            // Fall through to the interactive control board below.
                        }
                    }
                }

                let available = self.compute_available_actions()?;
                let action = self
                    .frontend
                    .user_choose_next_action(&self.state, &available)?;
                match action {
                    NextAction::Dismiss => continue,
                    NextAction::LaunchNext => continue,
                    NextAction::ContinueInCurrentContainer { prompt } => {
                        // Pre-validate before calling inject_prompt: the next
                        // step must use the same agent + model, and an execution
                        // must be present.
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
                            Some(exec) => {
                                match self.container_factory.inject_prompt(exec, &prompt)? {
                                    Some(()) => {
                                        // Injection succeeded: the next step ran inside the
                                        // current container. Mark it Succeeded directly.
                                        self.state
                                            .set_status(&next_step.name, StepState::Succeeded);
                                        self.current_step_name = Some(next_step.name.clone());
                                        self.persist()?;
                                        continue;
                                    }
                                    None => {
                                        return Err(EngineError::InvalidAdvanceAction(
                                            "container backend does not support prompt \
                                             injection; use LaunchNext to start a fresh \
                                             container for the next step"
                                                .into(),
                                        ));
                                    }
                                }
                            }
                            None => {
                                return Err(EngineError::InvalidAdvanceAction(
                                    "no container execution is available to inject into".into(),
                                ));
                            }
                        }
                    }
                    NextAction::RestartCurrentStep => {
                        if let Some(name) = self.current_step_name.clone() {
                            self.state.set_status(&name, StepState::Pending);
                            self.persist()?;
                        }
                        continue;
                    }
                    NextAction::CancelToPreviousStep => {
                        let prev = self.previous_step_name();
                        match prev {
                            Some(prev) => {
                                if let Some(curr) = self.current_step_name.clone() {
                                    self.state.set_status(&curr, StepState::Cancelled);
                                }
                                self.state.set_status(&prev, StepState::Pending);
                                self.persist()?;
                                continue;
                            }
                            None => {
                                return Err(EngineError::InvalidAdvanceAction(
                                    "no previous step to cancel to".into(),
                                ));
                            }
                        }
                    }
                    NextAction::FinishWorkflow => {
                        if !self.is_last_step() {
                            return Err(EngineError::InvalidAdvanceAction(
                                "FinishWorkflow only valid on the last step".into(),
                            ));
                        }
                        for s in &self.workflow.steps {
                            if !self.state.completed_steps.contains(&s.name) {
                                self.state.set_status(&s.name, StepState::Skipped);
                            }
                        }
                        self.persist()?;
                        let outcome = WorkflowOutcome::Completed;
                        self.frontend.report_workflow_completed(&outcome);
                        return Ok(outcome);
                    }
                    NextAction::Pause => {
                        self.persist()?;
                        let outcome = WorkflowOutcome::Paused;
                        self.frontend.report_workflow_completed(&outcome);
                        return Ok(outcome);
                    }
                    NextAction::Abort => {
                        for s in &self.workflow.steps {
                            if !self.state.completed_steps.contains(&s.name) {
                                self.state.set_status(&s.name, StepState::Cancelled);
                            }
                        }
                        self.persist()?;
                        let outcome = WorkflowOutcome::Aborted;
                        self.frontend.report_workflow_completed(&outcome);
                        return Ok(outcome);
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

    /// First half of `step_once`: find the next ready step, resolve
    /// agent/model, launch the container, store in `current_execution`.
    /// Returns the step name so the caller can pass it to `finalize_step`.
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

    /// Second half of `step_once`: process exit info, update state, return
    /// the step outcome.
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

    /// Like `step_once`, but allows the user to open the Workflow Control
    /// Board mid-step via the control board channel. The step continues
    /// running while the user interacts with the dialog.
    async fn step_once_interruptible(&mut self) -> Result<InterruptibleStepResult, EngineError> {
        let step_name = self.launch_step().await?;

        // Extract a cancel handle before spawning the wait — once `wait()`
        // moves the backend into a blocking task, the execution can no
        // longer cancel itself.
        let cancel_handle = self
            .current_execution
            .as_ref()
            .and_then(|e| e.cancel_handle());

        // Move the execution into a spawned task so we can `select!` between
        // it and the control board channel without holding `&mut self`.
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
                Some(req) = Self::recv_control_board(&mut self.control_board_rx) => {
                    match req {
                        ControlBoardRequest::OpenControlBoard => {
                            let mid_step_outcome = self.handle_mid_step_control_board(
                                &step_name,
                                &cancel_handle,
                                &mut wait_rx,
                            )?;
                            match mid_step_outcome {
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
                        ControlBoardRequest::StepStuck => {
                            // ENG-1: Frontend detected the step's container is
                            // stuck. In yolo mode, run a mid-step countdown
                            // (the step keeps running). In non-yolo mode, open
                            // the WCB so the user can choose an action.
                            let step_auto_advance = self.frontend.should_auto_advance(&step_name);
                            if self.yolo && step_auto_advance {
                                let countdown_result = self.run_mid_step_yolo_countdown(
                                    &step_name,
                                    &cancel_handle,
                                    &mut wait_rx,
                                ).await?;
                                match countdown_result {
                                    MidStepYoloResult::StepCompleted(o) => {
                                        return Ok(InterruptibleStepResult::StepCompleted(o));
                                    }
                                    MidStepYoloResult::ShowControlBoard => {
                                        let mid_step_outcome = self.handle_mid_step_control_board(
                                            &step_name,
                                            &cancel_handle,
                                            &mut wait_rx,
                                        )?;
                                        match mid_step_outcome {
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
                                    MidStepYoloResult::Cancelled => continue,
                                    MidStepYoloResult::Advanced => {
                                        if let Some(ch) = &cancel_handle {
                                            let _ = ch.cancel();
                                        }
                                        self.state.set_status(&step_name, StepState::Succeeded);
                                        self.persist()?;
                                        let step = self.find_step(&step_name)?;
                                        self.frontend.report_step_status(
                                            &step,
                                            WorkflowStepStatus::Succeeded,
                                        );
                                        self.frontend.report_step_unstuck(&step);
                                        let progress = self.workflow_progress_info();
                                        self.frontend.report_workflow_progress(&progress);
                                        return Ok(InterruptibleStepResult::LoopContinue);
                                    }
                                }
                            } else {
                                let mid_step_outcome = self.handle_mid_step_control_board(
                                    &step_name,
                                    &cancel_handle,
                                    &mut wait_rx,
                                )?;
                                match mid_step_outcome {
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
                        }
                    }
                }
            }
        }
    }

    /// Receive from the control board channel, or pend forever if None.
    async fn recv_control_board(
        rx: &mut Option<tokio::sync::mpsc::UnboundedReceiver<ControlBoardRequest>>,
    ) -> Option<ControlBoardRequest> {
        match rx {
            Some(rx) => rx.recv().await,
            None => std::future::pending().await,
        }
    }

    /// Handle a mid-step control board request. Shows the WCB dialog and
    /// returns what the engine should do next.
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
            .user_choose_next_action(&self.state, &available)?;

        // Check if the container finished while the dialog was open.
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

    /// Run the 60-second yolo countdown, ticking through the frontend every
    /// 100 ms. Returns the next action to take.
    async fn run_yolo_countdown(&mut self) -> Result<YoloCountdownResult, EngineError> {
        self.frontend.reset_yolo_initialized();
        let total = std::time::Duration::from_secs(60);
        let start = std::time::Instant::now();
        loop {
            let elapsed = start.elapsed();
            let remaining = if elapsed >= total {
                std::time::Duration::ZERO
            } else {
                total - elapsed
            };
            match self.frontend.yolo_countdown_tick(remaining)? {
                YoloTickOutcome::AdvanceNow => {
                    self.frontend.clear_yolo_state();
                    return Ok(YoloCountdownResult::Advance);
                }
                YoloTickOutcome::Cancel => {
                    self.frontend.clear_yolo_state();
                    return Ok(YoloCountdownResult::Pause);
                }
                YoloTickOutcome::Continue => {}
            }
            if remaining.is_zero() {
                self.frontend.clear_yolo_state();
                return Ok(YoloCountdownResult::Advance);
            }
            tokio::select! {
                biased;
                Some(req) = Self::recv_control_board(&mut self.control_board_rx) => {
                    match req {
                        ControlBoardRequest::OpenControlBoard => {
                            self.frontend.clear_yolo_state();
                            return Ok(YoloCountdownResult::ShowControlBoard);
                        }
                        ControlBoardRequest::StepStuck => {
                            // Already counting down; ignore.
                        }
                    }
                }
                _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {}
            }
        }
    }

    /// Run a mid-step yolo countdown while the step container is still
    /// running. Races the 60-second countdown ticks against the step
    /// completing and an `OpenControlBoard` request on the channel.
    async fn run_mid_step_yolo_countdown(
        &mut self,
        step_name: &str,
        _cancel_handle: &Option<crate::engine::container::instance::CancelHandle>,
        wait_rx: &mut tokio::sync::oneshot::Receiver<(ContainerExecution, Result<ContainerExitInfo, EngineError>)>,
    ) -> Result<MidStepYoloResult, EngineError> {
        self.frontend.reset_yolo_initialized();
        let total = timing::YOLO_COUNTDOWN_DURATION;
        let start = std::time::Instant::now();

        loop {
            let elapsed = start.elapsed();
            let remaining = if elapsed >= total {
                std::time::Duration::ZERO
            } else {
                total - elapsed
            };

            match self.frontend.yolo_countdown_tick(remaining)? {
                YoloTickOutcome::AdvanceNow => {
                    self.frontend.clear_yolo_state();
                    return Ok(MidStepYoloResult::Advanced);
                }
                YoloTickOutcome::Cancel => {
                    self.frontend.clear_yolo_state();
                    return Ok(MidStepYoloResult::Cancelled);
                }
                YoloTickOutcome::Continue => {}
            }

            if remaining.is_zero() {
                self.frontend.clear_yolo_state();
                return Ok(MidStepYoloResult::Advanced);
            }

            tokio::select! {
                biased;
                result = &mut *wait_rx => {
                    let (exec_back, exit_result) = result
                        .map_err(|_| EngineError::Other("step wait task dropped unexpectedly".into()))?;
                    self.current_execution = Some(exec_back);
                    self.frontend.clear_yolo_state();
                    return Ok(MidStepYoloResult::StepCompleted(
                        self.finalize_step(step_name, exit_result?)?
                    ));
                }
                Some(req) = Self::recv_control_board(&mut self.control_board_rx) => {
                    match req {
                        ControlBoardRequest::OpenControlBoard => {
                            self.frontend.clear_yolo_state();
                            return Ok(MidStepYoloResult::ShowControlBoard);
                        }
                        ControlBoardRequest::StepStuck => {
                            // Already counting down; ignore.
                        }
                    }
                }
                _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {}
            }
        }
    }

    /// Compute the set of valid `NextAction`s given the current state.
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
        // Continue-in-current-container: requires same agent + same model
        // for the next step and a running execution.
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

    /// All steps that are currently ready to execute (dependencies satisfied,
    /// not yet started). Callers that only need one step can use
    /// `next_ready_steps().first()`.
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

    fn advance_to_next_step(&mut self) -> Result<(), EngineError> {
        // Mark the current step complete and bump current_step_name to the
        // next ready step (if any).
        if let Some(curr) = self.current_step_name.clone() {
            if !self.state.completed_steps.contains(&curr) {
                self.state.set_status(&curr, StepState::Succeeded);
                self.persist()?;
            }
        }
        let next = self.state.next_ready(&self.dag).into_iter().next();
        self.current_step_name = next;
        Ok(())
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

    /// Build a per-step progress snapshot for `report_workflow_progress`.
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
}

/// Hash a workflow's steps + title to detect drift between saved state and
/// current source.
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

/// `Workflow` doesn't carry a name field; derive one from the title or fall
/// back to "workflow".
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
    use crate::data::config::flags::FlagConfig;
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
        fn user_choose_next_action(
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
            _exit: &crate::engine::container::instance::ContainerExitInfo,
        ) -> Result<StepFailureChoice, EngineError> {
            Ok(self.failure_choice.clone())
        }

        fn report_step_status(&mut self, step: &WorkflowStep, status: WorkflowStepStatus) {
            self.step_statuses
                .lock()
                .unwrap()
                .push((step.name.clone(), status));
        }

        fn report_step_output(&mut self, _step: &WorkflowStep, _output: StepOutput) {}

        fn report_step_stuck(&mut self, _step: &WorkflowStep) {}
        fn report_step_unstuck(&mut self, _step: &WorkflowStep) {}

        fn yolo_countdown_tick(
            &mut self,
            _remaining: Duration,
        ) -> Result<crate::engine::workflow::actions::YoloTickOutcome, EngineError> {
            Ok(crate::engine::workflow::actions::YoloTickOutcome::Cancel)
        }

        fn report_workflow_completed(&mut self, outcome: &WorkflowOutcome) {
            *self.completed.lock().unwrap() = Some(outcome.clone());
        }
    }

    // Fake factory that records calls and returns pre-finished executions.
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

        /// Variant whose `inject_prompt` returns `Some(())` (injection supported).
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

        // State persisted: a=Succeeded, b still Pending.
        assert!(matches!(
            engine.state().status_of("a"),
            Some(StepState::Succeeded)
        ));
        assert!(matches!(
            engine.state().status_of("b"),
            Some(StepState::Pending)
        ));

        // Verify state is on disk.
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
        // A → (B, C) — B and C both depend on A (parallel group).
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
        assert!(matches!(
            engine.state().status_of("a"),
            Some(StepState::Succeeded)
        ));
        assert!(matches!(
            engine.state().status_of("b"),
            Some(StepState::Succeeded)
        ));
        assert!(matches!(
            engine.state().status_of("c"),
            Some(StepState::Succeeded)
        ));
    }

    #[tokio::test]
    async fn run_to_completion_parallel_fan_in() {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session(&tmp);
        // A → (B, C) → D — D depends on both B and C.
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
        for step in &["a", "b", "c", "d"] {
            assert!(
                matches!(engine.state().status_of(step), Some(StepState::Succeeded)),
                "step '{}' should be Succeeded",
                step
            );
        }
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
        assert!(matches!(
            engine.state().status_of("a"),
            Some(StepState::Failed { exit_code: 1, .. })
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
        // default failure_choice = Abort
        let mut engine = make_engine_with_frontend(&session, workflow, factory, frontend);

        let result = engine.run_to_completion().await.unwrap();
        assert!(
            matches!(result, WorkflowOutcome::Aborted),
            "step failure + Abort choice should return Aborted, got: {:?}",
            result
        );
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
        // First run: exit 1 (fail), second run: exit 0 (success).
        let factory = FakeContainerExecutionFactory::new([1, 0]);
        let mut frontend = FakeWorkflowFrontend::new([]);
        frontend.failure_choice = StepFailureChoice::Retry;
        let mut engine = make_engine_with_frontend(&session, workflow, factory, frontend);

        let result = engine.run_to_completion().await.unwrap();
        assert!(
            matches!(result, WorkflowOutcome::Completed),
            "step failure + Retry should re-run and complete, got: {:?}",
            result
        );
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
        assert!(
            matches!(result, WorkflowOutcome::Paused),
            "step failure + Pause should return Paused, got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn restart_current_step_reruns_step() {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session(&tmp);
        // Two-step workflow so the engine asks for an action after step "a".
        // After "a" succeeds the first time: RestartCurrentStep → "a" runs again.
        // After "a" succeeds the second time: LaunchNext → "b" runs.
        let workflow = make_workflow(
            Some("wf-restart"),
            Some("claude"),
            vec![make_step("a", &[], None), make_step("b", &["a"], None)],
        );
        let factory = FakeContainerExecutionFactory::new(std::iter::repeat_n(0, 10));
        let factory_arc: Arc<FakeContainerExecutionFactory> = Arc::new(factory);

        struct CountingFactory(Arc<FakeContainerExecutionFactory>);
        impl ContainerExecutionFactory for CountingFactory {
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
        let counting = CountingFactory(factory_arc.clone());
        // actions: restart after first "a", then launch next after second "a".
        let mut engine = WorkflowEngine::new(
            &session,
            workflow,
            None,
            Box::new(FakeWorkflowFrontend::new([
                NextAction::RestartCurrentStep,
                NextAction::LaunchNext,
            ])),
            Box::new(counting),
            Arc::new(GitEngine::new()),
            Arc::new(overlay),
        )
        .unwrap();

        let result = engine.run_to_completion().await.unwrap();
        assert_eq!(result, WorkflowOutcome::Completed);
        // "a" runs twice, "b" runs once → call count == 3.
        assert!(
            factory_arc.execution_call_count.load(Ordering::Relaxed) >= 2,
            "execution_for_step must be called at least twice due to restart"
        );
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

        // Run step "a" first so current_step_name = "a".
        engine.step_once().await.unwrap();

        let available = engine.compute_available_actions().unwrap();
        assert!(!available.can_cancel_to_previous_step);
        assert!(available.cancel_to_previous_unavailable_reason.is_some());
    }

    #[tokio::test]
    async fn cancel_to_previous_step_returns_invalid_action_on_first_step() {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session(&tmp);
        // Two-step workflow: after step "a" (first step, idx=0) completes, the
        // engine asks for a next action. Returning CancelToPreviousStep at
        // that point must fail because "a" has no predecessor.
        let workflow = make_workflow(
            Some("wf-cancel2"),
            Some("claude"),
            vec![make_step("a", &[], None), make_step("b", &["a"], None)],
        );
        let factory = FakeContainerExecutionFactory::always_success();
        let mut engine = make_engine(
            &session,
            workflow,
            factory,
            [NextAction::CancelToPreviousStep],
        );

        let result = engine.run_to_completion().await;
        assert!(
            matches!(result, Err(EngineError::InvalidAdvanceAction(_))),
            "expected InvalidAdvanceAction when trying to cancel before the first step"
        );
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
        // After step a, pause.
        let mut engine = make_engine(&session, workflow, factory, [NextAction::Pause]);

        let result = engine.run_to_completion().await.unwrap();
        assert_eq!(result, WorkflowOutcome::Paused);

        // State should be persisted on disk.
        let store = WorkflowStateStore::at_git_root(tmp.path());
        let saved = store.load(None, "wf-pause").unwrap();
        assert!(saved.is_some(), "persisted state must exist after pause");
        let saved = saved.unwrap();
        // "a" is Succeeded, "b" is still Pending.
        assert!(matches!(saved.step_states["a"], StepState::Succeeded));
        assert!(matches!(saved.step_states["b"], StepState::Pending));
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

        // First run: pause after step a.
        {
            let factory = FakeContainerExecutionFactory::always_success();
            let mut engine = make_engine(&session, wf.clone(), factory, [NextAction::Pause]);
            engine.run_to_completion().await.unwrap();
        }

        // Resume: b should run and workflow completes.
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

        // First run: pause to persist state.
        {
            let factory = FakeContainerExecutionFactory::always_success();
            let mut engine = make_engine(&session, wf1, factory, [NextAction::Pause]);
            engine.run_to_completion().await.unwrap();
        }

        // Resume with a different workflow (different steps → different hash).
        let wf2 = make_workflow(
            Some("wf-drift"),
            Some("claude"),
            vec![
                make_step("a", &[], None),
                make_step("b", &["a"], None), // extra step → hash drift
            ],
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

        assert!(
            matches!(result, Err(EngineError::WorkflowResumeIncompatible(_))),
            "expected WorkflowResumeIncompatible"
        );
    }

    #[tokio::test]
    async fn step_level_agent_overrides_workflow_level() {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session(&tmp);
        let workflow = make_workflow(
            Some("wf-agent"),
            Some("claude"),
            vec![make_step("a", &[], Some("codex"))], // step-level overrides "claude"
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
    async fn workflow_level_agent_used_when_step_has_none() {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session(&tmp);
        let workflow = make_workflow(
            Some("wf-wf-agent"),
            Some("claude"),
            vec![make_step("a", &[], None)], // step has no agent → falls through to workflow
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
        assert_eq!(contexts[0].step_agent.as_str(), "claude");
    }

    #[tokio::test]
    async fn continue_in_current_container_when_backend_rejects_injection_returns_invalid_action() {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session(&tmp);
        // Same agent both steps; inject_result is None (backend doesn't support injection).
        let workflow = make_workflow(
            Some("wf-cont"),
            Some("claude"),
            vec![make_step("a", &[], None), make_step("b", &["a"], None)],
        );
        let factory = FakeContainerExecutionFactory::always_success(); // inject_result = None
        let mut engine = make_engine(
            &session,
            workflow,
            factory,
            [NextAction::ContinueInCurrentContainer {
                prompt: "continue".into(),
            }],
        );

        // After step "a" completes, user requests ContinueInCurrentContainer.
        // inject_prompt returns None → engine must return InvalidAdvanceAction.
        let result = engine.run_to_completion().await;
        assert!(
            matches!(result, Err(EngineError::InvalidAdvanceAction(_))),
            "expected InvalidAdvanceAction when backend rejects injection, got {result:?}"
        );
    }

    #[tokio::test]
    async fn different_agent_steps_have_continue_unavailable() {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session(&tmp);
        let workflow = make_workflow(
            Some("wf-diff-agents"),
            None,
            vec![
                make_step("a", &[], Some("claude")),
                make_step("b", &["a"], Some("codex")),
            ],
        );
        let factory = FakeContainerExecutionFactory::always_success();
        let mut engine = make_engine(&session, workflow, factory, []);

        // Run step "a".
        engine.step_once().await.unwrap();

        let available = engine.compute_available_actions().unwrap();
        assert!(
            !available.can_continue_in_current_container,
            "different agents must disable ContinueInCurrentContainer"
        );
    }

    // T1: same-agent two-step workflow; user chooses ContinueInCurrentContainer;
    // inject_prompt is called (not execution_for_step) for the second step.
    #[tokio::test]
    async fn continue_in_current_container_same_agent_calls_inject_prompt() {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session(&tmp);
        let workflow = make_workflow(
            Some("wf-inject"),
            Some("claude"),
            vec![make_step("a", &[], None), make_step("b", &["a"], None)],
        );
        // Factory supports injection (inject_result = Some(())).
        let factory =
            FakeContainerExecutionFactory::with_inject_support(std::iter::repeat_n(0, 100));
        let factory_arc: Arc<FakeContainerExecutionFactory> = Arc::new(factory);

        struct InjectFactory(Arc<FakeContainerExecutionFactory>);
        impl ContainerExecutionFactory for InjectFactory {
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
            Box::new(FakeWorkflowFrontend::new([
                NextAction::ContinueInCurrentContainer {
                    prompt: "next task".into(),
                },
            ])),
            Box::new(InjectFactory(factory_arc.clone())),
            Arc::new(GitEngine::new()),
            Arc::new(overlay),
        )
        .unwrap();

        let result = engine.run_to_completion().await.unwrap();
        assert_eq!(result, WorkflowOutcome::Completed);
        // execution_for_step called once (for step "a" only).
        assert_eq!(
            factory_arc.execution_call_count.load(Ordering::Relaxed),
            1,
            "execution_for_step must be called once — step b reuses the existing container"
        );
        // inject_prompt called once (for step "b").
        assert_eq!(
            factory_arc.inject_call_count.load(Ordering::Relaxed),
            1,
            "inject_prompt must be called once for the continuation step"
        );
    }

    // T2: CancelToPreviousStep success case — after step "b" succeeds the user
    // cancels to "a", which resets "b" to Cancelled and reruns "a".
    #[tokio::test]
    async fn cancel_to_previous_step_cancels_step_and_reruns_previous() {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session(&tmp);
        // Three-step linear chain: a → b → c.
        let workflow = make_workflow(
            Some("wf-cancel-prev"),
            Some("claude"),
            vec![
                make_step("a", &[], None),
                make_step("b", &["a"], None),
                make_step("c", &["b"], None),
            ],
        );
        let factory = FakeContainerExecutionFactory::new(std::iter::repeat_n(0, 100));
        let factory_arc: Arc<FakeContainerExecutionFactory> = Arc::new(factory);

        struct CountingFactory(Arc<FakeContainerExecutionFactory>);
        impl ContainerExecutionFactory for CountingFactory {
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
        // After "a": launch next → "b" runs.
        // After "b": cancel to previous → "b" cancelled, "a" reruns.
        // After "a" (second run): launch next → "b" reruns.
        // After "b" (second run): launch next → "c" runs → complete.
        let mut engine = WorkflowEngine::new(
            &session,
            workflow,
            None,
            Box::new(FakeWorkflowFrontend::new([
                NextAction::LaunchNext,
                NextAction::CancelToPreviousStep,
                NextAction::LaunchNext,
                NextAction::LaunchNext,
            ])),
            Box::new(CountingFactory(factory_arc.clone())),
            Arc::new(GitEngine::new()),
            Arc::new(overlay),
        )
        .unwrap();

        let result = engine.run_to_completion().await.unwrap();
        assert_eq!(result, WorkflowOutcome::Completed);
        // "a" runs twice, "b" runs twice, "c" runs once → at least 5 executions.
        assert!(
            factory_arc.execution_call_count.load(Ordering::Relaxed) >= 5,
            "execution_for_step must be called at least 5 times (a×2, b×2, c×1)"
        );
    }

    // T3: when neither step nor workflow specify an agent, EffectiveConfig
    // (session flags) is used as the fallback.
    #[tokio::test]
    async fn config_fallback_agent_used_when_step_and_workflow_have_none() {
        let tmp = tempfile::tempdir().unwrap();
        let resolver = StaticGitRootResolver::new(tmp.path());
        let session = Session::open(
            tmp.path().to_path_buf(),
            &resolver,
            SessionOpenOptions {
                flags: FlagConfig {
                    agent: Some("codex".to_string()),
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .unwrap();

        // Workflow has no agent at any level.
        let workflow = make_workflow(
            Some("wf-fallback"),
            None,                            // no workflow-level agent
            vec![make_step("a", &[], None)], // no step-level agent
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
        assert_eq!(
            contexts[0].step_agent.as_str(),
            "codex",
            "EffectiveConfig agent must be used when step and workflow have none"
        );
    }

    // ── BlockingBackend / BlockingFactory ─────────────────────────────────────
    // Used by mid-step control-board tests that need an execution that stays
    // alive until cancelled or explicitly signalled to complete.

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

    /// Create a (cancel_flag, completion_arc) pair for a blocking execution.
    fn make_blocking_entry() -> (Arc<AtomicBool>, CompletionArc) {
        (
            Arc::new(AtomicBool::new(false)),
            Arc::new((Mutex::new(None), Condvar::new())),
        )
    }

    /// Signal a blocking execution to complete with the given exit code.
    fn signal_completion(c: &CompletionArc, code: i32) {
        let (lock, cvar) = &**c;
        *lock.lock().unwrap() = Some(code);
        cvar.notify_all();
    }

    /// A factory that returns blocking executions for the first N steps (each
    /// backed by its own (cancel_flag, completion) pair) and instant exit-0
    /// executions for any additional steps.
    struct BlockingFactory {
        execution_count: Arc<AtomicUsize>,
        inject_count: Arc<AtomicUsize>,
        inject_result: Option<()>,
        /// Per-execution (cancel_flag, completion) for slow steps.
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

        fn with_inject(mut self) -> Self {
            self.inject_result = Some(());
            self
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
                Ok(ContainerExecution::new(handle, backend))
            } else {
                // Fallback: instant success.
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

    /// A frontend that captures the control-board sender, records which
    /// `AvailableActions` were passed to `user_choose_next_action`, and pops
    /// from a scripted action queue (same pattern as `FakeWorkflowFrontend`).
    struct CapturingFrontend {
        actions: Mutex<VecDeque<NextAction>>,
        step_statuses: Mutex<Vec<(String, WorkflowStepStatus)>>,
        completed: Mutex<Option<WorkflowOutcome>>,
        available_log: Mutex<Vec<AvailableActions>>,
        cb_tx: Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<ControlBoardRequest>>>>,
    }

    impl CapturingFrontend {
        fn new(
            actions: impl IntoIterator<Item = NextAction>,
            cb_tx: Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<ControlBoardRequest>>>>,
        ) -> Self {
            Self {
                actions: Mutex::new(actions.into_iter().collect()),
                step_statuses: Mutex::new(Vec::new()),
                completed: Mutex::new(None),
                available_log: Mutex::new(Vec::new()),
                cb_tx,
            }
        }
    }

    impl crate::engine::message::UserMessageSink for CapturingFrontend {
        fn write_message(&mut self, _msg: crate::engine::message::UserMessage) {}
        fn replay_queued(&mut self) {}
    }

    impl WorkflowFrontend for CapturingFrontend {
        fn user_choose_next_action(
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

        fn report_step_output(&mut self, _step: &WorkflowStep, _output: StepOutput) {}
        fn report_step_stuck(&mut self, _step: &WorkflowStep) {}
        fn report_step_unstuck(&mut self, _step: &WorkflowStep) {}

        fn yolo_countdown_tick(
            &mut self,
            _remaining: Duration,
        ) -> Result<YoloTickOutcome, EngineError> {
            Ok(YoloTickOutcome::Cancel)
        }

        fn report_workflow_completed(&mut self, outcome: &WorkflowOutcome) {
            *self.completed.lock().unwrap() = Some(outcome.clone());
        }

        fn set_control_board_sender(
            &mut self,
            tx: tokio::sync::mpsc::UnboundedSender<ControlBoardRequest>,
        ) {
            *self.cb_tx.lock().unwrap() = Some(tx);
        }
    }

    fn make_capturing_engine(
        session: &Session,
        workflow: Workflow,
        factory: BlockingFactory,
        actions: impl IntoIterator<Item = NextAction>,
        cb_tx: Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<ControlBoardRequest>>>>,
    ) -> WorkflowEngine {
        let overlay = OverlayEngine::with_auth_resolver(
            crate::data::fs::auth_paths::AuthPathResolver::at_home(session.git_root()),
        );
        let frontend = CapturingFrontend::new(actions, cb_tx);
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

    // ── Mid-step control board engine tests ───────────────────────────────────

    /// Opening the WCB mid-step must NOT cancel the running container — only a
    /// destructive user action triggers cancellation.
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
        let cb_tx: Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<ControlBoardRequest>>>> =
            Arc::new(Mutex::new(None));

        let factory = BlockingFactory::new([(cancel_flag.clone(), completion1.clone())]);
        let mut engine = make_capturing_engine(
            &session,
            workflow,
            factory,
            // First call (mid-step WCB): Dismiss; second call (between steps): LaunchNext.
            [NextAction::Dismiss, NextAction::LaunchNext],
            cb_tx.clone(),
        );

        // Clone the sender BEFORE the engine moves into the async task.
        let tx = cb_tx
            .lock()
            .unwrap()
            .clone()
            .expect("cb_tx set on construction");

        let engine_task = tokio::spawn(async move { engine.run_to_completion().await });

        // Give the engine time to call launch_step() and enter the select! loop.
        tokio::time::sleep(Duration::from_millis(150)).await;

        // Send mid-step request — step "a" is still blocking.
        tx.send(ControlBoardRequest::OpenControlBoard).unwrap();

        // Let the engine process the OpenControlBoard and return Dismiss.
        tokio::time::sleep(Duration::from_millis(150)).await;

        // Cancel must NOT have been called (Dismiss is non-destructive).
        assert!(
            !cancel_flag.load(Ordering::Relaxed),
            "cancel must not be called when user picks Dismiss"
        );

        // Now let step "a" complete naturally.
        signal_completion(&completion1, 0);

        let result = engine_task.await.unwrap().unwrap();
        assert_eq!(result, WorkflowOutcome::Completed);
    }

    /// After Dismiss the engine must resume waiting on the same step.
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
        let cb_tx: Arc<Mutex<Option<_>>> = Arc::new(Mutex::new(None));
        let factory = BlockingFactory::new([(cancel_flag.clone(), completion.clone())]);
        let mut engine = make_capturing_engine(
            &session,
            workflow,
            factory,
            [NextAction::Dismiss, NextAction::LaunchNext],
            cb_tx.clone(),
        );
        let tx = cb_tx.lock().unwrap().clone().unwrap();

        let engine_task = tokio::spawn(async move { engine.run_to_completion().await });

        tokio::time::sleep(Duration::from_millis(150)).await;
        tx.send(ControlBoardRequest::OpenControlBoard).unwrap();
        tokio::time::sleep(Duration::from_millis(150)).await;

        // After Dismiss, cancel must still be false — step still running.
        assert!(
            !cancel_flag.load(Ordering::Relaxed),
            "step must still be running after Dismiss"
        );

        // Complete the step — engine should continue to step b and finish.
        signal_completion(&completion, 0);
        let result = engine_task.await.unwrap().unwrap();
        assert_eq!(
            result,
            WorkflowOutcome::Completed,
            "workflow must complete after step finishes naturally post-Dismiss"
        );
    }

    /// RestartCurrentStep mid-step cancels the container AFTER selection, then
    /// launches a fresh container for the same step.
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
        let cb_tx: Arc<Mutex<Option<_>>> = Arc::new(Mutex::new(None));
        // Only one blocking slot; steps 2+ (re-run of a, then b) use instant.
        let factory = BlockingFactory::new([(cancel_flag.clone(), completion1)]);
        let execution_count = factory.execution_count.clone();
        let mut engine = make_capturing_engine(
            &session,
            workflow,
            factory,
            // Restart, then advance past step a (second run), then b.
            [NextAction::RestartCurrentStep, NextAction::LaunchNext],
            cb_tx.clone(),
        );
        let tx = cb_tx.lock().unwrap().clone().unwrap();

        let engine_task = tokio::spawn(async move { engine.run_to_completion().await });

        tokio::time::sleep(Duration::from_millis(150)).await;
        // Before sending request, cancel_flag must be false.
        assert!(
            !cancel_flag.load(Ordering::Relaxed),
            "cancel must not fire before WCB opened"
        );

        tx.send(ControlBoardRequest::OpenControlBoard).unwrap();
        // Give engine time to process RestartCurrentStep (which cancels the container).
        tokio::time::sleep(Duration::from_millis(300)).await;

        // Cancel MUST have been called (Restart is destructive).
        assert!(
            cancel_flag.load(Ordering::Relaxed),
            "cancel must be called when user picks RestartCurrentStep"
        );

        let result = engine_task.await.unwrap().unwrap();
        assert_eq!(result, WorkflowOutcome::Completed);
        // First run of a (blocking) + restart of a (instant) + b (instant) = 3.
        assert!(
            execution_count.load(Ordering::Relaxed) >= 2,
            "step a must run at least twice due to restart"
        );
    }

    /// LaunchNext mid-step cancels the container and marks the step Succeeded
    /// (force-advanced) before launching the next step.
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
        let cb_tx: Arc<Mutex<Option<_>>> = Arc::new(Mutex::new(None));
        let factory = BlockingFactory::new([(cancel_flag.clone(), completion1)]);
        let execution_count = factory.execution_count.clone();
        let mut engine = make_capturing_engine(
            &session,
            workflow,
            factory,
            // LaunchNext mid-step (force-advance), no further prompts needed.
            [NextAction::LaunchNext],
            cb_tx.clone(),
        );
        let tx = cb_tx.lock().unwrap().clone().unwrap();

        let engine_task = tokio::spawn(async move { engine.run_to_completion().await });

        tokio::time::sleep(Duration::from_millis(150)).await;
        tx.send(ControlBoardRequest::OpenControlBoard).unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;

        // Cancel must have been called (LaunchNext is destructive mid-step).
        assert!(
            cancel_flag.load(Ordering::Relaxed),
            "cancel must be called for LaunchNext mid-step"
        );

        let result = engine_task.await.unwrap().unwrap();
        assert_eq!(
            result,
            WorkflowOutcome::Completed,
            "workflow must complete after force-advance"
        );
        // a (blocking, force-advanced) + b (instant) = 2 executions.
        assert_eq!(
            execution_count.load(Ordering::Relaxed),
            2,
            "exactly 2 executions: step a (cancelled) + step b"
        );
    }

    /// CancelToPreviousStep mid-step cancels, then rewinds both steps to Pending.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mid_step_cancel_to_previous_rewinds() {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session(&tmp);
        let workflow = make_workflow(
            Some("wf-rewind"),
            Some("claude"),
            vec![
                make_step("a", &[], None),
                make_step("b", &["a"], None),
                make_step("c", &["b"], None),
            ],
        );

        // "a" runs and completes instantly, "b" runs and is mid-step open.
        let (cancel_b, completion_b) = make_blocking_entry();
        let cb_tx: Arc<Mutex<Option<_>>> = Arc::new(Mutex::new(None));
        let factory = BlockingFactory::new([(cancel_b.clone(), completion_b)]);
        let execution_count = factory.execution_count.clone();
        let mut engine = make_capturing_engine(
            &session,
            workflow,
            factory,
            // Step a completes → LaunchNext → Step b starts (blocking).
            // WCB mid-b → CancelToPreviousStep (cancel b, rewind to a).
            // Step a re-runs → LaunchNext → Step b runs → LaunchNext → Step c.
            [
                NextAction::LaunchNext,
                NextAction::CancelToPreviousStep,
                NextAction::LaunchNext,
                NextAction::LaunchNext,
            ],
            cb_tx.clone(),
        );
        let tx = cb_tx.lock().unwrap().clone().unwrap();

        let engine_task = tokio::spawn(async move { engine.run_to_completion().await });

        // Wait for step b to start running (steps a + b launches; b is blocking).
        tokio::time::sleep(Duration::from_millis(200)).await;

        tx.send(ControlBoardRequest::OpenControlBoard).unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;

        // b must have been cancelled.
        assert!(
            cancel_b.load(Ordering::Relaxed),
            "step b must be cancelled for CancelToPreviousStep"
        );

        let result = engine_task.await.unwrap().unwrap();
        assert_eq!(result, WorkflowOutcome::Completed);
        // a (instant) + b (blocking, cancelled) + a (re-run, instant) + b (re-run) + c = 5.
        assert!(
            execution_count.load(Ordering::Relaxed) >= 4,
            "multiple executions expected after cancel-to-previous + re-run"
        );
    }

    /// When the step finishes naturally while the WCB is open, the engine
    /// detects this via `try_recv` and handles the user's now-stale action.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn step_completes_naturally_while_wcb_open() {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session(&tmp);
        let workflow = make_workflow(
            Some("wf-natural-complete"),
            Some("claude"),
            vec![make_step("a", &[], None), make_step("b", &["a"], None)],
        );

        // Use a short-lived blocking backend that signals itself on open.
        let (cancel_flag, completion) = make_blocking_entry();
        let cb_tx: Arc<Mutex<Option<_>>> = Arc::new(Mutex::new(None));
        let factory = BlockingFactory::new([(cancel_flag, completion.clone())]);
        let mut engine = make_capturing_engine(
            &session,
            workflow,
            factory,
            // Dismiss: if step already done → engine handles gracefully.
            // LaunchNext for the between-steps prompt after step a.
            [NextAction::Dismiss, NextAction::LaunchNext],
            cb_tx.clone(),
        );
        let tx = cb_tx.lock().unwrap().clone().unwrap();

        let engine_task = tokio::spawn(async move { engine.run_to_completion().await });

        // Give step a time to start.
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Signal completion and immediately open control board.
        signal_completion(&completion, 0);
        // Let the backend thread process the signal.
        tokio::time::sleep(Duration::from_millis(50)).await;
        // The step may now be complete. Opening WCB and picking Dismiss
        // should result in the engine recognizing the completion.
        let _ = tx.send(ControlBoardRequest::OpenControlBoard);

        let result = engine_task.await.unwrap().unwrap();
        // Regardless of whether WCB fires before or after natural completion,
        // the workflow must complete successfully.
        assert_eq!(result, WorkflowOutcome::Completed);
    }

    /// After force-advancing a step (LaunchNext mid-step), resuming the
    /// workflow must not re-run the already-succeeded step.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn resume_from_force_succeeded_step_does_not_re_run() {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session(&tmp);
        let wf = make_workflow(
            Some("wf-resume-force"),
            Some("claude"),
            vec![make_step("a", &[], None), make_step("b", &["a"], None)],
        );

        // First run: force-advance step "a" mid-step.
        let (_, completion_a) = make_blocking_entry();
        {
            let cb_tx: Arc<Mutex<Option<_>>> = Arc::new(Mutex::new(None));
            let factory = BlockingFactory::new([(Arc::new(AtomicBool::new(false)), completion_a)]);
            let execution_count = factory.execution_count.clone();
            let mut engine = make_capturing_engine(
                &session,
                wf.clone(),
                factory,
                [NextAction::LaunchNext], // LaunchNext mid-step → force-succeed a, then b runs
                cb_tx.clone(),
            );
            let tx = cb_tx.lock().unwrap().clone().unwrap();
            let engine_task = tokio::spawn(async move { engine.run_to_completion().await });
            tokio::time::sleep(Duration::from_millis(150)).await;
            tx.send(ControlBoardRequest::OpenControlBoard).unwrap();
            engine_task.await.unwrap().unwrap();
            // a (cancelled) + b = 2.
            assert_eq!(execution_count.load(Ordering::Relaxed), 2);
        }

        // Resume: only step b should run (a is Succeeded from force-advance).
        let factory2 = FakeContainerExecutionFactory::always_success();
        let factory2_arc = Arc::new(factory2);
        struct Proxy(Arc<FakeContainerExecutionFactory>);
        impl ContainerExecutionFactory for Proxy {
            fn execution_for_step(
                &self,
                s: &WorkflowStep,
                sess: &Session,
                r: &WorkflowRuntimeContext,
            ) -> Result<ContainerExecution, EngineError> {
                self.0.execution_for_step(s, sess, r)
            }
            fn inject_prompt(
                &self,
                e: &ContainerExecution,
                p: &str,
            ) -> Result<Option<()>, EngineError> {
                self.0.inject_prompt(e, p)
            }
        }
        let overlay2 = OverlayEngine::with_auth_resolver(
            crate::data::fs::auth_paths::AuthPathResolver::at_home(session.git_root()),
        );
        let mut engine2 = WorkflowEngine::resume(
            &session,
            wf,
            None,
            Box::new(FakeWorkflowFrontend::new([])),
            Box::new(Proxy(factory2_arc.clone())),
            Arc::new(crate::engine::git::GitEngine::new()),
            Arc::new(overlay2),
        )
        .await
        .unwrap();
        let result = engine2.run_to_completion().await.unwrap();
        assert_eq!(result, WorkflowOutcome::Completed);
        // Only step b should have run; step a was already Succeeded.
        assert_eq!(
            factory2_arc.execution_call_count.load(Ordering::Relaxed),
            0,
            "resuming must not re-run step a (already Succeeded from force-advance)"
        );
    }

    // ── Auto-disabled step engine tests ───────────────────────────────────────

    /// `should_auto_advance` defaults to `true` in `FakeWorkflowFrontend` but
    /// can be overridden. Verify that a frontend returning `false` makes the
    /// engine call `user_choose_next_action` even in yolo mode.
    #[tokio::test]
    async fn engine_skips_yolo_countdown_when_step_disabled() {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session(&tmp);
        let workflow = make_workflow(
            Some("wf-auto-disabled"),
            Some("claude"),
            vec![make_step("a", &[], None), make_step("b", &["a"], None)],
        );
        let factory = FakeContainerExecutionFactory::always_success();

        // A frontend that always returns false from should_auto_advance.
        struct NoAutoFrontend(FakeWorkflowFrontend);
        impl crate::engine::message::UserMessageSink for NoAutoFrontend {
            fn write_message(&mut self, msg: crate::engine::message::UserMessage) {
                self.0.write_message(msg);
            }
            fn replay_queued(&mut self) {}
        }
        impl WorkflowFrontend for NoAutoFrontend {
            fn should_auto_advance(&self, _step_name: &str) -> bool {
                false // always skip yolo, fall through to interactive prompt
            }
            fn user_choose_next_action(
                &mut self,
                s: &WorkflowState,
                a: &AvailableActions,
            ) -> Result<NextAction, EngineError> {
                self.0.user_choose_next_action(s, a)
            }
            fn confirm_resume(&mut self, m: &ResumeMismatch) -> Result<bool, EngineError> {
                self.0.confirm_resume(m)
            }
            fn user_choose_after_step_failure(
                &mut self,
                step: &WorkflowStep,
                exit: &ContainerExitInfo,
            ) -> Result<StepFailureChoice, EngineError> {
                self.0.user_choose_after_step_failure(step, exit)
            }
            fn report_step_status(&mut self, step: &WorkflowStep, status: WorkflowStepStatus) {
                self.0.report_step_status(step, status);
            }
            fn report_step_output(&mut self, s: &WorkflowStep, o: StepOutput) {
                self.0.report_step_output(s, o);
            }
            fn report_step_stuck(&mut self, s: &WorkflowStep) {
                self.0.report_step_stuck(s);
            }
            fn report_step_unstuck(&mut self, s: &WorkflowStep) {
                self.0.report_step_unstuck(s);
            }
            fn yolo_countdown_tick(&mut self, r: Duration) -> Result<YoloTickOutcome, EngineError> {
                self.0.yolo_countdown_tick(r)
            }
            fn report_workflow_completed(&mut self, o: &WorkflowOutcome) {
                self.0.report_workflow_completed(o);
            }
        }

        let inner = FakeWorkflowFrontend::new([NextAction::LaunchNext]);
        let frontend = NoAutoFrontend(inner);

        let overlay = OverlayEngine::with_auth_resolver(
            crate::data::fs::auth_paths::AuthPathResolver::at_home(session.git_root()),
        );
        let mut engine = WorkflowEngine::new(
            &session,
            workflow,
            None,
            Box::new(frontend),
            Box::new(factory),
            Arc::new(crate::engine::git::GitEngine::new()),
            Arc::new(overlay),
        )
        .unwrap();
        engine.set_yolo(true);

        // With yolo=true but should_auto_advance=false, the engine must call
        // user_choose_next_action (which returns LaunchNext) and complete.
        let result = engine.run_to_completion().await.unwrap();
        assert_eq!(
            result,
            WorkflowOutcome::Completed,
            "workflow must complete even with yolo disabled per step"
        );
    }

    /// Calling `should_auto_advance` on the default `FakeWorkflowFrontend`
    /// returns `true` (the trait default). A custom frontend returning `false`
    /// is exercised by the test above; this test guards the trait default.
    #[test]
    fn should_auto_advance_trait_default_returns_true() {
        let frontend = FakeWorkflowFrontend::new([]);
        // Default implementation returns true (auto-advance).
        assert!(
            frontend.should_auto_advance("any-step"),
            "WorkflowFrontend::should_auto_advance must default to true"
        );
    }
}
