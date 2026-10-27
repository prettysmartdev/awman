//! Work item 0132 Part D engine snapshot and phase-state coverage.

use super::*;
use crate::data::workflow_prompt_template::WorkItemContext;
use crate::data::workflow_state::{PhaseStepStatus, WorkflowPhase};

#[derive(Default)]
struct StateSpy(Arc<Mutex<Vec<WorkflowState>>>, Option<WorkflowStateStore>);

impl crate::data::message::UserMessageSink for StateSpy {
    fn write_message(&mut self, _: crate::data::message::UserMessage) {}
    fn replay_queued(&mut self) {}
}

impl WorkflowFrontend for StateSpy {
    fn show_workflow_control_board(
        &mut self,
        _: &WorkflowState,
        _: &AvailableActions,
    ) -> Result<NextAction, EngineError> {
        Ok(NextAction::Pause)
    }
    fn yolo_countdown_tick(
        &mut self,
        _: &str,
        _: Duration,
        _: Duration,
    ) -> Result<YoloTickOutcome, EngineError> {
        Ok(YoloTickOutcome::Cancel)
    }
    fn report_step_status(&mut self, _: &WorkflowStep, _: WorkflowStepStatus) {}
    fn report_workflow_completed(&mut self, _: &WorkflowOutcome) {}
    fn confirm_resume(&mut self, _: &ResumeMismatch) -> Result<bool, EngineError> {
        Ok(true)
    }
    fn report_workflow_state(&mut self, state: &WorkflowState) {
        if let Some(store) = &self.1 {
            assert_eq!(
                store
                    .load(state.work_item, &state.workflow_name)
                    .unwrap()
                    .as_ref(),
                Some(state),
                "snapshot must be saved before publishing"
            );
        }
        self.0.lock().unwrap().push(state.clone());
    }
}

fn spy_engine(tmp: &tempfile::TempDir, frontend: Box<dyn WorkflowFrontend>) -> WorkflowEngine {
    let session = make_session(tmp);
    let wf = make_workflow(
        Some("wi0132-spy"),
        Some("claude"),
        vec![make_step("a", &[], None)],
    );
    WorkflowEngine::new(
        &session,
        WorkflowSpec::new(wf).with_work_item_context(None),
        WorkflowEngineDeps {
            frontend,
            agent_factory: Box::new(FakeAgentExecutionFactory::always_success()),
        },
    )
    .unwrap()
}

fn local_engine(tmp: &tempfile::TempDir) -> WorkflowEngine {
    let session = make_session(tmp);
    let wf = make_workflow(
        Some("wi0132-phase-state"),
        Some("claude"),
        vec![make_step("a", &[], None)],
    );
    WorkflowEngine::new(
        &session,
        WorkflowSpec::new(wf).with_work_item_context(None),
        WorkflowEngineDeps {
            frontend: Box::new(FakeWorkflowFrontend::new([])),
            agent_factory: Box::new(FakeAgentExecutionFactory::always_success()),
        },
    )
    .unwrap()
}

#[test]
fn persist_reports_the_state_after_it_has_been_saved() {
    let tmp = tempfile::tempdir().unwrap();
    let spy = StateSpy(
        Default::default(),
        Some(WorkflowStateStore::at_git_root(tmp.path())),
    );
    let seen = spy.0.clone();
    let mut engine = spy_engine(&tmp, Box::new(spy));
    engine.persist().unwrap();
    let reported = seen.lock().unwrap().last().unwrap().clone();
    let store = WorkflowStateStore::at_git_root(tmp.path());
    let saved = store.load(None, "wi0132-spy").unwrap().unwrap();
    assert_eq!(reported, saved);
}

#[test]
fn shared_frontend_forwards_persisted_workflow_state() {
    let tmp = tempfile::tempdir().unwrap();
    let spy = StateSpy::default();
    let seen = spy.0.clone();
    let shared: Arc<Mutex<Box<dyn WorkflowFrontend>>> = Arc::new(Mutex::new(Box::new(spy)));
    let mut engine = spy_engine(&tmp, Box::new(shared));
    engine.persist().unwrap();
    assert_eq!(seen.lock().unwrap().as_slice(), &[engine.state().clone()]);
}

#[test]
fn declaration_lists_pending_steps_with_substituted_descriptions() {
    use crate::data::workflow_definition::{SetupStep, TeardownStep};
    let tmp = tempfile::tempdir().unwrap();
    let session = make_session(&tmp);
    let wf = make_workflow(
        Some("wi0132-phase-declare"),
        Some("claude"),
        vec![make_step("a", &[], None)],
    );
    let mut engine = WorkflowEngine::new(
        &session,
        WorkflowSpec::new(wf).with_work_item_context(Some(WorkItemContext {
            number: 42,
            content: "## Summary\nImplement snapshot test\n".into(),
        })),
        WorkflowEngineDeps {
            frontend: Box::new(FakeWorkflowFrontend::new([])),
            agent_factory: Box::new(FakeAgentExecutionFactory::always_success()),
        },
    )
    .unwrap();
    let setup = vec![
        SetupStep::RunShell {
            command: "echo {{work_item}}".into(),
            env: None,
        },
        SetupStep::RunScript {
            path: "{{work_item_number}}/setup.sh".into(),
            env: None,
        },
    ];
    let teardown = vec![
        TeardownStep::RunShell {
            command: "echo {{work_item_content}}".into(),
            env: None,
        },
        TeardownStep::RunShell {
            command: "echo done {{work_item_number}}".into(),
            env: None,
        },
    ];
    engine.declare_phase_steps(&setup, &teardown).unwrap();
    let state = engine.state();
    assert_eq!(state.setup_step_states.len(), 2);
    assert_eq!(state.teardown_step_states.len(), 2);
    for step in state
        .setup_step_states
        .iter()
        .chain(&state.teardown_step_states)
    {
        assert_eq!(step.status, PhaseStepStatus::Pending);
    }
    assert!(state.setup_step_states[0].description.contains("42"));
    assert!(state.setup_step_states[1]
        .description
        .contains("0042/setup.sh"));
    assert!(state.teardown_step_states[0]
        .description
        .contains("Implement snapshot test"));
    assert!(state.teardown_step_states[1].description.contains("0042"));
}

#[test]
fn run_phase_refreshes_descriptions_resets_status_and_rebuilds_on_length_change() {
    use crate::data::workflow_definition::SetupStep;
    let tmp = tempfile::tempdir().unwrap();
    let mut engine = local_engine(&tmp);
    let initial = vec![SetupStep::RunShell {
        command: "echo declared".into(),
        env: None,
    }];
    engine.declare_phase_steps(&initial, &[]).unwrap();
    engine.state.setup_step_states[0].status = PhaseStepStatus::Failed {
        error: "old".into(),
    };
    let changed_same_len = vec![SetupStep::RunShell {
        command: "echo changed".into(),
        env: None,
    }];
    engine
        .run_phase(
            PhaseKind::Setup,
            &changed_same_len,
            &[],
            &[],
            Arc::new(MockBackgroundContainer::always_success()).factory(),
        )
        .unwrap();
    assert_eq!(engine.state.setup_step_states.len(), 1);
    assert_eq!(
        engine.state.setup_step_states[0].description,
        "run_shell: echo changed"
    );
    assert_eq!(
        engine.state.setup_step_states[0].status,
        PhaseStepStatus::Succeeded
    );

    let longer = vec![
        SetupStep::RunShell {
            command: "echo rebuilt".into(),
            env: None,
        },
        SetupStep::RunShell {
            command: "echo added".into(),
            env: None,
        },
    ];
    engine
        .run_phase(
            PhaseKind::Setup,
            &longer,
            &[true, true],
            &[],
            Arc::new(MockBackgroundContainer::always_success()).factory(),
        )
        .unwrap();
    assert_eq!(engine.state.setup_step_states.len(), 2);
    assert!(engine.state.setup_step_states[0]
        .description
        .contains("echo rebuilt"));
    assert!(engine
        .state
        .setup_step_states
        .iter()
        .all(|s| s.status == PhaseStepStatus::Succeeded));
}

#[test]
fn skipped_teardown_remains_pending_after_declaration() {
    use crate::data::workflow_definition::TeardownStep;
    let tmp = tempfile::tempdir().unwrap();
    let mut engine = local_engine(&tmp);
    let teardown = vec![TeardownStep::RunShell {
        command: "echo teardown".into(),
        env: None,
    }];
    engine.declare_phase_steps(&[], &teardown).unwrap();
    assert_eq!(engine.state.current_phase, WorkflowPhase::Main);
    assert_eq!(
        engine.state.teardown_step_states[0].status,
        PhaseStepStatus::Pending
    );
}

#[test]
fn failed_persist_publishes_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let spy = StateSpy::default();
    let seen = spy.0.clone();
    let mut engine = spy_engine(&tmp, Box::new(spy));
    seen.lock().unwrap().clear();
    let broken = tmp.path().join("not-a-directory");
    std::fs::write(&broken, "file").unwrap();
    engine.state_store = WorkflowStateStore::at_git_root(&broken);
    assert!(engine.persist().is_err());
    assert!(seen.lock().unwrap().is_empty());
}

#[test]
fn declaration_refreshes_same_length_resume_descriptions_preserving_statuses_and_phase() {
    use crate::data::workflow_definition::{SetupStep, TeardownStep};
    let tmp = tempfile::tempdir().unwrap();
    let mut engine = local_engine(&tmp);
    let setup = vec![SetupStep::RunShell {
        command: "old".into(),
        env: None,
    }];
    let old = vec![TeardownStep::RunShell {
        command: "old".into(),
        env: None,
    }];
    engine.declare_phase_steps(&setup, &old).unwrap();
    assert_eq!(engine.state.current_phase, WorkflowPhase::Setup);
    engine.state.setup_completed = true;
    engine.state.setup_step_states[0].status = PhaseStepStatus::Succeeded;
    engine.state.current_phase = WorkflowPhase::Teardown;
    let changed = vec![TeardownStep::RunShell {
        command: "new".into(),
        env: None,
    }];
    engine.declare_phase_steps(&setup, &changed).unwrap();
    assert_eq!(engine.state.current_phase, WorkflowPhase::Teardown);
    assert_eq!(
        engine.state.setup_step_states[0].status,
        PhaseStepStatus::Succeeded
    );
    assert_eq!(
        engine.state.teardown_step_states[0].description,
        "run_shell: new"
    );
}
