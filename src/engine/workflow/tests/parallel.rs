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
    /// `(title, body)` of every follow-up question the engine asked.
    group_prompts: Vec<(String, String)>,
}

struct ParallelTestFrontend {
    actions: Mutex<VecDeque<NextAction>>,
    engine_tx: Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<EngineRequest>>>>,
    record: Arc<Mutex<ParallelRecord>>,
    /// Return value for every per-step parallel yolo tick.
    yolo_tick: YoloTickOutcome,
    /// Answers to the engine's follow-up questions, in order; `KeepRunning`
    /// once exhausted.
    group_answers: Mutex<VecDeque<ParallelGroupDecision>>,
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
                group_answers: Mutex::new(VecDeque::new()),
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
    fn ask_parallel_group(
        &mut self,
        prompt: &crate::data::prompt::Prompt<ParallelGroupDecision>,
    ) -> Result<ParallelGroupDecision, EngineError> {
        self.record
            .lock()
            .unwrap()
            .group_prompts
            .push((prompt.title.clone(), prompt.body.clone()));
        Ok(self
            .group_answers
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(ParallelGroupDecision::KeepRunning))
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
        launch_id: 0,
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

/// A peer that fails while the rest of its group still runs is offered as a
/// retry on the WCB; choosing it relaunches the step at once, and the group
/// then drains cleanly with no post-group failure board.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parallel_group_wcb_retries_failed_peer_mid_group() {
    let tmp = tempfile::tempdir().unwrap();
    let session = make_session_with_max_concurrent(&tmp, Some(2));
    let workflow = make_workflow(
        Some("wf-retry-mid-group"),
        Some("claude"),
        vec![make_step("a", &[], None), make_step("b", &[], None)],
    );

    let (cancel_a, completion_a) = make_blocking_entry();
    let (cancel_b, completion_b) = make_blocking_entry();
    let (cancel_retry, completion_retry) = make_blocking_entry();
    let factory = BlockingFactory::new([
        (cancel_a, completion_a.clone()),
        (cancel_b, completion_b.clone()),
        (cancel_retry, completion_retry.clone()),
    ]);
    let engine_tx: Arc<Mutex<Option<_>>> = Arc::new(Mutex::new(None));
    let (frontend, record) = ParallelTestFrontend::new(
        [
            NextAction::Dismiss,
            NextAction::RetryFailedStep {
                step_name: "a".into(),
            },
        ],
        engine_tx.clone(),
        YoloTickOutcome::Continue,
    );
    let mut engine = build_parallel_engine(&session, workflow, factory, frontend);
    let tx = engine_tx.lock().unwrap().clone().unwrap();

    let engine_task = tokio::spawn(async move { engine.run_to_completion().await });
    tokio::time::sleep(Duration::from_millis(150)).await;

    // Before anything fails the board offers no retry.
    tx.send(EngineRequest::OpenControlBoard {
        step_name: "b".into(),
    })
    .unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;

    // "a" fails while "b" keeps running; the next board offers to retry it.
    signal_completion(&completion_a, 1);
    tokio::time::sleep(Duration::from_millis(150)).await;
    tx.send(EngineRequest::OpenControlBoard {
        step_name: "b".into(),
    })
    .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;

    {
        let r = record.lock().unwrap();
        assert_eq!(r.available.len(), 2);
        assert_eq!(r.available[0].retry_failed_step, None);
        assert_eq!(r.available[1].retry_failed_step.as_deref(), Some("a"));
        assert_eq!(
            r.launched,
            vec!["a", "b", "a"],
            "the retry must relaunch 'a' while 'b' is still running"
        );
    }

    signal_completion(&completion_retry, 0);
    signal_completion(&completion_b, 0);
    let result = engine_task.await.unwrap().unwrap();
    assert_eq!(result, WorkflowOutcome::Completed);
    assert_eq!(
        record.lock().unwrap().available.len(),
        2,
        "a retried failure must not get a post-group failure board"
    );
}

// ── Restart / back / next on a running parallel group ────────────────────

/// Stand-in for Layer 2's copy: each prompt's body spells out the facts the
/// engine passed in, so tests can check them.
fn test_group_prompts() -> ParallelGroupPrompts {
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

/// A parallel-test engine with the group prompts installed and `answers`
/// queued for the follow-up questions.
fn build_group_engine(
    session: &Session,
    workflow: Workflow,
    factory: BlockingFactory,
    frontend: ParallelTestFrontend,
    answers: impl IntoIterator<Item = ParallelGroupDecision>,
) -> WorkflowEngine {
    *frontend.group_answers.lock().unwrap() = answers.into_iter().collect();
    let mut engine = build_parallel_engine(session, workflow, factory, frontend);
    engine.set_parallel_group_prompts(test_group_prompts());
    engine
}

/// Open the WCB on `step` mid-group through the engine channel, then give the
/// engine time to act on the frontend's queued answer.
async fn open_board(tx: &tokio::sync::mpsc::UnboundedSender<EngineRequest>, step: &str) {
    tx.send(EngineRequest::OpenControlBoard {
        step_name: step.into(),
    })
    .unwrap();
    tokio::time::sleep(Duration::from_millis(250)).await;
}

/// A mid-group board offers restart, back and next for the whole group;
/// a group with nothing before it offers restart and next but not back.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parallel_group_board_offers_group_restart_back_and_next() {
    let tmp = tempfile::tempdir().unwrap();
    let session = make_session_with_max_concurrent(&tmp, Some(2));
    let workflow = make_workflow(
        Some("wf-group-board"),
        Some("claude"),
        vec![
            make_step("root", &[], None),
            make_step("a", &["root"], None),
            make_step("b", &["root"], None),
        ],
    );
    let (c0, root) = make_blocking_entry();
    signal_completion(&root, 0);
    let (ca, completion_a) = make_blocking_entry();
    let (cb, completion_b) = make_blocking_entry();
    let factory = BlockingFactory::new([
        (c0, root),
        (ca, completion_a.clone()),
        (cb, completion_b.clone()),
    ]);
    let engine_tx: Arc<Mutex<Option<_>>> = Arc::new(Mutex::new(None));
    let (frontend, record) = ParallelTestFrontend::new(
        [NextAction::LaunchNext, NextAction::Dismiss],
        engine_tx.clone(),
        YoloTickOutcome::Continue,
    );
    let mut engine = build_group_engine(&session, workflow, factory, frontend, []);
    let tx = engine_tx.lock().unwrap().clone().unwrap();
    let engine_task = tokio::spawn(async move { engine.run_to_completion().await });
    tokio::time::sleep(Duration::from_millis(250)).await;

    open_board(&tx, "a").await;
    {
        let r = record.lock().unwrap();
        let board = r.available.last().unwrap();
        assert!(board.acts_on_parallel_group);
        assert!(board.can_restart_current_step);
        assert!(board.restart_unavailable_reason.is_none());
        assert!(board.can_cancel_to_previous_step);
        assert!(board.cancel_to_previous_unavailable_reason.is_none());
        assert!(board.can_launch_next);
    }
    signal_completion(&completion_a, 0);
    signal_completion(&completion_b, 0);
    assert_eq!(
        engine_task.await.unwrap().unwrap(),
        WorkflowOutcome::Completed
    );

    // The first group has nothing to go back to.
    let tmp = tempfile::tempdir().unwrap();
    let session = make_session_with_max_concurrent(&tmp, Some(2));
    let workflow = make_workflow(
        Some("wf-first-group"),
        Some("claude"),
        vec![make_step("a", &[], None), make_step("b", &[], None)],
    );
    let (ca, completion_a) = make_blocking_entry();
    let (cb, completion_b) = make_blocking_entry();
    let factory = BlockingFactory::new([(ca, completion_a.clone()), (cb, completion_b.clone())]);
    let engine_tx: Arc<Mutex<Option<_>>> = Arc::new(Mutex::new(None));
    let (frontend, record) = ParallelTestFrontend::new(
        [NextAction::Dismiss],
        engine_tx.clone(),
        YoloTickOutcome::Continue,
    );
    let mut engine = build_group_engine(&session, workflow, factory, frontend, []);
    let tx = engine_tx.lock().unwrap().clone().unwrap();
    let engine_task = tokio::spawn(async move { engine.run_to_completion().await });
    tokio::time::sleep(Duration::from_millis(150)).await;
    open_board(&tx, "a").await;
    {
        let r = record.lock().unwrap();
        let board = r.available.last().unwrap();
        assert!(board.can_restart_current_step);
        assert!(board.can_launch_next);
        assert!(!board.can_cancel_to_previous_step);
        assert!(board.cancel_to_previous_unavailable_reason.is_some());
    }
    signal_completion(&completion_a, 0);
    signal_completion(&completion_b, 0);
    assert_eq!(
        engine_task.await.unwrap().unwrap(),
        WorkflowOutcome::Completed
    );
}

/// An engine given no prompt copy cannot ask the follow-up questions, so it
/// offers none of restart, back or next mid-group — never an enabled action
/// that silently does nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parallel_group_board_without_prompts_offers_no_group_actions() {
    let tmp = tempfile::tempdir().unwrap();
    let session = make_session_with_max_concurrent(&tmp, Some(2));
    let workflow = make_workflow(
        Some("wf-no-prompts"),
        Some("claude"),
        vec![make_step("a", &[], None), make_step("b", &[], None)],
    );
    let (ca, completion_a) = make_blocking_entry();
    let (cb, completion_b) = make_blocking_entry();
    let factory = BlockingFactory::new([(ca, completion_a.clone()), (cb, completion_b.clone())]);
    let engine_tx: Arc<Mutex<Option<_>>> = Arc::new(Mutex::new(None));
    let (frontend, record) = ParallelTestFrontend::new(
        [NextAction::Dismiss],
        engine_tx.clone(),
        YoloTickOutcome::Continue,
    );
    let mut engine = build_parallel_engine(&session, workflow, factory, frontend);
    let tx = engine_tx.lock().unwrap().clone().unwrap();
    let engine_task = tokio::spawn(async move { engine.run_to_completion().await });
    tokio::time::sleep(Duration::from_millis(150)).await;
    open_board(&tx, "a").await;
    {
        let r = record.lock().unwrap();
        let board = r.available.last().unwrap();
        assert!(!board.acts_on_parallel_group);
        assert!(!board.can_restart_current_step);
        assert!(!board.can_cancel_to_previous_step);
        assert!(!board.can_launch_next);
    }
    signal_completion(&completion_a, 0);
    signal_completion(&completion_b, 0);
    assert_eq!(
        engine_task.await.unwrap().unwrap(),
        WorkflowOutcome::Completed
    );
}

/// Restart → single agent → a running member: just its container is killed
/// and relaunched while its peer keeps running, and the killed container's
/// exit is ignored rather than read as the relaunched step failing. The
/// picker is handed every member with its state.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parallel_group_restart_single_running_step() {
    let tmp = tempfile::tempdir().unwrap();
    let session = make_session_with_max_concurrent(&tmp, Some(2));
    let workflow = make_workflow(
        Some("wf-restart-one"),
        Some("claude"),
        vec![make_step("a", &[], None), make_step("b", &[], None)],
    );
    let (cancel_a, completion_a) = make_blocking_entry();
    let (cancel_b, completion_b) = make_blocking_entry();
    let (cancel_a2, completion_a2) = make_blocking_entry();
    let factory = BlockingFactory::new([
        (cancel_a.clone(), completion_a),
        (cancel_b.clone(), completion_b.clone()),
        (cancel_a2, completion_a2.clone()),
    ]);
    let engine_tx: Arc<Mutex<Option<_>>> = Arc::new(Mutex::new(None));
    let (frontend, record) = ParallelTestFrontend::new(
        [NextAction::RestartCurrentStep],
        engine_tx.clone(),
        YoloTickOutcome::Continue,
    );
    let mut engine = build_group_engine(
        &session,
        workflow,
        factory,
        frontend,
        [
            ParallelGroupDecision::RestartOneAgent,
            ParallelGroupDecision::RestartStep("a".into()),
        ],
    );
    let tx = engine_tx.lock().unwrap().clone().unwrap();
    let engine_task = tokio::spawn(async move { engine.run_to_completion().await });
    tokio::time::sleep(Duration::from_millis(150)).await;

    open_board(&tx, "b").await;
    assert!(cancel_a.load(Ordering::Relaxed), "a's container is killed");
    assert!(!cancel_b.load(Ordering::Relaxed), "b keeps running");
    {
        let r = record.lock().unwrap();
        assert_eq!(r.launched, vec!["a", "b", "a"]);
        assert_eq!(r.group_prompts[0], ("scope".to_string(), "a,b".to_string()));
        assert_eq!(r.group_prompts[1].0, "which");
        assert!(
            r.group_prompts[1].1.contains("\"a\""),
            "{:?}",
            r.group_prompts
        );
        assert!(
            r.group_prompts[1].1.contains("Running"),
            "{:?}",
            r.group_prompts
        );
    }

    signal_completion(&completion_a2, 0);
    signal_completion(&completion_b, 0);
    assert_eq!(
        engine_task.await.unwrap().unwrap(),
        WorkflowOutcome::Completed
    );
    assert_eq!(
        record.lock().unwrap().available.len(),
        1,
        "the killed container must not surface as a failure board"
    );
}

/// Any member can be restarted, including one that already finished.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parallel_group_restart_single_completed_step() {
    let tmp = tempfile::tempdir().unwrap();
    let session = make_session_with_max_concurrent(&tmp, Some(2));
    let workflow = make_workflow(
        Some("wf-restart-done"),
        Some("claude"),
        vec![make_step("a", &[], None), make_step("b", &[], None)],
    );
    let (ca, completion_a) = make_blocking_entry();
    let (cb, completion_b) = make_blocking_entry();
    let (ca2, completion_a2) = make_blocking_entry();
    let factory = BlockingFactory::new([
        (ca, completion_a.clone()),
        (cb, completion_b.clone()),
        (ca2, completion_a2.clone()),
    ]);
    let engine_tx: Arc<Mutex<Option<_>>> = Arc::new(Mutex::new(None));
    let (frontend, record) = ParallelTestFrontend::new(
        [NextAction::RestartCurrentStep],
        engine_tx.clone(),
        YoloTickOutcome::Continue,
    );
    let mut engine = build_group_engine(
        &session,
        workflow,
        factory,
        frontend,
        [
            ParallelGroupDecision::RestartOneAgent,
            ParallelGroupDecision::RestartStep("a".into()),
        ],
    );
    let tx = engine_tx.lock().unwrap().clone().unwrap();
    let engine_task = tokio::spawn(async move { engine.run_to_completion().await });
    tokio::time::sleep(Duration::from_millis(150)).await;

    signal_completion(&completion_a, 0);
    tokio::time::sleep(Duration::from_millis(150)).await;
    open_board(&tx, "b").await;
    assert_eq!(record.lock().unwrap().launched, vec!["a", "b", "a"]);

    signal_completion(&completion_a2, 0);
    signal_completion(&completion_b, 0);
    assert_eq!(
        engine_task.await.unwrap().unwrap(),
        WorkflowOutcome::Completed
    );
}

/// Restart → whole group: what is running is killed and every member runs
/// again from scratch, finished ones included.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parallel_group_restart_whole_group() {
    let tmp = tempfile::tempdir().unwrap();
    let session = make_session_with_max_concurrent(&tmp, Some(2));
    let workflow = make_workflow(
        Some("wf-restart-group"),
        Some("claude"),
        vec![make_step("a", &[], None), make_step("b", &[], None)],
    );
    let (ca, completion_a) = make_blocking_entry();
    let (cancel_b, completion_b) = make_blocking_entry();
    let factory =
        BlockingFactory::new([(ca, completion_a.clone()), (cancel_b.clone(), completion_b)]);
    let execution_count = factory.execution_count.clone();
    let engine_tx: Arc<Mutex<Option<_>>> = Arc::new(Mutex::new(None));
    let (frontend, record) = ParallelTestFrontend::new(
        [NextAction::RestartCurrentStep],
        engine_tx.clone(),
        YoloTickOutcome::Continue,
    );
    let mut engine = build_group_engine(
        &session,
        workflow,
        factory,
        frontend,
        [ParallelGroupDecision::RestartWholeGroup],
    );
    let tx = engine_tx.lock().unwrap().clone().unwrap();
    let engine_task = tokio::spawn(async move { engine.run_to_completion().await });
    tokio::time::sleep(Duration::from_millis(150)).await;

    signal_completion(&completion_a, 0);
    tokio::time::sleep(Duration::from_millis(150)).await;
    open_board(&tx, "b").await;

    // The rerun uses instant-success containers (no blocking slots left).
    assert_eq!(
        engine_task.await.unwrap().unwrap(),
        WorkflowOutcome::Completed
    );
    assert!(cancel_b.load(Ordering::Relaxed), "running b was killed");
    assert_eq!(execution_count.load(Ordering::Relaxed), 4);
    assert_eq!(record.lock().unwrap().launched, vec!["a", "b", "a", "b"]);
}

/// Back, confirmed: the whole group is cancelled and the workflow returns to
/// the step before it, then runs forward again from there.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parallel_group_cancel_back_returns_to_previous_step() {
    let tmp = tempfile::tempdir().unwrap();
    let session = make_session_with_max_concurrent(&tmp, Some(2));
    let workflow = make_workflow(
        Some("wf-group-back"),
        Some("claude"),
        vec![
            make_step("root", &[], None),
            make_step("a", &["root"], None),
            make_step("b", &["root"], None),
        ],
    );
    let (c0, root) = make_blocking_entry();
    signal_completion(&root, 0);
    let (ca, completion_a) = make_blocking_entry();
    let (cancel_b, completion_b) = make_blocking_entry();
    let factory = BlockingFactory::new([
        (c0, root),
        (ca, completion_a.clone()),
        (cancel_b.clone(), completion_b),
    ]);
    let execution_count = factory.execution_count.clone();
    let engine_tx: Arc<Mutex<Option<_>>> = Arc::new(Mutex::new(None));
    let (frontend, record) = ParallelTestFrontend::new(
        [
            NextAction::LaunchNext,
            NextAction::CancelToPreviousStep,
            NextAction::LaunchNext,
        ],
        engine_tx.clone(),
        YoloTickOutcome::Continue,
    );
    let mut engine = build_group_engine(
        &session,
        workflow,
        factory,
        frontend,
        [ParallelGroupDecision::CancelGroup],
    );
    let tx = engine_tx.lock().unwrap().clone().unwrap();
    let engine_task = tokio::spawn(async move { engine.run_to_completion().await });
    tokio::time::sleep(Duration::from_millis(250)).await;

    // "a" has already finished; going back must rerun it too.
    signal_completion(&completion_a, 0);
    tokio::time::sleep(Duration::from_millis(150)).await;
    open_board(&tx, "b").await;

    assert_eq!(
        engine_task.await.unwrap().unwrap(),
        WorkflowOutcome::Completed
    );
    assert!(cancel_b.load(Ordering::Relaxed), "running b was killed");
    // root, a, b, then root, a, b again.
    assert_eq!(execution_count.load(Ordering::Relaxed), 6);
    let r = record.lock().unwrap();
    assert_eq!(r.launched, vec!["a", "b", "a", "b"]);
    assert_eq!(
        r.group_prompts[0],
        ("cancel".to_string(), "a,b|Back([\"root\"])".to_string())
    );
}

/// Next, confirmed: the whole group is cancelled, its unfinished members
/// are skipped, and the step after the group runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parallel_group_cancel_next_skips_to_following_step() {
    let tmp = tempfile::tempdir().unwrap();
    let session = make_session_with_max_concurrent(&tmp, Some(2));
    let workflow = make_workflow(
        Some("wf-group-next"),
        Some("claude"),
        vec![
            make_step("a", &[], None),
            make_step("b", &[], None),
            make_step("c", &["a", "b"], None),
        ],
    );
    let (ca, completion_a) = make_blocking_entry();
    let (cancel_b, completion_b) = make_blocking_entry();
    let factory =
        BlockingFactory::new([(ca, completion_a.clone()), (cancel_b.clone(), completion_b)]);
    let engine_tx: Arc<Mutex<Option<_>>> = Arc::new(Mutex::new(None));
    let (frontend, record) = ParallelTestFrontend::new(
        // The board, then the between-steps board after "c" is the last step.
        [NextAction::LaunchNext, NextAction::FinishWorkflow],
        engine_tx.clone(),
        YoloTickOutcome::Continue,
    );
    let mut engine = build_group_engine(
        &session,
        workflow,
        factory,
        frontend,
        [ParallelGroupDecision::CancelGroup],
    );
    let tx = engine_tx.lock().unwrap().clone().unwrap();
    let engine_task = tokio::spawn(async move { engine.run_to_completion().await });
    tokio::time::sleep(Duration::from_millis(150)).await;

    signal_completion(&completion_a, 0);
    tokio::time::sleep(Duration::from_millis(150)).await;
    open_board(&tx, "b").await;

    let _ = engine_task.await.unwrap().unwrap();
    assert!(cancel_b.load(Ordering::Relaxed), "running b was killed");
    let r = record.lock().unwrap();
    assert_eq!(
        r.group_prompts[0],
        ("cancel".to_string(), "a,b|Next([\"c\"])".to_string())
    );
    assert_eq!(r.launched, vec!["a", "b"], "the group is not rerun");
}

/// Declining the confirmation (or walking away from it) leaves the group
/// running exactly as it was.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parallel_group_declined_confirmation_keeps_group_running() {
    let tmp = tempfile::tempdir().unwrap();
    let session = make_session_with_max_concurrent(&tmp, Some(2));
    let workflow = make_workflow(
        Some("wf-group-keep"),
        Some("claude"),
        vec![make_step("a", &[], None), make_step("b", &[], None)],
    );
    let (cancel_a, completion_a) = make_blocking_entry();
    let (cancel_b, completion_b) = make_blocking_entry();
    let factory = BlockingFactory::new([
        (cancel_a.clone(), completion_a.clone()),
        (cancel_b.clone(), completion_b.clone()),
    ]);
    let engine_tx: Arc<Mutex<Option<_>>> = Arc::new(Mutex::new(None));
    let (frontend, record) = ParallelTestFrontend::new(
        [NextAction::LaunchNext, NextAction::RestartCurrentStep],
        engine_tx.clone(),
        YoloTickOutcome::Continue,
    );
    let mut engine = build_group_engine(
        &session,
        workflow,
        factory,
        frontend,
        [ParallelGroupDecision::KeepRunning],
    );
    let tx = engine_tx.lock().unwrap().clone().unwrap();
    let engine_task = tokio::spawn(async move { engine.run_to_completion().await });
    tokio::time::sleep(Duration::from_millis(150)).await;

    open_board(&tx, "a").await; // next → declined
    open_board(&tx, "a").await; // restart → scope question dismissed
    assert!(!cancel_a.load(Ordering::Relaxed));
    assert!(!cancel_b.load(Ordering::Relaxed));
    assert_eq!(record.lock().unwrap().launched, vec!["a", "b"]);

    signal_completion(&completion_a, 0);
    signal_completion(&completion_b, 0);
    assert_eq!(
        engine_task.await.unwrap().unwrap(),
        WorkflowOutcome::Completed
    );
}

/// A frontend that does not implement `ask_parallel_group` answers every
/// question with the prompt's dismissal answer: keep the group running.
#[test]
fn ask_parallel_group_defaults_to_keep_running() {
    let (mut frontend, _) = super::phases::MessageCapturingFrontend::new();
    let prompt = (test_group_prompts().restart_scope)(&["a".to_string()]);
    assert_eq!(
        frontend.ask_parallel_group(&prompt).unwrap(),
        ParallelGroupDecision::KeepRunning
    );
}
