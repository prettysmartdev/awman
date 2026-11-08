//! Failure, stuck and cancel handling inside parallel groups
//! (`parallel.rs`) and the post-group failure path (`control.rs`).
//!
//! Every container is scripted: it either exits at once with a fixed code or
//! is held until the test (or a frontend callback) releases it through a
//! channel. Nothing waits on wall-clock time: ordering comes from the engine's
//! own callbacks, and the stuck/yolo handlers are driven directly on
//! hand-built slots with deadlines already in the past or far in the future.

use super::super::*;
use super::*;

use std::collections::HashMap;
use std::sync::mpsc as std_mpsc;

use crate::engine::agent_runtime::execution::ExecutionBackend;

// ── Scripted factory ─────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug)]
enum Script {
    /// The container has already exited with this code.
    Exit(i32),
    /// The container runs until a code is sent through its hold channel (or
    /// its cancel handle fires).
    Hold,
}

/// Blocks in `wait_blocking` until a code arrives. Holds no sender itself, so
/// once every sender (factory, cancel handles) is gone the wait ends.
struct HeldBackend {
    rx: std_mpsc::Receiver<i32>,
    cancel_tx: std_mpsc::Sender<i32>,
    name: String,
    cancelled: Arc<Mutex<Vec<String>>>,
}

fn exit_info(code: i32) -> AgentExitInfo {
    let now = Utc::now();
    AgentExitInfo {
        exit_code: code,
        signal: None,
        started_at: now,
        ended_at: now,
    }
}

impl ExecutionBackend for HeldBackend {
    fn wait_blocking(self: Box<Self>) -> Result<AgentExitInfo, EngineError> {
        let HeldBackend { rx, cancel_tx, .. } = *self;
        drop(cancel_tx);
        Ok(exit_info(rx.recv().unwrap_or(KILLED_EXIT_CODE)))
    }

    fn cancel(&self) -> Result<(), EngineError> {
        self.cancelled.lock().unwrap().push(self.name.clone());
        let _ = self.cancel_tx.send(KILLED_EXIT_CODE);
        Ok(())
    }

    fn cancel_handle(&self) -> CancelHandle {
        let tx = self.cancel_tx.clone();
        let name = self.name.clone();
        let cancelled = Arc::clone(&self.cancelled);
        CancelHandle::new(move || {
            cancelled.lock().unwrap().push(name.clone());
            let _ = tx.send(KILLED_EXIT_CODE);
            Ok(())
        })
    }
}

type Holds = Arc<Mutex<HashMap<String, std_mpsc::Sender<i32>>>>;

#[derive(Default)]
struct ScriptedFactory {
    scripts: Mutex<HashMap<String, VecDeque<Script>>>,
    launched: Arc<Mutex<Vec<String>>>,
    holds: Holds,
    /// Step names whose cancel handle fired.
    cancelled: Arc<Mutex<Vec<String>>>,
    /// Container names `stop_agent` was asked to stop.
    stopped: Arc<Mutex<Vec<String>>>,
    recover_auth: bool,
    auth_calls: Arc<AtomicUsize>,
}

impl ScriptedFactory {
    fn new(scripts: &[(&str, &[Script])]) -> Self {
        let factory = Self::default();
        *factory.scripts.lock().unwrap() = scripts
            .iter()
            .map(|(name, s)| (name.to_string(), s.iter().copied().collect()))
            .collect();
        factory
    }
}

impl AgentExecutionFactory for ScriptedFactory {
    fn execution_for_step(
        &self,
        step: &WorkflowStep,
        _session: &Session,
        _runtime: &WorkflowRuntimeContext,
    ) -> Result<AgentExecution, EngineError> {
        let script = self
            .scripts
            .lock()
            .unwrap()
            .get_mut(&step.name)
            .and_then(VecDeque::pop_front)
            .unwrap_or(Script::Exit(0));
        let n = {
            let mut launched = self.launched.lock().unwrap();
            launched.push(step.name.clone());
            launched.len()
        };
        let handle = AgentHandle {
            id: format!("id-{}-{n}", step.name),
            image_tag: "img".into(),
            name: format!("c-{}-{n}", step.name),
            started_at: Utc::now(),
        };
        match script {
            Script::Exit(code) => Ok(AgentExecution::finished(handle, exit_info(code))),
            Script::Hold => {
                let (tx, rx) = std_mpsc::channel();
                self.holds
                    .lock()
                    .unwrap()
                    .insert(step.name.clone(), tx.clone());
                let (stuck_tx, _) = tokio::sync::broadcast::channel(4);
                Ok(AgentExecution::new(
                    handle,
                    Box::new(HeldBackend {
                        rx,
                        cancel_tx: tx,
                        name: step.name.clone(),
                        cancelled: Arc::clone(&self.cancelled),
                    }),
                    Arc::new(stuck_tx),
                    None,
                ))
            }
        }
    }

    fn inject_prompt(&self, _: &AgentExecution, _: &str) -> Result<Option<()>, EngineError> {
        Ok(None)
    }

    fn recover_auth_failure(&self, _: &AgentName, _: &str) -> Result<bool, EngineError> {
        self.auth_calls.fetch_add(1, Ordering::Relaxed);
        Ok(self.recover_auth)
    }

    fn stop_agent(&self, handle: &AgentHandle) -> Result<(), EngineError> {
        self.stopped.lock().unwrap().push(handle.name.clone());
        Ok(())
    }
}

fn release(holds: &Holds, step: &str, code: i32) {
    let tx = holds.lock().unwrap().get(step).cloned();
    tx.unwrap_or_else(|| panic!("{step} is not held"))
        .send(code)
        .unwrap();
}

// ── Recording frontend ───────────────────────────────────────────────────

#[derive(Default)]
struct Rec {
    exited: Vec<(String, i32)>,
    stuck: Vec<String>,
    unstuck: Vec<String>,
    countdown_started: Vec<String>,
    countdown_finished: Vec<String>,
    boards: Vec<AvailableActions>,
    completed: Option<WorkflowOutcome>,
    group_finished: usize,
}

type ExitHook = Box<dyn FnMut(&str, i32) + Send + Sync>;

struct Front {
    rec: Arc<Mutex<Rec>>,
    actions: VecDeque<NextAction>,
    interactive: bool,
    parallel_tick: YoloTickOutcome,
    retry_tick: YoloTickOutcome,
    on_exit: Option<ExitHook>,
    engine_tx: Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<EngineRequest>>>>,
}

impl Front {
    fn new(actions: impl IntoIterator<Item = NextAction>) -> (Self, Arc<Mutex<Rec>>) {
        let rec = Arc::new(Mutex::new(Rec::default()));
        (
            Self {
                rec: Arc::clone(&rec),
                actions: actions.into_iter().collect(),
                interactive: true,
                parallel_tick: YoloTickOutcome::Continue,
                retry_tick: YoloTickOutcome::AdvanceNow,
                on_exit: None,
                engine_tx: Arc::new(Mutex::new(None)),
            },
            rec,
        )
    }
}

impl crate::data::message::UserMessageSink for Front {
    fn write_message(&mut self, _msg: crate::data::message::UserMessage) {}
    fn replay_queued(&mut self) {}
}

impl WorkflowFrontend for Front {
    fn show_workflow_control_board(
        &mut self,
        _state: &WorkflowState,
        available: &AvailableActions,
    ) -> Result<NextAction, EngineError> {
        self.rec.lock().unwrap().boards.push(available.clone());
        Ok(self.actions.pop_front().unwrap_or(NextAction::Abort))
    }
    fn supports_interactive_recovery(&self) -> bool {
        self.interactive
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
        Ok(self.retry_tick.clone())
    }
    fn report_workflow_completed(&mut self, outcome: &WorkflowOutcome) {
        self.rec.lock().unwrap().completed = Some(outcome.clone());
    }
    fn attach_engine(&mut self, handles: EngineHandles) {
        if let Some(tx) = handles.requests {
            *self.engine_tx.lock().unwrap() = Some(tx);
        }
    }
    fn report_parallel_step_exited(&mut self, step_name: &str, exit_code: i32) {
        self.rec
            .lock()
            .unwrap()
            .exited
            .push((step_name.to_string(), exit_code));
        if let Some(hook) = self.on_exit.as_mut() {
            hook(step_name, exit_code);
        }
    }
    fn report_parallel_step_stuck(&mut self, step_name: &str) {
        self.rec.lock().unwrap().stuck.push(step_name.to_string());
    }
    fn report_parallel_step_unstuck(&mut self, step_name: &str) {
        self.rec.lock().unwrap().unstuck.push(step_name.to_string());
    }
    fn report_parallel_group_finished(&mut self) {
        self.rec.lock().unwrap().group_finished += 1;
    }
    fn parallel_step_yolo_countdown_started(&mut self, step_name: &str) {
        self.rec
            .lock()
            .unwrap()
            .countdown_started
            .push(step_name.to_string());
    }
    fn parallel_step_yolo_countdown_tick(
        &mut self,
        _: &str,
        _: Duration,
        _: Duration,
    ) -> Result<YoloTickOutcome, EngineError> {
        Ok(self.parallel_tick.clone())
    }
    fn parallel_step_yolo_countdown_finished(&mut self, step_name: &str) {
        self.rec
            .lock()
            .unwrap()
            .countdown_finished
            .push(step_name.to_string());
    }
}

// ── Builders ─────────────────────────────────────────────────────────────

struct Harness {
    engine: WorkflowEngine,
    rec: Arc<Mutex<Rec>>,
    launched: Arc<Mutex<Vec<String>>>,
    cancelled: Arc<Mutex<Vec<String>>>,
    stopped: Arc<Mutex<Vec<String>>>,
    auth_calls: Arc<AtomicUsize>,
    engine_tx: tokio::sync::mpsc::UnboundedSender<EngineRequest>,
    _tmp: tempfile::TempDir,
}

fn harness(
    steps: Vec<WorkflowStep>,
    factory: ScriptedFactory,
    front: Front,
    rec: Arc<Mutex<Rec>>,
    max_concurrent: Option<usize>,
) -> Harness {
    let tmp = tempfile::tempdir().unwrap();
    let session = make_session(&tmp);
    let workflow = make_workflow(Some("wf-coverage"), Some("claude"), steps);
    let launched = Arc::clone(&factory.launched);
    let cancelled = Arc::clone(&factory.cancelled);
    let stopped = Arc::clone(&factory.stopped);
    let auth_calls = Arc::clone(&factory.auth_calls);
    let engine_tx_slot = Arc::clone(&front.engine_tx);
    let mut engine = WorkflowEngine::new(
        &session,
        WorkflowSpec::new(workflow).with_work_item_context(None),
        WorkflowEngineDeps {
            frontend: Box::new(front),
            agent_factory: Box::new(factory),
        },
    )
    .unwrap();
    engine.max_concurrent = max_concurrent;
    let engine_tx = engine_tx_slot.lock().unwrap().clone().unwrap();
    Harness {
        engine,
        rec,
        launched,
        cancelled,
        stopped,
        auth_calls,
        engine_tx,
        _tmp: tmp,
    }
}

fn steps(names: &[&str]) -> Vec<WorkflowStep> {
    names.iter().map(|n| make_step(n, &[], None)).collect()
}

fn launched(h: &Harness) -> Vec<String> {
    h.launched.lock().unwrap().clone()
}

/// Which steps' containers were killed. Deduplicated: dropping an in-flight
/// wait also cancels its backend from a background thread, so a killed slot
/// can be reported twice.
fn cancelled_set(h: &Harness) -> Vec<String> {
    let mut names = h.cancelled.lock().unwrap().clone();
    names.sort();
    names.dedup();
    names
}

fn is_failed(state: Option<&StepState>, code: i32) -> bool {
    matches!(state, Some(StepState::Failed { exit_code, .. }) if *exit_code == code)
}

// ── Failures inside a running group ──────────────────────────────────────

#[tokio::test]
async fn every_non_abort_failure_is_recorded_in_exit_order_and_the_group_drains() {
    let (front, rec) = Front::new([]);
    let factory = ScriptedFactory::new(&[("a", &[Script::Exit(3)]), ("b", &[Script::Exit(4)])]);
    let mut h = harness(steps(&["a", "b"]), factory, front, rec, None);
    let group = h.engine.workflow.steps.clone();
    let GroupOutcome::Drained { failed } = h.engine.run_parallel_group(group).await.unwrap() else {
        panic!("a non-abort failure must let the group drain");
    };
    let exited = h.rec.lock().unwrap().exited.clone();
    assert_eq!(failed, exited, "failures are carried in exit order");
    let mut sorted = failed.clone();
    sorted.sort();
    assert_eq!(sorted, vec![("a".into(), 3), ("b".into(), 4)]);
    assert!(is_failed(h.engine.state().status_of("a"), 3));
    assert!(is_failed(h.engine.state().status_of("b"), 4));
    assert!(h.engine.active_steps.is_empty());
    assert_eq!(h.rec.lock().unwrap().group_finished, 1);
    assert!(!h.engine.abort_on_failure_triggered());
}

#[tokio::test]
async fn a_failure_in_the_only_slot_never_launches_the_queue() {
    let (front, rec) = Front::new([]);
    let factory = ScriptedFactory::new(&[("a", &[Script::Exit(1)])]);
    let mut h = harness(steps(&["a", "b", "c"]), factory, front, rec, Some(1));
    let group = h.engine.workflow.steps.clone();
    let GroupOutcome::Drained { failed } = h.engine.run_parallel_group(group).await.unwrap() else {
        panic!("expected a drained group");
    };
    assert_eq!(failed, vec![("a".to_string(), 1)]);
    assert_eq!(
        launched(&h),
        ["a"],
        "no queued step may start after a failure"
    );
    assert!(!matches!(
        h.engine.state().status_of("b"),
        Some(StepState::Succeeded | StepState::Failed { .. })
    ));
}

/// docs/05-workflows.md ("How the engine schedules steps"): a step that fails
/// without `abort_on_failure` "stops new steps from being queued into the
/// group, but lets its already-running siblings keep going" — including when
/// a sibling succeeds after the failure.
#[tokio::test]
async fn a_failure_stops_queued_launches_even_after_a_peer_succeeds() {
    let (mut front, rec) = Front::new([]);
    let factory = ScriptedFactory::new(&[
        ("a", &[Script::Exit(1)]),
        ("b", &[Script::Hold]),
        ("c", &[Script::Exit(0)]),
    ]);
    // Release "b" (successfully) only once "a"'s failure has been handled.
    let holds = Arc::clone(&factory.holds);
    front.on_exit = Some(Box::new(move |name, code| {
        if name == "a" && code == 1 {
            release(&holds, "b", 0);
        }
    }));
    let mut h = harness(steps(&["a", "b", "c"]), factory, front, rec, Some(2));
    let group = h.engine.workflow.steps.clone();
    let outcome = h.engine.run_parallel_group(group).await.unwrap();
    assert!(matches!(outcome, GroupOutcome::Drained { .. }));
    assert_eq!(launched(&h), ["a", "b"], "'c' must stay queued");
}

#[tokio::test]
async fn abort_on_failure_kills_running_peers_and_cancels_queued_steps() {
    let (front, rec) = Front::new([]);
    let factory = ScriptedFactory::new(&[("a", &[Script::Exit(9)]), ("b", &[Script::Hold])]);
    let mut all = steps(&["a", "b", "c"]);
    all[0].abort_on_failure = true;
    let mut h = harness(all, factory, front, rec, Some(2));
    let group = h.engine.workflow.steps.clone();
    let outcome = h.engine.run_parallel_group(group).await.unwrap();
    assert!(matches!(
        outcome,
        GroupOutcome::Ended(WorkflowOutcome::Aborted)
    ));
    assert!(h.engine.abort_on_failure_triggered());
    assert_eq!(launched(&h), ["a", "b"], "the queued step never starts");
    assert_eq!(cancelled_set(&h), ["b"]);
    for name in ["a", "b", "c"] {
        assert_eq!(
            h.engine.state().status_of(name),
            Some(&StepState::Cancelled),
            "{name}"
        );
    }
    let rec = h.rec.lock().unwrap();
    assert!(rec.exited.contains(&("b".into(), KILLED_EXIT_CODE)));
    assert_eq!(rec.completed, Some(WorkflowOutcome::Aborted));
}

#[tokio::test]
async fn auth_recovery_relaunches_a_failed_peer_exactly_once() {
    let (front, rec) = Front::new([]);
    let mut factory = ScriptedFactory::new(&[
        ("a", &[Script::Exit(1), Script::Exit(1)]),
        ("b", &[Script::Exit(0)]),
    ]);
    factory.recover_auth = true;
    let mut h = harness(steps(&["a", "b"]), factory, front, rec, None);
    let group = h.engine.workflow.steps.clone();
    let GroupOutcome::Drained { failed } = h.engine.run_parallel_group(group).await.unwrap() else {
        panic!("expected a drained group");
    };
    assert_eq!(failed, vec![("a".to_string(), 1)]);
    let mut l = launched(&h);
    l.sort();
    assert_eq!(l, ["a", "a", "b"]);
    assert_eq!(
        h.auth_calls.load(Ordering::Relaxed),
        1,
        "the second failure is not offered to auth recovery again"
    );
    assert_eq!(h.engine.state().status_of("b"), Some(&StepState::Succeeded));
}

// ── Cancel and the board while a group runs ──────────────────────────────

#[tokio::test]
async fn an_external_cancel_stops_every_slot_and_launches_nothing_queued() {
    let (front, rec) = Front::new([]);
    let factory = ScriptedFactory::new(&[("a", &[Script::Hold]), ("b", &[Script::Hold])]);
    let mut h = harness(steps(&["a", "b", "c"]), factory, front, rec, Some(2));
    let token = CancellationToken::new();
    h.engine.set_cancellation(token.clone());
    token.cancel();
    let group = h.engine.workflow.steps.clone();
    let outcome = h.engine.run_parallel_group(group).await.unwrap();
    assert!(matches!(
        outcome,
        GroupOutcome::Ended(WorkflowOutcome::Cancelled)
    ));
    assert_eq!(launched(&h), ["a", "b"]);
    let mut stopped = h.stopped.lock().unwrap().clone();
    stopped.sort();
    assert_eq!(
        stopped,
        ["c-a-1", "c-b-2"],
        "both containers stopped via the runtime"
    );
    assert_eq!(cancelled_set(&h), ["a", "b"]);
    assert!(!h.engine.abort_on_failure_triggered());
    assert!(h.engine.active_steps.is_empty());
}

#[tokio::test]
async fn pausing_from_the_board_mid_group_resets_running_steps_to_pending() {
    let (front, rec) = Front::new([NextAction::Pause]);
    let factory = ScriptedFactory::new(&[("a", &[Script::Hold]), ("b", &[Script::Exit(0)])]);
    let mut h = harness(steps(&["a", "b", "c"]), factory, front, rec, Some(2));
    // The board opens once both slots are live (requests are only read by
    // the group loop, after the initial launches).
    h.engine_tx
        .send(EngineRequest::OpenControlBoard {
            step_name: "a".into(),
        })
        .unwrap();
    let group = h.engine.workflow.steps.clone();
    let outcome = h.engine.run_parallel_group(group).await.unwrap();
    assert!(matches!(
        outcome,
        GroupOutcome::Ended(WorkflowOutcome::Paused)
    ));
    assert_eq!(cancelled_set(&h), ["a"]);
    assert_eq!(h.engine.state().status_of("a"), Some(&StepState::Pending));
    assert!(!h.engine.abort_on_failure_triggered());
    let rec = h.rec.lock().unwrap();
    assert_eq!(rec.boards.len(), 1);
    assert_eq!(rec.completed, Some(WorkflowOutcome::Paused));
}

#[tokio::test]
async fn aborting_from_the_board_mid_group_cancels_everything_unfinished() {
    let (front, rec) = Front::new([NextAction::Abort]);
    let factory = ScriptedFactory::new(&[("a", &[Script::Hold]), ("b", &[Script::Hold])]);
    let mut h = harness(steps(&["a", "b"]), factory, front, rec, None);
    h.engine_tx
        .send(EngineRequest::OpenControlBoard {
            step_name: "b".into(),
        })
        .unwrap();
    let group = h.engine.workflow.steps.clone();
    let outcome = h.engine.run_parallel_group(group).await.unwrap();
    assert!(matches!(
        outcome,
        GroupOutcome::Ended(WorkflowOutcome::Aborted)
    ));
    assert_eq!(cancelled_set(&h), ["a", "b"]);
    assert_eq!(h.engine.state().status_of("a"), Some(&StepState::Cancelled));
    assert_eq!(h.engine.state().status_of("b"), Some(&StepState::Cancelled));
    // The board was scoped to the focused container.
    assert_eq!(h.engine.current_step_name.as_deref(), Some("b"));
}

#[tokio::test]
async fn stuck_requests_mid_group_mark_only_the_named_slot() {
    let (front, rec) = Front::new([NextAction::Abort]);
    let factory = ScriptedFactory::new(&[("a", &[Script::Hold]), ("b", &[Script::Hold])]);
    let mut h = harness(steps(&["a", "b"]), factory, front, rec, None);
    for request in [
        EngineRequest::StepStuck {
            step_name: "b".into(),
        },
        EngineRequest::StepUnstuck {
            step_name: "b".into(),
        },
        EngineRequest::StepStuck {
            step_name: "a".into(),
        },
        // End the run deterministically once the stuck traffic is handled.
        EngineRequest::OpenControlBoard {
            step_name: "a".into(),
        },
    ] {
        h.engine_tx.send(request).unwrap();
    }
    let group = h.engine.workflow.steps.clone();
    h.engine.run_parallel_group(group).await.unwrap();
    let rec = h.rec.lock().unwrap();
    assert_eq!(rec.stuck, ["b", "a"]);
    assert_eq!(rec.unstuck, ["b"]);
    assert!(
        rec.countdown_started.is_empty(),
        "no yolo countdown without yolo"
    );
}

// ── Stuck / yolo handlers on hand-built slots ────────────────────────────

fn slot(name: &str, cancelled: &Arc<Mutex<Vec<String>>>) -> ActiveParallelStep {
    let cancelled = Arc::clone(cancelled);
    let n = name.to_string();
    ActiveParallelStep {
        step_name: name.to_string(),
        execution: None,
        cancel_handle: Some(CancelHandle::new(move || {
            cancelled.lock().unwrap().push(n.clone());
            Ok(())
        })),
        container_name: format!("c-{name}"),
        handle: fake_handle(&format!("c-{name}")),
        output_tail: None,
        awman_killed: false,
        stuck: false,
        yolo_deadline: None,
        countdown_kind: CountdownKind::StuckStep,
        output_watch: None,
        last_report: None,
        reported_problem: None,
        program_status: None,
        agent: AgentName::new("claude").unwrap(),
        model: None,
        launch_id: 0,
    }
}

fn slot_of<'a>(engine: &'a WorkflowEngine, name: &str) -> &'a ActiveParallelStep {
    engine
        .active_steps
        .iter()
        .find(|s| s.step_name == name)
        .unwrap()
}

fn hand_built(yolo: bool, tick: YoloTickOutcome) -> Harness {
    let (mut front, rec) = Front::new([]);
    front.parallel_tick = tick;
    let mut h = harness(
        steps(&["a", "b", "c"]),
        ScriptedFactory::default(),
        front,
        rec,
        Some(2),
    );
    h.engine.set_yolo(yolo);
    let cancelled = Arc::clone(&h.cancelled);
    h.engine.active_steps.push(slot("a", &cancelled));
    h.engine.active_steps.push(slot("b", &cancelled));
    h
}

#[tokio::test]
async fn without_yolo_stuck_and_unstuck_toggle_only_the_named_slot() {
    let mut h = hand_built(false, YoloTickOutcome::Continue);
    h.engine.handle_parallel_stuck_event("b", StuckEvent::Stuck);
    assert!(slot_of(&h.engine, "b").stuck);
    assert!(!slot_of(&h.engine, "a").stuck);
    assert!(slot_of(&h.engine, "b").yolo_deadline.is_none());
    h.engine
        .handle_parallel_stuck_event("b", StuckEvent::Unstuck);
    assert!(!slot_of(&h.engine, "b").stuck);
    let rec = h.rec.lock().unwrap();
    assert_eq!(rec.stuck, ["b"]);
    assert_eq!(rec.unstuck, ["b"]);
    assert!(
        rec.countdown_finished.is_empty(),
        "no countdown was running"
    );
    assert!(h.cancelled.lock().unwrap().is_empty(), "stuck never kills");
}

#[tokio::test]
async fn with_yolo_a_stuck_slot_starts_one_countdown_and_unstuck_ends_it() {
    let mut h = hand_built(true, YoloTickOutcome::Continue);
    h.engine.handle_parallel_stuck_event("a", StuckEvent::Stuck);
    let first = slot_of(&h.engine, "a").yolo_deadline.expect("countdown");
    // A repeated stuck report does not restart the countdown.
    h.engine.handle_parallel_stuck_event("a", StuckEvent::Stuck);
    assert_eq!(slot_of(&h.engine, "a").yolo_deadline, Some(first));
    assert!(slot_of(&h.engine, "b").yolo_deadline.is_none());
    assert!(
        !slot_of(&h.engine, "a").stuck,
        "yolo stuck is a countdown, not a mark"
    );

    h.engine
        .handle_parallel_stuck_event("a", StuckEvent::Unstuck);
    assert!(slot_of(&h.engine, "a").yolo_deadline.is_none());
    let rec = h.rec.lock().unwrap();
    assert_eq!(rec.countdown_started, ["a"]);
    assert_eq!(rec.countdown_finished, ["a"]);
    assert!(rec.stuck.is_empty());
}

#[tokio::test]
async fn startup_grace_expiry_marks_only_that_slot_awman_killed() {
    let mut h = hand_built(false, YoloTickOutcome::Continue);
    h.engine
        .handle_parallel_stuck_event("b", StuckEvent::StartupGraceExpired);
    assert!(slot_of(&h.engine, "b").awman_killed);
    assert!(!slot_of(&h.engine, "a").awman_killed);
    assert!(h.rec.lock().unwrap().stuck.is_empty());
}

#[tokio::test]
async fn stuck_events_for_unknown_steps_change_no_slot() {
    let mut h = hand_built(true, YoloTickOutcome::Continue);
    for event in [
        StuckEvent::Stuck,
        StuckEvent::StartupGraceExpired,
        StuckEvent::Unstuck,
    ] {
        h.engine.handle_parallel_stuck_event("ghost", event);
    }
    for name in ["a", "b"] {
        let s = slot_of(&h.engine, name);
        assert!(!s.stuck && !s.awman_killed && s.yolo_deadline.is_none());
    }
    assert!(h.rec.lock().unwrap().countdown_started.is_empty());
}

fn tick(h: &mut Harness, queue: &mut VecDeque<WorkflowStep>) {
    let mut waits: ParallelWaits = FuturesUnordered::new();
    let (stuck_tx, _stuck_rx) = tokio::sync::mpsc::unbounded_channel();
    h.engine
        .tick_parallel_yolo(
            &mut waits,
            queue,
            2,
            true,
            timing::YOLO_COUNTDOWN_DURATION,
            &stuck_tx,
            false,
            &mut HashSet::new(),
            &mut HashSet::new(),
        )
        .unwrap();
}

#[tokio::test]
async fn an_expired_yolo_countdown_kills_that_slot_advances_and_fills_it_from_the_queue() {
    let mut h = hand_built(true, YoloTickOutcome::Continue);
    h.engine.active_steps[1].yolo_deadline = Some(Instant::now() - Duration::from_secs(1));
    let mut queue: VecDeque<WorkflowStep> = steps(&["c"]).into();
    tick(&mut h, &mut queue);

    assert_eq!(
        *h.cancelled.lock().unwrap(),
        ["b"],
        "only the expired slot is killed"
    );
    assert_eq!(h.engine.state().status_of("b"), Some(&StepState::Succeeded));
    assert!(queue.is_empty());
    assert_eq!(launched(&h), ["c"], "the freed slot takes the queued step");
    let names: Vec<_> = h
        .engine
        .active_steps
        .iter()
        .map(|s| s.step_name.as_str())
        .collect();
    assert_eq!(names, ["a", "c"]);
    let rec = h.rec.lock().unwrap();
    assert_eq!(rec.exited, [("b".to_string(), KILLED_EXIT_CODE)]);
    assert_eq!(rec.countdown_finished, ["b"]);
}

#[tokio::test]
async fn a_running_yolo_countdown_only_advances_when_forced() {
    let far = Instant::now() + Duration::from_secs(3600);

    // Continue with time left: nothing happens.
    let mut h = hand_built(true, YoloTickOutcome::Continue);
    h.engine.active_steps[0].yolo_deadline = Some(far);
    let mut queue = VecDeque::new();
    tick(&mut h, &mut queue);
    assert_eq!(slot_of(&h.engine, "a").yolo_deadline, Some(far));
    assert!(h.cancelled.lock().unwrap().is_empty());

    // Cancel: the countdown stops, the container keeps running.
    let mut h = hand_built(true, YoloTickOutcome::Cancel);
    h.engine.active_steps[0].yolo_deadline = Some(far);
    tick(&mut h, &mut queue);
    assert!(slot_of(&h.engine, "a").yolo_deadline.is_none());
    assert!(h.cancelled.lock().unwrap().is_empty());
    assert_eq!(h.rec.lock().unwrap().countdown_finished, ["a"]);

    // AdvanceNow: advances immediately despite the time left; with an empty
    // queue nothing new launches.
    let mut h = hand_built(true, YoloTickOutcome::AdvanceNow);
    h.engine.active_steps[0].yolo_deadline = Some(far);
    tick(&mut h, &mut queue);
    assert_eq!(*h.cancelled.lock().unwrap(), ["a"]);
    assert_eq!(h.engine.state().status_of("a"), Some(&StepState::Succeeded));
    assert!(launched(&h).is_empty());
    assert_eq!(h.engine.active_steps.len(), 1);
}

#[tokio::test]
async fn slots_without_a_countdown_are_never_ticked() {
    let mut h = hand_built(true, YoloTickOutcome::AdvanceNow);
    let mut queue: VecDeque<WorkflowStep> = steps(&["c"]).into();
    tick(&mut h, &mut queue);
    assert!(h.cancelled.lock().unwrap().is_empty());
    assert_eq!(queue.len(), 1);
    assert_eq!(h.engine.active_steps.len(), 2);
}

// ── After the group drains: control.rs failure handling ──────────────────

#[tokio::test]
async fn the_post_group_failure_board_is_scoped_to_the_failed_step() {
    let (front, rec) = Front::new([NextAction::Abort]);
    // "a" fails while "b" is still held; "b" then exits 0 so the most recent
    // exit (b's) does not describe the failure.
    let factory = ScriptedFactory::new(&[("a", &[Script::Exit(5)]), ("b", &[Script::Hold])]);
    let holds = Arc::clone(&factory.holds);
    let mut front = front;
    front.on_exit = Some(Box::new(move |name, code| {
        if name == "a" && code == 5 {
            release(&holds, "b", 0);
        }
    }));
    let mut all = steps(&["a", "b"]);
    all.push(make_step("after", &["a", "b"], None));
    let mut h = harness(all, factory, front, rec, None);
    let outcome = h.engine.run_to_completion().await.unwrap();
    assert_eq!(outcome, WorkflowOutcome::Aborted);

    let rec = h.rec.lock().unwrap();
    assert_eq!(rec.boards.len(), 1);
    let board = &rec.boards[0];
    assert_eq!(board.focused_step.as_deref(), Some("a"));
    let failure = board.step_failure.as_ref().expect("a failure board");
    assert_eq!(failure.step_name, "a");
    assert_eq!(failure.exit_code, 5);
    assert!(
        !failure
            .detail_lines
            .iter()
            .any(|l| l.starts_with("Ran for")),
        "a later peer's exit must not be reported as the failure's runtime: {:?}",
        failure.detail_lines
    );
    assert!(!board.can_finish_workflow);
    assert!(board.can_restart_current_step);
    assert_eq!(h.engine.state().status_of("b"), Some(&StepState::Succeeded));
    assert_eq!(launched(&h), ["a", "b"], "'after' never ran");
}

#[tokio::test]
async fn unattended_group_failures_are_each_retried_once_then_complete() {
    let (mut front, rec) = Front::new([]);
    front.interactive = false;
    front.retry_tick = YoloTickOutcome::AdvanceNow;
    let factory = ScriptedFactory::new(&[
        ("a", &[Script::Exit(1), Script::Exit(0)]),
        ("b", &[Script::Exit(2), Script::Exit(0)]),
    ]);
    let mut h = harness(steps(&["a", "b"]), factory, front, rec, None);
    let outcome = h.engine.run_to_completion().await.unwrap();
    assert_eq!(outcome, WorkflowOutcome::Completed);
    let mut l = launched(&h);
    l.sort();
    assert_eq!(
        l,
        ["a", "a", "b", "b"],
        "each failed peer gets exactly one retry"
    );
    assert!(h.rec.lock().unwrap().boards.is_empty());
}

#[tokio::test]
async fn unattended_group_failure_fails_the_workflow_on_its_second_failure() {
    let (mut front, rec) = Front::new([]);
    front.interactive = false;
    front.retry_tick = YoloTickOutcome::AdvanceNow;
    let factory = ScriptedFactory::new(&[
        ("a", &[Script::Exit(1), Script::Exit(7)]),
        ("b", &[Script::Exit(0)]),
    ]);
    let mut h = harness(steps(&["a", "b"]), factory, front, rec, None);
    let outcome = h.engine.run_to_completion().await.unwrap();
    assert_eq!(
        outcome,
        WorkflowOutcome::Failed {
            last_step: "a".into(),
            exit_code: 7
        }
    );
    assert!(is_failed(h.engine.state().status_of("a"), 7));
    assert_eq!(h.engine.state().status_of("b"), Some(&StepState::Succeeded));
}

#[tokio::test]
async fn unattended_group_failure_with_a_cancelled_countdown_fails_without_retry() {
    let (mut front, rec) = Front::new([]);
    front.interactive = false;
    front.retry_tick = YoloTickOutcome::Cancel;
    let factory = ScriptedFactory::new(&[("a", &[Script::Exit(1)]), ("b", &[Script::Exit(0)])]);
    let mut h = harness(steps(&["a", "b", "c"]), factory, front, rec, Some(2));
    let outcome = h.engine.run_to_completion().await.unwrap();
    assert_eq!(
        outcome,
        WorkflowOutcome::Failed {
            last_step: "a".into(),
            exit_code: 1
        }
    );
    // No retry was launched; a queued step that never ran is cancelled.
    assert_eq!(launched(&h).iter().filter(|n| *n == "a").count(), 1);
    if !launched(&h).contains(&"c".to_string()) {
        assert_eq!(h.engine.state().status_of("c"), Some(&StepState::Cancelled));
    }
}

#[tokio::test]
async fn a_cancel_raised_by_a_peer_exit_ends_the_group_before_any_recovery() {
    let (mut front, rec) = Front::new([NextAction::RestartCurrentStep]);
    let token = CancellationToken::new();
    let factory = ScriptedFactory::new(&[
        ("a", &[Script::Exit(1)]),
        ("b", &[Script::Hold]),
        ("c", &[Script::Exit(0)]),
    ]);
    // "a" fails and releases "b"; "b"'s successful exit raises the cancel.
    let holds = Arc::clone(&factory.holds);
    let cancel_on_exit = token.clone();
    front.on_exit = Some(Box::new(move |name, code| match (name, code) {
        ("a", 1) => release(&holds, "b", 0),
        ("b", 0) => cancel_on_exit.cancel(),
        _ => {}
    }));
    let mut h = harness(steps(&["a", "b", "c"]), factory, front, rec, Some(2));
    h.engine.set_cancellation(token);
    let outcome = h.engine.run_to_completion().await.unwrap();
    assert_eq!(outcome, WorkflowOutcome::Cancelled);
    assert_eq!(
        launched(&h),
        ["a", "b"],
        "nothing launches once a cancel arrived"
    );
    assert!(
        h.rec.lock().unwrap().boards.is_empty(),
        "no recovery board after a cancel"
    );
    assert!(!h.engine.abort_on_failure_triggered());
}
