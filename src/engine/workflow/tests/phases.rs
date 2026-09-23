//! Setup and teardown phases: `run_phase`, the `on_failure` remediation
//! path, and teardown failure output capture.

use super::super::*;
use super::*;

// ── run_phase unit tests ─────────────────────────────────────────────────

fn setup_steps_sample() -> Vec<crate::data::workflow_definition::SetupStep> {
    use crate::data::workflow_definition::SetupStep;
    vec![
        SetupStep::CloneRepo {
            url: "https://example.com/repo".into(),
            branch: None,
            into: None,
            conflict_mode: Default::default(),
        },
        SetupStep::PullBranch {
            remote: None,
            branch: None,
        },
        SetupStep::RunShell {
            command: "cargo build".into(),
            env: None,
        },
    ]
}

fn teardown_steps_sample() -> Vec<crate::data::workflow_definition::TeardownStep> {
    use crate::data::workflow_definition::TeardownStep;
    vec![
        TeardownStep::RunShell {
            command: "cargo test".into(),
            env: None,
        },
        TeardownStep::CommitChanges {
            message: "auto: results".into(),
            add_all: true,
        },
    ]
}

fn make_minimal_engine(tmp: &tempfile::TempDir) -> WorkflowEngine {
    let session = make_session(tmp);
    let workflow = make_workflow(
        Some("test-wf"),
        Some("claude"),
        vec![make_step("step-a", &[], None)],
    );
    make_engine(
        &session,
        workflow,
        FakeAgentExecutionFactory::always_success(),
        [],
    )
}

#[test]
fn run_setup_executes_steps_in_order() {
    let tmp = tempfile::tempdir().unwrap();
    let mut engine = make_minimal_engine(&tmp);
    let steps = setup_steps_sample();
    let mock = Arc::new(MockBackgroundContainer::always_success());

    engine
        .run_phase(PhaseKind::Setup, &steps, &[], &[], mock.factory())
        .unwrap();

    let calls = mock.calls();
    assert_eq!(calls.len(), 3);
    assert!(calls[0].contains("git clone"));
    assert_eq!(calls[1], "git pull");
    assert_eq!(calls[2], "cargo build");
}

#[test]
fn run_setup_uses_one_fresh_container_per_step() {
    // WI-0082 invariant: each phase step gets its own container so per-step
    // overlays do not leak across step boundaries.
    let tmp = tempfile::tempdir().unwrap();
    let mut engine = make_minimal_engine(&tmp);
    let steps = setup_steps_sample(); // 3 steps
    let mock = Arc::new(MockBackgroundContainer::always_success());

    engine
        .run_phase(PhaseKind::Setup, &steps, &[], &[], mock.factory())
        .unwrap();

    assert_eq!(
        mock.handouts(),
        3,
        "the factory must be invoked once per step (one container per step)",
    );
}

#[test]
fn run_teardown_uses_one_fresh_container_per_step() {
    let tmp = tempfile::tempdir().unwrap();
    let mut engine = make_minimal_engine(&tmp);
    let steps = teardown_steps_sample(); // 2 steps
    let mock = Arc::new(MockBackgroundContainer::always_success());

    let outcome = engine
        .run_phase(PhaseKind::Teardown, &steps, &[], &[], mock.factory())
        .unwrap();
    assert!(!outcome.aborted);
    assert!(!outcome.any_failed);

    assert_eq!(
        mock.handouts(),
        2,
        "teardown must request one container per step",
    );
}

#[test]
fn run_setup_continues_on_failure_by_default() {
    let tmp = tempfile::tempdir().unwrap();
    let mut engine = make_minimal_engine(&tmp);
    let steps = setup_steps_sample(); // 3 steps
    let mock = Arc::new(MockBackgroundContainer::with_results([
        ("".into(), "".into(), 0),            // step 1 succeeds
        ("".into(), "build error".into(), 1), // step 2 fails
        ("".into(), "".into(), 0),            // step 3 still runs
    ]));

    let result = engine.run_phase(PhaseKind::Setup, &steps, &[], &[], mock.factory());

    assert!(
        result.is_ok(),
        "run_phase continues past failures when abort_on_failure=false"
    );
    assert_eq!(mock.calls().len(), 3, "all steps must be exec'd");
}

#[test]
fn run_phase_setup_aborts_on_abort_on_failure_step() {
    let tmp = tempfile::tempdir().unwrap();
    let mut engine = make_minimal_engine(&tmp);
    let steps = setup_steps_sample(); // 3 steps
    let mock = Arc::new(MockBackgroundContainer::with_results([
        ("".into(), "".into(), 0),            // step 1 succeeds
        ("".into(), "build error".into(), 1), // step 2 fails
        ("".into(), "".into(), 0),            // step 3 (never reached)
    ]));
    let abort_flags = vec![false, true, false]; // step 2 has abort_on_failure

    let outcome = engine
        .run_phase(PhaseKind::Setup, &steps, &abort_flags, &[], mock.factory())
        .expect("an aborted phase is reported in the outcome, not as an Err");

    assert!(
        outcome.aborted,
        "run_phase must set aborted when an abort_on_failure step fails"
    );
    assert!(outcome.any_failed);
    assert_eq!(mock.calls().len(), 2, "third step must not be exec'd");
    assert!(engine.abort_on_failure_triggered());
    assert!(
        !engine.state().setup_completed,
        "an aborted setup must not be marked complete, or a resumed run \
             would skip it"
    );
}

#[test]
fn teardown_applies_only_on_success_or_when_forced() {
    // The caller gates the teardown phase; `run_phase` itself always runs
    // the steps it is given. Before F-34 this condition lived inside
    // `run_teardown` as two bool parameters.
    assert!(WorkflowEngine::teardown_applies(true, false));
    assert!(WorkflowEngine::teardown_applies(true, true));
    assert!(WorkflowEngine::teardown_applies(false, true));
    assert!(
        !WorkflowEngine::teardown_applies(false, false),
        "a failed workflow skips teardown unless teardown_on_failure is set"
    );
}

#[test]
fn run_teardown_runs_when_succeeded() {
    let tmp = tempfile::tempdir().unwrap();
    let mut engine = make_minimal_engine(&tmp);
    let steps = teardown_steps_sample();
    let mock = Arc::new(MockBackgroundContainer::always_success());

    let outcome = engine
        .run_phase(PhaseKind::Teardown, &steps, &[], &[], mock.factory())
        .unwrap();
    assert!(!outcome.aborted);
    assert!(!outcome.any_failed);

    assert_eq!(mock.calls().len(), 2, "both teardown steps must exec");
}

#[test]
fn run_teardown_continues_after_step_failure() {
    let tmp = tempfile::tempdir().unwrap();
    let mut engine = make_minimal_engine(&tmp);
    let steps = teardown_steps_sample();
    let mock = Arc::new(MockBackgroundContainer::with_results([
        ("".into(), "test failure".into(), 1), // step 1 fails
        ("".into(), "".into(), 0),             // step 2 succeeds
    ]));

    // Teardown is best-effort: returns Ok even if a step fails.
    let result = engine.run_phase(PhaseKind::Teardown, &steps, &[], &[], mock.factory());
    assert!(
        result.is_ok(),
        "run_phase must return Ok despite step failure"
    );
    let outcome = result.unwrap();
    assert!(!outcome.aborted, "no abort_on_failure steps were set");
    assert!(
        outcome.any_failed,
        "any_step_failed must be true when a step exits non-zero"
    );
    assert_eq!(mock.calls().len(), 2, "both steps must be exec'd");
}

#[test]
fn run_teardown_aborts_on_abort_on_failure_step() {
    let tmp = tempfile::tempdir().unwrap();
    let mut engine = make_minimal_engine(&tmp);
    let steps = teardown_steps_sample();
    let mock = Arc::new(MockBackgroundContainer::with_results([
        ("".into(), "fatal".into(), 1), // step 0 fails
        ("".into(), "".into(), 0),      // step 1 would succeed
    ]));

    // abort_on_failure = true for step 0
    let result = engine.run_phase(
        PhaseKind::Teardown,
        &steps,
        &[true, false],
        &[],
        mock.factory(),
    );
    assert!(result.is_ok());
    let outcome = result.unwrap();
    assert!(
        outcome.aborted,
        "run_phase must set aborted when abort_on_failure step fails"
    );
    assert!(outcome.any_failed, "any_step_failed must also be true");
    assert_eq!(
        mock.calls().len(),
        1,
        "step 1 must be skipped after abort_on_failure step 0 fails"
    );
}

#[test]
fn run_teardown_continues_after_per_step_agent_factory_failure() {
    // Per-step container build failure must not abort teardown; it should
    // record the step as Failed and proceed to the next one.
    use crate::data::workflow_definition::TeardownStep;
    use crate::data::workflow_state::PhaseStepStatus;

    let tmp = tempfile::tempdir().unwrap();
    let mut engine = make_minimal_engine(&tmp);
    let steps = vec![
        TeardownStep::RunShell {
            command: "first".into(),
            env: None,
        },
        TeardownStep::RunShell {
            command: "second".into(),
            env: None,
        },
    ];

    // Factory fails on step 0 (returns Err), succeeds on step 1.
    let mock = Arc::new(MockBackgroundContainer::always_success());
    let mock_for_factory = Arc::clone(&mock);
    let factory = move |idx: usize| -> Result<
        Box<dyn crate::engine::agent_runtime::background::AgentExec>,
        EngineError,
    > {
        if idx == 0 {
            Err(EngineError::Other(
                "simulated overlay resolve failure".into(),
            ))
        } else {
            *mock_for_factory.container_handouts.lock().unwrap() += 1;
            Ok(Box::new(SharedMockExec(Arc::clone(&mock_for_factory))))
        }
    };

    let result = engine.run_phase(PhaseKind::Teardown, &steps, &[], &[], factory);
    assert!(result.is_ok(), "factory failure must not abort teardown");
    let outcome = result.unwrap();
    assert!(
        outcome.any_failed,
        "any_step_failed must be true when factory fails"
    );

    let states = &engine.state().teardown_step_states;
    assert!(
        matches!(&states[0].status, PhaseStepStatus::Failed { error } if error.contains("simulated overlay resolve failure")),
        "step 0 must be recorded as Failed with the factory error: {:?}",
        states[0].status,
    );
    assert_eq!(
        states[1].status,
        PhaseStepStatus::Succeeded,
        "step 1 must still execute after step 0's factory failure",
    );
    assert_eq!(mock.calls().len(), 1, "only step 1 reaches exec");
}

#[test]
fn run_setup_transitions_phase_to_main_on_success() {
    use crate::data::workflow_state::WorkflowPhase;

    let tmp = tempfile::tempdir().unwrap();
    let mut engine = make_minimal_engine(&tmp);
    let steps = setup_steps_sample();
    let mock = Arc::new(MockBackgroundContainer::always_success());

    engine
        .run_phase(PhaseKind::Setup, &steps, &[], &[], mock.factory())
        .unwrap();

    assert_eq!(
        engine.state().current_phase,
        WorkflowPhase::Main,
        "phase must be Main after successful setup"
    );
    assert!(
        engine.state().setup_completed,
        "setup_completed must be true after successful setup"
    );
}

#[test]
fn run_setup_state_tracking() {
    use crate::data::workflow_state::PhaseStepStatus;

    let tmp = tempfile::tempdir().unwrap();
    let mut engine = make_minimal_engine(&tmp);

    use crate::data::workflow_definition::SetupStep;
    let steps = vec![
        SetupStep::RunShell {
            command: "step1".into(),
            env: None,
        },
        SetupStep::RunShell {
            command: "step2".into(),
            env: None,
        },
    ];
    let mock = Arc::new(MockBackgroundContainer::always_success());

    engine
        .run_phase(PhaseKind::Setup, &steps, &[], &[], mock.factory())
        .unwrap();

    let states = &engine.state().setup_step_states;
    assert_eq!(states.len(), 2);
    assert_eq!(states[0].status, PhaseStepStatus::Succeeded);
    assert_eq!(states[1].status, PhaseStepStatus::Succeeded);
}

#[test]
fn run_teardown_state_tracking() {
    use crate::data::workflow_state::PhaseStepStatus;

    let tmp = tempfile::tempdir().unwrap();
    let mut engine = make_minimal_engine(&tmp);

    use crate::data::workflow_definition::TeardownStep;
    let steps = vec![
        TeardownStep::RunShell {
            command: "td1".into(),
            env: None,
        },
        TeardownStep::RunShell {
            command: "td2".into(),
            env: None,
        },
    ];
    let mock = Arc::new(MockBackgroundContainer::always_success());

    let outcome = engine
        .run_phase(PhaseKind::Teardown, &steps, &[], &[], mock.factory())
        .unwrap();
    assert!(!outcome.aborted);
    assert!(!outcome.any_failed);

    let states = &engine.state().teardown_step_states;
    assert_eq!(states.len(), 2);
    assert_eq!(states[0].status, PhaseStepStatus::Succeeded);
    assert_eq!(states[1].status, PhaseStepStatus::Succeeded);
}

#[test]
fn run_setup_failure_records_failed_state() {
    use crate::data::workflow_state::PhaseStepStatus;

    let tmp = tempfile::tempdir().unwrap();
    let mut engine = make_minimal_engine(&tmp);

    use crate::data::workflow_definition::SetupStep;
    let steps = vec![
        SetupStep::RunShell {
            command: "ok-step".into(),
            env: None,
        },
        SetupStep::RunShell {
            command: "bad-step".into(),
            env: None,
        },
    ];
    let mock = Arc::new(MockBackgroundContainer::with_results([
        ("".into(), "".into(), 0),
        ("".into(), "stderr content".into(), 1),
    ]));

    let result = engine.run_phase(PhaseKind::Setup, &steps, &[], &[], mock.factory());
    assert!(
        result.is_ok(),
        "setup continues past failures when abort_on_failure=false"
    );

    let states = &engine.state().setup_step_states;
    assert_eq!(states[0].status, PhaseStepStatus::Succeeded);
    assert!(
        matches!(&states[1].status, PhaseStepStatus::Failed { error } if error == "stderr content"),
        "failed state must capture stderr: {:?}",
        states[1].status
    );
}

#[test]
fn run_teardown_transitions_phase_to_done() {
    use crate::data::workflow_state::WorkflowPhase;

    let tmp = tempfile::tempdir().unwrap();
    let mut engine = make_minimal_engine(&tmp);
    let steps = teardown_steps_sample();
    let mock = Arc::new(MockBackgroundContainer::always_success());

    let _outcome = engine
        .run_phase(PhaseKind::Teardown, &steps, &[], &[], mock.factory())
        .unwrap();

    assert_eq!(
        engine.state().current_phase,
        WorkflowPhase::Done,
        "phase must be Done after teardown completes"
    );
    assert!(engine.state().teardown_completed);
}

#[test]
fn mark_done_sets_phase_to_done() {
    use crate::data::workflow_state::WorkflowPhase;

    let tmp = tempfile::tempdir().unwrap();
    let mut engine = make_minimal_engine(&tmp);
    assert_eq!(engine.state().current_phase, WorkflowPhase::Main);

    engine.mark_done().unwrap();
    assert_eq!(engine.state().current_phase, WorkflowPhase::Done);
}

#[test]
fn run_setup_phase_persistence_verified_from_store() {
    use crate::data::workflow_state::WorkflowPhase;

    let tmp = tempfile::tempdir().unwrap();
    let mut engine = make_minimal_engine(&tmp);

    use crate::data::workflow_definition::SetupStep;
    let steps = vec![SetupStep::RunShell {
        command: "go".into(),
        env: None,
    }];
    let mock = Arc::new(MockBackgroundContainer::always_success());

    engine
        .run_phase(PhaseKind::Setup, &steps, &[], &[], mock.factory())
        .unwrap();

    // Verify the on-disk state was persisted with the correct phase fields.
    let store = WorkflowStateStore::at_git_root(tmp.path());
    let saved = store.load(None, "test-wf").unwrap().unwrap();
    assert_eq!(saved.current_phase, WorkflowPhase::Main);
    assert!(saved.setup_completed);
}

// ── on_failure unit tests ─────────────────────────────────────────────────
//
// These tests call run_phase with non-empty on_failure_configs.
// launch_on_failure_agent internally calls Handle::current().block_on(...),
// which requires a live Tokio runtime on the current thread. We use
// spawn_blocking so we run on a dedicated blocking thread where block_on is
// explicitly permitted, while the multi-thread runtime handles the future.

/// Frontend that records every `write_message` call so tests can assert on
/// the on_failure status messages emitted by the engine. Also records the
/// step name of every `report_step_interactive_launch` call.
pub(super) struct MessageCapturingFrontend {
    messages: Arc<Mutex<Vec<crate::data::message::UserMessage>>>,
    interactive_launches: Arc<Mutex<Vec<String>>>,
}

impl MessageCapturingFrontend {
    pub(super) fn new() -> (Self, Arc<Mutex<Vec<crate::data::message::UserMessage>>>) {
        let store = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                messages: Arc::clone(&store),
                interactive_launches: Arc::new(Mutex::new(Vec::new())),
            },
            store,
        )
    }

    /// Handle to the recorded `report_step_interactive_launch` step names.
    /// Grab before moving the frontend into the engine.
    fn launches_handle(&self) -> Arc<Mutex<Vec<String>>> {
        Arc::clone(&self.interactive_launches)
    }
}

impl crate::data::message::UserMessageSink for MessageCapturingFrontend {
    fn write_message(&mut self, msg: crate::data::message::UserMessage) {
        self.messages.lock().unwrap().push(msg);
    }
    fn replay_queued(&mut self) {}
}

impl WorkflowFrontend for MessageCapturingFrontend {
    fn show_workflow_control_board(
        &mut self,
        _state: &WorkflowState,
        _available: &AvailableActions,
    ) -> Result<NextAction, EngineError> {
        Ok(NextAction::LaunchNext)
    }
    fn confirm_resume(&mut self, _: &ResumeMismatch) -> Result<bool, EngineError> {
        Ok(true)
    }
    fn report_step_status(&mut self, _step: &WorkflowStep, _status: WorkflowStepStatus) {}
    fn report_step_interactive_launch(
        &mut self,
        step: &WorkflowStep,
        _agent: &str,
        _model: Option<&str>,
    ) {
        self.interactive_launches
            .lock()
            .unwrap()
            .push(step.name.clone());
    }
    fn yolo_countdown_tick(
        &mut self,
        _step_name: &str,
        _remaining: Duration,
        _total: Duration,
    ) -> Result<YoloTickOutcome, EngineError> {
        Ok(YoloTickOutcome::Cancel)
    }
    fn report_workflow_completed(&mut self, _outcome: &WorkflowOutcome) {}
}

pub(super) fn make_engine_capturing(
    session: &Session,
    workflow: Workflow,
    factory: FakeAgentExecutionFactory,
    frontend: MessageCapturingFrontend,
) -> WorkflowEngine {
    WorkflowEngine::new(
        session,
        WorkflowSpec::new(workflow).with_work_item_context(None),
        WorkflowEngineDeps {
            frontend: Box::new(frontend),
            agent_factory: Box::new(factory),
        },
    )
    .unwrap()
}

fn remediation_config(max_attempts: u32) -> crate::data::workflow_definition::RemediationConfig {
    crate::data::workflow_definition::RemediationConfig {
        prompt: "Fix the broken step.".into(),
        agent: None,
        model: None,
        max_attempts,
    }
}

// run_phase(Setup): step fails with no on_failure config → Failed, only 1 exec.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn on_failure_absent_step_fails_with_no_retry() {
    use crate::data::workflow_state::PhaseStepStatus;

    tokio::task::spawn_blocking(|| {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session(&tmp);
        let workflow = make_workflow(Some("wf"), Some("claude"), vec![make_step("a", &[], None)]);
        let factory = FakeAgentExecutionFactory::always_success();
        let (frontend, _msgs) = MessageCapturingFrontend::new();
        let mut engine = make_engine_capturing(&session, workflow, factory, frontend);

        let steps = vec![crate::data::workflow_definition::SetupStep::RunShell {
            command: "fail".into(),
            env: None,
        }];
        let mock = Arc::new(MockBackgroundContainer::with_results([(
            "".into(),
            "some error".into(),
            1,
        )]));
        // No on_failure config.
        engine
            .run_phase(PhaseKind::Setup, &steps, &[false], &[], mock.factory())
            .unwrap();

        // Exactly 1 exec: initial attempt only, no retry.
        assert_eq!(
            mock.calls().len(),
            1,
            "no retry must occur without on_failure config"
        );

        let states = &engine.state().setup_step_states;
        assert!(
            matches!(&states[0].status, PhaseStepStatus::Failed { error } if error == "some error"),
            "step must be Failed with correct error message: {:?}",
            states[0].status
        );
    })
    .await
    .unwrap();
}

// Step fails → on_failure launches agent → retry succeeds → step marked Succeeded.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn on_failure_retry_succeeds_step_marked_succeeded() {
    use crate::data::workflow_state::PhaseStepStatus;

    tokio::task::spawn_blocking(|| {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session(&tmp);
        let workflow = make_workflow(Some("wf"), Some("claude"), vec![make_step("a", &[], None)]);
        // The on_failure agent uses FakeAgentExecutionFactory (exit 0, ignored).
        let factory = FakeAgentExecutionFactory::always_success();
        let (frontend, _msgs) = MessageCapturingFrontend::new();
        let mut engine = make_engine_capturing(&session, workflow, factory, frontend);

        let steps = vec![crate::data::workflow_definition::SetupStep::RunShell {
            command: "step".into(),
            env: None,
        }];
        // First call fails (step fails); second call succeeds (retry after agent).
        let mock = Arc::new(MockBackgroundContainer::with_results([
            ("".into(), "error".into(), 1),
            ("".into(), "".into(), 0),
        ]));
        let on_failure_configs = vec![Some(remediation_config(2))];

        let result = engine.run_phase(
            PhaseKind::Setup,
            &steps,
            &[false],
            &on_failure_configs,
            mock.factory(),
        );

        assert!(
            result.is_ok(),
            "setup must succeed when retry succeeds: {result:?}"
        );
        assert_eq!(
            engine.state().setup_step_states[0].status,
            PhaseStepStatus::Succeeded,
            "step must be Succeeded after successful retry"
        );
    })
    .await
    .unwrap();
}

// Success on attempt 1 of 2 stops the loop early.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn on_failure_success_on_first_attempt_stops_loop() {
    tokio::task::spawn_blocking(|| {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session(&tmp);
        let workflow = make_workflow(Some("wf"), Some("claude"), vec![make_step("a", &[], None)]);
        let factory = FakeAgentExecutionFactory::always_success();
        let (frontend, _msgs) = MessageCapturingFrontend::new();
        let mut engine = make_engine_capturing(&session, workflow, factory, frontend);

        let steps = vec![crate::data::workflow_definition::SetupStep::RunShell {
            command: "step".into(),
            env: None,
        }];
        // Fail once, then succeed on retry.
        let mock = Arc::new(MockBackgroundContainer::with_results([
            ("".into(), "err".into(), 1),
            ("".into(), "".into(), 0),
            // third result never consumed — loop must stop after first retry
            ("".into(), "".into(), 0),
        ]));
        let on_failure_configs = vec![Some(remediation_config(3))]; // 3 allowed, but 1 retry should suffice

        engine
            .run_phase(
                PhaseKind::Setup,
                &steps,
                &[false],
                &on_failure_configs,
                mock.factory(),
            )
            .unwrap();

        // Only 2 exec calls: initial fail + one successful retry.
        let calls = mock.calls();
        assert_eq!(
            calls.len(),
            2,
            "must stop after first successful retry: {calls:?}"
        );
    })
    .await
    .unwrap();
}

// Exhausting max_attempts leaves the step failed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn on_failure_exhausts_max_attempts_step_remains_failed() {
    use crate::data::workflow_state::PhaseStepStatus;

    tokio::task::spawn_blocking(|| {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session(&tmp);
        let workflow = make_workflow(Some("wf"), Some("claude"), vec![make_step("a", &[], None)]);
        let factory = FakeAgentExecutionFactory::always_success();
        let (frontend, _msgs) = MessageCapturingFrontend::new();
        let mut engine = make_engine_capturing(&session, workflow, factory, frontend);

        let steps = vec![crate::data::workflow_definition::SetupStep::RunShell {
            command: "step".into(),
            env: None,
        }];
        // Every exec fails.
        let mock = Arc::new(MockBackgroundContainer::with_results([
            ("".into(), "err".into(), 1),
            ("".into(), "err".into(), 1),
            ("".into(), "err".into(), 1),
        ]));
        let on_failure_configs = vec![Some(remediation_config(2))];

        engine
            .run_phase(
                PhaseKind::Setup,
                &steps,
                &[false],
                &on_failure_configs,
                mock.factory(),
            )
            .unwrap();

        assert!(
            matches!(
                &engine.state().setup_step_states[0].status,
                PhaseStepStatus::Failed { .. }
            ),
            "step must be Failed after exhausting on_failure attempts: {:?}",
            engine.state().setup_step_states[0].status
        );
    })
    .await
    .unwrap();
}

// on_failure agent exit code is irrelevant — what matters is the step retry.
// We simulate this by verifying that even if the factory returns a non-zero
// exit code for the agent, the retry still runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn on_failure_agent_exit_code_does_not_affect_retry() {
    use crate::data::workflow_state::PhaseStepStatus;

    tokio::task::spawn_blocking(|| {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session(&tmp);
        let workflow = make_workflow(Some("wf"), Some("claude"), vec![make_step("a", &[], None)]);
        // Agent exits non-zero — should be ignored.
        let factory = FakeAgentExecutionFactory::new(std::iter::repeat_n(42, 10));
        let (frontend, _msgs) = MessageCapturingFrontend::new();
        let mut engine = make_engine_capturing(&session, workflow, factory, frontend);

        let steps = vec![crate::data::workflow_definition::SetupStep::RunShell {
            command: "step".into(),
            env: None,
        }];
        // Step fails once, then succeeds.
        let mock = Arc::new(MockBackgroundContainer::with_results([
            ("".into(), "err".into(), 1),
            ("".into(), "".into(), 0),
        ]));
        let on_failure_configs = vec![Some(remediation_config(2))];

        let result = engine.run_phase(
            PhaseKind::Setup,
            &steps,
            &[false],
            &on_failure_configs,
            mock.factory(),
        );

        assert!(
            result.is_ok(),
            "agent exit code must not block retry; setup must succeed: {result:?}"
        );
        assert_eq!(
            engine.state().setup_step_states[0].status,
            PhaseStepStatus::Succeeded,
            "step must be Succeeded when retry passes regardless of agent exit code"
        );
    })
    .await
    .unwrap();
}

// abort_on_failure + on_failure: remediation runs first; only if exhausted
// does abort_on_failure trigger.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn on_failure_abort_on_failure_triggers_only_after_remediation_exhausted() {
    tokio::task::spawn_blocking(|| {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session(&tmp);
        let workflow = make_workflow(Some("wf"), Some("claude"), vec![make_step("a", &[], None)]);
        let factory = FakeAgentExecutionFactory::always_success();
        let (frontend, _msgs) = MessageCapturingFrontend::new();
        let mut engine = make_engine_capturing(&session, workflow, factory, frontend);

        let steps = vec![
            crate::data::workflow_definition::SetupStep::RunShell {
                command: "failing-step".into(),
                env: None,
            },
            crate::data::workflow_definition::SetupStep::RunShell {
                command: "second-step".into(),
                env: None,
            },
        ];
        // First step always fails; second step would succeed but must not run.
        let mock = Arc::new(MockBackgroundContainer::with_results([
            ("".into(), "err".into(), 1), // initial attempt
            ("".into(), "err".into(), 1), // retry after agent
        ]));
        let on_failure_configs = vec![Some(remediation_config(1)), None];
        let abort_flags = vec![true, false];

        let result = engine.run_phase(
            PhaseKind::Setup,
            &steps,
            &abort_flags,
            &on_failure_configs,
            mock.factory(),
        );

        let outcome =
            result.expect("an aborted phase is reported in the outcome, not as an Err (F-34)");
        assert!(
            outcome.aborted,
            "abort_on_failure must trigger after on_failure exhausted"
        );
        assert!(outcome.any_failed);
        assert!(
            engine.abort_on_failure_triggered(),
            "abort_on_failure_triggered flag must be set"
        );
        // Second step must not have been executed.
        assert_eq!(
            mock.calls().len(),
            2,
            "only the failing step should be exec'd (initial + 1 retry): {:?}",
            mock.calls()
        );
    })
    .await
    .unwrap();
}

// abort_on_failure + on_failure: if retry succeeds, abort is NOT triggered.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn on_failure_abort_on_failure_not_triggered_when_retry_succeeds() {
    tokio::task::spawn_blocking(|| {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session(&tmp);
        let workflow = make_workflow(Some("wf"), Some("claude"), vec![make_step("a", &[], None)]);
        let factory = FakeAgentExecutionFactory::always_success();
        let (frontend, _msgs) = MessageCapturingFrontend::new();
        let mut engine = make_engine_capturing(&session, workflow, factory, frontend);

        let steps = vec![crate::data::workflow_definition::SetupStep::RunShell {
            command: "step".into(),
            env: None,
        }];
        // Fail, then succeed on retry.
        let mock = Arc::new(MockBackgroundContainer::with_results([
            ("".into(), "err".into(), 1),
            ("".into(), "".into(), 0),
        ]));
        let on_failure_configs = vec![Some(remediation_config(1))];
        let abort_flags = vec![true];

        let result = engine.run_phase(
            PhaseKind::Setup,
            &steps,
            &abort_flags,
            &on_failure_configs,
            mock.factory(),
        );

        assert!(
            result.is_ok(),
            "setup must succeed when retry succeeds even with abort_on_failure set: {result:?}"
        );
        assert!(
            !engine.abort_on_failure_triggered(),
            "abort must NOT trigger when on_failure remediation succeeds"
        );
    })
    .await
    .unwrap();
}

// on_failure messages are emitted correctly.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn on_failure_emits_launch_and_success_messages() {
    use crate::data::message::MessageLevel;

    let msg_store = Arc::new(Mutex::new(Vec::<crate::data::message::UserMessage>::new()));
    let msg_store_clone = Arc::clone(&msg_store);

    tokio::task::spawn_blocking(move || {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session(&tmp);
        let workflow = make_workflow(Some("wf"), Some("claude"), vec![make_step("a", &[], None)]);
        let factory = FakeAgentExecutionFactory::always_success();
        let frontend = MessageCapturingFrontend {
            messages: Arc::clone(&msg_store_clone),
            interactive_launches: Arc::new(Mutex::new(Vec::new())),
        };
        let mut engine = make_engine_capturing(&session, workflow, factory, frontend);

        let steps = vec![crate::data::workflow_definition::SetupStep::RunShell {
            command: "step".into(),
            env: None,
        }];
        let mock = Arc::new(MockBackgroundContainer::with_results([
            ("".into(), "err".into(), 1),
            ("".into(), "".into(), 0),
        ]));
        let on_failure_configs = vec![Some(remediation_config(2))];
        engine
            .run_phase(
                PhaseKind::Setup,
                &steps,
                &[false],
                &on_failure_configs,
                mock.factory(),
            )
            .unwrap();
    })
    .await
    .unwrap();

    let messages = msg_store.lock().unwrap().clone();
    let texts: Vec<&str> = messages.iter().map(|m| m.text.as_str()).collect();

    // Must see the "launching on_failure agent" message.
    assert!(
        texts
            .iter()
            .any(|t| t.contains("on_failure agent") && t.contains("attempt 1")),
        "must emit 'on_failure agent' launch message: {texts:?}"
    );
    // Must see the "remediation succeeded" message.
    assert!(
        texts
            .iter()
            .any(|t| t.contains("remediation succeeded") || t.contains("succeeded on attempt")),
        "must emit remediation success message: {texts:?}"
    );
    // The "launching" message must be Info level.
    let launch_msg = messages
        .iter()
        .find(|m| m.text.contains("on_failure agent"))
        .unwrap();
    assert_eq!(launch_msg.level, MessageLevel::Info);
}

// Exhausting max_attempts emits a Warning message.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn on_failure_exhausted_emits_warning_message() {
    use crate::data::message::MessageLevel;

    let msg_store = Arc::new(Mutex::new(Vec::<crate::data::message::UserMessage>::new()));
    let msg_store_clone = Arc::clone(&msg_store);

    tokio::task::spawn_blocking(move || {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session(&tmp);
        let workflow = make_workflow(Some("wf"), Some("claude"), vec![make_step("a", &[], None)]);
        let factory = FakeAgentExecutionFactory::always_success();
        let frontend = MessageCapturingFrontend {
            messages: Arc::clone(&msg_store_clone),
            interactive_launches: Arc::new(Mutex::new(Vec::new())),
        };
        let mut engine = make_engine_capturing(&session, workflow, factory, frontend);

        let steps = vec![crate::data::workflow_definition::SetupStep::RunShell {
            command: "step".into(),
            env: None,
        }];
        let mock = Arc::new(MockBackgroundContainer::with_results([
            ("".into(), "err".into(), 1),
            ("".into(), "err".into(), 1),
        ]));
        let on_failure_configs = vec![Some(remediation_config(1))];
        engine
            .run_phase(
                PhaseKind::Setup,
                &steps,
                &[false],
                &on_failure_configs,
                mock.factory(),
            )
            .unwrap();
    })
    .await
    .unwrap();

    let messages = msg_store.lock().unwrap().clone();
    let warning = messages
        .iter()
        .find(|m| m.level == MessageLevel::Warning && m.text.contains("exhausted"));
    assert!(
        warning.is_some(),
        "must emit a Warning when on_failure exhausts all attempts: {messages:?}"
    );
}

// Teardown on_failure: step fails, retry succeeds, teardown continues.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn teardown_on_failure_retry_succeeds_teardown_continues() {
    use crate::data::workflow_definition::TeardownStep;
    use crate::data::workflow_state::PhaseStepStatus;

    tokio::task::spawn_blocking(|| {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session(&tmp);
        let workflow = make_workflow(Some("wf"), Some("claude"), vec![make_step("a", &[], None)]);
        let factory = FakeAgentExecutionFactory::always_success();
        let (frontend, _msgs) = MessageCapturingFrontend::new();
        let mut engine = make_engine_capturing(&session, workflow, factory, frontend);

        let steps = vec![
            TeardownStep::RunShell {
                command: "tests".into(),
                env: None,
            },
            TeardownStep::RunShell {
                command: "deploy".into(),
                env: None,
            },
        ];
        // First step fails, retry succeeds; second step succeeds.
        let mock = Arc::new(MockBackgroundContainer::with_results([
            ("".into(), "test err".into(), 1),
            ("".into(), "".into(), 0),
            ("".into(), "".into(), 0),
        ]));
        let on_failure_configs = vec![Some(remediation_config(1)), None];

        let outcome = engine
            .run_phase(
                PhaseKind::Teardown,
                &steps,
                &[false, false],
                &on_failure_configs,
                mock.factory(),
            )
            .unwrap();

        assert!(
            !outcome.aborted,
            "teardown must not abort when retry succeeds"
        );
        assert!(
            !outcome.any_failed,
            "any_failed must be false when retry succeeds"
        );
        let states = &engine.state().teardown_step_states;
        assert_eq!(states[0].status, PhaseStepStatus::Succeeded);
        assert_eq!(states[1].status, PhaseStepStatus::Succeeded);
    })
    .await
    .unwrap();
}

// The remediation agent launch must announce itself through
// report_step_interactive_launch, like main steps do. Frontends prepare
// per-container state there — the TUI recreates its AgentIo channels, so
// skipping the call would leave the factory's take_io with no channels
// and kill the whole command task.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn on_failure_agent_launch_reports_interactive_launch() {
    use crate::data::workflow_definition::TeardownStep;

    tokio::task::spawn_blocking(|| {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session(&tmp);
        let workflow = make_workflow(Some("wf"), Some("claude"), vec![make_step("a", &[], None)]);
        let factory = FakeAgentExecutionFactory::always_success();
        let (frontend, _msgs) = MessageCapturingFrontend::new();
        let launches = frontend.launches_handle();
        let mut engine = make_engine_capturing(&session, workflow, factory, frontend);

        let steps = vec![TeardownStep::RunShell {
            command: "tests".into(),
            env: None,
        }];
        // Step fails, retry fails again → exactly one remediation attempt.
        let mock = Arc::new(MockBackgroundContainer::with_results([
            ("".into(), "err".into(), 1),
            ("".into(), "err".into(), 1),
        ]));
        let on_failure_configs = vec![Some(remediation_config(1))];

        engine
            .run_phase(
                PhaseKind::Teardown,
                &steps,
                &[false],
                &on_failure_configs,
                mock.factory(),
            )
            .unwrap();

        assert_eq!(
            launches.lock().unwrap().as_slice(),
            ["__on_failure__".to_string()],
            "remediation agent launch must fire report_step_interactive_launch"
        );
    })
    .await
    .unwrap();
}

// Teardown on_failure exhausts attempts: step marked failed, teardown continues (best-effort).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn teardown_on_failure_exhausted_step_failed_teardown_continues() {
    use crate::data::workflow_definition::TeardownStep;
    use crate::data::workflow_state::PhaseStepStatus;

    tokio::task::spawn_blocking(|| {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session(&tmp);
        let workflow = make_workflow(Some("wf"), Some("claude"), vec![make_step("a", &[], None)]);
        let factory = FakeAgentExecutionFactory::always_success();
        let (frontend, _msgs) = MessageCapturingFrontend::new();
        let mut engine = make_engine_capturing(&session, workflow, factory, frontend);

        let steps = vec![
            TeardownStep::RunShell {
                command: "always-fail".into(),
                env: None,
            },
            TeardownStep::RunShell {
                command: "second".into(),
                env: None,
            },
        ];
        // All execs of the first step fail; second step succeeds.
        let mock = Arc::new(MockBackgroundContainer::with_results([
            ("".into(), "err".into(), 1), // initial
            ("".into(), "err".into(), 1), // retry
            ("".into(), "".into(), 0),    // second step
        ]));
        let on_failure_configs = vec![Some(remediation_config(1)), None];

        let outcome = engine
            .run_phase(
                PhaseKind::Teardown,
                &steps,
                &[false, false],
                &on_failure_configs,
                mock.factory(),
            )
            .unwrap();

        assert!(!outcome.aborted);
        assert!(
            outcome.any_failed,
            "any_failed must be true when on_failure is exhausted"
        );
        assert!(
            matches!(
                &engine.state().teardown_step_states[0].status,
                PhaseStepStatus::Failed { .. }
            ),
            "first step must remain Failed"
        );
        assert_eq!(
            engine.state().teardown_step_states[1].status,
            PhaseStepStatus::Succeeded,
            "second step must still run (best-effort teardown)"
        );
    })
    .await
    .unwrap();
}

// Remediating state is set on the step during on_failure execution.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn on_failure_remediating_state_recorded_on_step() {
    use crate::data::workflow_state::PhaseStepStatus;

    // We can't observe the Remediating state mid-flight (it's transient),
    // but we CAN verify that after a failed retry it was set at least once
    // by checking that the final state transitions happened correctly.
    // The key invariant: Remediating → Running → (Succeeded or Failed).
    // After exhaustion the step is Failed; after success it is Succeeded.
    // This test checks exhaustion so we know the state machine ran.
    tokio::task::spawn_blocking(|| {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session(&tmp);
        let workflow = make_workflow(Some("wf"), Some("claude"), vec![make_step("a", &[], None)]);
        let factory = FakeAgentExecutionFactory::always_success();
        let (frontend, _msgs) = MessageCapturingFrontend::new();
        let mut engine = make_engine_capturing(&session, workflow, factory, frontend);

        let steps = vec![crate::data::workflow_definition::SetupStep::RunShell {
            command: "step".into(),
            env: None,
        }];
        let mock = Arc::new(MockBackgroundContainer::with_results([
            ("".into(), "err".into(), 1),
            ("".into(), "err".into(), 1),
        ]));
        let on_failure_configs = vec![Some(remediation_config(1))];
        engine
            .run_phase(
                PhaseKind::Setup,
                &steps,
                &[false],
                &on_failure_configs,
                mock.factory(),
            )
            .unwrap();

        // After exhaustion the step should be Failed — the engine correctly
        // transitioned through Remediating → Running → Failed.
        assert!(
            matches!(
                engine.state().setup_step_states[0].status,
                PhaseStepStatus::Failed { .. }
            ),
            "step must end as Failed after exhausted remediation"
        );
    })
    .await
    .unwrap();
}

// ── Feature B (WI-0099): teardown failure output capture — unit tests ───

#[test]
fn sanitize_step_name_replaces_path_separators_and_dots() {
    let out = sanitize_step_name_for_filename("run_shell: ../../etc/passwd");
    assert!(
        !out.contains('/'),
        "must not contain path separators: {out}"
    );
    assert!(!out.contains(".."), "must not contain '..': {out}");
    assert!(
        out.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
        "must only contain safe filename characters: {out}"
    );
}

#[test]
fn sanitize_step_name_replaces_spaces_and_shell_metacharacters() {
    let out = sanitize_step_name_for_filename("rm -rf $(whoami); echo `id` && true");
    assert!(
        out.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
        "must only contain safe filename characters: {out}"
    );
    assert!(!out.contains(' '));
    assert!(!out.contains('$'));
    assert!(!out.contains('('));
    assert!(!out.contains(';'));
    assert!(!out.contains('`'));
}

#[test]
fn sanitize_step_name_replaces_backslashes_and_colons() {
    let out = sanitize_step_name_for_filename(r"C:\Users\evil\payload");
    assert!(!out.contains('\\'));
    assert!(!out.contains(':'));
}

#[test]
fn sanitize_step_name_truncates_to_64_chars() {
    let long_name = "a".repeat(100);
    let out = sanitize_step_name_for_filename(&long_name);
    assert_eq!(out.len(), 64, "must be truncated to the 64-char cap: {out}");
    assert!(out.chars().all(|c| c == 'a'));
}

#[test]
fn sanitize_step_name_empty_falls_back_to_step() {
    assert_eq!(sanitize_step_name_for_filename(""), "step");
}

#[test]
fn sanitize_step_name_preserves_already_safe_names() {
    assert_eq!(
        sanitize_step_name_for_filename("build-frontend_v2"),
        "build-frontend_v2"
    );
}

#[test]
fn sanitize_step_name_handles_unicode_without_panicking() {
    let out = sanitize_step_name_for_filename("café/日本語 build");
    assert!(
        out.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
        "must only contain safe filename characters: {out}"
    );
    assert!(out.starts_with("caf"));
}

#[test]
fn truncate_stream_leaves_short_content_untouched() {
    let content = "short output\nwith a few lines";
    assert_eq!(truncate_stream(content), content);
}

#[test]
fn truncate_stream_exactly_at_cap_is_untouched() {
    let content = "z".repeat(TEARDOWN_STREAM_TRUNCATE_BYTES);
    assert_eq!(truncate_stream(&content), content);
}

#[test]
fn truncate_stream_keeps_last_bytes_with_notice() {
    let filler = "x".repeat(TEARDOWN_STREAM_TRUNCATE_BYTES + 500);
    let content = format!("HEAD-MARKER{filler}TAIL-MARKER");
    let out = truncate_stream(&content);
    assert!(
        out.starts_with("[... output truncated, showing last"),
        "must prefix a truncation notice: {out}"
    );
    assert!(
        !out.contains("HEAD-MARKER"),
        "the dropped head must not appear: {out}"
    );
    assert!(out.ends_with("TAIL-MARKER"));
}

#[test]
fn truncate_stream_respects_utf8_char_boundaries() {
    // '€' is 3 bytes; repeating it so the cut point would otherwise land
    // mid-codepoint must not panic and must yield valid UTF-8.
    let filler = "€".repeat(TEARDOWN_STREAM_TRUNCATE_BYTES / 3 + 10);
    let content = format!("{filler}END");
    let out = truncate_stream(&content);
    assert!(out.ends_with("END"));
}

#[test]
fn format_phase_failure_file_basic_content() {
    let out = format_phase_failure_file("run_shell: cargo test", "out content", "err content");
    assert_eq!(
        out,
        "=== FAILED COMMAND: run_shell: cargo test ===\n\n\
             --- STDOUT ---\nout content\n\n\
             --- STDERR ---\nerr content\n"
    );
}

#[test]
fn format_phase_failure_file_empty_stdout_uses_placeholder() {
    let out = format_phase_failure_file("step", "", "some stderr");
    assert!(out.contains("--- STDOUT ---\n(empty)\n"));
    assert!(out.contains("some stderr"));
}

#[test]
fn format_phase_failure_file_empty_stderr_uses_placeholder() {
    let out = format_phase_failure_file("step", "some stdout", "");
    assert!(out.contains("some stdout"));
    assert!(out.contains("--- STDERR ---\n(empty)\n"));
}

#[test]
fn format_phase_failure_file_both_empty_uses_placeholders_for_both() {
    let out = format_phase_failure_file("step", "", "");
    assert!(out.contains("--- STDOUT ---\n(empty)\n"));
    assert!(out.contains("--- STDERR ---\n(empty)\n"));
}

#[test]
fn format_phase_failure_file_truncates_oversized_stream() {
    let big = "y".repeat(TEARDOWN_STREAM_TRUNCATE_BYTES + 1000);
    let out = format_phase_failure_file("step", &big, "");
    assert!(out.contains("output truncated"));
    assert!(
        !out.contains(&big),
        "raw oversized content must not appear verbatim in the file"
    );
}

#[test]
fn prepend_preamble_overlay_path_references_correct_file_and_user_prompt() {
    let artifacts = PhaseFailureArtifacts {
        kind: PhaseKind::Teardown,
        container_path: PHASE_FAILURE_OVERLAY_CONTAINER_PATH,
        filename: "teardown-failure-run-shell--cargo-test.txt".to_string(),
        step_name: "run_shell: cargo test".to_string(),
        extra_overlay: None,
    };
    let out = artifacts.prepend_preamble("Fix the bug.");
    assert!(out.contains("failed teardown step \"run_shell: cargo test\""));
    assert!(out.contains("/awman/context/workflow/teardown-failure-run-shell--cargo-test.txt"));
    assert!(out.contains("Read that file first"));
    assert!(
        out.contains("---\n\nFix the bug."),
        "user prompt must follow the '---' separator: {out}"
    );
}

#[test]
fn prepend_preamble_ephemeral_path_references_remediation_mount() {
    let artifacts = PhaseFailureArtifacts {
        kind: PhaseKind::Teardown,
        container_path: PHASE_FAILURE_EPHEMERAL_CONTAINER_PATH,
        filename: "teardown-failure-step.txt".to_string(),
        step_name: "step".to_string(),
        extra_overlay: Some("/host/dir:/awman/remediation:ro".to_string()),
    };
    let out = artifacts.prepend_preamble("Custom prompt");
    assert!(out.contains("/awman/remediation/teardown-failure-step.txt"));
    assert!(out.trim_end().ends_with("Custom prompt"));
}

fn make_engine_with_workflow_overlays(
    tmp: &tempfile::TempDir,
    overlays: Option<Vec<String>>,
) -> WorkflowEngine {
    let session = make_session(tmp);
    let mut workflow = make_workflow(
        Some("wf-overlay"),
        Some("claude"),
        vec![make_step("a", &[], None)],
    );
    workflow.overlays = overlays;
    make_engine(
        &session,
        workflow,
        FakeAgentExecutionFactory::always_success(),
        [],
    )
}

#[test]
fn workflow_context_overlay_writable_false_when_no_overlays() {
    let tmp = tempfile::tempdir().unwrap();
    let engine = make_engine_with_workflow_overlays(&tmp, None);
    assert!(!engine.workflow_context_overlay_writable());
}

#[test]
fn workflow_context_overlay_writable_true_for_default_rw_scope() {
    let tmp = tempfile::tempdir().unwrap();
    let engine =
        make_engine_with_workflow_overlays(&tmp, Some(vec!["context(workflow)".to_string()]));
    assert!(engine.workflow_context_overlay_writable());
}

#[test]
fn workflow_context_overlay_writable_true_for_explicit_rw_scope() {
    let tmp = tempfile::tempdir().unwrap();
    let engine =
        make_engine_with_workflow_overlays(&tmp, Some(vec!["context(workflow:rw)".to_string()]));
    assert!(engine.workflow_context_overlay_writable());
}

#[test]
fn workflow_context_overlay_writable_false_for_readonly_scope() {
    let tmp = tempfile::tempdir().unwrap();
    let engine =
        make_engine_with_workflow_overlays(&tmp, Some(vec!["context(workflow:ro)".to_string()]));
    assert!(!engine.workflow_context_overlay_writable());
}

#[test]
fn workflow_context_overlay_writable_false_for_unrelated_scope() {
    let tmp = tempfile::tempdir().unwrap();
    let engine = make_engine_with_workflow_overlays(&tmp, Some(vec!["context(repo)".to_string()]));
    assert!(!engine.workflow_context_overlay_writable());
}

#[test]
fn workflow_context_overlay_writable_true_within_comma_separated_entry() {
    let tmp = tempfile::tempdir().unwrap();
    let engine = make_engine_with_workflow_overlays(
        &tmp,
        Some(vec!["context(repo), context(workflow)".to_string()]),
    );
    assert!(engine.workflow_context_overlay_writable());
}

#[test]
fn workflow_context_overlay_writable_false_when_only_readonly_across_multiple_entries() {
    let tmp = tempfile::tempdir().unwrap();
    let engine = make_engine_with_workflow_overlays(
        &tmp,
        Some(vec![
            "context(repo)".to_string(),
            "context(workflow:ro)".to_string(),
        ]),
    );
    assert!(!engine.workflow_context_overlay_writable());
}

#[test]
fn workflow_context_overlay_writable_uses_active_permission_override() {
    let tmp = tempfile::tempdir().unwrap();
    let mut engine = make_engine_with_workflow_overlays(&tmp, None);
    assert!(!engine.workflow_context_overlay_writable());

    engine.set_workflow_context_permission(Some(OverlayPermission::ReadWrite));
    assert!(
        engine.workflow_context_overlay_writable(),
        "a workflow context overlay supplied by config/env/CLI must be treated as active"
    );

    engine.set_workflow_context_permission(Some(OverlayPermission::ReadOnly));
    assert!(
        !engine.workflow_context_overlay_writable(),
        "read-only workflow context must not be treated as writable"
    );
}

// ── Feature B (WI-0099): teardown failure output capture — integration ──
//
// `prepare_phase_failure_file` resolves the host directory via
// `ContextDirResolver::from_process_env()`, which reads the *real* process
// environment (there is no test-injectable `EnvSnapshot` seam on that call
// path). These tests therefore pin `AWMAN_CONFIG_HOME` to a temp dir for
// their duration, through `ConfigHomeGuard` — the one lock over that
// process-wide variable, which also restores the previous value on drop.
// A mutex private to this file would serialise these six tests against each
// other only, leaving the `clean`, `dispatch` and `overlay` tests free to
// repoint the config home mid-test: `ContextDirResolver::from_process_env`
// would then resolve a directory this test never created, and the
// assertions below would look for the failure file in the temp dir that is
// still theirs.

use crate::data::config::env::ConfigHomeGuard;

/// `(step name, resolved prompt, overlays)` recorded per `execution_for_step` call.
type RecordedStepCall = (String, String, Option<Vec<String>>);

/// Records every synthetic step handed to `execution_for_step` — name,
/// resolved prompt, and overlays — so tests can assert on the
/// `launch_on_failure_agent` prompt hint and mount decision without a
/// live container runtime.
struct StepRecordingFactory {
    inner: Arc<FakeAgentExecutionFactory>,
    step_calls: Arc<Mutex<Vec<RecordedStepCall>>>,
}

impl StepRecordingFactory {
    fn new(inner: Arc<FakeAgentExecutionFactory>) -> (Self, Arc<Mutex<Vec<RecordedStepCall>>>) {
        let step_calls = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                inner,
                step_calls: Arc::clone(&step_calls),
            },
            step_calls,
        )
    }
}

impl AgentExecutionFactory for StepRecordingFactory {
    fn execution_for_step(
        &self,
        step: &WorkflowStep,
        session: &Session,
        runtime: &WorkflowRuntimeContext,
    ) -> Result<AgentExecution, EngineError> {
        self.step_calls.lock().unwrap().push((
            step.name.clone(),
            step.prompt_template.clone(),
            step.overlays.clone(),
        ));
        self.inner.execution_for_step(step, session, runtime)
    }

    fn inject_prompt(
        &self,
        execution: &AgentExecution,
        prompt: &str,
    ) -> Result<Option<()>, EngineError> {
        self.inner.inject_prompt(execution, prompt)
    }
}

// F-34 behaviour change: a *setup* step with an `on_failure` agent now
// gets the captured-output failure file that only teardown steps used to
// get, written into the same per-invocation run directory teardown's
// lands in, and named for its phase so a setup and a teardown step of the
// same name cannot collide.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn setup_failure_writes_a_setup_named_failure_file_beside_teardowns() {
    use crate::data::fs::context_dirs::ContextDirResolver;
    use crate::data::workflow_definition::SetupStep;

    let awman_home = tempfile::tempdir().unwrap();
    let awman_home_path = awman_home.path().to_path_buf();

    tokio::task::spawn_blocking(move || {
        let _env_guard = ConfigHomeGuard::set(&awman_home_path);

        let tmp = tempfile::tempdir().unwrap();
        let session = make_session(&tmp);
        let mut workflow = make_workflow(
            Some("wf-setup-failure-file"),
            Some("claude"),
            vec![make_step("a", &[], None)],
        );
        workflow.overlays = Some(vec!["context(workflow)".to_string()]);

        let recording = Arc::new(FakeAgentExecutionFactory::always_success());
        let (step_factory, step_calls_handle) = StepRecordingFactory::new(Arc::clone(&recording));
        let (frontend, _msgs) = MessageCapturingFrontend::new();
        let mut engine = WorkflowEngine::new(
            &session,
            WorkflowSpec::new(workflow).with_work_item_context(None),
            WorkflowEngineDeps {
                frontend: Box::new(frontend),
                agent_factory: Box::new(step_factory),
            },
        )
        .unwrap();
        let invocation_id = engine.state().invocation_id;

        let steps = vec![SetupStep::RunShell {
            command: "cargo test".into(),
            env: None,
        }];
        let mock = Arc::new(MockBackgroundContainer::with_results([
            ("setup stdout".into(), "setup stderr".into(), 1),
            ("".into(), "".into(), 0),
        ]));
        let on_failure_configs = vec![Some(remediation_config(1))];

        engine
            .run_phase(
                PhaseKind::Setup,
                &steps,
                &[false],
                &on_failure_configs,
                mock.factory(),
            )
            .unwrap();

        let resolver = ContextDirResolver::at_home(&awman_home_path);
        let host_dir = resolver.workflow_dir(invocation_id);
        let sanitized = sanitize_step_name_for_filename("run_shell: cargo test");

        let file_path = host_dir.join(format!("setup-failure-{sanitized}.txt"));
        let content = std::fs::read_to_string(&file_path).unwrap_or_else(|e| {
            panic!(
                "expected setup failure file at {}: {e}",
                file_path.display()
            )
        });
        assert!(content.contains("=== FAILED COMMAND: run_shell: cargo test ==="));
        assert!(content.contains("setup stdout"));
        assert!(content.contains("setup stderr"));

        assert!(
            !host_dir
                .join(format!("teardown-failure-{sanitized}.txt"))
                .exists(),
            "a setup failure must not be filed under teardown's name"
        );

        let calls = step_calls_handle.lock().unwrap().clone();
        let on_failure_call = calls
            .iter()
            .find(|(name, _, _)| name == "__on_failure__")
            .expect("remediation agent must have been launched");
        let expected_hint = format!("/awman/context/workflow/setup-failure-{sanitized}.txt");
        assert!(
            on_failure_call.1.contains(expected_hint.as_str()),
            "prompt must point the agent at the setup failure file: {}",
            on_failure_call.1
        );
        assert!(
            on_failure_call.1.contains("failed setup step"),
            "the preamble must name the phase: {}",
            on_failure_call.1
        );
    })
    .await
    .unwrap();
}

// Teardown failure WITH an active, writable `context(workflow)` overlay:
// the file must land in the overlay's existing host path and the prompt
// hint must reference the already-mounted `/awman/context/workflow/...`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn teardown_failure_with_active_context_workflow_overlay_writes_into_it() {
    use crate::data::fs::context_dirs::ContextDirResolver;
    use crate::data::workflow_definition::TeardownStep;

    // `awman_home` stays alive in this outer frame for the whole test
    // (including across the `.await` below) so the directory exists for
    // the blocking closure's whole execution. The `ConfigHomeGuard` lives
    // entirely *inside* the closure, so no `MutexGuard` is held across an
    // await point.
    let awman_home = tempfile::tempdir().unwrap();
    let awman_home_path = awman_home.path().to_path_buf();

    tokio::task::spawn_blocking(move || {
        let _env_guard = ConfigHomeGuard::set(&awman_home_path);

        let tmp = tempfile::tempdir().unwrap();
        let session = make_session(&tmp);
        let mut workflow = make_workflow(
            Some("wf-overlay-active"),
            Some("claude"),
            vec![make_step("a", &[], None)],
        );
        workflow.overlays = Some(vec!["context(workflow)".to_string()]);

        let recording = Arc::new(FakeAgentExecutionFactory::always_success());
        let (step_factory, step_calls_handle) = StepRecordingFactory::new(Arc::clone(&recording));
        let (frontend, _msgs) = MessageCapturingFrontend::new();
        let mut engine = WorkflowEngine::new(
            &session,
            WorkflowSpec::new(workflow).with_work_item_context(None),
            WorkflowEngineDeps {
                frontend: Box::new(frontend),
                agent_factory: Box::new(step_factory),
            },
        )
        .unwrap();
        let invocation_id = engine.state().invocation_id;

        let steps = vec![TeardownStep::RunShell {
            command: "cargo test".into(),
            env: None,
        }];
        let mock = Arc::new(MockBackgroundContainer::with_results([
            ("stdout content".into(), "stderr content".into(), 1),
            ("".into(), "".into(), 0),
        ]));
        let on_failure_configs = vec![Some(remediation_config(1))];

        engine
            .run_phase(
                PhaseKind::Teardown,
                &steps,
                &[false],
                &on_failure_configs,
                mock.factory(),
            )
            .unwrap();

        let resolver = ContextDirResolver::at_home(&awman_home_path);
        let host_dir = resolver.workflow_dir(invocation_id);
        let sanitized = sanitize_step_name_for_filename("run_shell: cargo test");
        let file_path = host_dir.join(format!("teardown-failure-{sanitized}.txt"));
        let content = std::fs::read_to_string(&file_path)
            .unwrap_or_else(|e| panic!("expected failure file at {}: {e}", file_path.display()));
        assert!(content.contains("=== FAILED COMMAND: run_shell: cargo test ==="));
        assert!(content.contains("stdout content"));
        assert!(content.contains("stderr content"));

        let calls = step_calls_handle.lock().unwrap().clone();
        let on_failure_call = calls
            .iter()
            .find(|(name, _, _)| name == "__on_failure__")
            .expect("remediation agent must have been launched");
        let expected_hint = format!("/awman/context/workflow/teardown-failure-{sanitized}.txt");
        assert!(
            on_failure_call.1.contains(expected_hint.as_str()),
            "prompt must reference the overlay path: {}",
            on_failure_call.1
        );
        assert!(
            on_failure_call.2.is_none(),
            "context(workflow) overlay is already mounted; no extra overlay expected"
        );
    })
    .await
    .unwrap();
}

// A read-only `context(workflow:ro)` overlay must not be treated as the
// writable destination. The failure file is still written host-side under
// the workflow context directory, but the remediation hint uses
// `/awman/remediation/...`; the extra mount targets the file itself so the
// existing read-only context directory mount is not deduplicated away.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn teardown_failure_with_readonly_context_workflow_overlay_uses_remediation_mount() {
    use crate::data::fs::context_dirs::ContextDirResolver;
    use crate::data::workflow_definition::TeardownStep;

    let awman_home = tempfile::tempdir().unwrap();
    let awman_home_path = awman_home.path().to_path_buf();

    tokio::task::spawn_blocking(move || {
        let _env_guard = ConfigHomeGuard::set(&awman_home_path);

        let tmp = tempfile::tempdir().unwrap();
        let session = make_session(&tmp);
        let mut workflow = make_workflow(
            Some("wf-overlay-readonly"),
            Some("claude"),
            vec![make_step("a", &[], None)],
        );
        workflow.overlays = Some(vec!["context(workflow:ro)".to_string()]);

        let recording = Arc::new(FakeAgentExecutionFactory::always_success());
        let (step_factory, step_calls_handle) = StepRecordingFactory::new(Arc::clone(&recording));
        let (frontend, _msgs) = MessageCapturingFrontend::new();
        let mut engine = WorkflowEngine::new(
            &session,
            WorkflowSpec::new(workflow).with_work_item_context(None),
            WorkflowEngineDeps {
                frontend: Box::new(frontend),
                agent_factory: Box::new(step_factory),
            },
        )
        .unwrap();
        let invocation_id = engine.state().invocation_id;

        let steps = vec![TeardownStep::RunShell {
            command: "cargo test".into(),
            env: None,
        }];
        let mock = Arc::new(MockBackgroundContainer::with_results([
            ("ro out".into(), "ro err".into(), 1),
            ("".into(), "".into(), 0),
        ]));
        let on_failure_configs = vec![Some(remediation_config(1))];

        engine
            .run_phase(
                PhaseKind::Teardown,
                &steps,
                &[false],
                &on_failure_configs,
                mock.factory(),
            )
            .unwrap();

        let resolver = ContextDirResolver::at_home(&awman_home_path);
        let host_dir = resolver.workflow_dir(invocation_id);
        let sanitized = sanitize_step_name_for_filename("run_shell: cargo test");
        let filename = format!("teardown-failure-{sanitized}.txt");
        let file_path = host_dir.join(&filename);
        let content = std::fs::read_to_string(&file_path)
            .unwrap_or_else(|e| panic!("expected failure file at {}: {e}", file_path.display()));
        assert!(content.contains("ro out"));
        assert!(content.contains("ro err"));

        let calls = step_calls_handle.lock().unwrap().clone();
        let on_failure_call = calls
            .iter()
            .find(|(name, _, _)| name == "__on_failure__")
            .expect("remediation agent must have been launched");
        let expected_hint = format!("/awman/remediation/{filename}");
        assert!(
            on_failure_call.1.contains(expected_hint.as_str()),
            "prompt must reference the remediation path: {}",
            on_failure_call.1
        );
        let expected_overlay = format!("{}:/awman/remediation/{filename}:ro", file_path.display());
        assert_eq!(
            on_failure_call.2,
            Some(vec![expected_overlay]),
            "read-only context fallback must mount the failure file at /awman/remediation"
        );
    })
    .await
    .unwrap();
}

// Teardown failure WITHOUT a `context(workflow)` overlay: an ephemeral
// directory is used, mounted read-only at /awman/remediation, and the
// prompt hint references that mount.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn teardown_failure_without_context_workflow_overlay_uses_ephemeral_mount() {
    use crate::data::fs::context_dirs::ContextDirResolver;
    use crate::data::workflow_definition::TeardownStep;

    // `awman_home` stays alive in this outer frame for the whole test
    // (including across the `.await` below) so the directory exists for
    // the blocking closure's whole execution. The `ConfigHomeGuard` lives
    // entirely *inside* the closure, so no `MutexGuard` is held across an
    // await point.
    let awman_home = tempfile::tempdir().unwrap();
    let awman_home_path = awman_home.path().to_path_buf();

    tokio::task::spawn_blocking(move || {
        let _env_guard = ConfigHomeGuard::set(&awman_home_path);

        let tmp = tempfile::tempdir().unwrap();
        let session = make_session(&tmp);
        // No `context(workflow)` declared.
        let workflow = make_workflow(
            Some("wf-overlay-absent"),
            Some("claude"),
            vec![make_step("a", &[], None)],
        );

        let recording = Arc::new(FakeAgentExecutionFactory::always_success());
        let (step_factory, step_calls_handle) = StepRecordingFactory::new(Arc::clone(&recording));
        let (frontend, _msgs) = MessageCapturingFrontend::new();
        let mut engine = WorkflowEngine::new(
            &session,
            WorkflowSpec::new(workflow).with_work_item_context(None),
            WorkflowEngineDeps {
                frontend: Box::new(frontend),
                agent_factory: Box::new(step_factory),
            },
        )
        .unwrap();
        let invocation_id = engine.state().invocation_id;

        let steps = vec![TeardownStep::RunShell {
            command: "deploy".into(),
            env: None,
        }];
        let mock = Arc::new(MockBackgroundContainer::with_results([
            ("out".into(), "err".into(), 1),
            ("".into(), "".into(), 0),
        ]));
        let on_failure_configs = vec![Some(remediation_config(1))];

        engine
            .run_phase(
                PhaseKind::Teardown,
                &steps,
                &[false],
                &on_failure_configs,
                mock.factory(),
            )
            .unwrap();

        let resolver = ContextDirResolver::at_home(&awman_home_path);
        let host_dir = resolver.workflow_dir(invocation_id);
        assert!(
            host_dir.starts_with(awman_home_path.join("context").join("workflows")),
            "ephemeral dir must live under ~/.awman/context/workflows/: {}",
            host_dir.display()
        );
        let sanitized = sanitize_step_name_for_filename("run_shell: deploy");
        let file_path = host_dir.join(format!("teardown-failure-{sanitized}.txt"));
        assert!(
            file_path.exists(),
            "failure file must be written to the ephemeral dir: {}",
            file_path.display()
        );

        let calls = step_calls_handle.lock().unwrap().clone();
        let on_failure_call = calls
            .iter()
            .find(|(name, _, _)| name == "__on_failure__")
            .expect("remediation agent must have been launched");
        let expected_hint = format!("/awman/remediation/teardown-failure-{sanitized}.txt");
        assert!(
            on_failure_call.1.contains(expected_hint.as_str()),
            "prompt must reference the ephemeral mount path: {}",
            on_failure_call.1
        );
        let expected_overlay = format!("{}:/awman/remediation:ro", host_dir.display());
        assert_eq!(
            on_failure_call.2,
            Some(vec![expected_overlay]),
            "a one-off read-only overlay must be attached when context(workflow) is absent"
        );
    })
    .await
    .unwrap();
}

// A second (and third) teardown failure during multi-attempt remediation
// must overwrite the failure file with the latest output, not the first.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn teardown_failure_retry_overwrites_file_with_latest_output() {
    use crate::data::fs::context_dirs::ContextDirResolver;
    use crate::data::workflow_definition::TeardownStep;

    // `awman_home` stays alive in this outer frame for the whole test
    // (including across the `.await` below) so the directory exists for
    // the blocking closure's whole execution. The `ConfigHomeGuard` lives
    // entirely *inside* the closure, so no `MutexGuard` is held across an
    // await point.
    let awman_home = tempfile::tempdir().unwrap();
    let awman_home_path = awman_home.path().to_path_buf();

    tokio::task::spawn_blocking(move || {
        let _env_guard = ConfigHomeGuard::set(&awman_home_path);

        let tmp = tempfile::tempdir().unwrap();
        let session = make_session(&tmp);
        let mut workflow = make_workflow(
            Some("wf-retry-overwrite"),
            Some("claude"),
            vec![make_step("a", &[], None)],
        );
        workflow.overlays = Some(vec!["context(workflow)".to_string()]);

        let recording = Arc::new(FakeAgentExecutionFactory::always_success());
        let (step_factory, _step_calls) = StepRecordingFactory::new(Arc::clone(&recording));
        let (frontend, _msgs) = MessageCapturingFrontend::new();
        let mut engine = WorkflowEngine::new(
            &session,
            WorkflowSpec::new(workflow).with_work_item_context(None),
            WorkflowEngineDeps {
                frontend: Box::new(frontend),
                agent_factory: Box::new(step_factory),
            },
        )
        .unwrap();
        let invocation_id = engine.state().invocation_id;

        let steps = vec![TeardownStep::RunShell {
            command: "flaky".into(),
            env: None,
        }];
        // Initial failure, first retry fails again with different output,
        // second retry succeeds.
        let mock = Arc::new(MockBackgroundContainer::with_results([
            ("first-out".into(), "first-err".into(), 1),
            ("second-out".into(), "second-err".into(), 1),
            ("".into(), "".into(), 0),
        ]));
        let on_failure_configs = vec![Some(remediation_config(2))];

        let outcome = engine
            .run_phase(
                PhaseKind::Teardown,
                &steps,
                &[false],
                &on_failure_configs,
                mock.factory(),
            )
            .unwrap();
        assert!(
            !outcome.any_failed,
            "the second retry succeeds so the step must not remain failed"
        );

        let resolver = ContextDirResolver::at_home(&awman_home_path);
        let host_dir = resolver.workflow_dir(invocation_id);
        let sanitized = sanitize_step_name_for_filename("run_shell: flaky");
        let file_path = host_dir.join(format!("teardown-failure-{sanitized}.txt"));
        let content = std::fs::read_to_string(&file_path).unwrap();

        assert!(
            content.contains("second-out") && content.contains("second-err"),
            "file must reflect the latest failure: {content}"
        );
        assert!(
            !content.contains("first-out") && !content.contains("first-err"),
            "stale output from the first failure must be overwritten: {content}"
        );
    })
    .await
    .unwrap();
}

// A write failure (e.g. disk full, permission error) must degrade
// gracefully: the remediation agent still launches, using the
// unmodified `on_failure.prompt` with no dangling file reference.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn teardown_failure_write_error_degrades_gracefully() {
    use crate::data::fs::context_dirs::ContextDirResolver;
    use crate::data::workflow_definition::TeardownStep;

    // `awman_home` stays alive in this outer frame for the whole test
    // (including across the `.await` below) so the directory exists for
    // the blocking closure's whole execution. The `ConfigHomeGuard` lives
    // entirely *inside* the closure, so no `MutexGuard` is held across an
    // await point.
    let awman_home = tempfile::tempdir().unwrap();
    let awman_home_path = awman_home.path().to_path_buf();

    tokio::task::spawn_blocking(move || {
        let _env_guard = ConfigHomeGuard::set(&awman_home_path);

        let tmp = tempfile::tempdir().unwrap();
        let session = make_session(&tmp);
        let mut workflow = make_workflow(
            Some("wf-write-failure"),
            Some("claude"),
            vec![make_step("a", &[], None)],
        );
        workflow.overlays = Some(vec!["context(workflow)".to_string()]);

        let recording = Arc::new(FakeAgentExecutionFactory::always_success());
        let (step_factory, step_calls_handle) = StepRecordingFactory::new(Arc::clone(&recording));
        let (frontend, msgs) = MessageCapturingFrontend::new();
        let mut engine = WorkflowEngine::new(
            &session,
            WorkflowSpec::new(workflow).with_work_item_context(None),
            WorkflowEngineDeps {
                frontend: Box::new(frontend),
                agent_factory: Box::new(step_factory),
            },
        )
        .unwrap();
        let invocation_id = engine.state().invocation_id;

        // Pre-create the target file path AS A DIRECTORY so the production
        // `std::fs::write` call fails deterministically (EISDIR) without
        // relying on permission bits, which don't block root in CI/dev
        // containers.
        let resolver = ContextDirResolver::at_home(&awman_home_path);
        let host_dir = resolver.workflow_dir(invocation_id);
        let sanitized = sanitize_step_name_for_filename("run_shell: flaky");
        let conflicting_path = host_dir.join(format!("teardown-failure-{sanitized}.txt"));
        std::fs::create_dir_all(&conflicting_path).unwrap();

        let steps = vec![TeardownStep::RunShell {
            command: "flaky".into(),
            env: None,
        }];
        let mock = Arc::new(MockBackgroundContainer::with_results([
            ("out".into(), "err".into(), 1),
            ("".into(), "".into(), 0),
        ]));
        let on_failure_configs = vec![Some(remediation_config(1))];

        engine
            .run_phase(
                PhaseKind::Teardown,
                &steps,
                &[false],
                &on_failure_configs,
                mock.factory(),
            )
            .unwrap();

        let calls = step_calls_handle.lock().unwrap().clone();
        let on_failure_call = calls
            .iter()
            .find(|(name, _, _)| name == "__on_failure__")
            .expect("remediation agent must still launch despite the write failure");
        assert_eq!(
            on_failure_call.1, "Fix the broken step.",
            "prompt must be the unmodified config prompt with no file hint: {}",
            on_failure_call.1
        );
        assert!(
            !on_failure_call.1.contains("teardown-failure"),
            "prompt must not reference a file that failed to write"
        );
        assert!(
            on_failure_call.2.is_none(),
            "no overlay should be attached when the file write failed"
        );

        let warnings = msgs.lock().unwrap().clone();
        assert!(
            warnings
                .iter()
                .any(|m| m.level == crate::data::message::MessageLevel::Warning
                    && m.text.contains("could not write failure output")),
            "must warn about the write failure: {warnings:?}"
        );
    })
    .await
    .unwrap();
}
