//! The client-ready view of `GET /v1/workflows/{command_id}` and its alias
//! `GET /v1/commands/{id}/workflow` (WI 0137 §6).
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
//!   `exit_code` and `error` when failed, `container` while running;
//!   `step_attempt` once the step has been launched (from the state's
//!   additive `step_attempts`);
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
                    container_id: Some(container),
                }) => {
                    obj.insert("container".into(), json!(container));
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::workflow_definition::WorkflowStep;

    fn step(name: &str, deps: &[&str]) -> WorkflowStep {
        WorkflowStep {
            name: name.to_string(),
            depends_on: deps.iter().map(|d| d.to_string()).collect(),
            prompt_template: String::new(),
            agent: None,
            model: None,
            overlays: None,
            abort_on_failure: false,
        }
    }

    fn state_value(steps: &[WorkflowStep]) -> (WorkflowState, Value) {
        let state = WorkflowState::new("w".into(), steps, "h".into(), None);
        let value = serde_json::to_value(&state).unwrap();
        (state, value)
    }

    #[test]
    fn keeps_every_legacy_field_and_adds_computed_ones() {
        let (mut state, _) = state_value(&[step("a", &[]), step("b", &["a"])]);
        state.step_states.insert("a".into(), StepState::Succeeded);
        state.step_states.insert(
            "b".into(),
            StepState::Failed {
                exit_code: 1,
                error_message: Some("boom".into()),
            },
        );
        let original = serde_json::to_value(&state).unwrap();
        let view = client_view(original.clone());
        for (key, val) in original.as_object().unwrap() {
            if key != "steps" {
                assert_eq!(&view[key], val, "legacy key {key} changed");
            }
        }
        assert_eq!(view["stages"], json!([["a"], ["b"]]));
        assert_eq!(view["phase"], "main");
        assert_eq!(view["current_phase"], "Main");
        assert_eq!(view["steps"][0]["name"], "a");
        assert_eq!(view["steps"][0]["status"], "succeeded");
        assert_eq!(view["steps"][1]["index"], 1);
        assert_eq!(view["steps"][1]["stage"], 1);
        assert_eq!(view["steps"][1]["status"], "failed");
        assert_eq!(view["steps"][1]["exit_code"], 1);
        assert_eq!(view["steps"][1]["error"], "boom");
        assert_eq!(view["step_states"], original["step_states"]);
    }

    #[test]
    fn running_step_exposes_container() {
        let (mut state, _) = state_value(&[step("a", &[])]);
        state.step_states.insert(
            "a".into(),
            StepState::Running {
                container_id: Some("c1".into()),
            },
        );
        let view = client_view(serde_json::to_value(&state).unwrap());
        assert_eq!(view["steps"][0]["status"], "running");
        assert_eq!(view["steps"][0]["container"], "c1");
    }

    #[test]
    fn phase_step_states_gain_snake_case_state() {
        use crate::data::workflow_state::PhaseStepState;
        let (mut state, _) = state_value(&[step("a", &[])]);
        state.setup_step_states.push(PhaseStepState {
            description: "clone".into(),
            status: PhaseStepStatus::Remediating { attempt: 1, of: 2 },
        });
        state.teardown_step_states.push(PhaseStepState {
            description: "push".into(),
            status: PhaseStepStatus::Failed { error: "x".into() },
        });
        let view = client_view(serde_json::to_value(&state).unwrap());
        assert_eq!(view["setup_step_states"][0]["state"], "remediating");
        assert_eq!(view["teardown_step_states"][0]["state"], "failed");
        assert!(view["teardown_step_states"][0]["status"].is_object());
    }

    #[test]
    fn non_state_value_passes_through() {
        let v = json!({"hello": 1});
        assert_eq!(client_view(v.clone()), v);
    }
}
