//! Workflow definition parsing, DAG, state persistence — no Docker required.
//!
//! Parity matrix items 33–35 from WI 0073:
//!   33. Workflow file parsing: .md, .toml, .yaml produce identical Workflow structs
//!   34. Prompt template substitution (covered in data-layer colocated tests)
//!   35. Workflow state persistence: save/load round-trip + captured-fixture
//!       forward-compatibility against `tests/fixtures/workflow_state/v1.json`.

use std::collections::HashSet;

use amux::data::error::DataError;
use amux::data::workflow_dag::WorkflowDag;
use amux::data::workflow_definition::{Workflow, WorkflowFormat, WorkflowStep};
use amux::data::workflow_state::{StepState, WorkflowState, WORKFLOW_STATE_SCHEMA_VERSION};
use amux::data::EngineWorkflowStateStore;

// ─── Workflow parsing parity across formats ───────────────────────────────────

const CANONICAL_WORKFLOW: &str = r#"# Test Workflow

## Step: alpha
Prompt:
Do the first thing.

## Step: beta
Depends-on: alpha
Prompt:
Do the second thing.
"#;

const CANONICAL_TOML: &str = r#"
[[step]]
name = "alpha"
prompt = "Do the first thing."

[[step]]
name = "beta"
depends_on = ["alpha"]
prompt = "Do the second thing."
"#;

const CANONICAL_YAML: &str = r#"
steps:
  - name: alpha
    prompt: "Do the first thing."
  - name: beta
    depends_on: [alpha]
    prompt: "Do the second thing."
"#;

fn step_names(wf: &Workflow) -> Vec<&str> {
    wf.steps.iter().map(|s| s.name.as_str()).collect()
}

fn step_deps<'a>(wf: &'a Workflow, name: &str) -> Vec<&'a str> {
    wf.steps
        .iter()
        .find(|s| s.name == name)
        .map(|s| s.depends_on.iter().map(|d| d.as_str()).collect())
        .unwrap_or_default()
}

#[test]
fn workflow_markdown_parses_steps_and_title() {
    let wf = Workflow::parse(CANONICAL_WORKFLOW, WorkflowFormat::Markdown).unwrap();
    assert_eq!(wf.title.as_deref(), Some("Test Workflow"));
    assert_eq!(step_names(&wf), vec!["alpha", "beta"]);
    assert_eq!(step_deps(&wf, "beta"), vec!["alpha"]);
}

#[test]
fn workflow_toml_parses_correctly() {
    let wf = Workflow::parse(CANONICAL_TOML, WorkflowFormat::Toml).unwrap();
    assert_eq!(step_names(&wf), vec!["alpha", "beta"]);
    assert_eq!(step_deps(&wf, "beta"), vec!["alpha"]);
}

#[test]
fn workflow_yaml_parses_correctly() {
    let wf = Workflow::parse(CANONICAL_YAML, WorkflowFormat::Yaml).unwrap();
    assert_eq!(step_names(&wf), vec!["alpha", "beta"]);
    assert_eq!(step_deps(&wf, "beta"), vec!["alpha"]);
}

#[test]
fn workflow_md_toml_yaml_produce_equivalent_structure() {
    let md = Workflow::parse(CANONICAL_WORKFLOW, WorkflowFormat::Markdown).unwrap();
    let toml = Workflow::parse(CANONICAL_TOML, WorkflowFormat::Toml).unwrap();
    let yaml = Workflow::parse(CANONICAL_YAML, WorkflowFormat::Yaml).unwrap();

    for (a, b) in md.steps.iter().zip(toml.steps.iter()) {
        assert_eq!(a.name, b.name, "name mismatch md vs toml");
        assert_eq!(a.depends_on, b.depends_on, "deps mismatch md vs toml");
    }
    for (a, b) in md.steps.iter().zip(yaml.steps.iter()) {
        assert_eq!(a.name, b.name, "name mismatch md vs yaml");
        assert_eq!(a.depends_on, b.depends_on, "deps mismatch md vs yaml");
    }
}

#[test]
fn workflow_load_from_disk_md() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("test.md");
    std::fs::write(&path, CANONICAL_WORKFLOW).unwrap();
    let wf = Workflow::load(&path).unwrap();
    assert_eq!(step_names(&wf), vec!["alpha", "beta"]);
}

#[test]
fn workflow_load_from_disk_toml() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("test.toml");
    std::fs::write(&path, CANONICAL_TOML).unwrap();
    let wf = Workflow::load(&path).unwrap();
    assert_eq!(step_names(&wf), vec!["alpha", "beta"]);
}

#[test]
fn workflow_load_from_disk_yaml() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("test.yaml");
    std::fs::write(&path, CANONICAL_YAML).unwrap();
    let wf = Workflow::load(&path).unwrap();
    assert_eq!(step_names(&wf), vec!["alpha", "beta"]);
}

#[test]
fn workflow_load_unsupported_extension_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("test.json");
    std::fs::write(&path, "{}").unwrap();
    let err = Workflow::load(&path).unwrap_err();
    assert!(matches!(err, DataError::WorkflowState(_)));
}

// ─── WorkflowDag (complex graphs) ────────────────────────────────────────────

fn make_step(name: &str, deps: &[&str]) -> WorkflowStep {
    WorkflowStep {
        name: name.to_string(),
        depends_on: deps.iter().map(|s| s.to_string()).collect(),
        prompt_template: format!("Run {name}"),
        agent: None,
        model: None,
    }
}

#[test]
fn dag_three_step_linear_chain() {
    let steps = vec![
        make_step("a", &[]),
        make_step("b", &["a"]),
        make_step("c", &["b"]),
    ];
    let dag = WorkflowDag::build(&steps).unwrap();
    let order = dag.topological_order();
    let pos = |n: &str| order.iter().position(|x| x == n).unwrap();
    assert!(pos("a") < pos("b") && pos("b") < pos("c"));
}

#[test]
fn dag_diamond_dependency_resolves_correctly() {
    // a → b, a → c, b → d, c → d
    let steps = vec![
        make_step("a", &[]),
        make_step("b", &["a"]),
        make_step("c", &["a"]),
        make_step("d", &["b", "c"]),
    ];
    let dag = WorkflowDag::build(&steps).unwrap();
    let order = dag.topological_order();
    let pos = |n: &str| order.iter().position(|x| x == n).unwrap();
    assert!(pos("a") < pos("b"));
    assert!(pos("a") < pos("c"));
    assert!(pos("b") < pos("d"));
    assert!(pos("c") < pos("d"));
}

#[test]
fn dag_ready_steps_after_partial_completion() {
    let steps = vec![
        make_step("a", &[]),
        make_step("b", &["a"]),
        make_step("c", &["a"]),
        make_step("d", &["b", "c"]),
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
}

#[test]
fn dag_missing_dependency_error() {
    let steps = vec![make_step("a", &["nonexistent"])];
    let err = WorkflowDag::build(&steps).unwrap_err();
    assert!(matches!(err, DataError::MissingDependency { .. }));
}

#[test]
fn dag_cycle_detection() {
    let steps = vec![make_step("a", &["b"]), make_step("b", &["a"])];
    let err = WorkflowDag::build(&steps).unwrap_err();
    assert!(matches!(err, DataError::CyclicDependency { .. }));
}

// ─── WorkflowState: save / load round-trip ───────────────────────────────────

#[test]
fn workflow_state_save_load_roundtrip() {
    let tmp = tempfile::tempdir().unwrap();
    let store = EngineWorkflowStateStore::at_git_root(tmp.path());

    let steps = vec![make_step("a", &[]), make_step("b", &["a"])];
    let mut state = WorkflowState::new("my-wf".into(), &steps, "abc123".into(), None);
    state.set_status("a", StepState::Succeeded);

    store.save(&state).unwrap();

    let loaded = store.load(None, "my-wf").unwrap().expect("state exists");
    assert_eq!(loaded.workflow_name, "my-wf");
    assert_eq!(loaded.schema_version, WORKFLOW_STATE_SCHEMA_VERSION);
    assert!(loaded.completed_steps.contains("a"));
    assert!(matches!(loaded.status_of("a"), Some(StepState::Succeeded)));
}

#[test]
fn workflow_state_save_load_with_work_item() {
    let tmp = tempfile::tempdir().unwrap();
    let store = EngineWorkflowStateStore::at_git_root(tmp.path());

    let steps = vec![make_step("alpha", &[])];
    let state = WorkflowState::new("my-wf".into(), &steps, "hash".into(), Some(42));

    store.save(&state).unwrap();

    let loaded = store.load(Some(42), "my-wf").unwrap().expect("state");
    assert_eq!(loaded.work_item, Some(42));
}

#[test]
fn workflow_state_load_absent_returns_none() {
    let tmp = tempfile::tempdir().unwrap();
    let store = EngineWorkflowStateStore::at_git_root(tmp.path());
    let result = store.load(None, "nonexistent").unwrap();
    assert!(result.is_none());
}

#[test]
fn workflow_state_delete_removes_file() {
    let tmp = tempfile::tempdir().unwrap();
    let store = EngineWorkflowStateStore::at_git_root(tmp.path());

    let steps = vec![make_step("a", &[])];
    let state = WorkflowState::new("del-wf".into(), &steps, "h".into(), None);
    store.save(&state).unwrap();

    assert!(store.load(None, "del-wf").unwrap().is_some());
    store.delete(None, "del-wf").unwrap();
    assert!(store.load(None, "del-wf").unwrap().is_none());
}

#[test]
fn workflow_state_is_complete_after_all_steps_succeed() {
    let steps = vec![make_step("a", &[]), make_step("b", &["a"])];
    let mut state = WorkflowState::new("wf".into(), &steps, "h".into(), None);
    assert!(!state.is_complete());
    state.set_status("a", StepState::Succeeded);
    state.set_status("b", StepState::Succeeded);
    assert!(state.is_complete());
}

#[test]
fn workflow_state_interrupted_running_steps_detected() {
    let steps = vec![make_step("a", &[])];
    let mut state = WorkflowState::new("wf".into(), &steps, "h".into(), None);
    state.set_status("a", StepState::Running { container_id: None });
    let interrupted = state.interrupted_running_steps();
    assert_eq!(interrupted, vec!["a"]);
}

#[test]
fn workflow_state_schema_version_constant() {
    assert_eq!(
        WorkflowState::schema_version(),
        WORKFLOW_STATE_SCHEMA_VERSION
    );
}

// ─── Workflow DAG + WorkflowState integration ────────────────────────────────

#[test]
fn state_next_ready_uses_dag_correctly() {
    let steps = vec![
        make_step("init", &[]),
        make_step("build", &["init"]),
        make_step("test", &["build"]),
    ];
    let dag = WorkflowDag::build(&steps).unwrap();
    let mut state = WorkflowState::new("ci".into(), &steps, "h".into(), None);

    assert_eq!(state.next_ready(&dag), vec!["init"]);
    state.set_status("init", StepState::Succeeded);
    assert_eq!(state.next_ready(&dag), vec!["build"]);
    state.set_status("build", StepState::Succeeded);
    assert_eq!(state.next_ready(&dag), vec!["test"]);
}

// ─── Captured fixture forward-compat ─────────────────────────────────────────
//
// `tests/fixtures/workflow_state/v1.json` is a captured snapshot of the
// schema-v1 on-disk shape that prior amux releases wrote. The new
// `WorkflowState` deserializer must continue to load it without loss.

#[test]
fn workflow_state_v1_fixture_deserializes_cleanly() {
    let fixture = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/workflow_state/v1.json",
    ))
    .expect("fixture file must exist");

    let state: WorkflowState =
        serde_json::from_str(&fixture).expect("schema-v1 fixture must deserialize");

    assert_eq!(state.schema_version, 1);
    assert_eq!(state.workflow_name, "example");
    assert_eq!(state.workflow_hash, "abc12345");
    assert_eq!(state.work_item, None);
    assert!(state.step_states.contains_key("alpha"));
    assert!(state.step_states.contains_key("beta"));
    assert_eq!(state.step_states["alpha"], StepState::Pending);
    assert_eq!(state.step_states["beta"], StepState::Pending);
    assert!(state.completed_steps.is_empty());
    assert!(state.current_step_index.is_none());
}

#[test]
fn workflow_state_v1_fixture_round_trip_through_store() {
    // Loading the fixture, persisting it via the store, and reloading must
    // produce the same object — the round-trip is lossless across schema-v1.
    let fixture = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/workflow_state/v1.json",
    ))
    .unwrap();
    let original: WorkflowState = serde_json::from_str(&fixture).unwrap();

    let tmp = tempfile::tempdir().unwrap();
    let store = EngineWorkflowStateStore::at_git_root(tmp.path());
    store.save(&original).unwrap();
    let reloaded = store
        .load(None, &original.workflow_name)
        .unwrap()
        .expect("state must persist");

    assert_eq!(reloaded.workflow_name, original.workflow_name);
    assert_eq!(reloaded.workflow_hash, original.workflow_hash);
    assert_eq!(reloaded.step_states, original.step_states);
    assert_eq!(reloaded.completed_steps, original.completed_steps);
    assert_eq!(reloaded.work_item, original.work_item);
}
