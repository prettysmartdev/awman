//! A run-pinned, read-only projection of the workflow state saved for a run.
//!
//! Callers pass a run row, never a path supplied by a client. This keeps the
//! persisted path and its identity checks together at the filesystem boundary.

use chrono::Duration;
use serde::{Deserialize, Serialize};

use std::path::Path;

use crate::data::fs::task_store::{Run, RunId, RunStatus};
use crate::data::workflow_state::{
    group_stages, PhaseStepStatus, StepState, WorkflowState, WORKFLOW_STATE_SCHEMA_VERSION,
};

const MAX_STATE_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RunWorkflow {
    Dag(RunDag),
    NoWorkflow,
    Unavailable { reason: RunWorkflowUnavailable },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunWorkflowUnavailable {
    Missing,
    Unreadable,
    Incompatible,
    Mismatch,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunDag {
    pub run_id: RunId,
    pub workflow_name: String,
    pub nodes: Vec<RunDagNode>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunDagNode {
    pub name: String,
    pub kind: RunDagNodeKind,
    pub outcome: RunDagOutcome,
    pub depends_on: Vec<String>,
    pub agent: Option<String>,
    pub model: Option<String>,
    pub stage: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunDagNodeKind {
    Setup,
    Step,
    Teardown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunDagOutcome {
    Pending,
    Running,
    Succeeded,
    Failed,
    Cancelled,
    Unfinished,
}

/// Load and project only the state path persisted on `run`.
///
/// The state file lives in `<root>/.awman/workflows/` of a workflow worktree,
/// which is mounted into the agent's container and therefore agent-writable.
/// The read is one descriptor walk below `<root>` that never follows a
/// symlink, never blocks on a FIFO and never reads more than
/// [`MAX_STATE_BYTES`] + 1 bytes, so a file replaced between checks cannot
/// redirect, stall or exhaust the daemon (security.md, agent-writable files).
pub fn project_run_workflow(run: &Run) -> RunWorkflow {
    let Some(path) = run.workflow_state_path.as_deref() else {
        return RunWorkflow::NoWorkflow;
    };
    let bytes = match read_state_bytes(path) {
        Ok(bytes) => bytes,
        Err(reason) => return RunWorkflow::Unavailable { reason },
    };
    let raw: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(raw) => raw,
        Err(_) => {
            return RunWorkflow::Unavailable {
                reason: RunWorkflowUnavailable::Unreadable,
            }
        }
    };
    if raw
        .get("schema_version")
        .and_then(serde_json::Value::as_u64)
        .is_some_and(|version| version > u64::from(WORKFLOW_STATE_SCHEMA_VERSION))
    {
        return RunWorkflow::Unavailable {
            reason: RunWorkflowUnavailable::Incompatible,
        };
    }
    let state: WorkflowState = match serde_json::from_value(raw) {
        Ok(state) => state,
        Err(_) => {
            return RunWorkflow::Unavailable {
                reason: RunWorkflowUnavailable::Unreadable,
            }
        }
    };
    project_workflow_state(run, &state)
}

/// Split `<root>/.awman/workflows/<file>` into the trusted root and the
/// agent-controllable components below it. Any other shape is refused: the
/// projection only reads paths the state store itself produced.
fn split_state_path(path: &Path) -> Option<(&Path, [&std::ffi::OsStr; 3])> {
    let file = path.file_name()?;
    let workflows_dir = path.parent()?;
    let awman_dir = workflows_dir.parent()?;
    let root = awman_dir.parent()?;
    let workflows = workflows_dir.file_name()?;
    let awman = awman_dir.file_name()?;
    (awman == ".awman" && workflows == "workflows" && root.is_absolute())
        .then_some((root, [awman, workflows, file]))
}

fn read_limited(file: std::fs::File) -> Result<Vec<u8>, RunWorkflowUnavailable> {
    use std::io::Read;
    let mut bytes = Vec::new();
    file.take(MAX_STATE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| RunWorkflowUnavailable::Unreadable)?;
    if bytes.len() as u64 > MAX_STATE_BYTES {
        return Err(RunWorkflowUnavailable::Unreadable);
    }
    Ok(bytes)
}

#[cfg(unix)]
fn read_state_bytes(path: &Path) -> Result<Vec<u8>, RunWorkflowUnavailable> {
    use nix::errno::Errno;
    use nix::fcntl::{open, openat, OFlag};
    use nix::sys::stat::Mode;
    use std::fs::File;

    let Some((root, [awman, workflows, name])) = split_state_path(path) else {
        return Err(RunWorkflowUnavailable::Unreadable);
    };
    let missing = |error: Errno| match error {
        Errno::ENOENT => RunWorkflowUnavailable::Missing,
        _ => RunWorkflowUnavailable::Unreadable,
    };
    let dir_flags = OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC | OFlag::O_RDONLY;
    // The worktree (or run directory) root is created by awman on the host;
    // the agent sees it only as its mount point and cannot replace it.
    let root = open(
        root,
        OFlag::O_DIRECTORY | OFlag::O_CLOEXEC | OFlag::O_RDONLY,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(missing)?;
    let awman = openat(&root, awman, dir_flags, Mode::empty())
        .map(File::from)
        .map_err(missing)?;
    let workflows = openat(&awman, workflows, dir_flags, Mode::empty())
        .map(File::from)
        .map_err(missing)?;
    let file_flags = OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC;
    let file = openat(&workflows, name, file_flags, Mode::empty())
        .map(File::from)
        .map_err(missing)?;
    if !file
        .metadata()
        .is_ok_and(|metadata| metadata.file_type().is_file())
    {
        return Err(RunWorkflowUnavailable::Unreadable);
    }
    read_limited(file)
}

#[cfg(not(unix))]
fn read_state_bytes(path: &Path) -> Result<Vec<u8>, RunWorkflowUnavailable> {
    if split_state_path(path).is_none()
        || crate::data::fs::path_guard::ensure_not_symlinked(path).is_err()
    {
        return Err(RunWorkflowUnavailable::Unreadable);
    }
    let file = std::fs::File::open(path).map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => RunWorkflowUnavailable::Missing,
        _ => RunWorkflowUnavailable::Unreadable,
    })?;
    if !file.metadata().is_ok_and(|metadata| metadata.is_file()) {
        return Err(RunWorkflowUnavailable::Unreadable);
    }
    read_limited(file)
}

/// Pure projection used by the loader and unit contracts.
pub fn project_workflow_state(run: &Run, state: &WorkflowState) -> RunWorkflow {
    if state.schema_version > WORKFLOW_STATE_SCHEMA_VERSION
        || (state.steps.is_empty() && !state.step_states.is_empty())
    {
        return RunWorkflow::Unavailable {
            reason: RunWorkflowUnavailable::Incompatible,
        };
    }
    if state.started_at < run.started_at - Duration::seconds(5)
        || run
            .finished_at
            .is_some_and(|finished| state.started_at > finished)
    {
        return RunWorkflow::Unavailable {
            reason: RunWorkflowUnavailable::Mismatch,
        };
    }

    let finished = run.status != RunStatus::Running;
    let mut nodes = Vec::new();
    nodes.extend(state.setup_step_states.iter().map(|item| RunDagNode {
        name: item.description.clone(),
        kind: RunDagNodeKind::Setup,
        outcome: phase_outcome(&item.status, finished),
        depends_on: Vec::new(),
        agent: None,
        model: None,
        stage: 0,
    }));

    let refs: Vec<(&str, &[String])> = state
        .steps
        .iter()
        .map(|step| (step.name.as_str(), step.depends_on.as_slice()))
        .collect();
    // Main steps are emitted stage by stage (definition order within a stage)
    // so consumers can group them by `stage` without re-sorting. `stage` is
    // 0-based, matching the live workflow route; setup and teardown nodes
    // are told apart by `kind`, not by their (always 0) stage.
    let mut ordered: Vec<(usize, usize)> = Vec::with_capacity(state.steps.len());
    for (stage, indices) in group_stages(&refs).iter().enumerate() {
        let mut indices = indices.clone();
        indices.sort_unstable();
        ordered.extend(indices.into_iter().map(|index| (stage, index)));
    }
    nodes.extend(
        ordered
            .into_iter()
            .map(|(stage, index)| (stage, &state.steps[index]))
            .map(|(stage, step)| RunDagNode {
                name: step.name.clone(),
                kind: RunDagNodeKind::Step,
                outcome: step_outcome(state.step_states.get(&step.name), finished),
                depends_on: step.depends_on.clone(),
                agent: step.agent.clone(),
                model: step.model.clone(),
                stage,
            }),
    );
    nodes.extend(state.teardown_step_states.iter().map(|item| RunDagNode {
        name: item.description.clone(),
        kind: RunDagNodeKind::Teardown,
        outcome: phase_outcome(&item.status, finished),
        depends_on: Vec::new(),
        agent: None,
        model: None,
        stage: 0,
    }));
    RunWorkflow::Dag(RunDag {
        run_id: run.id.clone(),
        workflow_name: state.workflow_name.clone(),
        nodes,
    })
}

fn step_outcome(state: Option<&StepState>, finished: bool) -> RunDagOutcome {
    match state.unwrap_or(&StepState::Pending) {
        StepState::Pending => RunDagOutcome::Pending,
        StepState::Running { .. } if finished => RunDagOutcome::Unfinished,
        StepState::Running { .. } => RunDagOutcome::Running,
        StepState::Succeeded => RunDagOutcome::Succeeded,
        StepState::Failed { .. } => RunDagOutcome::Failed,
        StepState::Cancelled | StepState::Skipped => RunDagOutcome::Cancelled,
    }
}

fn phase_outcome(status: &PhaseStepStatus, finished: bool) -> RunDagOutcome {
    match status {
        PhaseStepStatus::Pending => RunDagOutcome::Pending,
        PhaseStepStatus::Running if finished => RunDagOutcome::Unfinished,
        PhaseStepStatus::Running => RunDagOutcome::Running,
        PhaseStepStatus::Succeeded => RunDagOutcome::Succeeded,
        PhaseStepStatus::Failed { .. } => RunDagOutcome::Failed,
        PhaseStepStatus::Remediating { .. } if finished => RunDagOutcome::Unfinished,
        PhaseStepStatus::Remediating { .. } => RunDagOutcome::Running,
        PhaseStepStatus::Skipped => RunDagOutcome::Cancelled,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::fs::task_store::RunPhase;
    use crate::data::ids::TaskId;
    use chrono::{DateTime, TimeZone, Utc};

    fn run(path: Option<std::path::PathBuf>, status: RunStatus) -> Run {
        let started_at = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        Run {
            id: RunId::new(),
            task_id: TaskId::new(),
            status,
            workflow_path: None,
            workflow_state_path: path,
            session_id: None,
            started_at,
            finished_at: (status != RunStatus::Running)
                .then_some(started_at + Duration::minutes(1)),
            error: None,
            reason: None,
            unmet_env: Vec::new(),
            phase: None::<RunPhase>,
            image_warning: None,
        }
    }

    fn state_json(version: u32, started: DateTime<Utc>, steps: serde_json::Value) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "schema_version": version, "workflow_name": "saved-flow", "workflow_hash": "h",
            "step_states": {"plan": "Succeeded", "build": {"Running": {"container_id": null}}},
            "completed_steps": ["plan"], "current_step_index": 1,
            "started_at": started, "updated_at": started, "steps": steps,
            "setup_step_states": [{"description":"checkout", "status":"Succeeded"}],
            "teardown_step_states": [{"description":"publish", "status":"Pending"}]
        }))
        .unwrap()
    }

    #[test]
    fn projection_keeps_setup_dependencies_stages_and_finished_running_outcome() {
        let run = run(None, RunStatus::WorkflowExecuted);
        let state: WorkflowState = serde_json::from_slice(&state_json(
            WORKFLOW_STATE_SCHEMA_VERSION,
            run.started_at,
            serde_json::json!([
                {"name":"plan","depends_on":[],"agent":"claude","model":"sonnet"},
                {"name":"build","depends_on":["plan"],"agent":"codex"}
            ]),
        ))
        .unwrap();
        let RunWorkflow::Dag(dag) = project_workflow_state(&run, &state) else {
            panic!("expected DAG")
        };
        assert_eq!(dag.workflow_name, "saved-flow");
        assert_eq!(dag.nodes[0].kind, RunDagNodeKind::Setup);
        assert_eq!(dag.nodes[0].outcome, RunDagOutcome::Succeeded);
        assert_eq!(dag.nodes[1].stage, 0);
        assert_eq!(dag.nodes[2].depends_on, ["plan"]);
        assert_eq!(dag.nodes[2].outcome, RunDagOutcome::Unfinished);
        assert_eq!(dag.nodes[3].kind, RunDagNodeKind::Teardown);
    }

    #[test]
    fn missing_no_workflow_future_schema_and_identity_mismatch_are_explicit() {
        assert_eq!(
            project_run_workflow(&run(None, RunStatus::NotTriggered)),
            RunWorkflow::NoWorkflow
        );
        let sandbox = awman_test_support::TestSandbox::new();
        let missing = run(
            Some(sandbox.work().join(".awman/workflows/missing-state.json")),
            RunStatus::Failed,
        );
        assert_eq!(
            project_run_workflow(&missing),
            RunWorkflow::Unavailable {
                reason: RunWorkflowUnavailable::Missing
            }
        );
        let row = run(None, RunStatus::Failed);
        let future: WorkflowState = serde_json::from_slice(&state_json(
            WORKFLOW_STATE_SCHEMA_VERSION + 1,
            row.started_at,
            serde_json::json!([]),
        ))
        .unwrap();
        assert_eq!(
            project_workflow_state(&row, &future),
            RunWorkflow::Unavailable {
                reason: RunWorkflowUnavailable::Incompatible
            }
        );
        let late: WorkflowState = serde_json::from_slice(&state_json(
            WORKFLOW_STATE_SCHEMA_VERSION,
            row.started_at + Duration::minutes(2),
            serde_json::json!([
                {"name":"plan","depends_on":[]},
                {"name":"build","depends_on":["plan"]}
            ]),
        ))
        .unwrap();
        assert_eq!(
            project_workflow_state(&row, &late),
            RunWorkflow::Unavailable {
                reason: RunWorkflowUnavailable::Mismatch
            }
        );
    }

    fn dag(run: &Run, steps: serde_json::Value) -> RunDag {
        let state: WorkflowState = serde_json::from_slice(&state_json(
            WORKFLOW_STATE_SCHEMA_VERSION,
            run.started_at,
            steps,
        ))
        .unwrap();
        match project_workflow_state(run, &state) {
            RunWorkflow::Dag(dag) => dag,
            other => panic!("expected DAG, got {other:?}"),
        }
    }

    #[test]
    fn steps_defined_out_of_stage_order_are_grouped_by_stage() {
        // `lint` has no dependency but is defined after `build ← plan`.
        let row = run(None, RunStatus::WorkflowExecuted);
        let dag = dag(
            &row,
            serde_json::json!([
                {"name":"plan","depends_on":[]},
                {"name":"build","depends_on":["plan"]},
                {"name":"lint","depends_on":[]}
            ]),
        );
        let steps: Vec<(&str, usize)> = dag
            .nodes
            .iter()
            .filter(|node| node.kind == RunDagNodeKind::Step)
            .map(|node| (node.name.as_str(), node.stage))
            .collect();
        assert_eq!(steps, [("plan", 0), ("lint", 0), ("build", 1)]);
    }

    #[test]
    fn a_step_left_remediating_in_a_finished_run_is_unfinished() {
        let finished = run(None, RunStatus::Interrupted);
        let live = run(None, RunStatus::Running);
        let remediating = PhaseStepStatus::Remediating { attempt: 1, of: 3 };
        assert_eq!(
            phase_outcome(&remediating, finished.status != RunStatus::Running),
            RunDagOutcome::Unfinished
        );
        assert_eq!(
            phase_outcome(&remediating, live.status != RunStatus::Running),
            RunDagOutcome::Running
        );
    }

    #[test]
    fn pre_dag_schema_and_state_older_than_the_run_are_unavailable() {
        let row = run(None, RunStatus::Failed);
        let pre_dag: WorkflowState = serde_json::from_slice(&state_json(
            WORKFLOW_STATE_SCHEMA_VERSION,
            row.started_at,
            serde_json::json!([]),
        ))
        .unwrap();
        assert_eq!(
            project_workflow_state(&row, &pre_dag),
            RunWorkflow::Unavailable {
                reason: RunWorkflowUnavailable::Incompatible
            }
        );
        let earlier: WorkflowState = serde_json::from_slice(&state_json(
            WORKFLOW_STATE_SCHEMA_VERSION,
            row.started_at - Duration::minutes(10),
            serde_json::json!([{"name":"plan","depends_on":[]}]),
        ))
        .unwrap();
        assert_eq!(
            project_workflow_state(&row, &earlier),
            RunWorkflow::Unavailable {
                reason: RunWorkflowUnavailable::Mismatch
            }
        );
    }

    /// `<root>/.awman/workflows/state.json`, written as the state store would.
    fn state_file(root: &Path, bytes: &[u8]) -> std::path::PathBuf {
        let dir = root.join(".awman").join("workflows");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.json");
        std::fs::write(&path, bytes).unwrap();
        path
    }

    fn unreadable() -> RunWorkflow {
        RunWorkflow::Unavailable {
            reason: RunWorkflowUnavailable::Unreadable,
        }
    }

    #[test]
    fn loader_reads_the_store_layout_and_rejects_bad_json_and_other_layouts() {
        let sandbox = awman_test_support::TestSandbox::new();
        let mut row = run(None, RunStatus::WorkflowExecuted);
        let good = state_json(
            WORKFLOW_STATE_SCHEMA_VERSION,
            row.started_at,
            serde_json::json!([{"name":"plan","depends_on":[]}]),
        );
        row.workflow_state_path = Some(state_file(&sandbox.work(), &good));
        assert!(matches!(project_run_workflow(&row), RunWorkflow::Dag(_)));

        row.workflow_state_path = Some(state_file(&sandbox.work(), b"{not json"));
        assert_eq!(project_run_workflow(&row), unreadable());

        let elsewhere = sandbox.work().join("state.json");
        std::fs::write(&elsewhere, &good).unwrap();
        row.workflow_state_path = Some(elsewhere);
        assert_eq!(project_run_workflow(&row), unreadable());
    }

    #[test]
    fn loader_refuses_oversize_state_without_reading_past_the_cap() {
        let sandbox = awman_test_support::TestSandbox::new();
        let mut row = run(None, RunStatus::WorkflowExecuted);
        let path = state_file(&sandbox.work(), b"");
        let file = std::fs::File::options().write(true).open(&path).unwrap();
        file.set_len(MAX_STATE_BYTES + 1).unwrap();
        row.workflow_state_path = Some(path);
        assert_eq!(project_run_workflow(&row), unreadable());
    }

    #[cfg(unix)]
    #[test]
    fn loader_never_follows_a_symlinked_file_or_ancestor_and_never_blocks_on_a_fifo() {
        use std::os::unix::fs::symlink;
        let sandbox = awman_test_support::TestSandbox::new();
        let mut row = run(None, RunStatus::WorkflowExecuted);
        let good = state_json(
            WORKFLOW_STATE_SCHEMA_VERSION,
            row.started_at,
            serde_json::json!([{"name":"plan","depends_on":[]}]),
        );
        // A valid state outside the worktree that a link could point at.
        let outside = state_file(&sandbox.tmp(), &good);

        // Final component replaced by a symlink.
        let root = sandbox.work().join("final");
        let path = state_file(&root, b"{}");
        std::fs::remove_file(&path).unwrap();
        symlink(&outside, &path).unwrap();
        row.workflow_state_path = Some(path);
        assert_eq!(project_run_workflow(&row), unreadable());

        // Ancestor `.awman` replaced by a symlink to a directory elsewhere.
        let root = sandbox.work().join("ancestor");
        std::fs::create_dir_all(&root).unwrap();
        symlink(sandbox.tmp().join(".awman"), root.join(".awman")).unwrap();
        row.workflow_state_path = Some(root.join(".awman/workflows/state.json"));
        assert_eq!(project_run_workflow(&row), unreadable());

        // A FIFO in place of the file: must return at once, not block.
        let root = sandbox.work().join("fifo");
        let path = state_file(&root, b"{}");
        std::fs::remove_file(&path).unwrap();
        nix::unistd::mkfifo(&path, nix::sys::stat::Mode::from_bits_truncate(0o600)).unwrap();
        row.workflow_state_path = Some(path);
        assert_eq!(project_run_workflow(&row), unreadable());
    }
}
