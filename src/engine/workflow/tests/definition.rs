//! The workflow definition, DAG and state-store contracts the engine runs
//! on (re-derived from the legacy `tests/engine/workflow_end_to_end.rs`):
//! TOML/YAML parse to the same steps, Markdown is refused, scheduling follows
//! dependencies, and persisted state round-trips — including the schema-v1
//! shape earlier releases wrote. Memory and a private temp dir only.

use std::collections::HashSet;

use proptest::prelude::*;

use crate::data::error::DataError;
use crate::data::workflow_dag::WorkflowDag;
use crate::data::workflow_definition::{Workflow, WorkflowFormat, WorkflowStep};
use crate::data::workflow_state::{
    StepState, WorkflowPhase, WorkflowState, WORKFLOW_STATE_SCHEMA_VERSION,
};
use crate::data::WorkflowStateStore;

const MARKDOWN: &str = "# Test Workflow\n\n## Step: alpha\nPrompt:\nDo the first thing.\n";

const TOML: &str = r#"
[[step]]
name = "alpha"
prompt = "Do the first thing."

[[step]]
name = "beta"
depends_on = ["alpha"]
prompt = "Do the second thing."
"#;

const YAML: &str = r#"
steps:
  - name: alpha
    prompt: "Do the first thing."
  - name: beta
    depends_on: [alpha]
    prompt: "Do the second thing."
"#;

/// The schema-v1 state file earlier releases wrote (captured shape).
const STATE_V1: &str = r#"{
  "schema_version": 1,
  "workflow_name": "example",
  "workflow_hash": "abc12345",
  "work_item": null,
  "step_states": { "alpha": "Pending", "beta": "Pending" },
  "completed_steps": [],
  "current_step_index": null,
  "started_at": "2026-01-01T00:00:00Z",
  "updated_at": "2026-01-01T00:00:00Z"
}"#;

fn step(name: &str, deps: &[&str]) -> WorkflowStep {
    WorkflowStep {
        name: name.to_string(),
        depends_on: deps.iter().map(|s| s.to_string()).collect(),
        prompt_template: format!("Run {name}"),
        agent: None,
        model: None,
        overlays: None,
        abort_on_failure: false,
    }
}

fn shape(wf: &Workflow) -> Vec<(String, Vec<String>, String)> {
    wf.steps
        .iter()
        .map(|s| {
            (
                s.name.clone(),
                s.depends_on.clone(),
                s.prompt_template.clone(),
            )
        })
        .collect()
}

#[test]
fn toml_and_yaml_definitions_parse_to_the_same_steps() {
    let toml = Workflow::parse(TOML, WorkflowFormat::Toml).unwrap();
    let yaml = Workflow::parse(YAML, WorkflowFormat::Yaml).unwrap();
    assert_eq!(shape(&toml), shape(&yaml));
    assert_eq!(
        shape(&toml)
            .iter()
            .map(|(n, d, _)| (n.as_str(), d.clone()))
            .collect::<Vec<_>>(),
        vec![("alpha", vec![]), ("beta", vec!["alpha".to_string()])]
    );
}

#[test]
fn loading_chooses_the_format_by_extension_and_refuses_markdown_and_unknown() {
    let tmp = tempfile::tempdir().unwrap();
    for (file, text) in [("w.toml", TOML), ("w.yaml", YAML)] {
        let path = tmp.path().join(file);
        std::fs::write(&path, text).unwrap();
        let wf = Workflow::load(&path).unwrap();
        assert_eq!(wf.steps.len(), 2, "{file}");
    }
    let md = tmp.path().join("w.md");
    std::fs::write(&md, MARKDOWN).unwrap();
    assert!(matches!(
        Workflow::load(&md).unwrap_err(),
        DataError::MarkdownNoLongerSupported { .. }
    ));
    let json = tmp.path().join("w.json");
    std::fs::write(&json, "{}").unwrap();
    assert!(matches!(
        Workflow::load(&json).unwrap_err(),
        DataError::WorkflowState(_)
    ));
}

#[test]
fn dag_rejects_missing_dependencies_and_cycles() {
    assert!(matches!(
        WorkflowDag::build(&[step("a", &["nonexistent"])]).unwrap_err(),
        DataError::MissingDependency { .. }
    ));
    assert!(matches!(
        WorkflowDag::build(&[step("a", &["b"]), step("b", &["a"])]).unwrap_err(),
        DataError::CyclicDependency { .. }
    ));
}

#[test]
fn diamond_releases_both_branches_then_the_join() {
    let steps = vec![
        step("a", &[]),
        step("b", &["a"]),
        step("c", &["a"]),
        step("d", &["b", "c"]),
    ];
    let dag = WorkflowDag::build(&steps).unwrap();
    let mut done: HashSet<String> = HashSet::new();
    assert_eq!(dag.ready_steps(&done), vec!["a"]);
    done.insert("a".into());
    let mut ready = dag.ready_steps(&done);
    ready.sort();
    assert_eq!(ready, vec!["b", "c"]);
    done.insert("b".into());
    assert_eq!(dag.ready_steps(&done), vec!["c"]);
    done.insert("c".into());
    assert_eq!(dag.ready_steps(&done), vec!["d"]);

    let mut state = WorkflowState::new("ci".into(), &steps, "h".into(), None);
    assert_eq!(state.next_ready(&dag), vec!["a"]);
    state.set_status("a", StepState::Succeeded);
    let mut next = state.next_ready(&dag);
    next.sort();
    assert_eq!(next, vec!["b", "c"]);
}

/// A random acyclic graph: step `i` may depend only on steps before it.
fn acyclic_steps() -> impl Strategy<Value = Vec<WorkflowStep>> {
    (1usize..9).prop_flat_map(|n| {
        proptest::collection::vec(proptest::collection::vec(any::<bool>(), n), n).prop_map(
            move |edges| {
                (0..n)
                    .map(|i| {
                        let deps: Vec<String> = (0..i)
                            .filter(|&j| edges[i][j])
                            .map(|j| format!("s{j}"))
                            .collect();
                        let deps: Vec<&str> = deps.iter().map(String::as_str).collect();
                        step(&format!("s{i}"), &deps)
                    })
                    .collect()
            },
        )
    })
}

proptest! {
    /// Scheduling law: the topological order puts every dependency first,
    /// and a step is ready exactly when all its dependencies are done.
    #[test]
    fn scheduling_respects_every_dependency(steps in acyclic_steps()) {
        let dag = WorkflowDag::build(&steps).unwrap();
        let order = dag.topological_order();
        prop_assert_eq!(order.len(), steps.len());
        let pos = |n: &str| order.iter().position(|x| x == n).unwrap();
        for s in &steps {
            for d in &s.depends_on {
                prop_assert!(pos(d) < pos(&s.name));
            }
        }
        let mut done: HashSet<String> = HashSet::new();
        for name in &order {
            for ready in dag.ready_steps(&done) {
                let s = steps.iter().find(|s| s.name == ready).unwrap();
                prop_assert!(s.depends_on.iter().all(|d| done.contains(d)));
                prop_assert!(!done.contains(&ready));
            }
            done.insert(name.clone());
        }
        prop_assert!(dag.ready_steps(&done).is_empty());
    }
}

#[test]
fn state_round_trips_through_the_store_and_delete_removes_it() {
    let tmp = tempfile::tempdir().unwrap();
    let store = WorkflowStateStore::at_git_root(tmp.path());
    assert!(store.load(None, "my-wf").unwrap().is_none());

    let steps = vec![step("a", &[]), step("b", &["a"])];
    let mut state = WorkflowState::new("my-wf".into(), &steps, "abc123".into(), None);
    state.set_status("a", StepState::Succeeded);
    store.save(&state).unwrap();
    let loaded = store.load(None, "my-wf").unwrap().expect("state exists");
    assert_eq!(loaded.schema_version, WORKFLOW_STATE_SCHEMA_VERSION);
    assert_eq!(
        WorkflowState::schema_version(),
        WORKFLOW_STATE_SCHEMA_VERSION
    );
    assert!(loaded.completed_steps.contains("a"));
    assert!(matches!(loaded.status_of("a"), Some(StepState::Succeeded)));
    assert!(!loaded.is_complete());

    let item = WorkflowState::new("my-wf".into(), &steps, "h".into(), Some(42));
    store.save(&item).unwrap();
    assert_eq!(
        store.load(Some(42), "my-wf").unwrap().unwrap().work_item,
        Some(42)
    );

    store.delete(None, "my-wf").unwrap();
    assert!(store.load(None, "my-wf").unwrap().is_none());
    assert!(store.load(Some(42), "my-wf").unwrap().is_some());
}

#[test]
fn completion_and_interrupted_steps_follow_step_states() {
    let steps = vec![step("a", &[]), step("b", &["a"])];
    let mut state = WorkflowState::new("wf".into(), &steps, "h".into(), None);
    state.set_status("a", StepState::Running { container_id: None });
    assert_eq!(state.interrupted_running_steps(), vec!["a"]);
    state.set_status("a", StepState::Succeeded);
    state.set_status("b", StepState::Succeeded);
    assert!(state.is_complete());
}

#[test]
fn a_schema_v1_state_file_loads_and_round_trips_losslessly() {
    let original: WorkflowState = serde_json::from_str(STATE_V1).unwrap();
    assert_eq!(original.schema_version, 1);
    assert_eq!(original.workflow_name, "example");
    assert_eq!(original.step_states["alpha"], StepState::Pending);
    assert!(original.completed_steps.is_empty());
    assert!(original.current_step_index.is_none());
    // Fields added later default: the main phase, no setup/teardown state.
    assert_eq!(original.current_phase, WorkflowPhase::Main);
    assert!(original.setup_step_states.is_empty());
    assert!(original.teardown_step_states.is_empty());
    assert!(!original.setup_completed && !original.teardown_completed);

    let tmp = tempfile::tempdir().unwrap();
    let store = WorkflowStateStore::at_git_root(tmp.path());
    store.save(&original).unwrap();
    let reloaded = store.load(None, "example").unwrap().unwrap();
    assert_eq!(reloaded.workflow_hash, original.workflow_hash);
    assert_eq!(reloaded.step_states, original.step_states);
    assert_eq!(reloaded.completed_steps, original.completed_steps);
    assert_eq!(reloaded.work_item, original.work_item);
}
