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
    AvailableActions, NextAction, ResumeMismatch, StepFailureChoice, StepOutcome,
    WorkflowOutcome, WorkflowStepProgressInfo, WorkflowStepStatus, YoloTickOutcome,
};
use crate::engine::workflow::factory::{ContainerExecutionFactory, WorkflowRuntimeContext};
use crate::engine::workflow::frontend::WorkflowFrontend;

pub mod actions;
pub mod factory;
pub mod frontend;
pub mod timing;

pub use actions::{
    StepOutput, StepOutputKind, WorkflowOutcome as Outcome, WorkflowStepStatus as Status,
};
pub use factory::{ContainerExecutionFactory as Factory, WorkflowRuntimeContext as RuntimeContext};
pub use frontend::WorkflowFrontend as Frontend;

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
    /// When true, skip the inter-step user prompt and auto-advance after a
    /// 60-second countdown (giving the user a chance to intervene).
    yolo: bool,
    /// Exit info from the most recent step execution, used by the step-failure
    /// dialog so it can display timing and signal information.
    last_exit_info: Option<ContainerExitInfo>,
}

impl WorkflowEngine {
    pub fn new(
        session: &Session,
        workflow: Workflow,
        frontend: Box<dyn WorkflowFrontend>,
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
        );
        let state_store = WorkflowStateStore::new(session);
        let effective_config = session.effective_config();
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
            yolo: false,
            last_exit_info: None,
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
        mut frontend: Box<dyn WorkflowFrontend>,
        container_factory: Box<dyn ContainerExecutionFactory>,
        git_engine: Arc<GitEngine>,
        overlay_engine: Arc<OverlayEngine>,
    ) -> Result<Self, EngineError> {
        let dag = WorkflowDag::build(&workflow.steps).map_err(EngineError::Data)?;
        let store = WorkflowStateStore::new(session);
        let workflow_name = workflow_name_for(&workflow);
        let saved = store.load(&workflow_name)?;

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
            None => WorkflowState::new(workflow_name, &workflow.steps, workflow_hash),
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
            yolo: false,
            last_exit_info: None,
        })
    }

    pub fn state(&self) -> &WorkflowState {
        &self.state
    }

    /// Drive every step until the workflow finishes, the user pauses, or a
    /// step fails terminally.
    pub async fn run_to_completion(&mut self) -> Result<WorkflowOutcome, EngineError> {
        loop {
            if self.state.is_complete() {
                let progress = self.workflow_progress_info();
                self.frontend.report_workflow_progress(&progress);
                let outcome = WorkflowOutcome::Completed;
                self.frontend.report_workflow_completed(&outcome);
                return Ok(outcome);
            }
            let outcome = self.step_once().await?;
            if let WorkflowStepStatus::Failed { exit_code } = outcome.status {
                let progress = self.workflow_progress_info();
                self.frontend.report_workflow_progress(&progress);

                let step = self.find_step(&outcome.step_name)?;
                let exit_info = self.last_exit_info.clone().unwrap_or_else(|| {
                    ContainerExitInfo {
                        exit_code,
                        signal: None,
                        started_at: chrono::Utc::now(),
                        ended_at: chrono::Utc::now(),
                    }
                });
                let choice = self.frontend.user_choose_after_step_failure(
                    &step, &exit_info,
                )?;
                match choice {
                    StepFailureChoice::Retry => {
                        self.state.set_status(
                            &outcome.step_name,
                            StepState::Pending,
                        );
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
                                self.state.set_status(
                                    &s.name,
                                    StepState::Cancelled,
                                );
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
                if self.yolo {
                    let advance = self.run_yolo_countdown().await?;
                    if advance {
                        continue;
                    } else {
                        self.persist()?;
                        let outcome = WorkflowOutcome::Paused;
                        self.frontend.report_workflow_completed(&outcome);
                        return Ok(outcome);
                    }
                }

                let available = self.compute_available_actions()?;
                let action = self
                    .frontend
                    .user_choose_next_action(&self.state, &available)?;
                match action {
                    NextAction::LaunchNext => continue,
                    NextAction::ContinueInCurrentContainer { prompt } => {
                        // Pre-validate before calling inject_prompt: the next
                        // step must use the same agent + model, and an execution
                        // must be present.
                        let next_step = match self.next_ready_step()? {
                            Some(s) => s,
                            None => return Err(EngineError::InvalidAdvanceAction(
                                "ContinueInCurrentContainer: no next step is ready".into(),
                            )),
                        };
                        let next_agent = self.resolve_agent(&next_step)?;
                        let next_model = self.resolve_model(&next_step);
                        let agent_ok = self.current_step_agent.as_ref()
                            .map(|a| *a == next_agent)
                            .unwrap_or(false);
                        let model_ok = self.current_step_model == next_model;
                        if !agent_ok || !model_ok {
                            return Err(EngineError::InvalidAdvanceAction(
                                "ContinueInCurrentContainer requires the same agent and model \
                                 for the current and next steps".into(),
                            ));
                        }
                        match &self.current_execution {
                            Some(exec) => {
                                match self.container_factory.inject_prompt(exec, &prompt)? {
                                    Some(()) => {
                                        // Injection succeeded: the next step ran inside the
                                        // current container. Mark it Succeeded directly.
                                        self.state.set_status(
                                            &next_step.name,
                                            StepState::Succeeded,
                                        );
                                        self.current_step_name = Some(next_step.name.clone());
                                        self.persist()?;
                                        continue;
                                    }
                                    None => {
                                        return Err(EngineError::InvalidAdvanceAction(
                                            "container backend does not support prompt \
                                             injection; use LaunchNext to start a fresh \
                                             container for the next step".into(),
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
        let ready = self.state.next_ready(&self.dag);
        let step_name = ready.first().cloned().ok_or_else(|| {
            EngineError::InvalidAdvanceAction("no ready steps remaining".into())
        })?;
        let step = self.find_step(&step_name)?;

        // Resolve agent + model.
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

        // Emit the interactive-launch notice so the CLI can print the banner.
        self.frontend.report_step_interactive_launch(
            &step,
            resolved_agent.as_str(),
            resolved_model.as_deref(),
        );

        // Mark running and launch.
        self.state.set_status(
            &step.name,
            StepState::Running {
                container_id: None,
            },
        );
        self.frontend
            .report_step_status(&step, WorkflowStepStatus::Running);
        self.persist()?;

        let execution = self
            .container_factory
            .execution_for_step(&step, &self.session, &runtime)?;

        // Persist the container ID now that we know it.
        self.state.set_status(
            &step.name,
            StepState::Running {
                container_id: Some(execution.handle().id.clone()),
            },
        );
        self.persist()?;

        // Store before waiting so the execution is available for
        // ContinueInCurrentContainer prompt injection after this step completes.
        self.current_execution = Some(execution);
        let exit = {
            let exec = self.current_execution.as_mut().expect("just stored");
            exec.wait().await?
        };

        // Store exit info for the step-failure dialog.
        self.last_exit_info = Some(exit.clone());

        // Persist new step state based on exit code.
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
        self.state.set_status(&step.name, step_state);
        self.frontend.report_step_status(&step, status.clone());
        self.current_step_name = Some(step.name.clone());
        self.current_step_agent = Some(resolved_agent);
        self.current_step_model = resolved_model;
        self.persist()?;

        let remaining = self
            .workflow
            .steps
            .iter()
            .filter(|s| !self.state.completed_steps.contains(&s.name))
            .count();
        Ok(StepOutcome {
            step_name: step.name,
            status,
            remaining,
        })
    }

    /// Run the 60-second yolo countdown, ticking through the frontend every
    /// second. Returns `true` to advance to the next step, `false` to pause.
    async fn run_yolo_countdown(&mut self) -> Result<bool, EngineError> {
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
                YoloTickOutcome::AdvanceNow => return Ok(true),
                YoloTickOutcome::Cancel => return Ok(false),
                YoloTickOutcome::Continue => {}
            }
            if remaining.is_zero() {
                return Ok(true);
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
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
            ..Default::default()
        };
        // Continue-in-current-container: requires same agent + same model
        // for the next step and a running execution.
        if let Some(next) = self.next_ready_step()? {
            let next_agent = self.resolve_agent(&next)?;
            let next_model = self.resolve_model(&next);
            let ok = match (&self.current_step_agent, &self.current_step_model) {
                (Some(curr_a), curr_m) => {
                    *curr_a == next_agent && *curr_m == next_model
                }
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
            a.cancel_to_previous_unavailable_reason =
                Some("this is the first step".into());
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
            .ok_or_else(|| {
                EngineError::Other(format!("step '{name}' not found in workflow"))
            })
    }

    /// Build a per-step progress snapshot for `report_workflow_progress`.
    fn workflow_progress_info(&self) -> Vec<WorkflowStepProgressInfo> {
        use crate::data::workflow_state::StepState;
        self.workflow.steps.iter().map(|step| {
            let agent = self.resolve_agent(step)
                .map(|a| a.as_str().to_string())
                .unwrap_or_else(|_| "?".to_string());
            let model = self.resolve_model(step);
            let status = match self.state.status_of(&step.name) {
                None | Some(StepState::Pending) => WorkflowStepStatus::Pending,
                Some(StepState::Running { .. }) => WorkflowStepStatus::Running,
                Some(StepState::Succeeded) => WorkflowStepStatus::Succeeded,
                Some(StepState::Failed { exit_code, .. }) => {
                    WorkflowStepStatus::Failed { exit_code: *exit_code }
                }
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
        }).collect()
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
        self.state_store.save(&self.state).map_err(EngineError::Data)?;
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
fn workflow_name_for(workflow: &Workflow) -> String {
    workflow
        .title
        .as_deref()
        .unwrap_or("workflow")
        .to_string()
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};
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

        fn confirm_resume(
            &mut self,
            _mismatch: &ResumeMismatch,
        ) -> Result<bool, EngineError> {
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

        fn report_step_output(
            &mut self,
            _step: &WorkflowStep,
            _output: StepOutput,
        ) {
        }

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
            let code = self
                .exit_codes
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(0);
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
        make_engine_with_frontend(session, workflow, factory, FakeWorkflowFrontend::new(actions))
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
        let saved = store.load("my-wf").unwrap();
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
        assert!(available
            .cancel_to_previous_unavailable_reason
            .is_some());
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
        let saved = store.load("wf-pause").unwrap();
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
        let factory = FakeContainerExecutionFactory::with_inject_support(
            std::iter::repeat_n(0, 100),
        );
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
            Box::new(FakeWorkflowFrontend::new([
                NextAction::ContinueInCurrentContainer { prompt: "next task".into() },
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
            None,  // no workflow-level agent
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
}
