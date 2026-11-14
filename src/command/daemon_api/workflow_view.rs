//! The client-ready view of `GET /v1/worker/commands/{id}/workflow`.
//!
//! The route reads the engine's persisted `WorkflowState` JSON and hands it
//! to [`client_view`], which adds computed fields **at read time** — the
//! state file on disk stays an engine format, and every pre-0137 field stays
//! where it is for existing clients (`step_states`, PascalCase `status`
//! values, `current_phase`; the docs mark them legacy).
//!
//! Added fields:
//!
//! - `stages: [[step_name…]…]`: the grouping the TUI's Workflow Overview
//!   uses, from the shared `WorkflowState::stages`;
//! - on each `steps[i]`: `index`, `stage`, snake_case `status`
//!   (`StepStatusKind`, the enum `workflow_step_transition` uses),
//!   `exit_code` and `error` when failed, `container` (the runtime container
//!   name, as in `source.container`, kept once the step finishes) and
//!   `container_id` while running;
//!   `step_attempt` once the step has been launched (from the state's
//!   additive `step_attempts`; `container` from `step_containers`);
//! - `phase`: `current_phase` in snake_case;
//! - a snake_case `state` beside the legacy `status` on every
//!   `setup_step_states` / `teardown_step_states` entry.

use serde_json::{json, Value};

use crate::data::execution_event::StepStatusKind;
use crate::data::workflow_state::{PhaseStepStatus, StepState, WorkflowPhase, WorkflowState};

fn step_status_kind(state: &StepState) -> StepStatusKind {
    match state {
        StepState::Pending => StepStatusKind::Pending,
        StepState::Running { .. } => StepStatusKind::Running,
        StepState::Succeeded => StepStatusKind::Succeeded,
        StepState::Failed { .. } => StepStatusKind::Failed,
        StepState::Cancelled => StepStatusKind::Cancelled,
        StepState::Skipped => StepStatusKind::Skipped,
    }
}

fn phase_name(phase: WorkflowPhase) -> &'static str {
    match phase {
        WorkflowPhase::Setup => "setup",
        WorkflowPhase::Main => "main",
        WorkflowPhase::Teardown => "teardown",
        WorkflowPhase::Done => "done",
    }
}

fn phase_step_state_name(status: &PhaseStepStatus) -> &'static str {
    match status {
        PhaseStepStatus::Pending => "pending",
        PhaseStepStatus::Running => "running",
        PhaseStepStatus::Succeeded => "succeeded",
        PhaseStepStatus::Failed { .. } => "failed",
        PhaseStepStatus::Remediating { .. } => "remediating",
        PhaseStepStatus::Skipped => "skipped",
    }
}

/// Add `state` to each entry of a `setup_step_states` / `teardown_step_states`
/// array. `names` is the snake_case state per entry, in order.
fn add_phase_states(view: &mut Value, key: &str, names: Vec<&'static str>) {
    if let Some(entries) = view.get_mut(key).and_then(Value::as_array_mut) {
        for (entry, name) in entries.iter_mut().zip(names) {
            if let Some(obj) = entry.as_object_mut() {
                obj.insert("state".into(), json!(name));
            }
        }
    }
}

/// Add the computed, client-ready fields to a serialised `WorkflowState`.
///
/// A value that is not a `WorkflowState` is returned unchanged.
pub fn client_view(state: Value) -> Value {
    let Ok(parsed) = serde_json::from_value::<WorkflowState>(state.clone()) else {
        return state;
    };
    let mut view = state;
    let stages = parsed.stages();

    if let Some(steps) = view.get_mut("steps").and_then(Value::as_array_mut) {
        for (i, (entry, info)) in steps.iter_mut().zip(&parsed.steps).enumerate() {
            let Some(obj) = entry.as_object_mut() else {
                continue;
            };
            obj.insert("index".into(), json!(i));
            if let Some(stage) = stages
                .iter()
                .position(|names| names.iter().any(|n| n == &info.name))
            {
                obj.insert("stage".into(), json!(stage));
            }
            if let Some(attempt) = parsed.step_attempt(&info.name) {
                obj.insert("step_attempt".into(), json!(attempt));
            }
            let step_state = parsed.step_states.get(&info.name);
            let kind = step_state.map_or(StepStatusKind::Pending, step_status_kind);
            obj.insert("status".into(), json!(kind));
            // `container` is the runtime container *name*, the value events
            // carry in `source.container`, so a client can correlate the two.
            // It stays after the step finishes; a step reset to pending for a
            // rerun has no current container.
            if kind != StepStatusKind::Pending {
                if let Some(name) = parsed.step_container(&info.name) {
                    obj.insert("container".into(), json!(name));
                }
            }
            match step_state {
                Some(StepState::Failed {
                    exit_code,
                    error_message,
                }) => {
                    obj.insert("exit_code".into(), json!(exit_code));
                    if let Some(error) = error_message {
                        obj.insert("error".into(), json!(error));
                    }
                }
                Some(StepState::Running {
                    container_id: Some(id),
                }) => {
                    obj.insert("container_id".into(), json!(id));
                }
                _ => {}
            }
        }
    }

    add_phase_states(
        &mut view,
        "setup_step_states",
        parsed
            .setup_step_states
            .iter()
            .map(|s| phase_step_state_name(&s.status))
            .collect(),
    );
    add_phase_states(
        &mut view,
        "teardown_step_states",
        parsed
            .teardown_step_states
            .iter()
            .map(|s| phase_step_state_name(&s.status))
            .collect(),
    );

    if let Some(obj) = view.as_object_mut() {
        obj.insert("stages".into(), json!(stages));
        obj.insert("phase".into(), json!(phase_name(parsed.current_phase)));
    }
    view
}
