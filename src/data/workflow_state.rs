//! Engine-level workflow execution state — Layer 0.
//!
//! `WorkflowState` is the canonical, fully-serializable snapshot of a workflow
//! invocation's execution progress. The Layer 1 `WorkflowEngine` reads/writes
//! this snapshot through `WorkflowStateStore` after every step transition.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::data::workflow_dag::WorkflowDag;
use crate::data::workflow_definition::WorkflowStep;

/// Current schema version for persisted `WorkflowState`. Bumped when the
/// on-disk shape changes incompatibly.
pub const WORKFLOW_STATE_SCHEMA_VERSION: u32 = 3;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum StepState {
    Pending,
    Running {
        #[serde(default)]
        container_id: Option<String>,
    },
    Succeeded,
    Failed {
        exit_code: i32,
        #[serde(default)]
        error_message: Option<String>,
    },
    Cancelled,
    Skipped,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowStepInfo {
    pub name: String,
    pub depends_on: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum WorkflowPhase {
    Setup,
    #[default]
    Main,
    Teardown,
    Done,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PhaseStepStatus {
    Pending,
    Running,
    Succeeded,
    Failed { error: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhaseStepState {
    pub description: String,
    pub status: PhaseStepStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowState {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    pub workflow_name: String,
    pub workflow_hash: String,
    #[serde(default)]
    pub work_item: Option<u32>,
    pub step_states: HashMap<String, StepState>,
    pub completed_steps: HashSet<String>,
    pub current_step_index: Option<usize>,
    pub started_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default)]
    pub steps: Vec<WorkflowStepInfo>,
    #[serde(default)]
    pub current_phase: WorkflowPhase,
    #[serde(default)]
    pub setup_completed: bool,
    #[serde(default)]
    pub teardown_completed: bool,
    #[serde(default)]
    pub setup_step_states: Vec<PhaseStepState>,
    #[serde(default)]
    pub teardown_step_states: Vec<PhaseStepState>,
}

fn default_schema_version() -> u32 {
    0
}

impl WorkflowState {
    /// Construct a fresh state for a workflow that is about to run for the first time.
    pub fn new(
        workflow_name: String,
        steps: &[WorkflowStep],
        hash: String,
        work_item: Option<u32>,
    ) -> Self {
        let now = Utc::now();
        let mut step_states = HashMap::with_capacity(steps.len());
        for s in steps {
            step_states.insert(s.name.clone(), StepState::Pending);
        }
        let step_infos: Vec<WorkflowStepInfo> = steps
            .iter()
            .map(|s| WorkflowStepInfo {
                name: s.name.clone(),
                depends_on: s.depends_on.clone(),
                agent: s.agent.clone(),
                model: s.model.clone(),
            })
            .collect();
        Self {
            schema_version: WORKFLOW_STATE_SCHEMA_VERSION,
            workflow_name,
            workflow_hash: hash,
            work_item,
            step_states,
            completed_steps: HashSet::new(),
            current_step_index: None,
            started_at: now,
            updated_at: now,
            steps: step_infos,
            current_phase: WorkflowPhase::Main,
            setup_completed: false,
            teardown_completed: false,
            setup_step_states: Vec::new(),
            teardown_step_states: Vec::new(),
        }
    }

    /// Current schema version constant.
    pub fn schema_version() -> u32 {
        WORKFLOW_STATE_SCHEMA_VERSION
    }

    /// Has every step transitioned to a terminal state (Succeeded, Skipped,
    /// or terminal Failed/Cancelled)?
    pub fn is_complete(&self) -> bool {
        self.step_states.values().all(|s| {
            matches!(
                s,
                StepState::Succeeded
                    | StepState::Skipped
                    | StepState::Failed { .. }
                    | StepState::Cancelled
            )
        })
    }

    /// Steps that were in `Running` state when persisted, indicating an
    /// interrupted/crashed run.
    pub fn interrupted_running_steps(&self) -> Vec<String> {
        self.step_states
            .iter()
            .filter(|(_, s)| matches!(s, StepState::Running { .. }))
            .map(|(name, _)| name.clone())
            .collect()
    }

    /// Steps ready to run given current `completed_steps`.
    pub fn next_ready(&self, dag: &WorkflowDag) -> Vec<String> {
        dag.ready_steps(&self.completed_steps)
    }

    /// Mark a step as the given state and update `updated_at`. If the new state
    /// is `Succeeded` or `Skipped`, the step is added to `completed_steps`;
    /// otherwise it is removed.
    pub fn set_status(&mut self, step_name: &str, status: StepState) {
        let is_completed = matches!(status, StepState::Succeeded | StepState::Skipped);
        self.step_states.insert(step_name.to_string(), status);
        if is_completed {
            self.completed_steps.insert(step_name.to_string());
        } else {
            self.completed_steps.remove(step_name);
        }
        self.updated_at = Utc::now();
    }

    /// Status of a step. `None` if the step name is unknown.
    pub fn status_of(&self, step_name: &str) -> Option<&StepState> {
        self.step_states.get(step_name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(name: &str, deps: &[&str]) -> WorkflowStep {
        WorkflowStep {
            name: name.to_string(),
            depends_on: deps.iter().map(|s| s.to_string()).collect(),
            prompt_template: String::new(),
            agent: None,
            model: None,
            overlays: None,
            abort_on_failure: false,
        }
    }

    #[test]
    fn new_state_initializes_pending() {
        let steps = vec![step("a", &[]), step("b", &["a"])];
        let s = WorkflowState::new("wf".into(), &steps, "h".into(), None);
        assert!(matches!(s.status_of("a"), Some(StepState::Pending)));
        assert!(s.completed_steps.is_empty());
        assert_eq!(s.schema_version, WORKFLOW_STATE_SCHEMA_VERSION);
    }

    #[test]
    fn set_status_updates_completed_set() {
        let steps = vec![step("a", &[])];
        let mut s = WorkflowState::new("wf".into(), &steps, "h".into(), None);
        s.set_status("a", StepState::Succeeded);
        assert!(s.completed_steps.contains("a"));
        s.set_status("a", StepState::Pending);
        assert!(!s.completed_steps.contains("a"));
    }

    #[test]
    fn round_trips_through_json() {
        let steps = vec![step("a", &[])];
        let s = WorkflowState::new("wf".into(), &steps, "h".into(), None);
        let j = serde_json::to_string(&s).unwrap();
        let back: WorkflowState = serde_json::from_str(&j).unwrap();
        assert_eq!(s, back);
    }

    #[test]
    fn schema_version_returns_constant() {
        assert_eq!(
            WorkflowState::schema_version(),
            WORKFLOW_STATE_SCHEMA_VERSION
        );
    }

    #[test]
    fn is_complete_when_all_succeeded() {
        let steps = vec![step("a", &[])];
        let mut s = WorkflowState::new("wf".into(), &steps, "h".into(), None);
        s.set_status("a", StepState::Succeeded);
        assert!(s.is_complete());
    }

    #[test]
    fn is_complete_false_when_pending() {
        let steps = vec![step("a", &[])];
        let s = WorkflowState::new("wf".into(), &steps, "h".into(), None);
        assert!(!s.is_complete());
    }
}
