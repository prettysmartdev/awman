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
pub const WORKFLOW_STATE_SCHEMA_VERSION: u32 = 4;

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
    Remediating { attempt: u32, of: u32 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhaseStepState {
    pub description: String,
    pub status: PhaseStepStatus,
}

/// Which of the two shell phases a step belongs to.
///
/// Setup and teardown run through the same engine code path
/// (`WorkflowEngine::run_phase`); this is the one value that tells the two
/// apart, replacing the `is_setup: bool` flags and `phase: &str` strings that
/// used to thread through it (F-34).
///
/// The on-disk `WorkflowState` shape is unchanged: the two step-state vectors
/// stay as they are, and `PhaseKind` selects between them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PhaseKind {
    Setup,
    Teardown,
}

impl PhaseKind {
    /// The phase's wire and log name. This is the string the API's
    /// `StatusMessage.phase` field carries, so it must stay `"setup"` /
    /// `"teardown"`.
    pub fn label(self) -> &'static str {
        match self {
            PhaseKind::Setup => "setup",
            PhaseKind::Teardown => "teardown",
        }
    }

    /// The `WorkflowPhase` the engine sits in while this phase runs.
    pub fn workflow_phase(self) -> WorkflowPhase {
        match self {
            PhaseKind::Setup => WorkflowPhase::Setup,
            PhaseKind::Teardown => WorkflowPhase::Teardown,
        }
    }

    /// The `WorkflowPhase` the engine moves to once this phase completes.
    pub fn next_workflow_phase(self) -> WorkflowPhase {
        match self {
            PhaseKind::Setup => WorkflowPhase::Main,
            PhaseKind::Teardown => WorkflowPhase::Done,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowState {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    /// Unique identifier for this workflow invocation. Persisted so resumed
    /// runs reuse the same id (and the same `context(workflow)` directory).
    #[serde(default = "default_invocation_id")]
    pub invocation_id: uuid::Uuid,
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

fn default_invocation_id() -> uuid::Uuid {
    uuid::Uuid::new_v4()
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
            invocation_id: uuid::Uuid::new_v4(),
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

    /// The step states for one phase. The two vectors keep their own
    /// serialized fields (the on-disk shape is unchanged); this is how the
    /// engine's one phase code path picks between them (F-34).
    pub fn phase_step_states(&self, kind: PhaseKind) -> &[PhaseStepState] {
        match kind {
            PhaseKind::Setup => &self.setup_step_states,
            PhaseKind::Teardown => &self.teardown_step_states,
        }
    }

    /// Mutable counterpart of [`WorkflowState::phase_step_states`].
    pub fn phase_step_states_mut(&mut self, kind: PhaseKind) -> &mut Vec<PhaseStepState> {
        match kind {
            PhaseKind::Setup => &mut self.setup_step_states,
            PhaseKind::Teardown => &mut self.teardown_step_states,
        }
    }

    /// Mark a phase as completed. Setup and teardown record completion in
    /// their own flags.
    pub fn set_phase_completed(&mut self, kind: PhaseKind) {
        match kind {
            PhaseKind::Setup => self.setup_completed = true,
            PhaseKind::Teardown => self.teardown_completed = true,
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

    /// Index (into `dag.topological_order()`) of the step this run stopped on:
    /// the first `Failed` or `Cancelled` step, or — when the run was
    /// interrupted rather than failed — the first step that never succeeded.
    ///
    /// `None` means every step succeeded or was skipped: there is nothing left
    /// to resume.
    pub fn resume_stop_point(&self, dag: &WorkflowDag) -> Option<usize> {
        let order = dag.topological_order();
        let stopped = order.iter().position(|name| {
            matches!(
                self.status_of(name),
                Some(StepState::Failed { .. }) | Some(StepState::Cancelled)
            )
        });
        stopped.or_else(|| {
            order.iter().position(|name| {
                !matches!(
                    self.status_of(name),
                    Some(StepState::Succeeded) | Some(StepState::Skipped)
                )
            })
        })
    }

    /// Rewind this state so a resumed run restarts at `start`.
    ///
    /// Steps at or after `start` in topological order go back to `Pending`;
    /// earlier steps that never succeeded are marked `Skipped` so they count as
    /// satisfied dependencies without being re-run. Unknown `start` names are
    /// a no-op.
    ///
    /// This is what makes a failed or aborted run resumable at all: such a run
    /// leaves *every* step in a terminal status, which [`Self::is_complete`]
    /// reads as "finished" — so without a rewind the engine would report
    /// instant success and run nothing.
    pub fn rewind_to(&mut self, dag: &WorkflowDag, start: &str) {
        let order = dag.topological_order();
        let Some(start_idx) = order.iter().position(|n| n == start) else {
            return;
        };
        for (i, name) in order.iter().enumerate() {
            if i >= start_idx {
                self.set_status(name, StepState::Pending);
            } else if !matches!(
                self.status_of(name),
                Some(StepState::Succeeded) | Some(StepState::Skipped)
            ) {
                self.set_status(name, StepState::Skipped);
            }
        }
    }

    /// Drop every step this state records that `dag` does not define, and
    /// return their names in sorted order.
    ///
    /// A saved state outlives edits to its workflow file. A step that was
    /// removed or renamed since the state was written can never run again —
    /// [`Self::next_ready`] asks the DAG, which has never heard of it — but it
    /// still counts towards [`Self::is_complete`], which reads `step_states`.
    /// A non-terminal orphan therefore makes `is_complete` permanently false
    /// and strands the run. Pruning is the only reading that can be right: the
    /// DAG decides what runs, so anything outside it is not part of this
    /// workflow any more.
    pub fn retain_steps_in(&mut self, dag: &WorkflowDag) -> Vec<String> {
        let known: HashSet<String> = dag.topological_order().into_iter().collect();
        let mut dropped: Vec<String> = self
            .step_states
            .keys()
            .filter(|name| !known.contains(*name))
            .cloned()
            .collect();
        dropped.sort();
        for name in &dropped {
            self.step_states.remove(name);
            self.completed_steps.remove(name);
        }
        self.steps.retain(|s| !dropped.contains(&s.name));
        if !dropped.is_empty() {
            self.updated_at = Utc::now();
        }
        dropped
    }

    /// Steps left in a non-recoverable terminal status (`Failed`/`Cancelled`)
    /// by the run that saved this state.
    pub fn unrecovered_steps(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .step_states
            .iter()
            .filter(|(_, s)| matches!(s, StepState::Failed { .. } | StepState::Cancelled))
            .map(|(name, _)| name.clone())
            .collect();
        names.sort();
        names
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

    /// Summarise this run for [`SessionState`](crate::data::session::SessionState).
    pub fn summary(&self) -> WorkflowSummary {
        WorkflowSummary {
            invocation_id: self.invocation_id,
            workflow_name: self.workflow_name.clone(),
            work_item: self.work_item,
            phase: self.current_phase,
            total_steps: self.steps.len(),
            completed_steps: self.completed_steps.len(),
            failed_steps: self
                .step_states
                .values()
                .filter(|s| matches!(s, StepState::Failed { .. }))
                .count(),
            current_step: self
                .current_step_index
                .and_then(|i| self.steps.get(i))
                .map(|s| s.name.clone()),
            updated_at: self.updated_at,
        }
    }
}

/// What a session knows about the workflow running in it right now.
///
/// Derived from [`WorkflowState`] by [`WorkflowState::summary`] and mirrored
/// into `SessionState::current_workflow` whenever the engine persists
/// (WI 0114 F-30/F-22, decision Q3). It is deliberately a *projection*: the
/// run's authoritative state is the `WorkflowState` the engine owns, and this
/// carries only what a session view — the TUI's tab header, a status
/// response — needs in order to render without loading the full state file.
///
/// Not to be confused with
/// [`exec_workflow::WorkflowSummary`](crate::command::commands::exec_workflow::WorkflowSummary),
/// which is the Layer 2 report of a run that has *finished*.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowSummary {
    /// The run's id, stable across resumes.
    pub invocation_id: uuid::Uuid,
    pub workflow_name: String,
    pub work_item: Option<u32>,
    pub phase: WorkflowPhase,
    pub total_steps: usize,
    pub completed_steps: usize,
    pub failed_steps: usize,
    /// Name of the step the run is currently on, if any.
    pub current_step: Option<String>,
    pub updated_at: DateTime<Utc>,
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

    // ── WorkflowSummary ──────────────────────────────────────────────────────

    /// The projection `SessionState::current_workflow` carries (F-30). Counts
    /// come from the live state, so a summary can never disagree with the run
    /// it describes — which is what the deleted `WorkflowInvocation` model
    /// could.
    #[test]
    fn summary_projects_counts_and_current_step_from_the_live_state() {
        let steps = vec![step("a", &[]), step("b", &["a"]), step("c", &["b"])];
        let mut s = WorkflowState::new("deploy".into(), &steps, "h".into(), Some(114));
        s.set_status("a", StepState::Succeeded);
        s.set_status(
            "b",
            StepState::Failed {
                exit_code: 2,
                error_message: None,
            },
        );
        s.current_step_index = Some(1);

        let summary = s.summary();
        assert_eq!(summary.invocation_id, s.invocation_id);
        assert_eq!(summary.workflow_name, "deploy");
        assert_eq!(summary.work_item, Some(114));
        assert_eq!(summary.total_steps, 3);
        assert_eq!(summary.completed_steps, 1);
        assert_eq!(summary.failed_steps, 1);
        assert_eq!(summary.current_step.as_deref(), Some("b"));
        assert_eq!(summary.phase, s.current_phase);
        assert_eq!(summary.updated_at, s.updated_at);
    }

    #[test]
    fn summary_has_no_current_step_before_the_run_starts() {
        let steps = vec![step("a", &[])];
        let s = WorkflowState::new("wf".into(), &steps, "h".into(), None);
        let summary = s.summary();
        assert!(summary.current_step.is_none());
        assert_eq!(summary.completed_steps, 0);
        assert_eq!(summary.failed_steps, 0);
        assert_eq!(summary.total_steps, 1);
    }

    /// An index past the end of `steps` (a truncated dynamic run) must not
    /// panic; the summary simply reports no current step.
    #[test]
    fn summary_tolerates_a_current_step_index_out_of_range() {
        let steps = vec![step("a", &[])];
        let mut s = WorkflowState::new("wf".into(), &steps, "h".into(), None);
        s.current_step_index = Some(9);
        assert!(s.summary().current_step.is_none());
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

    // ── WI-0115 §2: resume support ───────────────────────────────────────

    fn linear_fixture(statuses: &[StepState]) -> (WorkflowState, WorkflowDag) {
        let steps = vec![step("a", &[]), step("b", &["a"]), step("c", &["b"])];
        let mut s = WorkflowState::new("wf".into(), &steps, "h".into(), None);
        for (st, status) in steps.iter().zip(statuses) {
            s.set_status(&st.name, status.clone());
        }
        let dag = WorkflowDag::build(&steps).unwrap();
        (s, dag)
    }

    fn failed() -> StepState {
        StepState::Failed {
            exit_code: 1,
            error_message: None,
        }
    }

    #[test]
    fn resume_stop_point_finds_the_failed_step() {
        let (s, dag) = linear_fixture(&[StepState::Succeeded, failed(), StepState::Cancelled]);
        assert_eq!(s.resume_stop_point(&dag), Some(1));
    }

    #[test]
    fn resume_stop_point_falls_back_to_the_first_unfinished_step() {
        let (s, dag) =
            linear_fixture(&[StepState::Succeeded, StepState::Pending, StepState::Pending]);
        assert_eq!(s.resume_stop_point(&dag), Some(1));
    }

    #[test]
    fn resume_stop_point_is_none_when_everything_finished() {
        let (s, dag) = linear_fixture(&[
            StepState::Succeeded,
            StepState::Succeeded,
            StepState::Skipped,
        ]);
        assert_eq!(s.resume_stop_point(&dag), None);
    }

    #[test]
    fn rewind_to_the_failed_step_makes_an_aborted_state_runnable_again() {
        // What an abort leaves behind: every step terminal, so `is_complete`
        // reads the run as finished and the engine would run nothing.
        let (mut s, dag) = linear_fixture(&[
            StepState::Succeeded,
            StepState::Cancelled,
            StepState::Cancelled,
        ]);
        assert!(
            s.is_complete(),
            "precondition: an aborted state looks complete"
        );

        s.rewind_to(&dag, "b");
        assert_eq!(s.status_of("a"), Some(&StepState::Succeeded));
        assert_eq!(s.status_of("b"), Some(&StepState::Pending));
        assert_eq!(s.status_of("c"), Some(&StepState::Pending));
        assert!(!s.is_complete());
        assert_eq!(s.next_ready(&dag), vec!["b".to_string()]);
    }

    #[test]
    fn rewind_to_a_later_step_skips_the_ones_before_it() {
        let (mut s, dag) = linear_fixture(&[StepState::Succeeded, failed(), StepState::Cancelled]);
        s.rewind_to(&dag, "c");
        assert_eq!(
            s.status_of("b"),
            Some(&StepState::Skipped),
            "an un-run predecessor must count as satisfied, not block its dependent"
        );
        assert!(s.completed_steps.contains("b"));
        assert_eq!(s.next_ready(&dag), vec!["c".to_string()]);
    }

    #[test]
    fn rewind_to_an_earlier_step_reruns_everything_after_it() {
        let (mut s, dag) = linear_fixture(&[StepState::Succeeded, failed(), StepState::Cancelled]);
        s.rewind_to(&dag, "a");
        for name in ["a", "b", "c"] {
            assert_eq!(s.status_of(name), Some(&StepState::Pending), "{name}");
        }
    }

    #[test]
    fn rewind_to_an_unknown_step_changes_nothing() {
        let (mut s, dag) = linear_fixture(&[StepState::Succeeded, failed(), StepState::Cancelled]);
        let before = s.step_states.clone();
        s.rewind_to(&dag, "nope");
        assert_eq!(s.step_states, before);
    }

    #[test]
    fn retain_steps_in_drops_steps_the_workflow_no_longer_defines() {
        let (mut s, dag) = linear_fixture(&[StepState::Succeeded, failed(), StepState::Cancelled]);
        // The workflow file gained and lost a step since this state was saved.
        s.set_status("publish", StepState::Cancelled);
        s.steps.push(WorkflowStepInfo {
            name: "publish".into(),
            depends_on: vec!["c".into()],
            agent: None,
            model: None,
        });

        assert_eq!(s.retain_steps_in(&dag), vec!["publish".to_string()]);
        assert_eq!(s.status_of("publish"), None);
        assert!(!s.steps.iter().any(|i| i.name == "publish"));
        for name in ["a", "b", "c"] {
            assert!(s.status_of(name).is_some(), "{name} must survive");
        }
    }

    #[test]
    fn retain_steps_in_clears_dropped_steps_from_completed() {
        let (mut s, dag) = linear_fixture(&[
            StepState::Succeeded,
            StepState::Succeeded,
            StepState::Succeeded,
        ]);
        s.set_status("publish", StepState::Succeeded);
        assert!(s.completed_steps.contains("publish"));

        s.retain_steps_in(&dag);
        assert!(
            !s.completed_steps.contains("publish"),
            "a dropped step must not keep counting as a satisfied dependency"
        );
    }

    /// The reset in `resume_with_state_root` turns an orphan from terminal to
    /// `Pending`, and `is_complete` reads `step_states` while `next_ready`
    /// reads the DAG — so an unpruned orphan makes the run unfinishable.
    #[test]
    fn retain_steps_in_is_what_lets_a_drifted_state_finish() {
        let (mut s, dag) = linear_fixture(&[
            StepState::Succeeded,
            StepState::Succeeded,
            StepState::Succeeded,
        ]);
        s.set_status("publish", StepState::Pending);
        assert!(
            !s.is_complete(),
            "precondition: the orphan blocks completion"
        );
        assert!(
            s.next_ready(&dag).is_empty(),
            "precondition: and can never be run"
        );

        s.retain_steps_in(&dag);
        assert!(s.is_complete());
    }

    #[test]
    fn retain_steps_in_is_a_no_op_when_nothing_drifted() {
        let (mut s, dag) = linear_fixture(&[StepState::Succeeded, failed(), StepState::Cancelled]);
        let before = s.step_states.clone();
        assert!(s.retain_steps_in(&dag).is_empty());
        assert_eq!(s.step_states, before);
    }

    #[test]
    fn unrecovered_steps_lists_failed_and_cancelled_only() {
        let (s, _) = linear_fixture(&[StepState::Succeeded, failed(), StepState::Cancelled]);
        assert_eq!(
            s.unrecovered_steps(),
            vec!["b".to_string(), "c".to_string()]
        );
    }
}
