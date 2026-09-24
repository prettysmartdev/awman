//! Read-only questions about the run, plus the two writes that only record
//! what already happened (`persist`, `mirror_summary`).
//!
//! Split out of `workflow/mod.rs` by WI 0114 F-51. A child module of
//! `workflow`, so it reaches `WorkflowEngine`'s private fields exactly as the
//! code did before the move; methods it defines are `pub(super)` so the
//! other halves of the engine can still call them.

use super::*;

impl WorkflowEngine {
    pub fn next_ready_steps(&self) -> Result<Vec<WorkflowStep>, EngineError> {
        self.state
            .next_ready(&self.dag)
            .into_iter()
            .map(|name| self.find_step(&name))
            .collect()
    }

    pub(super) fn next_ready_step(&self) -> Result<Option<WorkflowStep>, EngineError> {
        match self.state.next_ready(&self.dag).into_iter().next() {
            Some(name) => Ok(Some(self.find_step(&name)?)),
            None => Ok(None),
        }
    }

    pub(super) fn previous_step_name(&self) -> Option<String> {
        let curr = self.current_step_name.as_ref()?;
        let order = self.dag.topological_order();
        let idx = order.iter().position(|n| n == curr)?;
        if idx == 0 {
            None
        } else {
            Some(order[idx - 1].clone())
        }
    }

    /// The direct dependencies of `steps` that are not themselves in `steps`,
    /// in topological order: the step or group that ran before them.
    pub(super) fn dependencies_of(&self, steps: &[String]) -> Vec<String> {
        let deps: HashSet<&String> = self
            .workflow
            .steps
            .iter()
            .filter(|s| steps.contains(&s.name))
            .flat_map(|s| s.depends_on.iter())
            .filter(|d| !steps.contains(d))
            .collect();
        self.dag
            .topological_order()
            .into_iter()
            .filter(|n| deps.contains(n))
            .collect()
    }

    /// The steps outside `steps` that depend directly on one of them, in
    /// topological order: the step or group that runs after them.
    pub(super) fn direct_dependents_of(&self, steps: &[String]) -> Vec<String> {
        let after: HashSet<&String> = self
            .workflow
            .steps
            .iter()
            .filter(|s| !steps.contains(&s.name))
            .filter(|s| s.depends_on.iter().any(|d| steps.contains(d)))
            .map(|s| &s.name)
            .collect();
        self.dag
            .topological_order()
            .into_iter()
            .filter(|n| after.contains(n))
            .collect()
    }

    /// Every step downstream of `roots` (transitively), excluding the roots.
    pub(super) fn dependents_of(&self, roots: &[String]) -> Vec<String> {
        let mut reached: HashSet<String> = roots.iter().cloned().collect();
        let mut out = Vec::new();
        loop {
            let next: Vec<String> = self
                .workflow
                .steps
                .iter()
                .filter(|s| !reached.contains(&s.name))
                .filter(|s| s.depends_on.iter().any(|d| reached.contains(d)))
                .map(|s| s.name.clone())
                .collect();
            if next.is_empty() {
                return out;
            }
            reached.extend(next.iter().cloned());
            out.extend(next);
        }
    }

    pub(super) fn is_last_step(&self) -> bool {
        let curr = match self.current_step_name.as_ref() {
            Some(c) => c,
            None => return false,
        };
        let order = self.dag.topological_order();
        order.last().map(|s| s == curr).unwrap_or(false)
    }

    pub(super) fn find_step(&self, name: &str) -> Result<WorkflowStep, EngineError> {
        self.workflow
            .steps
            .iter()
            .find(|s| s.name == name)
            .cloned()
            .ok_or_else(|| EngineError::Other(format!("step '{name}' not found in workflow")))
    }

    pub(super) fn workflow_progress_info(&self) -> Vec<WorkflowStepProgressInfo> {
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
                    has_step_override: step.agent.is_some() || step.model.is_some(),
                    status,
                    depends_on: step.depends_on.clone(),
                    max_concurrent: self.max_concurrent,
                }
            })
            .collect()
    }

    pub(super) fn resolve_agent(&self, step: &WorkflowStep) -> Result<AgentName, EngineError> {
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

    pub(super) fn resolve_model(&self, step: &WorkflowStep) -> Option<String> {
        if let Some(m) = step.model.as_deref() {
            return Some(m.to_string());
        }
        if let Some(m) = self.workflow.model.as_ref() {
            return Some(m.clone());
        }
        self.effective_config.model()
    }

    pub(super) fn build_workflow_step_info(
        &self,
        current_step_name: &str,
    ) -> Option<crate::engine::context_prompt::WorkflowStepInfo> {
        use crate::engine::context_prompt::{WorkflowStepInfo as CtxStepInfo, WorkflowStepState};

        let title = self
            .workflow
            .title
            .clone()
            .unwrap_or_else(|| "Untitled Workflow".to_string());
        let total = self.workflow.steps.len();
        let mut current_index = 0;
        let mut steps = Vec::with_capacity(total);

        for (i, step) in self.workflow.steps.iter().enumerate() {
            let state = if step.name == current_step_name {
                current_index = i;
                WorkflowStepState::InProgress
            } else {
                match self.state.status_of(&step.name) {
                    Some(StepState::Succeeded) => WorkflowStepState::Completed,
                    Some(StepState::Running { .. }) => WorkflowStepState::InProgress,
                    _ => WorkflowStepState::Pending,
                }
            };
            steps.push((step.name.clone(), state));
        }

        let work_item_number = self.work_item_context.as_ref().map(|c| c.number);
        let work_item_title = self
            .work_item_context
            .as_ref()
            .and_then(|c| c.content.lines().next().map(|l| l.trim().to_string()));

        Some(CtxStepInfo {
            workflow_title: title,
            current_step_name: current_step_name.to_string(),
            current_step_index: current_index,
            total_steps: total,
            steps,
            work_item_number,
            work_item_title,
        })
    }

    pub(super) fn persist(&self) -> Result<(), EngineError> {
        self.state_store
            .save(&self.state)
            .map_err(EngineError::Data)?;
        self.mirror_summary();
        Ok(())
    }

    /// Publish the run's summary to the shared session, if there is one.
    ///
    /// `try_write` rather than `write`: `persist` is synchronous and called
    /// from inside the async run loop, so blocking on the lock here would
    /// block the runtime thread. A contended tick is simply skipped — the
    /// engine persists after every transition, so the next one re-publishes,
    /// and a frontend polling the session is at most one transition behind.
    pub(super) fn mirror_summary(&self) {
        let Some(session) = self.session_mirror.as_ref() else {
            return;
        };
        if let Ok(mut guard) = session.try_write() {
            guard
                .state_mut()
                .set_current_workflow(Some(self.state.summary()));
        }
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
