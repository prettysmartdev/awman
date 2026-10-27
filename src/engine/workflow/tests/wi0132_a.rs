//! Work item 0132 Part A regression coverage.

use super::*;

#[tokio::test]
async fn yolo_last_step_offers_finish_and_finishes_workflow() {
    let tmp = tempfile::tempdir().unwrap();
    let session = make_session(&tmp);
    let workflow = make_workflow(
        Some("wi0132-yolo-chain"),
        Some("claude"),
        vec![make_step("a", &[], None), make_step("b", &["a"], None)],
    );
    let frontend = FakeWorkflowFrontend::new([]);
    let boards = frontend.boards();
    let mut engine = WorkflowEngine::new(
        &session,
        WorkflowSpec::new(workflow).with_work_item_context(None),
        WorkflowEngineDeps {
            frontend: Box::new(frontend),
            agent_factory: Box::new(FakeAgentExecutionFactory::always_success()),
        },
    )
    .unwrap();
    engine.set_yolo(true);

    assert_eq!(
        engine.run_to_completion().await.unwrap(),
        WorkflowOutcome::Completed
    );
    let boards = boards.lock().unwrap();
    assert_eq!(boards.len(), 1, "yolo opens one final review board");
    assert!(!boards[0].can_launch_next);
    assert!(boards[0].can_finish_workflow);
}

#[tokio::test]
async fn yolo_multiple_sink_dag_offers_finish_and_finishes_workflow() {
    let tmp = tempfile::tempdir().unwrap();
    let session = super::parallel::make_session_with_max_concurrent(&tmp, Some(2));
    let workflow = make_workflow(
        Some("wi0132-multi-sink"),
        Some("claude"),
        vec![
            make_step("a", &[], None),
            make_step("b", &["a"], None),
            make_step("c", &[], None),
        ],
    );
    let frontend = FakeWorkflowFrontend::new([]);
    let boards = frontend.boards();
    let mut engine = WorkflowEngine::new(
        &session,
        WorkflowSpec::new(workflow).with_work_item_context(None),
        WorkflowEngineDeps {
            frontend: Box::new(frontend),
            agent_factory: Box::new(FakeAgentExecutionFactory::always_success()),
        },
    )
    .unwrap();
    engine.set_yolo(true);

    assert_eq!(
        engine.run_to_completion().await.unwrap(),
        WorkflowOutcome::Completed
    );
    let boards = boards.lock().unwrap();
    assert_eq!(boards.len(), 1);
    assert!(!boards[0].can_launch_next && boards[0].can_finish_workflow);
}

#[tokio::test]
async fn api_default_failure_board_aborts_and_never_offers_finish() {
    let tmp = tempfile::tempdir().unwrap();
    let session = make_session(&tmp);
    let workflow = make_workflow(
        Some("wi0132-failed-last"),
        Some("claude"),
        vec![make_step("a", &[], None)],
    );
    let frontend = FakeWorkflowFrontend::new([]);
    let boards = frontend.boards();
    let mut engine = WorkflowEngine::new(
        &session,
        WorkflowSpec::new(workflow),
        WorkflowEngineDeps {
            frontend: Box::new(frontend),
            agent_factory: Box::new(FakeAgentExecutionFactory::new([1])),
        },
    )
    .unwrap();

    assert_eq!(
        engine.run_to_completion().await.unwrap(),
        WorkflowOutcome::Aborted
    );
    assert!(!boards.lock().unwrap()[0].can_finish_workflow);
}

#[tokio::test]
async fn resumed_workflow_with_all_steps_done_finishes() {
    let tmp = tempfile::tempdir().unwrap();
    let session = make_session(&tmp);
    let workflow = make_workflow(
        Some("wi0132-resumed-complete"),
        Some("claude"),
        vec![make_step("a", &[], None), make_step("b", &["a"], None)],
    );
    let mut state = WorkflowState::new(
        "wi0132-resumed-complete".into(),
        &workflow.steps,
        "test-hash".into(),
        None,
    );
    state.set_status("a", StepState::Succeeded);
    let store = WorkflowStateStore::at_git_root(tmp.path());
    store.save(&state).unwrap();

    let frontend = FakeWorkflowFrontend::new([]);
    let boards = frontend.boards();
    let mut engine = WorkflowEngine::resume(
        &session,
        WorkflowSpec::new(workflow).with_work_item_context(None),
        WorkflowEngineDeps {
            frontend: Box::new(frontend),
            agent_factory: Box::new(FakeAgentExecutionFactory::always_success()),
        },
    )
    .await
    .unwrap();
    engine.set_yolo(true);
    assert_eq!(
        engine.run_to_completion().await.unwrap(),
        WorkflowOutcome::Completed
    );
    let boards = boards.lock().unwrap();
    assert_eq!(boards.len(), 1);
    assert!(boards[0].can_finish_workflow && !boards[0].can_launch_next);
}

#[tokio::test]
async fn terminal_parallel_group_completes_without_control_boards() {
    let tmp = tempfile::tempdir().unwrap();
    let session = super::parallel::make_session_with_max_concurrent(&tmp, Some(2));
    let workflow = make_workflow(
        Some("wi0132-parallel-terminal"),
        Some("claude"),
        vec![make_step("a", &[], None), make_step("b", &[], None)],
    );
    let frontend = FakeWorkflowFrontend::new([]);
    let boards = frontend.boards();
    let mut engine = WorkflowEngine::new(
        &session,
        WorkflowSpec::new(workflow).with_work_item_context(None),
        WorkflowEngineDeps {
            frontend: Box::new(frontend),
            agent_factory: Box::new(FakeAgentExecutionFactory::always_success()),
        },
    )
    .unwrap();
    engine.set_yolo(true);

    assert_eq!(
        engine.run_to_completion().await.unwrap(),
        WorkflowOutcome::Completed
    );
    assert!(
        boards.lock().unwrap().is_empty(),
        "terminal parallel execution does not open a final board"
    );
}

struct PeerBoardFrontend {
    request_tx: Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<EngineRequest>>>>,
    boards: Arc<Mutex<Vec<AvailableActions>>>,
    answers: Arc<Mutex<Vec<NextAction>>>,
}

impl crate::data::message::UserMessageSink for PeerBoardFrontend {
    fn write_message(&mut self, _: crate::data::message::UserMessage) {}
    fn replay_queued(&mut self) {}
}

impl WorkflowFrontend for PeerBoardFrontend {
    fn show_workflow_control_board(
        &mut self,
        _: &WorkflowState,
        available: &AvailableActions,
    ) -> Result<NextAction, EngineError> {
        self.boards.lock().unwrap().push(available.clone());
        let answer = api_headless_next_action(available);
        self.answers.lock().unwrap().push(answer.clone());
        Ok(answer)
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
    fn attach_engine(&mut self, handles: EngineHandles) {
        *self.request_tx.lock().unwrap() = handles.requests;
    }
}

fn peer_test_group_prompts() -> ParallelGroupPrompts {
    use crate::data::prompt::{Choice, Prompt};
    fn prompt(title: &str, body: String) -> Prompt<ParallelGroupDecision> {
        Prompt::new(
            title,
            body,
            vec![Choice::new('x', "x", ParallelGroupDecision::KeepRunning)],
            Some(ParallelGroupDecision::KeepRunning),
        )
    }
    ParallelGroupPrompts {
        restart_scope: |group| prompt("scope", group.join(",")),
        restart_which: |members| prompt("which", format!("{members:?}")),
        cancel_group: |group, exit| prompt("cancel", format!("{}|{exit:?}", group.join(","))),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parallel_peers_running_control_board_answers_launch_next() {
    let tmp = tempfile::tempdir().unwrap();
    let session = super::parallel::make_session_with_max_concurrent(&tmp, Some(2));
    let workflow = make_workflow(
        Some("wi0132-parallel-peers"),
        Some("claude"),
        vec![make_step("a", &[], None), make_step("b", &[], None)],
    );
    let (cancel_a, done_a) = make_blocking_entry();
    let (cancel_b, done_b) = make_blocking_entry();
    let factory = BlockingFactory::new([(cancel_a, done_a.clone()), (cancel_b, done_b.clone())]);
    let executions = factory.execution_count.clone();
    let request_tx = Arc::new(Mutex::new(None));
    let boards = Arc::new(Mutex::new(Vec::new()));
    let answers = Arc::new(Mutex::new(Vec::new()));
    let mut engine = WorkflowEngine::new(
        &session,
        WorkflowSpec::new(workflow).with_work_item_context(None),
        WorkflowEngineDeps {
            frontend: Box::new(PeerBoardFrontend {
                request_tx: request_tx.clone(),
                boards: boards.clone(),
                answers: answers.clone(),
            }),
            agent_factory: Box::new(factory),
        },
    )
    .unwrap();
    engine.set_parallel_group_prompts(peer_test_group_prompts());
    let tx = request_tx.lock().unwrap().clone().unwrap();
    let task = tokio::spawn(async move { engine.run_to_completion().await });
    wait_for_execution_count(&executions, 2).await;
    tx.send(EngineRequest::OpenControlBoard {
        step_name: "a".into(),
    })
    .unwrap();
    for _ in 0..100 {
        if !boards.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let board = boards
        .lock()
        .unwrap()
        .first()
        .cloned()
        .expect("board should open while peers run");
    assert!(board.can_launch_next);
    assert!(!board.can_finish_workflow);
    assert_eq!(
        answers.lock().unwrap().as_slice(),
        &[NextAction::LaunchNext]
    );
    signal_completion(&done_a, 0);
    signal_completion(&done_b, 0);
    assert_eq!(task.await.unwrap().unwrap(), WorkflowOutcome::Completed);
}

#[tokio::test]
async fn sequential_multi_sink_finishes_on_a_non_topologically_last_step() {
    let tmp = tempfile::tempdir().unwrap();
    let session = super::parallel::make_session_with_max_concurrent(&tmp, Some(1));
    let workflow = make_workflow(
        Some("sequential-sinks"),
        Some("claude"),
        vec![
            make_step("c", &["b"], None),
            make_step("a", &[], None),
            make_step("b", &[], None),
        ],
    );
    let frontend = FakeWorkflowFrontend::new([]);
    let boards = frontend.boards();
    let mut engine = WorkflowEngine::new(
        &session,
        WorkflowSpec::new(workflow),
        WorkflowEngineDeps {
            frontend: Box::new(frontend),
            agent_factory: Box::new(FakeAgentExecutionFactory::always_success()),
        },
    )
    .unwrap();
    engine.set_yolo(true);
    assert_eq!(
        engine.run_to_completion().await.unwrap(),
        WorkflowOutcome::Completed
    );
    let boards = boards.lock().unwrap();
    assert_eq!(boards.len(), 1);
    assert!(!boards[0].can_launch_next && boards[0].can_finish_workflow);
}

#[tokio::test]
async fn unattended_failed_last_step_retries_once_then_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let session = make_session(&tmp);
    let frontend = FakeWorkflowFrontend::new([]).unattended();
    let boards = frontend.boards();
    let mut engine = WorkflowEngine::new(
        &session,
        WorkflowSpec::new(make_workflow(
            Some("failed-unattended"),
            Some("claude"),
            vec![make_step("a", &[], None)],
        )),
        WorkflowEngineDeps {
            frontend: Box::new(frontend),
            agent_factory: Box::new(FakeAgentExecutionFactory::new([1, 1])),
        },
    )
    .unwrap();
    engine.set_yolo(true);
    assert!(matches!(
        engine.run_to_completion().await.unwrap(),
        WorkflowOutcome::Failed { .. }
    ));
    assert!(boards.lock().unwrap().is_empty());
}

#[tokio::test]
async fn repair_identity_after_parallel_launch_is_independent_of_ui_focus() {
    for serial in [false, true] {
        let tmp = tempfile::tempdir().unwrap();
        let session = super::parallel::make_session_with_max_concurrent(&tmp, Some(2));
        let mut first = make_step("first", &[], Some("claude"));
        first.model = Some("first-model".into());
        let deps = if serial { vec!["first"] } else { vec![] };
        let mut a = make_step("a", &deps, Some("claude"));
        a.model = Some("a-model".into());
        let mut b = make_step("b", &deps, Some("codex"));
        b.model = Some("b-model".into());
        let mut steps = if serial { vec![first] } else { vec![] };
        steps.extend([a, b]);
        let mut engine = make_engine(
            &session,
            make_workflow(Some("repair-identity"), Some("claude"), steps),
            FakeAgentExecutionFactory::always_success(),
            [],
        );
        engine.set_yolo(true);
        assert_eq!(
            engine.run_to_completion().await.unwrap(),
            WorkflowOutcome::Completed
        );
        assert_eq!(engine.last_step_agent().unwrap().as_str(), "codex");
        assert_eq!(engine.last_step_model().as_deref(), Some("b-model"));
        // These are the exact fields a UI focus change assigns.
        engine.current_step_agent = Some(AgentName::new("claude").unwrap());
        engine.current_step_model = Some("focused-model".into());
        assert_eq!(engine.last_step_agent().unwrap().as_str(), "codex");
        assert_eq!(engine.last_step_model().as_deref(), Some("b-model"));
    }
}
