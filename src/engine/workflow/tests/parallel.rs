//! Parallel groups (WI-0096): fan-out, per-slot yolo, and board scoping.

use super::super::*;
use super::*;

// ── WI-0096 parallel-group engine tests ──────────────────────────────────

use crate::data::config::flags::FlagConfig;

/// Open a session whose effective `max_concurrent_agents` is `max`
/// (via a flag override, the highest-precedence source).
pub(super) fn make_session_with_max_concurrent(
    tmp: &tempfile::TempDir,
    max: Option<usize>,
) -> Session {
    Session::for_tests_with_options(
        tmp.path(),
        SessionOpenOptions {
            flags: FlagConfig {
                max_concurrent_agents: max,
                ..Default::default()
            },
            ..Default::default()
        },
    )
}

/// Records every parallel-group callback so tests can assert on the
/// engine's scheduling decisions after it is moved into a spawned task.
#[derive(Default)]
struct ParallelRecord {
    launched: Vec<String>,
    exited: Vec<(String, i32)>,
    stuck: Vec<String>,
    unstuck: Vec<String>,
    group_finished: bool,
    available: Vec<AvailableActions>,
}

struct ParallelTestFrontend {
    actions: Mutex<VecDeque<NextAction>>,
    engine_tx: Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<EngineRequest>>>>,
    record: Arc<Mutex<ParallelRecord>>,
    /// Return value for every per-step parallel yolo tick.
    yolo_tick: YoloTickOutcome,
}

impl ParallelTestFrontend {
    fn new(
        actions: impl IntoIterator<Item = NextAction>,
        engine_tx: Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<EngineRequest>>>>,
        yolo_tick: YoloTickOutcome,
    ) -> (Self, Arc<Mutex<ParallelRecord>>) {
        let record = Arc::new(Mutex::new(ParallelRecord::default()));
        (
            Self {
                actions: Mutex::new(actions.into_iter().collect()),
                engine_tx,
                record: record.clone(),
                yolo_tick,
            },
            record,
        )
    }
}

impl crate::data::message::UserMessageSink for ParallelTestFrontend {
    fn write_message(&mut self, _msg: crate::data::message::UserMessage) {}
    fn replay_queued(&mut self) {}
}

impl WorkflowFrontend for ParallelTestFrontend {
    fn show_workflow_control_board(
        &mut self,
        _state: &WorkflowState,
        available: &AvailableActions,
    ) -> Result<NextAction, EngineError> {
        self.record
            .lock()
            .unwrap()
            .available
            .push(available.clone());
        Ok(self
            .actions
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(NextAction::Pause))
    }
    fn confirm_resume(&mut self, _: &ResumeMismatch) -> Result<bool, EngineError> {
        Ok(true)
    }
    fn report_step_status(&mut self, _: &WorkflowStep, _: WorkflowStepStatus) {}
    fn yolo_countdown_tick(
        &mut self,
        _: &str,
        _: Duration,
        _: Duration,
    ) -> Result<YoloTickOutcome, EngineError> {
        Ok(YoloTickOutcome::Cancel)
    }
    fn report_workflow_completed(&mut self, _: &WorkflowOutcome) {}
    fn attach_engine(&mut self, handles: EngineHandles) {
        if let Some(tx) = handles.requests {
            *self.engine_tx.lock().unwrap() = Some(tx);
        }
    }
    fn report_parallel_step_launched(&mut self, step_name: &str, _: &str, _: Option<&str>) {
        self.record
            .lock()
            .unwrap()
            .launched
            .push(step_name.to_string());
    }
    fn report_parallel_step_dequeued(&mut self, step_name: &str, _: &str, _: Option<&str>) {
        self.record
            .lock()
            .unwrap()
            .launched
            .push(step_name.to_string());
    }
    fn report_parallel_step_exited(&mut self, step_name: &str, exit_code: i32) {
        self.record
            .lock()
            .unwrap()
            .exited
            .push((step_name.to_string(), exit_code));
    }
    fn report_parallel_step_stuck(&mut self, step_name: &str) {
        self.record
            .lock()
            .unwrap()
            .stuck
            .push(step_name.to_string());
    }
    fn report_parallel_step_unstuck(&mut self, step_name: &str) {
        self.record
            .lock()
            .unwrap()
            .unstuck
            .push(step_name.to_string());
    }
    fn report_parallel_group_finished(&mut self) {
        self.record.lock().unwrap().group_finished = true;
    }
    fn parallel_step_yolo_countdown_tick(
        &mut self,
        _: &str,
        _: Duration,
        _: Duration,
    ) -> Result<YoloTickOutcome, EngineError> {
        Ok(self.yolo_tick.clone())
    }
}

fn build_parallel_engine(
    session: &Session,
    workflow: Workflow,
    factory: BlockingFactory,
    frontend: ParallelTestFrontend,
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

/// Full 4-step, fully-parallel workflow with `max_concurrent = 2` runs to
/// completion and launches exactly one container per step.
#[tokio::test]
async fn run_to_completion_full_parallel_group_max_concurrent_2() {
    let tmp = tempfile::tempdir().unwrap();
    let session = make_session_with_max_concurrent(&tmp, Some(2));
    assert_eq!(
        session.effective_config().effective_max_concurrent_agents(),
        Some(2)
    );
    let workflow = make_workflow(
        Some("wf-full-parallel"),
        Some("claude"),
        vec![
            make_step("a", &[], None),
            make_step("b", &[], None),
            make_step("c", &[], None),
            make_step("d", &[], None),
        ],
    );
    let factory = FakeAgentExecutionFactory::always_success();
    let mut engine = make_engine(&session, workflow, factory, []);
    assert_eq!(engine.max_concurrent(), Some(2));

    let result = engine.run_to_completion().await.unwrap();
    assert_eq!(result, WorkflowOutcome::Completed);
    for s in ["a", "b", "c", "d"] {
        assert!(
            matches!(engine.state().status_of(s), Some(StepState::Succeeded)),
            "step {s} must have succeeded"
        );
    }
}

/// Scheduling: with 4 concurrently-ready steps and `max_concurrent = 2`,
/// exactly 2 start initially; the 3rd starts only when the 1st finishes and
/// the 4th only when the 2nd finishes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parallel_group_launches_respect_max_concurrent_cap() {
    let tmp = tempfile::tempdir().unwrap();
    let session = make_session_with_max_concurrent(&tmp, Some(2));
    let workflow = make_workflow(
        Some("wf-staged"),
        Some("claude"),
        vec![
            make_step("a", &[], None),
            make_step("b", &[], None),
            make_step("c", &[], None),
            make_step("d", &[], None),
        ],
    );

    let entries: Vec<_> = (0..4).map(|_| make_blocking_entry()).collect();
    let factory = BlockingFactory::new(entries.iter().cloned());
    let execution_count = factory.execution_count.clone();
    let engine_tx: Arc<Mutex<Option<_>>> = Arc::new(Mutex::new(None));
    let (frontend, record) =
        ParallelTestFrontend::new([], engine_tx.clone(), YoloTickOutcome::Continue);
    let mut engine = build_parallel_engine(&session, workflow, factory, frontend);

    let engine_task = tokio::spawn(async move { engine.run_to_completion().await });

    // Only 2 of the 4 steps start initially.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        execution_count.load(Ordering::Relaxed),
        2,
        "max_concurrent=2 must cap the initial launch at 2"
    );
    assert_eq!(record.lock().unwrap().launched, vec!["a", "b"]);

    // Finishing the 1st frees a slot; the 3rd (c) launches.
    signal_completion(&entries[0].1, 0);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(execution_count.load(Ordering::Relaxed), 3);
    assert_eq!(record.lock().unwrap().launched, vec!["a", "b", "c"]);

    // Finishing the 2nd frees the last slot; the 4th (d) launches.
    signal_completion(&entries[1].1, 0);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(execution_count.load(Ordering::Relaxed), 4);
    assert_eq!(record.lock().unwrap().launched, vec!["a", "b", "c", "d"]);

    // Drain the rest and confirm completion.
    signal_completion(&entries[2].1, 0);
    signal_completion(&entries[3].1, 0);
    let result = engine_task.await.unwrap().unwrap();
    assert_eq!(result, WorkflowOutcome::Completed);
}

/// `abort_on_failure` on one running step kills the other running peer and
/// aborts the whole workflow.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parallel_group_abort_on_failure_kills_peer_and_aborts() {
    let tmp = tempfile::tempdir().unwrap();
    let session = make_session_with_max_concurrent(&tmp, Some(2));
    let mut step_a = make_step("a", &[], None);
    step_a.abort_on_failure = true;
    let workflow = make_workflow(
        Some("wf-abort-parallel"),
        Some("claude"),
        vec![step_a, make_step("b", &[], None)],
    );

    let (cancel_a, completion_a) = make_blocking_entry();
    let (cancel_b, _completion_b) = make_blocking_entry();
    let factory = BlockingFactory::new([
        (cancel_a.clone(), completion_a.clone()),
        (cancel_b.clone(), _completion_b),
    ]);
    let engine_tx: Arc<Mutex<Option<_>>> = Arc::new(Mutex::new(None));
    let (frontend, _record) =
        ParallelTestFrontend::new([], engine_tx.clone(), YoloTickOutcome::Continue);
    let mut engine = build_parallel_engine(&session, workflow, factory, frontend);

    let engine_task = tokio::spawn(async move { engine.run_to_completion().await });

    // Both launch; then step "a" fails.
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(!cancel_b.load(Ordering::Relaxed));
    signal_completion(&completion_a, 1);

    let result = engine_task.await.unwrap().unwrap();
    assert_eq!(result, WorkflowOutcome::Aborted);
    assert!(
        cancel_b.load(Ordering::Relaxed),
        "abort_on_failure must kill the still-running peer 'b'"
    );
}

/// Yolo countdown expiry on slot 0 kills that container, launches the
/// queued step into the freed slot, and leaves the other slot running.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parallel_group_yolo_expiry_launches_queued_step() {
    let tmp = tempfile::tempdir().unwrap();
    let session = make_session_with_max_concurrent(&tmp, Some(2));
    let workflow = make_workflow(
        Some("wf-yolo-queue"),
        Some("claude"),
        vec![
            make_step("a", &[], None),
            make_step("b", &[], None),
            make_step("c", &[], None),
        ],
    );

    let (cancel_a, _c_a) = make_blocking_entry();
    let (cancel_b, completion_b) = make_blocking_entry();
    let (cancel_c, completion_c) = make_blocking_entry();
    let factory = BlockingFactory::new([
        (cancel_a.clone(), _c_a),
        (cancel_b.clone(), completion_b.clone()),
        (cancel_c.clone(), completion_c.clone()),
    ]);
    let execution_count = factory.execution_count.clone();
    let engine_tx: Arc<Mutex<Option<_>>> = Arc::new(Mutex::new(None));
    let (frontend, _record) =
        ParallelTestFrontend::new([], engine_tx.clone(), YoloTickOutcome::AdvanceNow);
    let mut engine = build_parallel_engine(&session, workflow, factory, frontend);
    engine.set_yolo(true);
    let tx = {
        // set_engine_sender fires during construction.
        engine_tx.lock().unwrap().clone().unwrap()
    };

    let engine_task = tokio::spawn(async move { engine.run_to_completion().await });

    // a + b launched (2), c queued.
    wait_for_execution_count(&execution_count, 2).await;

    // Mark slot "a" stuck → yolo countdown → the AdvanceNow tick expires it.
    tx.send(EngineRequest::StepStuck {
        step_name: "a".to_string(),
    })
    .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;

    assert!(
        cancel_a.load(Ordering::Relaxed),
        "yolo expiry must kill slot 'a'"
    );
    assert_eq!(
        execution_count.load(Ordering::Relaxed),
        3,
        "the queued step 'c' must launch into the freed slot"
    );
    assert!(
        !cancel_b.load(Ordering::Relaxed),
        "slot 'b' must keep running"
    );

    // Complete the survivors.
    signal_completion(&completion_b, 0);
    signal_completion(&completion_c, 0);
    let result = engine_task.await.unwrap().unwrap();
    assert_eq!(result, WorkflowOutcome::Completed);
}

/// Yolo expiry with no queued step: the group drains — no new launch, the
/// other slot continues, and the group finishes when it exits.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parallel_group_yolo_expiry_draining_no_new_launch() {
    let tmp = tempfile::tempdir().unwrap();
    let session = make_session_with_max_concurrent(&tmp, Some(2));
    let workflow = make_workflow(
        Some("wf-yolo-drain"),
        Some("claude"),
        vec![make_step("a", &[], None), make_step("b", &[], None)],
    );

    let (cancel_a, _c_a) = make_blocking_entry();
    let (cancel_b, completion_b) = make_blocking_entry();
    let factory = BlockingFactory::new([
        (cancel_a.clone(), _c_a),
        (cancel_b.clone(), completion_b.clone()),
    ]);
    let execution_count = factory.execution_count.clone();
    let engine_tx: Arc<Mutex<Option<_>>> = Arc::new(Mutex::new(None));
    let (frontend, _record) =
        ParallelTestFrontend::new([], engine_tx.clone(), YoloTickOutcome::AdvanceNow);
    let mut engine = build_parallel_engine(&session, workflow, factory, frontend);
    engine.set_yolo(true);
    let tx = engine_tx.lock().unwrap().clone().unwrap();

    let engine_task = tokio::spawn(async move { engine.run_to_completion().await });

    wait_for_execution_count(&execution_count, 2).await;

    tx.send(EngineRequest::StepStuck {
        step_name: "a".to_string(),
    })
    .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;

    assert!(
        cancel_a.load(Ordering::Relaxed),
        "yolo expiry must kill slot 'a'"
    );
    assert_eq!(
        execution_count.load(Ordering::Relaxed),
        2,
        "no queued step means nothing new launches (draining)"
    );
    assert!(
        !cancel_b.load(Ordering::Relaxed),
        "the surviving slot keeps running"
    );

    signal_completion(&completion_b, 0);
    let result = engine_task.await.unwrap().unwrap();
    assert_eq!(result, WorkflowOutcome::Completed);
}

/// `StepStuck { step_name }` (yolo off) marks only the named slot stuck;
/// the other slot is unaffected.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parallel_step_stuck_routes_to_named_slot_only() {
    let tmp = tempfile::tempdir().unwrap();
    let session = make_session_with_max_concurrent(&tmp, Some(2));
    let workflow = make_workflow(
        Some("wf-stuck-route"),
        Some("claude"),
        vec![make_step("a", &[], None), make_step("b", &[], None)],
    );

    let (_ca, completion_a) = make_blocking_entry();
    let (_cb, completion_b) = make_blocking_entry();
    let factory = BlockingFactory::new([
        (Arc::new(AtomicBool::new(false)), completion_a.clone()),
        (Arc::new(AtomicBool::new(false)), completion_b.clone()),
    ]);
    let engine_tx: Arc<Mutex<Option<_>>> = Arc::new(Mutex::new(None));
    let (frontend, record) =
        ParallelTestFrontend::new([], engine_tx.clone(), YoloTickOutcome::Continue);
    let mut engine = build_parallel_engine(&session, workflow, factory, frontend);
    // Not yolo mode.
    let tx = engine_tx.lock().unwrap().clone().unwrap();

    let engine_task = tokio::spawn(async move { engine.run_to_completion().await });

    tokio::time::sleep(Duration::from_millis(150)).await;
    tx.send(EngineRequest::StepStuck {
        step_name: "b".to_string(),
    })
    .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;

    {
        let rec = record.lock().unwrap();
        assert_eq!(rec.stuck, vec!["b"], "only 'b' must be reported stuck");
        assert!(
            !rec.stuck.contains(&"a".to_string()),
            "'a' must not be reported stuck"
        );
    }

    signal_completion(&completion_a, 0);
    signal_completion(&completion_b, 0);
    let result = engine_task.await.unwrap().unwrap();
    assert_eq!(result, WorkflowOutcome::Completed);
}

/// WCB scoping (§10): when the focused step has running parallel peers,
/// `can_cancel_to_previous_step` and `can_finish_workflow` are forced false
/// and `restart_unavailable_reason` is set.
#[tokio::test]
async fn compute_available_actions_scopes_wcb_with_parallel_peers() {
    let tmp = tempfile::tempdir().unwrap();
    let session = make_session_with_max_concurrent(&tmp, Some(2));
    let workflow = make_workflow(
        Some("wf-wcb-peers"),
        Some("claude"),
        vec![make_step("a", &[], None), make_step("b", &[], None)],
    );
    let factory = FakeAgentExecutionFactory::always_success();
    let mut engine = make_engine(&session, workflow, factory, []);

    let dummy = |name: &str| ActiveParallelStep {
        step_name: name.to_string(),
        execution: None,
        cancel_handle: None,
        container_name: format!("container-{name}"),
        output_tail: None,
        awman_killed: false,
        stuck: false,
        yolo_deadline: None,
        agent: AgentName::new("claude").unwrap(),
        model: None,
    };

    // Two live slots, "a" focused → one running peer.
    engine.active_steps.push(dummy("a"));
    engine.active_steps.push(dummy("b"));
    engine.current_step_name = Some("a".to_string());
    engine.current_step_agent = Some(AgentName::new("claude").unwrap());

    let a = engine.compute_available_actions().unwrap();
    assert_eq!(a.parallel_peer_count, 2);
    assert_eq!(a.parallel_peers_running, 1);
    assert!(!a.can_cancel_to_previous_step);
    assert!(a.cancel_to_previous_unavailable_reason.is_some());
    assert!(!a.can_finish_workflow);
    assert!(a.finish_workflow_unavailable_reason.is_some());
    assert!(a.restart_unavailable_reason.is_some());

    // Drop to a single live slot → no peers, no forced scoping.
    engine.active_steps.pop();
    let b = engine.compute_available_actions().unwrap();
    assert_eq!(b.parallel_peers_running, 0);
    assert!(b.restart_unavailable_reason.is_none());
}
