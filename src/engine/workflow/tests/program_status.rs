//! How the workflow engine reacts to a step's agent reporting its own status
//! over OSC 7501 (WI 0141, `engine::workflow::program_status`).
//!
//! Each held container gets its own `StuckEvent` broadcast sender, which the
//! test keeps, so it can play the part of the bridge's `StuckDetector` and
//! publish `Reported` events exactly as a reporting agent would. Countdown
//! "expiry" is the frontend answering `AdvanceNow`, the same shortcut the
//! unattended tests use; nothing waits out a real countdown.

use super::super::*;
use super::*;

use crate::data::program_status::{BlockedKind, EffectiveStatus, ProgramState};

// ── Scripted factory with a test-owned stuck sender per launch ───────────

struct Launch {
    step: String,
    cancelled: Arc<AtomicBool>,
    completion: CompletionArc,
    events: Arc<tokio::sync::broadcast::Sender<StuckEvent>>,
    tail: Arc<OutputTail>,
    /// The execution's OSC 7501 tracker, as the bridge would own it.
    tracker: crate::engine::agent_runtime::program_status::SharedProgramStatusTracker,
}

type Launches = Arc<Mutex<Vec<Launch>>>;

/// Every launch is held until the test completes it (or the engine cancels
/// it, which exits with -1 like the shared `BlockingBackend`), except steps
/// listed in `instant`, which have already exited 0.
struct StatusFactory {
    launches: Launches,
    instant: Vec<String>,
}

impl StatusFactory {
    fn new(instant: &[&str]) -> (Self, Launches) {
        let launches: Launches = Arc::default();
        (
            Self {
                launches: launches.clone(),
                instant: instant.iter().map(|s| s.to_string()).collect(),
            },
            launches,
        )
    }
}

impl AgentExecutionFactory for StatusFactory {
    fn execution_for_step(
        &self,
        step: &WorkflowStep,
        _session: &Session,
        _runtime: &WorkflowRuntimeContext,
    ) -> Result<AgentExecution, EngineError> {
        let now = Utc::now();
        let n = self.launches.lock().unwrap().len();
        let handle = AgentHandle {
            id: format!("id-{}-{n}", step.name),
            image_tag: "img".into(),
            name: format!("c-{}-{n}", step.name),
            started_at: now,
        };
        if self.instant.contains(&step.name) {
            return Ok(AgentExecution::finished(
                handle,
                AgentExitInfo {
                    exit_code: 0,
                    signal: None,
                    started_at: now,
                    ended_at: now,
                },
            ));
        }
        let (cancelled, completion) = make_blocking_entry();
        let (tx, _) = tokio::sync::broadcast::channel(16);
        let events = Arc::new(tx);
        let tail = Arc::new(OutputTail::with_default_capacity());
        let tracker = crate::engine::agent_runtime::program_status::ProgramStatusTracker::shared();
        self.launches.lock().unwrap().push(Launch {
            step: step.name.clone(),
            cancelled: cancelled.clone(),
            completion: completion.clone(),
            events: events.clone(),
            tail: tail.clone(),
            tracker: tracker.clone(),
        });
        Ok(AgentExecution::new(
            handle,
            Box::new(BlockingBackend {
                cancel_flag: cancelled,
                completion,
            }),
            events,
            Some(tail),
        )
        .with_program_status(tracker))
    }

    fn inject_prompt(&self, _: &AgentExecution, _: &str) -> Result<Option<()>, EngineError> {
        Ok(None)
    }
}

// ── Recording frontend ───────────────────────────────────────────────────

#[derive(Default)]
struct Log {
    countdowns: Vec<(String, CountdownKind)>,
    finished: Vec<String>,
    boards: Vec<AvailableActions>,
    statuses: Vec<(String, WorkflowStepStatus)>,
    agent_status: Vec<(String, Option<ProgramState>)>,
    /// The blocked kind and message of every published status.
    agent_payloads: Vec<(Option<BlockedKind>, Option<String>)>,
    parallel_countdowns: Vec<(String, CountdownKind)>,
}

struct StatusFront {
    log: Arc<Mutex<Log>>,
    actions: VecDeque<NextAction>,
    interactive: bool,
    /// What every countdown tick answers; a test may change it mid-run.
    tick: Arc<Mutex<YoloTickOutcome>>,
    /// What `user_interacting_with` answers (active-tab suppression).
    interacting: Arc<AtomicBool>,
}

impl StatusFront {
    fn new(interactive: bool, tick: YoloTickOutcome, actions: &[NextAction]) -> Self {
        Self {
            log: Arc::default(),
            actions: actions.iter().cloned().collect(),
            interactive,
            tick: Arc::new(Mutex::new(tick)),
            interacting: Arc::default(),
        }
    }
}

impl crate::data::message::UserMessageSink for StatusFront {
    fn write_message(&mut self, _: crate::data::message::UserMessage) {}
    fn replay_queued(&mut self) {}
}

impl WorkflowFrontend for StatusFront {
    fn show_workflow_control_board(
        &mut self,
        _: &WorkflowState,
        available: &AvailableActions,
    ) -> Result<NextAction, EngineError> {
        self.log.lock().unwrap().boards.push(available.clone());
        Ok(self
            .actions
            .pop_front()
            .unwrap_or_else(|| api_headless_next_action(available)))
    }
    fn yolo_countdown_tick(
        &mut self,
        _: &str,
        _: Duration,
        _: Duration,
    ) -> Result<YoloTickOutcome, EngineError> {
        Ok(self.tick.lock().unwrap().clone())
    }
    fn yolo_countdown_started(&mut self, step: &str, kind: CountdownKind) {
        self.log
            .lock()
            .unwrap()
            .countdowns
            .push((step.to_string(), kind));
    }
    fn yolo_countdown_finished(&mut self, step: &str) {
        self.log.lock().unwrap().finished.push(step.to_string());
    }
    fn parallel_step_countdown_started(&mut self, step: &str, kind: CountdownKind) {
        self.log
            .lock()
            .unwrap()
            .parallel_countdowns
            .push((step.to_string(), kind));
    }
    fn parallel_step_yolo_countdown_tick(
        &mut self,
        _: &str,
        _: Duration,
        _: Duration,
    ) -> Result<YoloTickOutcome, EngineError> {
        Ok(self.tick.lock().unwrap().clone())
    }
    fn report_agent_status(&mut self, step: &str, status: Option<&EffectiveStatus>) {
        self.log
            .lock()
            .unwrap()
            .agent_status
            .push((step.to_string(), status.map(|s| s.state)));
        self.log.lock().unwrap().agent_payloads.push((
            status.and_then(|s| s.kind),
            status.and_then(|s| s.msg.clone()),
        ));
    }
    fn report_step_status(&mut self, step: &WorkflowStep, status: WorkflowStepStatus) {
        self.log
            .lock()
            .unwrap()
            .statuses
            .push((step.name.clone(), status));
    }
    fn supports_interactive_recovery(&self) -> bool {
        self.interactive
    }
    fn user_interacting_with(&self, _: &str) -> bool {
        self.interacting.load(Ordering::SeqCst)
    }
    fn confirm_resume(&mut self, _: &ResumeMismatch) -> Result<bool, EngineError> {
        Ok(true)
    }
    fn report_workflow_completed(&mut self, _: &WorkflowOutcome) {}
}

// ── Harness ──────────────────────────────────────────────────────────────

struct Run {
    task: tokio::task::JoinHandle<(WorkflowEngine, Result<WorkflowOutcome, EngineError>)>,
    launches: Launches,
    log: Arc<Mutex<Log>>,
    tick: Arc<Mutex<YoloTickOutcome>>,
    _tmp: tempfile::TempDir,
}

/// Steps run in the order given, each depending on the one before.
fn chain(names: &[&str]) -> Vec<WorkflowStep> {
    names
        .iter()
        .enumerate()
        .map(|(i, n)| {
            let deps: Vec<&str> = if i == 0 { vec![] } else { vec![names[i - 1]] };
            make_step(n, &deps, None)
        })
        .collect()
}

fn start(
    steps: Vec<WorkflowStep>,
    instant: &[&str],
    yolo: bool,
    front: StatusFront,
    max_concurrent: Option<usize>,
) -> Run {
    let tmp = tempfile::tempdir().unwrap();
    let session = make_session(&tmp);
    let (factory, launches) = StatusFactory::new(instant);
    let log = front.log.clone();
    let tick = front.tick.clone();
    let mut engine = make_engine_dyn(
        &session,
        make_workflow(Some("wf-status"), Some("claude"), steps),
        Box::new(factory),
        Box::new(front),
    );
    engine.set_yolo(yolo);
    engine.max_concurrent = max_concurrent;
    let task = tokio::spawn(async move {
        let result = engine.run_to_completion().await;
        (engine, result)
    });
    Run {
        task,
        launches,
        log,
        tick,
        _tmp: tmp,
    }
}

fn make_engine_dyn(
    session: &Session,
    workflow: Workflow,
    factory: Box<dyn AgentExecutionFactory>,
    frontend: Box<dyn WorkflowFrontend>,
) -> WorkflowEngine {
    WorkflowEngine::new(
        session,
        WorkflowSpec::new(workflow).with_work_item_context(None),
        WorkflowEngineDeps {
            frontend,
            agent_factory: factory,
        },
    )
    .unwrap()
}

async fn until(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !cond() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

impl Run {
    /// Wait for launch `n` (0-based) and for the engine to subscribe to it.
    async fn launched(&self, n: usize) -> Arc<tokio::sync::broadcast::Sender<StuckEvent>> {
        until(&format!("launch #{n}"), || {
            self.launches
                .lock()
                .unwrap()
                .get(n)
                .is_some_and(|l| l.events.receiver_count() > 0)
        })
        .await;
        self.launches.lock().unwrap()[n].events.clone()
    }

    fn launch_step(&self, n: usize) -> String {
        self.launches.lock().unwrap()[n].step.clone()
    }

    fn complete(&self, n: usize, code: i32) {
        signal_completion(&self.launches.lock().unwrap()[n].completion, code);
    }

    fn cancelled(&self, n: usize) -> bool {
        self.launches.lock().unwrap()[n]
            .cancelled
            .load(Ordering::Relaxed)
    }

    fn launch_count(&self) -> usize {
        self.launches.lock().unwrap().len()
    }

    /// The agent reported `status` but the detector has not published it
    /// yet (debounced, or torn down at exit): only the tracker knows.
    async fn report_unpublished(&self, n: usize, status: Option<EffectiveStatus>) {
        use crate::data::program_status::{ParsedOsc7501, ProgramStatusReport, ReportState};
        use crate::engine::agent_runtime::program_status::ScanEvent;
        self.launched(n).await;
        let report = ProgramStatusReport {
            id: None,
            state: status
                .as_ref()
                .map_or(ReportState::Clear, |s| ReportState::State(s.state)),
            kind: status.as_ref().and_then(|s| s.kind),
            progress: None,
            app: None,
            title: None,
            msg: status.as_ref().and_then(|s| s.msg.clone()),
        };
        let tracker = self.launches.lock().unwrap()[n].tracker.clone();
        assert!(tracker.lock().unwrap().apply(
            &ScanEvent::Osc(ParsedOsc7501::Report(report)),
            std::time::Instant::now()
        ));
    }

    async fn report(&self, n: usize, status: Option<EffectiveStatus>) {
        let tx = self.launched(n).await;
        tx.send(StuckEvent::Reported(status)).unwrap();
    }

    fn countdowns(&self) -> Vec<(String, CountdownKind)> {
        self.log.lock().unwrap().countdowns.clone()
    }

    fn boards(&self) -> Vec<AvailableActions> {
        self.log.lock().unwrap().boards.clone()
    }

    fn statuses_of(&self, step: &str) -> Vec<WorkflowStepStatus> {
        self.log
            .lock()
            .unwrap()
            .statuses
            .iter()
            .filter(|(s, _)| s == step)
            .map(|(_, st)| st.clone())
            .collect()
    }

    async fn wait_countdowns(&self, n: usize) {
        until(&format!("{n} countdown(s)"), || {
            self.log.lock().unwrap().countdowns.len() >= n
        })
        .await;
    }

    async fn wait_finished(&self, n: usize) {
        until(&format!("{n} finished countdown(s)"), || {
            self.log.lock().unwrap().finished.len() >= n
        })
        .await;
    }

    async fn wait_boards(&self, n: usize) {
        until(&format!("{n} board(s)"), || {
            self.log.lock().unwrap().boards.len() >= n
        })
        .await;
    }

    async fn finish(self) -> (WorkflowEngine, WorkflowOutcome, Arc<Mutex<Log>>) {
        let (engine, result) = tokio::time::timeout(Duration::from_secs(10), self.task)
            .await
            .expect("workflow finished")
            .unwrap();
        (engine, result.unwrap(), self.log)
    }
}

fn status(state: ProgramState) -> Option<EffectiveStatus> {
    Some(EffectiveStatus {
        state,
        kind: None,
        progress: None,
        msg: None,
        app: None,
        id: None,
    })
}

fn with_msg(state: ProgramState, kind: Option<BlockedKind>, msg: &str) -> Option<EffectiveStatus> {
    let mut s = status(state);
    if let Some(s) = s.as_mut() {
        s.kind = kind;
        s.msg = Some(msg.to_string());
    }
    s
}

fn failed_reason(engine: &WorkflowEngine, step: &str) -> Option<String> {
    match engine.state().status_of(step) {
        Some(StepState::Failed { error_message, .. }) => error_message.clone(),
        other => panic!("expected {step} Failed, got {other:?}"),
    }
}

/// A short pause in which nothing may happen.
async fn quiet_window() {
    tokio::time::sleep(Duration::from_millis(150)).await;
}

// ── Yolo ─────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn yolo_done_runs_an_agent_done_countdown_then_advances() {
    let front = StatusFront::new(true, YoloTickOutcome::AdvanceNow, &[]);
    let run = start(chain(&["a", "b"]), &["b"], true, front, None);
    run.report(0, status(ProgramState::Done)).await;
    let (engine, outcome, _) = run_finish_checked(run, |run| {
        assert_eq!(
            run.countdowns(),
            [("a".to_string(), CountdownKind::AgentDone)]
        );
        assert!(run.cancelled(0), "expiry kills the container");
    })
    .await;
    assert_eq!(outcome, WorkflowOutcome::Completed);
    assert_eq!(engine.state().status_of("a"), Some(&StepState::Succeeded));
    assert_eq!(engine.state().status_of("b"), Some(&StepState::Succeeded));
}

/// Run assertions once the engine is done but before the run is consumed.
async fn run_finish_checked(
    run: Run,
    check: impl FnOnce(&Run),
) -> (WorkflowEngine, WorkflowOutcome, Arc<Mutex<Log>>) {
    until("workflow end", || run.task.is_finished()).await;
    check(&run);
    run.finish().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn yolo_done_or_idle_on_the_last_step_completes_the_workflow() {
    for state in [ProgramState::Done, ProgramState::Idle] {
        let front = StatusFront::new(true, YoloTickOutcome::AdvanceNow, &[]);
        let run = start(chain(&["only"]), &[], true, front, None);
        run.report(0, status(state)).await;
        let (engine, outcome, log) = run.finish().await;
        let log = log.lock().unwrap();
        assert_eq!(outcome, WorkflowOutcome::Completed, "{state:?}");
        assert_eq!(
            log.countdowns,
            [("only".to_string(), CountdownKind::AgentDone)],
            "{state:?}: no is_last_step() skip for AgentDone"
        );
        assert!(
            log.boards.is_empty(),
            "{state:?}: the last step completes without a review board"
        );
        assert_eq!(
            engine.state().status_of("only"),
            Some(&StepState::Succeeded)
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn yolo_working_never_starts_a_countdown() {
    let front = StatusFront::new(true, YoloTickOutcome::AdvanceNow, &[]);
    let run = start(chain(&["only"]), &[], true, front, None);
    run.report(0, status(ProgramState::Working)).await;
    quiet_window().await;
    assert!(run.countdowns().is_empty(), "working is busy, not stuck");
    assert!(!run.cancelled(0));
    run.complete(0, 0);
    let (engine, outcome, _) = run.finish().await;
    assert_eq!(outcome, WorkflowOutcome::Completed);
    assert_eq!(
        engine.state().status_of("only"),
        Some(&StepState::Succeeded)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn yolo_error_keeps_the_container_alive_until_it_exits() {
    // Attended: the failure goes to the recovery board, which pauses.
    let front = StatusFront::new(true, YoloTickOutcome::AdvanceNow, &[NextAction::Pause]);
    let run = start(chain(&["a", "b"]), &[], true, front, None);
    run.report(0, with_msg(ProgramState::Error, None, "tests failed"))
        .await;
    quiet_window().await;
    assert!(!run.cancelled(0), "a live agent can repair its error");
    assert!(run.countdowns().is_empty());
    run.complete(0, 0);
    let (engine, outcome, log) = run.finish().await;
    let log = log.lock().unwrap();
    assert_eq!(outcome, WorkflowOutcome::Paused);
    assert!(log.countdowns.is_empty());
    assert_eq!(
        failed_reason(&engine, "a").as_deref(),
        Some("agent reported error: tests failed")
    );
    assert!(
        !log.statuses
            .iter()
            .any(|(s, st)| s == "a" && *st == WorkflowStepStatus::Succeeded),
        "an error never advances"
    );
    let failure = log.boards[0].step_failure.as_ref().expect("recovery board");
    assert!(failure
        .detail_lines
        .contains(&"Reason: agent reported error: tests failed".to_string()));
    assert_ne!(engine.state().status_of("b"), Some(&StepState::Succeeded));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn yolo_error_without_a_user_takes_the_failure_retry_path() {
    let front = StatusFront::new(false, YoloTickOutcome::AdvanceNow, &[]);
    let run = start(chain(&["a"]), &[], true, front, None);
    run.report(0, status(ProgramState::Error)).await;
    quiet_window().await;
    assert!(!run.cancelled(0));
    run.complete(0, 0);
    // A self-exit after the error fails the first attempt and triggers retry.
    run.launched(1).await;
    run.complete(1, 0);
    let (engine, outcome, log) = run.finish().await;
    assert_eq!(outcome, WorkflowOutcome::Completed);
    assert_eq!(
        log.lock().unwrap().countdowns,
        [("a".to_string(), CountdownKind::FailureRetry)]
    );
    assert_eq!(engine.state().step_attempt("a"), Some(2));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn yolo_error_then_working_clears_the_error_and_the_step_continues() {
    let front = StatusFront::new(true, YoloTickOutcome::Continue, &[]);
    let run = start(chain(&["only"]), &[], true, front, None);
    run.report(0, status(ProgramState::Error)).await;
    quiet_window().await;
    assert!(run.countdowns().is_empty());
    run.report(0, status(ProgramState::Working)).await;
    quiet_window().await;
    *run.tick.lock().unwrap() = YoloTickOutcome::AdvanceNow;
    quiet_window().await;
    assert!(!run.cancelled(0), "the recovered agent keeps running");
    run.complete(0, 0);
    let (engine, outcome, _) = run.finish().await;
    assert_eq!(outcome, WorkflowOutcome::Completed);
    assert_eq!(
        engine.state().status_of("only"),
        Some(&StepState::Succeeded),
        "working cleared the reported problem"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn yolo_error_then_output_keeps_running_but_preserves_the_error() {
    let front = StatusFront::new(true, YoloTickOutcome::Continue, &[NextAction::Pause]);
    let run = start(chain(&["only"]), &[], true, front, None);
    run.report(0, status(ProgramState::Error)).await;
    quiet_window().await;
    run.launches.lock().unwrap()[0]
        .tail
        .push_bytes(b"retrying...\n");
    quiet_window().await;
    assert!(!run.cancelled(0), "output must not kill the agent");
    run.complete(0, 0);
    let (engine, _, _) = run.finish().await;
    // Output is not a status report: the error stands, so exit 0 still fails.
    assert!(matches!(
        engine.state().status_of("only"),
        Some(StepState::Failed { .. })
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn yolo_blocked_with_a_user_pauses_without_a_countdown() {
    let front = StatusFront::new(true, YoloTickOutcome::AdvanceNow, &[]);
    let run = start(chain(&["a", "b"]), &["b"], true, front, None);
    run.report(
        0,
        with_msg(
            ProgramState::Blocked,
            Some(BlockedKind::Permission),
            "allow?",
        ),
    )
    .await;
    quiet_window().await;
    assert!(run.countdowns().is_empty(), "blocked never auto-advances");
    assert!(run.boards().is_empty());
    assert!(!run.cancelled(0));
    assert_eq!(run.launch_count(), 1, "b has not started");
    // The user answers in the container; the agent carries on and finishes.
    run.report(0, status(ProgramState::Working)).await;
    until("working recorded", || {
        run.log.lock().unwrap().agent_status.len() == 2
    })
    .await;
    run.complete(0, 0);
    let (engine, outcome, log) = run.finish().await;
    assert_eq!(outcome, WorkflowOutcome::Completed);
    assert_eq!(engine.state().status_of("a"), Some(&StepState::Succeeded));
    assert_eq!(
        log.lock().unwrap().agent_status,
        [
            ("a".to_string(), Some(ProgramState::Blocked)),
            ("a".to_string(), Some(ProgramState::Working)),
        ]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_exit_beats_a_pending_done_countdown() {
    let front = StatusFront::new(true, YoloTickOutcome::Continue, &[]);
    let run = start(chain(&["a", "b"]), &["b"], true, front, None);
    run.report(0, status(ProgramState::Done)).await;
    run.wait_countdowns(1).await;
    run.complete(0, 0);
    let (engine, outcome, _) = run_finish_checked(run, |run| {
        assert_eq!(
            run.statuses_of("a"),
            [WorkflowStepStatus::Running, WorkflowStepStatus::Succeeded],
            "advanced exactly once"
        );
        assert!(!run.cancelled(0), "the exited container is not killed");
    })
    .await;
    assert_eq!(outcome, WorkflowOutcome::Completed);
    assert_eq!(engine.state().status_of("b"), Some(&StepState::Succeeded));
}

// ── No user ──────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unattended_blocked_fails_the_step_retries_once_then_fails_the_workflow() {
    let front = StatusFront::new(false, YoloTickOutcome::AdvanceNow, &[]);
    let run = start(chain(&["a", "b"]), &[], false, front, None);
    run.report(0, status(ProgramState::Blocked)).await;
    run.report(1, status(ProgramState::Blocked)).await;
    let (engine, outcome, log) = run.finish().await;
    assert_eq!(
        outcome,
        WorkflowOutcome::Failed {
            last_step: "a".into(),
            exit_code: -1,
        }
    );
    assert_eq!(
        log.lock().unwrap().countdowns,
        [
            ("a".to_string(), CountdownKind::AgentBlocked),
            ("a".to_string(), CountdownKind::FailureRetry),
            ("a".to_string(), CountdownKind::AgentBlocked),
        ]
    );
    assert_ne!(engine.state().status_of("b"), Some(&StepState::Succeeded));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unattended_done_advances_even_without_yolo() {
    let front = StatusFront::new(false, YoloTickOutcome::AdvanceNow, &[]);
    let run = start(chain(&["a", "b"]), &["b"], false, front, None);
    run.report(0, status(ProgramState::Idle)).await;
    let (engine, outcome, log) = run.finish().await;
    assert_eq!(outcome, WorkflowOutcome::Completed);
    assert_eq!(
        log.lock().unwrap().countdowns,
        [("a".to_string(), CountdownKind::AgentDone)]
    );
    assert_eq!(engine.state().status_of("a"), Some(&StepState::Succeeded));
}

// ── Non-yolo with a user ─────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_yolo_done_opens_the_lightweight_confirm_at_once() {
    let front = StatusFront::new(true, YoloTickOutcome::Cancel, &[NextAction::LaunchNext]);
    let run = start(chain(&["a", "b"]), &["b"], false, front, None);
    run.report(0, status(ProgramState::Done)).await;
    let (engine, outcome, log) = run.finish().await;
    let log = log.lock().unwrap();
    assert_eq!(outcome, WorkflowOutcome::Completed);
    assert!(log.countdowns.is_empty(), "no countdown without yolo");
    let board = &log.boards[0];
    assert_eq!(
        board.simple_advance,
        Some(SimpleAdvance {
            completed_step: "a".into(),
            next_step: "b".into(),
        })
    );
    assert_eq!(
        board.reported_status.as_ref().map(|s| s.state),
        Some(ProgramState::Done)
    );
    assert_eq!(engine.state().status_of("a"), Some(&StepState::Succeeded));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_yolo_blocked_and_error_open_the_control_board_with_the_report() {
    for (state, kind) in [
        (ProgramState::Blocked, Some(BlockedKind::Question)),
        (ProgramState::Error, None),
    ] {
        let front = StatusFront::new(true, YoloTickOutcome::Cancel, &[NextAction::Abort]);
        let run = start(chain(&["a", "b"]), &[], false, front, None);
        run.report(0, with_msg(state, kind, "which branch?")).await;
        let (_, outcome, log) = run.finish().await;
        let log = log.lock().unwrap();
        assert_eq!(outcome, WorkflowOutcome::Aborted, "{state:?}");
        assert!(log.countdowns.is_empty());
        let board = &log.boards[0];
        assert!(board.simple_advance.is_none(), "{state:?}: full board");
        assert!(board.can_dismiss, "{state:?}: the step is still running");
        let reported = board.reported_status.as_ref().unwrap();
        assert_eq!((reported.state, reported.kind), (state, kind));
        assert_eq!(reported.msg.as_deref(), Some("which branch?"));
    }
}

// ── Finaliser ────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_self_exit_0_after_error_is_failed() {
    let front = StatusFront::new(
        true,
        YoloTickOutcome::Cancel,
        &[NextAction::Dismiss, NextAction::Pause],
    );
    let run = start(chain(&["a", "b"]), &[], false, front, None);
    run.report(0, with_msg(ProgramState::Error, None, "gave up"))
        .await;
    run.wait_boards(1).await;
    run.complete(0, 0);
    let (engine, outcome, log) = run.finish().await;
    assert_eq!(outcome, WorkflowOutcome::Paused);
    assert_eq!(
        engine.state().status_of("a"),
        Some(&StepState::Failed {
            exit_code: 0,
            error_message: Some("agent reported error: gave up".into()),
        })
    );
    let log = log.lock().unwrap();
    assert!(log.boards[1].step_failure.is_some(), "recovery board");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_exit_after_done_keeps_its_exit_code() {
    for code in [0, 3] {
        let front = StatusFront::new(
            true,
            YoloTickOutcome::Cancel,
            &[NextAction::Dismiss, NextAction::Pause],
        );
        let run = start(chain(&["a", "b"]), &["b"], false, front, None);
        run.report(0, status(ProgramState::Done)).await;
        run.wait_boards(1).await;
        run.complete(0, code);
        let (engine, _, _) = run.finish().await;
        let st = engine.state().status_of("a").cloned();
        if code == 0 {
            assert_eq!(st, Some(StepState::Succeeded));
        } else {
            assert!(
                matches!(
                    st,
                    Some(StepState::Failed {
                        exit_code: 3,
                        error_message: None
                    })
                ),
                "{st:?}"
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unattended_error_waits_for_self_exit_and_never_advances() {
    let front = StatusFront::new(false, YoloTickOutcome::AdvanceNow, &[]);
    let run = start(chain(&["a", "b"]), &[], true, front, None);
    run.report(0, with_msg(ProgramState::Error, None, "boom"))
        .await;
    quiet_window().await;
    assert!(!run.cancelled(0));
    assert!(run.countdowns().is_empty());
    run.complete(0, 0);
    run.launched(1).await;
    run.report(1, with_msg(ProgramState::Error, None, "boom"))
        .await;
    run.complete(1, 0);
    let (engine, outcome, _) = run.finish().await;
    assert!(matches!(outcome, WorkflowOutcome::Failed { .. }));
    assert!(matches!(
        engine.state().status_of("a"),
        Some(StepState::Failed { .. })
    ));
    assert_eq!(run_launch_steps(&engine), 0, "b never ran");
}

fn run_launch_steps(engine: &WorkflowEngine) -> usize {
    engine.state().step_attempt("b").unwrap_or(0) as usize
}

// ── Non-reporting agents (regression) ────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_non_reporting_agent_keeps_the_silence_path() {
    // Exactly today's sequence: Stuck → StuckStep countdown → Unstuck
    // recovers, and the step then completes. No report reaches anyone.
    let front = StatusFront::new(true, YoloTickOutcome::Continue, &[]);
    let run = start(chain(&["a", "b"]), &["b"], true, front, None);
    let tx = run.launched(0).await;
    tx.send(StuckEvent::Stuck).unwrap();
    run.wait_countdowns(1).await;
    tx.send(StuckEvent::Unstuck).unwrap();
    run.wait_finished(1).await;
    run.complete(0, 0);
    let (engine, outcome, log) = run.finish().await;
    let log = log.lock().unwrap();
    assert_eq!(outcome, WorkflowOutcome::Completed);
    assert_eq!(
        log.countdowns,
        [("a".to_string(), CountdownKind::StuckStep)]
    );
    assert!(log.agent_status.is_empty());
    assert!(log.boards.iter().all(|b| b.reported_status.is_none()));
    assert_eq!(engine.state().status_of("a"), Some(&StepState::Succeeded));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_first_report_during_a_silence_countdown_takes_over() {
    let front = StatusFront::new(true, YoloTickOutcome::Continue, &[]);
    let run = start(chain(&["a", "b"]), &["b"], true, front, None);
    let tx = run.launched(0).await;
    tx.send(StuckEvent::Stuck).unwrap();
    run.wait_countdowns(1).await;
    tx.send(StuckEvent::Reported(status(ProgramState::Done)))
        .unwrap();
    run.wait_countdowns(2).await;
    assert_eq!(
        run.countdowns(),
        [
            ("a".to_string(), CountdownKind::StuckStep),
            ("a".to_string(), CountdownKind::AgentDone),
        ]
    );
    *run.tick.lock().unwrap() = YoloTickOutcome::AdvanceNow;
    let (engine, outcome, _) = run.finish().await;
    assert_eq!(outcome, WorkflowOutcome::Completed);
    assert_eq!(engine.state().status_of("a"), Some(&StepState::Succeeded));
}

// ── Parallel groups ──────────────────────────────────────────────────────

fn fan_out() -> Vec<WorkflowStep> {
    vec![make_step("x", &[], None), make_step("y", &[], None)]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parallel_done_advances_only_its_own_step() {
    let front = StatusFront::new(false, YoloTickOutcome::AdvanceNow, &[]);
    let run = start(fan_out(), &[], true, front, None);
    run.launched(1).await;
    let x = (0..2).find(|&n| run.launch_step(n) == "x").unwrap();
    let y = 1 - x;
    run.report(x, status(ProgramState::Done)).await;
    until("x killed", || run.cancelled(x)).await;
    quiet_window().await;
    assert!(!run.cancelled(y), "the sibling keeps running");
    assert_eq!(
        run.log.lock().unwrap().parallel_countdowns,
        [("x".to_string(), CountdownKind::AgentDone)]
    );
    run.complete(y, 0);
    let (engine, outcome, _) = run.finish().await;
    assert_eq!(outcome, WorkflowOutcome::Completed);
    assert_eq!(engine.state().status_of("x"), Some(&StepState::Succeeded));
    assert_eq!(engine.state().status_of("y"), Some(&StepState::Succeeded));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parallel_error_fails_only_its_own_step() {
    // Attended yolo: the drained group's failure goes to the recovery board,
    // which pauses; the sibling's own result is untouched.
    let front = StatusFront::new(true, YoloTickOutcome::AdvanceNow, &[NextAction::Pause]);
    let run = start(
        vec![
            make_step("x", &[], None),
            make_step("y", &[], None),
            make_step("z", &["x", "y"], None),
        ],
        &[],
        true,
        front,
        None,
    );
    run.launched(1).await;
    let x = (0..2).find(|&n| run.launch_step(n) == "x").unwrap();
    let y = 1 - x;
    run.report(x, with_msg(ProgramState::Error, None, "lint failed"))
        .await;
    quiet_window().await;
    assert!(
        !run.cancelled(x),
        "the errored agent stays available for repair"
    );
    assert!(run.log.lock().unwrap().parallel_countdowns.is_empty());
    assert!(!run.cancelled(y), "one agent's error never fails a sibling");
    run.complete(x, 0);
    run.complete(y, 0);
    let (engine, outcome, _) = run.finish().await;
    assert_eq!(outcome, WorkflowOutcome::Paused);
    assert_eq!(
        failed_reason(&engine, "x").as_deref(),
        Some("agent reported error: lint failed")
    );
    assert_eq!(engine.state().status_of("y"), Some(&StepState::Succeeded));
    assert_ne!(engine.state().status_of("z"), Some(&StepState::Succeeded));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parallel_non_yolo_report_opens_the_board_for_that_step() {
    let front = StatusFront::new(true, YoloTickOutcome::Cancel, &[NextAction::Dismiss]);
    let run = start(fan_out(), &[], false, front, None);
    run.launched(1).await;
    let y = (0..2).find(|&n| run.launch_step(n) == "y").unwrap();
    run.report(
        y,
        with_msg(ProgramState::Blocked, Some(BlockedKind::Auth), "log in"),
    )
    .await;
    run.wait_boards(1).await;
    let board = run.boards()[0].clone();
    assert_eq!(board.focused_step.as_deref(), Some("y"));
    assert_eq!(
        board.reported_status.as_ref().map(|s| s.kind),
        Some(Some(BlockedKind::Auth))
    );
    // Dismissed: both keep running; the agent recovers and both finish.
    run.report(y, status(ProgramState::Working)).await;
    // Exiting while still `blocked` would (rightly) fail the step.
    until("working recorded", || {
        run.log.lock().unwrap().agent_status.last()
            == Some(&("y".to_string(), Some(ProgramState::Working)))
    })
    .await;
    run.complete(0, 0);
    run.complete(1, 0);
    let (engine, outcome, log) = run.finish().await;
    let boards = log.lock().unwrap().boards.clone();
    assert_eq!(outcome, WorkflowOutcome::Completed, "boards: {boards:#?}");
    assert_eq!(engine.state().status_of("y"), Some(&StepState::Succeeded));
}

// ── Review regressions ───────────────────────────────────────────────────

/// Review finding: a report the agent made just before exiting 0 — still
/// queued or debounced when the exit is observed — must settle the step's
/// status before finalisation, so `error` then exit 0 is Failed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn error_reported_at_exit_still_fails_an_exit_zero_step() {
    let front = StatusFront::new(true, YoloTickOutcome::Continue, &[NextAction::Pause]);
    let run = start(chain(&["only", "next"]), &[], false, front, None);
    run.report_unpublished(0, with_msg(ProgramState::Error, None, "tests failed"))
        .await;
    run.complete(0, 0);
    let (engine, _, log) = run.finish().await;
    match engine.state().status_of("only") {
        Some(StepState::Failed { error_message, .. }) => assert_eq!(
            error_message.as_deref(),
            Some("agent reported error: tests failed")
        ),
        other => panic!("expected Failed, got {other:?}"),
    }
    assert!(
        log.lock()
            .unwrap()
            .agent_status
            .contains(&("only".to_string(), Some(ProgramState::Error))),
        "the settled status is published"
    );
}

/// The same race with the report already published: the exit and the
/// report are ready together and the biased select takes the exit first.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queued_blocked_report_and_exit_together_fail_the_step() {
    let front = StatusFront::new(true, YoloTickOutcome::Continue, &[NextAction::Pause]);
    let run = start(chain(&["only", "next"]), &[], true, front, None);
    let tx = run.launched(0).await;
    run.report_unpublished(0, with_msg(ProgramState::Blocked, None, "approve?"))
        .await;
    run.complete(0, 0);
    tx.send(StuckEvent::Reported(with_msg(
        ProgramState::Blocked,
        None,
        "approve?",
    )))
    .unwrap();
    let (engine, _, _) = run.finish().await;
    assert!(matches!(
        engine.state().status_of("only"),
        Some(StepState::Failed { .. })
    ));
    assert_ne!(
        engine.state().status_of("next"),
        Some(&StepState::Succeeded),
        "the successor must not run after a reported problem"
    );
}

/// Review finding: a parallel step that reported `error` and exits 0
/// before the group loop handled the report is Failed too.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parallel_error_reported_at_exit_fails_the_step() {
    let front = StatusFront::new(true, YoloTickOutcome::Continue, &[NextAction::Pause]);
    let run = start(fan_out(), &[], false, front, None);
    run.launched(1).await;
    run.report_unpublished(0, status(ProgramState::Error)).await;
    run.complete(0, 0);
    run.complete(1, 0);
    let (engine, _, _) = run.finish().await;
    let x = run_step_named(&engine, "x");
    assert!(matches!(x, Some(StepState::Failed { .. })), "x: {x:?}");
    assert_eq!(engine.state().status_of("y"), Some(&StepState::Succeeded));
}

fn run_step_named(engine: &WorkflowEngine, name: &str) -> Option<StepState> {
    engine.state().status_of(name).cloned()
}

/// Output after an error is not a status report; exit 0 still fails.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn output_after_error_does_not_clear_the_error() {
    let front = StatusFront::new(true, YoloTickOutcome::Continue, &[NextAction::Pause]);
    let run = start(chain(&["only"]), &[], true, front, None);
    run.report(0, with_msg(ProgramState::Error, None, "failed tests"))
        .await;
    quiet_window().await;
    run.launches.lock().unwrap()[0]
        .tail
        .push_bytes(b"writing diagnostics...\n");
    quiet_window().await;
    assert!(run.countdowns().is_empty());
    assert!(!run.cancelled(0));
    run.complete(0, 0);
    let (engine, _, _) = run.finish().await;
    assert!(matches!(
        engine.state().status_of("only"),
        Some(StepState::Failed { .. })
    ));
}

/// Review finding: in an attended yolo run whose final group is parallel,
/// an `AgentDone` expiry kills and advances its own step, and the group
/// completes without the old silence-only final review board.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn final_parallel_done_steps_advance_without_review() {
    let front = StatusFront::new(true, YoloTickOutcome::AdvanceNow, &[NextAction::Pause]);
    let run = start(fan_out(), &[], true, front, None);
    run.launched(1).await;
    run.report(0, status(ProgramState::Done)).await;
    run.report(1, status(ProgramState::Done)).await;
    let (engine, outcome, log) = run.finish().await;
    assert_eq!(outcome, WorkflowOutcome::Completed);
    assert!(
        log.lock().unwrap().boards.is_empty(),
        "no final review board"
    );
    assert!(run_cancelled_both(&engine));
}

fn run_cancelled_both(engine: &WorkflowEngine) -> bool {
    ["x", "y"]
        .iter()
        .all(|n| engine.state().status_of(n) == Some(&StepState::Succeeded))
}

/// Review finding: a second report with the same countdown kind (blocked
/// permission → blocked auth) keeps the deadline but its new kind/message
/// is recorded and published.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn same_countdown_kind_still_updates_the_status_payload() {
    let front = StatusFront::new(false, YoloTickOutcome::Continue, &[NextAction::Pause]);
    let run = start(chain(&["only"]), &[], true, front, None);
    run.report(
        0,
        with_msg(
            ProgramState::Blocked,
            Some(BlockedKind::Permission),
            "allow write?",
        ),
    )
    .await;
    run.wait_countdowns(1).await;
    let before = run.log.lock().unwrap().agent_status.len();
    run.report(
        0,
        with_msg(
            ProgramState::Blocked,
            Some(BlockedKind::Auth),
            "login required",
        ),
    )
    .await;
    quiet_window().await;
    let after = run.log.lock().unwrap().agent_status.len();
    assert_eq!(after, before + 1, "the new blocked kind/msg was published");
    assert_eq!(
        run.log.lock().unwrap().agent_payloads.last().cloned(),
        Some((Some(BlockedKind::Auth), Some("login required".to_string())))
    );
    assert_eq!(run.countdowns().len(), 1, "no new countdown");
    run.task.abort();
}

/// Review finding: active-tab suppression must not consume the `done`
/// countdown. While the user interacts no countdown starts and nothing is
/// killed; an exit during suppression is still handled as the step's exit.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn suppressed_done_countdown_still_handles_the_exit() {
    let front = StatusFront::new(true, YoloTickOutcome::AdvanceNow, &[NextAction::Pause]);
    front.interacting.store(true, Ordering::SeqCst);
    let run = start(chain(&["only"]), &[], true, front, None);
    run.report(0, status(ProgramState::Done)).await;
    quiet_window().await;
    assert!(
        run.countdowns().is_empty(),
        "no countdown while the user types"
    );
    assert!(!run.cancelled(0));
    run.complete(0, 3);
    let (engine, _, _) = run.finish().await;
    assert!(matches!(
        engine.state().status_of("only"),
        Some(StepState::Failed { exit_code: 3, .. })
    ));
}

/// …and once the user is quiet the full countdown starts (and here expires).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn done_countdown_starts_after_the_user_goes_quiet() {
    let front = StatusFront::new(true, YoloTickOutcome::AdvanceNow, &[]);
    let interacting = front.interacting.clone();
    interacting.store(true, Ordering::SeqCst);
    let run = start(chain(&["only"]), &[], true, front, None);
    run.report(0, status(ProgramState::Done)).await;
    quiet_window().await;
    assert!(run.countdowns().is_empty());
    assert!(
        !run.cancelled(0),
        "the agent is not killed during suppression"
    );
    interacting.store(false, Ordering::SeqCst);
    run.wait_countdowns(1).await;
    let (_, outcome, log) = run.finish().await;
    assert_eq!(outcome, WorkflowOutcome::Completed);
    assert_eq!(
        log.lock().unwrap().countdowns,
        [("only".to_string(), CountdownKind::AgentDone)]
    );
}
