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
    Failed {
        error: String,
    },
    Remediating {
        attempt: u32,
        of: u32,
    },
    /// Not run because the workflow was cancelled (a teardown `push_branch`,
    /// `create_pull_request` or `poll_ci` after a cancel). Additive: an older
    /// binary cannot resume a state that carries it, which is acceptable for
    /// a run that was cancelled.
    Skipped,
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
    #[serde(default)]
    pub work_item_key: Option<String>,
    #[serde(default)]
    pub work_item_source_id: Option<String>,
    #[serde(default)]
    pub work_item_content_hash: Option<String>,
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
    /// When an external cancel request stopped this run. Additive; absent on
    /// every run that was not cancelled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancelled_at: Option<DateTime<Utc>>,
    /// How many times each main step has been launched (WI 0137 §5): the
    /// 1-based `step_attempt` of its latest launch. Additive; persisted so a
    /// run resumed after a daemon restart keeps counting from here.
    ///
    /// This is a recorded engine fact, not a computed view field: WI 0137
    /// requires `step_attempt` to restart "from the saved state's count"
    /// after a restart, which only the persisted state can supply. State
    /// files without it load with an empty map, and older binaries ignore
    /// it (no `deny_unknown_fields`), so the schema version is unchanged.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub step_attempts: HashMap<String, u32>,
    /// The runtime container **name** of each main step's latest launch:
    /// the same value events carry in `source.container` (WI 0137 §5/§6).
    /// Kept after the step finishes so a client can correlate a finished
    /// step with its events. Additive, like `step_attempts`.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub step_containers: HashMap<String, String>,
}

/// Group steps into stages by topological depth.
///
/// `steps` is `(name, depends_on)` in definition order. A step's depth is one
/// more than its deepest dependency; every step at the same depth shares a
/// stage. Returns, per non-empty stage in depth order, the indices into
/// `steps` in definition order. Dependencies naming no step are ignored.
///
/// This is the one grouping the TUI's Workflow Overview and
/// `GET /v1/workflows/{id}` both use.
pub fn group_stages(steps: &[(&str, &[String])]) -> Vec<Vec<usize>> {
    let index_of: HashMap<&str, usize> = steps
        .iter()
        .enumerate()
        .map(|(i, (name, _))| (*name, i))
        .collect();

    let mut depths = vec![0usize; steps.len()];
    // A DAG settles within `len` passes; the bound keeps a cyclic definition
    // from spinning forever.
    for _ in 0..=steps.len() {
        let mut changed = false;
        for (i, (_, deps)) in steps.iter().enumerate() {
            for dep in deps.iter() {
                if let Some(&d) = index_of.get(dep.as_str()) {
                    if depths[d] + 1 > depths[i] {
                        depths[i] = depths[d] + 1;
                        changed = true;
                    }
                }
            }
        }
        if !changed {
            break;
        }
    }

    let max_depth = depths.iter().copied().max().unwrap_or(0);
    (0..=max_depth)
        .map(|d| {
            (0..steps.len())
                .filter(|&i| depths[i] == d)
                .collect::<Vec<_>>()
        })
        .filter(|stage| !stage.is_empty())
        .collect()
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
            work_item_key: work_item.map(|number| format!("{number:04}")),
            work_item_source_id: None,
            work_item_content_hash: None,
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
            cancelled_at: None,
            step_attempts: HashMap::new(),
            step_containers: HashMap::new(),
        }
    }

    /// The step's position in [`Self::steps`], the index every event and
    /// `GET /v1/workflows/{id}` use for it.
    pub fn step_index(&self, step_name: &str) -> Option<usize> {
        self.steps.iter().position(|s| s.name == step_name)
    }

    /// The `step_attempt` of the step's latest launch, if it was launched.
    pub fn step_attempt(&self, step_name: &str) -> Option<u32> {
        self.step_attempts.get(step_name).copied()
    }

    /// Count a new launch of `step_name` and return its 1-based attempt.
    pub fn record_step_launch(&mut self, step_name: &str) -> u32 {
        let attempt = self.step_attempts.entry(step_name.to_string()).or_insert(0);
        *attempt += 1;
        *attempt
    }

    /// The runtime container name of the step's latest launch, if any.
    pub fn step_container(&self, step_name: &str) -> Option<&str> {
        self.step_containers.get(step_name).map(String::as_str)
    }

    /// Record the runtime container name of the step's current launch.
    pub fn record_step_container(&mut self, step_name: &str, container_name: &str) {
        self.step_containers
            .insert(step_name.to_string(), container_name.to_string());
    }

    /// The main steps grouped into stages by dependency depth, as step names.
    /// Setup and teardown steps are not part of it. See [`group_stages`].
    pub fn stages(&self) -> Vec<Vec<String>> {
        let deps: Vec<(&str, &[String])> = self
            .steps
            .iter()
            .map(|s| (s.name.as_str(), s.depends_on.as_slice()))
            .collect();
        group_stages(&deps)
            .into_iter()
            .map(|stage| {
                stage
                    .into_iter()
                    .map(|i| self.steps[i].name.clone())
                    .collect()
            })
            .collect()
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
            self.step_attempts.remove(name);
            self.step_containers.remove(name);
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

/// Why a countdown reported through the `yolo_countdown_*` hooks is running.
///
/// The two share one reporting channel deliberately — an unattended frontend
/// surfaces both the same way (WI-0115 §3) — but they mean opposite things to
/// whoever is reading, so the frontend is told which it is rather than left to
/// assume the common one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CountdownKind {
    /// A step's container has gone quiet; when the countdown expires the
    /// engine kills it and advances to the next step.
    #[default]
    StuckStep,
    /// A step's container exited non-zero and no one can be asked what to do;
    /// when the countdown expires the engine retries that same step.
    FailureRetry,
    /// The step's agent said it has finished (it reported `done`, or `idle`:
    /// its turn ended). When the countdown expires the engine kills the
    /// container and advances, exactly like `StuckStep` expiry; on the last
    /// step that completes the workflow. A `working` report or the container
    /// exiting first cancels it. Short: `agent_done_countdown_duration`.
    /// Reader-facing label: "Agent reported done: advancing in 8s".
    ///
    AgentDone,
    /// The step's agent is waiting for an answer (it reported `blocked` on a
    /// permission, a question or a login) and no one is there to give it.
    /// When the countdown expires the engine kills the container and
    /// **fails** the step, which then takes the normal unattended failure
    /// path (one retry, then the workflow fails). Never an advance.
    /// Reader-facing label: "Agent is waiting for input: failing step in 42s".
    AgentBlocked,
    /// The step's agent reported an `error`. When the countdown expires the
    /// engine kills the container and **fails** the step, so `on_failure`
    /// remediation and the `FailureRetry` path apply. Any new output or a
    /// `working` report first cancels it (the agent recovered). Never an
    /// advance. Reader-facing label: "Agent reported an error: failing step
    /// in 42s".
    AgentError,
}
