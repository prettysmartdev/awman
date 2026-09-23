//! Tests for `engine::workflow`, split by what they exercise (WI 0114 F-51).
//!
//! This module holds only what more than one of them needs: the fake runtime,
//! factory and frontend implementations, the session/workflow builders, the
//! blocking factory the mid-step tests drive, and the mock background
//! container the phase tests use.

mod parallel;
mod phases;
mod single;

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::Utc;

use super::*;
use crate::data::session::{AgentHandle, SessionOpenOptions};
use crate::data::workflow_definition::{Workflow, WorkflowStep};
use crate::engine::agent_runtime::execution::{AgentExecution, AgentExitInfo};

// ── Fake implementations ─────────────────────────────────────────────────

struct FakeWorkflowFrontend {
    actions: Mutex<VecDeque<NextAction>>,
    step_statuses: Mutex<Vec<(String, WorkflowStepStatus)>>,
    completed: Mutex<Option<WorkflowOutcome>>,
    confirm_resume_response: bool,
    /// What `supports_interactive_recovery` reports. `true` (the default)
    /// drives a step failure through the `actions` queue; `false` puts the
    /// engine on the unattended countdown-and-retry path.
    interactive: bool,
    /// What `yolo_countdown_tick` returns. `AdvanceNow` collapses the 60s
    /// retry countdown to a single tick so unattended tests stay fast;
    /// `Cancel` (the default) is the pre-existing safe answer.
    yolo_tick: YoloTickOutcome,
    /// Every board the engine raised, shared so a test can read them back
    /// after the engine has taken ownership of the frontend.
    boards: Arc<Mutex<Vec<AvailableActions>>>,
}

impl FakeWorkflowFrontend {
    fn new(actions: impl IntoIterator<Item = NextAction>) -> Self {
        Self {
            actions: Mutex::new(actions.into_iter().collect()),
            step_statuses: Mutex::new(Vec::new()),
            completed: Mutex::new(None),
            confirm_resume_response: true,
            interactive: true,
            yolo_tick: YoloTickOutcome::Cancel,
            boards: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Handle on the boards this frontend will be shown.
    fn boards(&self) -> Arc<Mutex<Vec<AvailableActions>>> {
        self.boards.clone()
    }

    fn unattended(mut self) -> Self {
        self.interactive = false;
        self
    }

    fn with_yolo_tick(mut self, tick: YoloTickOutcome) -> Self {
        self.yolo_tick = tick;
        self
    }

    fn with_confirm_resume(mut self, response: bool) -> Self {
        self.confirm_resume_response = response;
        self
    }
}

impl crate::data::message::UserMessageSink for FakeWorkflowFrontend {
    fn write_message(&mut self, _msg: crate::data::message::UserMessage) {}
    fn replay_queued(&mut self) {}
}

impl WorkflowFrontend for FakeWorkflowFrontend {
    fn show_workflow_control_board(
        &mut self,
        _state: &WorkflowState,
        available: &AvailableActions,
    ) -> Result<NextAction, EngineError> {
        self.boards.lock().unwrap().push(available.clone());
        let action = self
            .actions
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(NextAction::LaunchNext);
        Ok(action)
    }

    fn supports_interactive_recovery(&self) -> bool {
        self.interactive
    }

    fn confirm_resume(&mut self, _mismatch: &ResumeMismatch) -> Result<bool, EngineError> {
        Ok(self.confirm_resume_response)
    }

    fn report_step_status(&mut self, step: &WorkflowStep, status: WorkflowStepStatus) {
        self.step_statuses
            .lock()
            .unwrap()
            .push((step.name.clone(), status));
    }

    fn yolo_countdown_tick(
        &mut self,
        _step_name: &str,
        _remaining: Duration,
        _total: Duration,
    ) -> Result<YoloTickOutcome, EngineError> {
        Ok(self.yolo_tick.clone())
    }

    fn report_workflow_completed(&mut self, outcome: &WorkflowOutcome) {
        *self.completed.lock().unwrap() = Some(outcome.clone());
    }
}

struct FakeAgentExecutionFactory {
    exit_codes: Mutex<VecDeque<i32>>,
    pub execution_call_count: AtomicUsize,
    pub inject_call_count: AtomicUsize,
    pub recorded_contexts: Mutex<Vec<WorkflowRuntimeContext>>,
    inject_result: Option<()>,
    /// When set, each produced execution carries an output tail pre-filled
    /// with these lines (exercises the container failure-log path).
    tail_lines: Option<Vec<String>>,
    /// Container name stamped on each produced execution's handle.
    container_name: String,
}

impl FakeAgentExecutionFactory {
    fn new(exit_codes: impl IntoIterator<Item = i32>) -> Self {
        Self {
            exit_codes: Mutex::new(exit_codes.into_iter().collect()),
            execution_call_count: AtomicUsize::new(0),
            inject_call_count: AtomicUsize::new(0),
            recorded_contexts: Mutex::new(Vec::new()),
            inject_result: None,
            tail_lines: None,
            container_name: "fake-container".to_string(),
        }
    }

    fn always_success() -> Self {
        Self::new(std::iter::repeat_n(0, 100))
    }

    /// Produce executions whose output tail is pre-filled with `lines` and
    /// whose container handle is named `container_name`.
    fn with_output_tail(
        exit_codes: impl IntoIterator<Item = i32>,
        container_name: &str,
        lines: impl IntoIterator<Item = &'static str>,
    ) -> Self {
        Self {
            tail_lines: Some(lines.into_iter().map(|s| s.to_string()).collect()),
            container_name: container_name.to_string(),
            ..Self::new(exit_codes)
        }
    }
}

impl AgentExecutionFactory for FakeAgentExecutionFactory {
    fn execution_for_step(
        &self,
        _step: &WorkflowStep,
        _session: &Session,
        runtime: &WorkflowRuntimeContext,
    ) -> Result<AgentExecution, EngineError> {
        self.execution_call_count.fetch_add(1, Ordering::Relaxed);
        self.recorded_contexts.lock().unwrap().push(runtime.clone());
        let code = self.exit_codes.lock().unwrap().pop_front().unwrap_or(0);
        let now = Utc::now();
        let info = AgentExitInfo {
            exit_code: code,
            signal: None,
            started_at: now,
            ended_at: now,
        };
        let handle = AgentHandle {
            id: format!("fake-{}", self.execution_call_count.load(Ordering::Relaxed)),
            image_tag: "fake-image:latest".into(),
            name: self.container_name.clone(),
            started_at: now,
        };
        match &self.tail_lines {
            Some(lines) => {
                let tail = OutputTail::with_default_capacity();
                for line in lines {
                    tail.push_bytes(line.as_bytes());
                    tail.push_bytes(b"\n");
                }
                Ok(AgentExecution::finished_with_tail(
                    handle,
                    info,
                    Some(Arc::new(tail)),
                ))
            }
            None => Ok(AgentExecution::finished(handle, info)),
        }
    }

    fn inject_prompt(
        &self,
        _execution: &AgentExecution,
        _prompt: &str,
    ) -> Result<Option<()>, EngineError> {
        self.inject_call_count.fetch_add(1, Ordering::Relaxed);
        Ok(self.inject_result)
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────

fn make_session(tmp: &tempfile::TempDir) -> Session {
    Session::for_tests(tmp.path())
}

/// Session whose env snapshot pins `AWMAN_CONFIG_HOME` to `home`, so the
/// engine resolves `~/.awman/logs/` under a temp dir instead of the real
/// home — no process-global env mutation, no cross-test races.
fn make_session_with_home(tmp: &tempfile::TempDir, home: &std::path::Path) -> Session {
    Session::for_tests_isolated(tmp.path(), home)
}

fn make_step(name: &str, deps: &[&str], agent: Option<&str>) -> WorkflowStep {
    WorkflowStep {
        name: name.to_string(),
        depends_on: deps.iter().map(|s| s.to_string()).collect(),
        prompt_template: "do something".to_string(),
        agent: agent.map(|s| s.to_string()),
        model: None,
        overlays: None,
        abort_on_failure: false,
    }
}

fn make_workflow(
    title: Option<&str>,
    wf_agent: Option<&str>,
    steps: Vec<WorkflowStep>,
) -> Workflow {
    Workflow {
        title: title.map(|s| s.to_string()),
        steps,
        agent: wf_agent.map(|s| s.to_string()),
        model: None,
        setup: Vec::new(),
        teardown: Vec::new(),
        teardown_on_failure: false,
        overlays: None,
    }
}

fn make_engine(
    session: &Session,
    workflow: Workflow,
    factory: FakeAgentExecutionFactory,
    actions: impl IntoIterator<Item = NextAction>,
) -> WorkflowEngine {
    make_engine_with_frontend(
        session,
        workflow,
        factory,
        FakeWorkflowFrontend::new(actions),
    )
}

fn make_engine_with_frontend(
    session: &Session,
    workflow: Workflow,
    factory: FakeAgentExecutionFactory,
    frontend: FakeWorkflowFrontend,
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

// ── Blocking factory for mid-step tests ──────────────────────────────────

use std::sync::Condvar;

type CompletionArc = Arc<(Mutex<Option<i32>>, Condvar)>;

struct BlockingBackend {
    cancel_flag: Arc<AtomicBool>,
    completion: CompletionArc,
}

impl crate::engine::agent_runtime::execution::ExecutionBackend for BlockingBackend {
    fn wait_blocking(self: Box<Self>) -> Result<AgentExitInfo, EngineError> {
        let (lock, cvar) = &*self.completion;
        loop {
            if self.cancel_flag.load(Ordering::Relaxed) {
                let now = Utc::now();
                return Ok(AgentExitInfo {
                    exit_code: -1,
                    signal: None,
                    started_at: now,
                    ended_at: now,
                });
            }
            let guard = lock.lock().unwrap();
            let (guard, _) = cvar.wait_timeout(guard, Duration::from_millis(20)).unwrap();
            if let Some(code) = *guard {
                let now = Utc::now();
                return Ok(AgentExitInfo {
                    exit_code: code,
                    signal: None,
                    started_at: now,
                    ended_at: now,
                });
            }
        }
    }

    fn cancel(&self) -> Result<(), EngineError> {
        self.cancel_flag.store(true, Ordering::Relaxed);
        let (_, cvar) = &*self.completion;
        cvar.notify_all();
        Ok(())
    }

    fn cancel_handle(&self) -> Option<crate::engine::agent_runtime::execution::CancelHandle> {
        let flag = self.cancel_flag.clone();
        let completion = self.completion.clone();
        Some(crate::engine::agent_runtime::execution::CancelHandle::new(
            move || {
                flag.store(true, Ordering::Relaxed);
                let (_, cvar) = &*completion;
                cvar.notify_all();
                Ok(())
            },
        ))
    }
}

fn make_blocking_entry() -> (Arc<AtomicBool>, CompletionArc) {
    (
        Arc::new(AtomicBool::new(false)),
        Arc::new((Mutex::new(None), Condvar::new())),
    )
}

fn signal_completion(c: &CompletionArc, code: i32) {
    let (lock, cvar) = &**c;
    *lock.lock().unwrap() = Some(code);
    cvar.notify_all();
}

/// Wait until `count` reaches `expected` launches, then assert it.
///
/// Launch is asynchronous: the engine task must be scheduled and each
/// slot spawned before the factory increments the counter. A fixed
/// sleep raced that on loaded CI runners (observed: 1 launch after
/// 150 ms), so poll with a generous deadline instead. The assertion is
/// unchanged; only the wait is bounded rather than fixed.
async fn wait_for_execution_count(count: &AtomicUsize, expected: usize) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while count.load(Ordering::Relaxed) < expected && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(count.load(Ordering::Relaxed), expected);
}

struct BlockingFactory {
    execution_count: Arc<AtomicUsize>,
    inject_count: Arc<AtomicUsize>,
    inject_result: Option<()>,
    blocking_slots: Mutex<VecDeque<(Arc<AtomicBool>, CompletionArc)>>,
}

impl BlockingFactory {
    fn new(slots: impl IntoIterator<Item = (Arc<AtomicBool>, CompletionArc)>) -> Self {
        Self {
            execution_count: Arc::new(AtomicUsize::new(0)),
            inject_count: Arc::new(AtomicUsize::new(0)),
            inject_result: None,
            blocking_slots: Mutex::new(slots.into_iter().collect()),
        }
    }
}

impl AgentExecutionFactory for BlockingFactory {
    fn execution_for_step(
        &self,
        _step: &WorkflowStep,
        _session: &Session,
        _runtime: &WorkflowRuntimeContext,
    ) -> Result<AgentExecution, EngineError> {
        let idx = self.execution_count.fetch_add(1, Ordering::Relaxed);
        let slot = self.blocking_slots.lock().unwrap().pop_front();
        if let Some((cancel_flag, completion)) = slot {
            let backend = Box::new(BlockingBackend {
                cancel_flag,
                completion,
            });
            let now = Utc::now();
            let handle = AgentHandle {
                id: format!("blocking-{idx}"),
                image_tag: "test:latest".into(),
                name: "blocking-container".into(),
                started_at: now,
            };
            let (stuck_tx, _) = tokio::sync::broadcast::channel(4);
            Ok(AgentExecution::new(
                handle,
                backend,
                std::sync::Arc::new(stuck_tx),
                None,
            ))
        } else {
            let now = Utc::now();
            let info = AgentExitInfo {
                exit_code: 0,
                signal: None,
                started_at: now,
                ended_at: now,
            };
            let handle = AgentHandle {
                id: format!("instant-{idx}"),
                image_tag: "test:latest".into(),
                name: "instant-container".into(),
                started_at: now,
            };
            Ok(AgentExecution::finished(handle, info))
        }
    }

    fn inject_prompt(
        &self,
        _execution: &AgentExecution,
        _prompt: &str,
    ) -> Result<Option<()>, EngineError> {
        self.inject_count.fetch_add(1, Ordering::Relaxed);
        Ok(self.inject_result)
    }
}

struct CapturingFrontend {
    actions: Mutex<VecDeque<NextAction>>,
    step_statuses: Mutex<Vec<(String, WorkflowStepStatus)>>,
    completed: Mutex<Option<WorkflowOutcome>>,
    available_log: Mutex<Vec<AvailableActions>>,
    engine_tx: Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<EngineRequest>>>>,
    /// Exit codes passed to `report_container_exited`, shared so tests
    /// can assert on them after the engine consumes the frontend.
    container_exits: Arc<Mutex<Vec<i32>>>,
}

impl CapturingFrontend {
    fn new(
        actions: impl IntoIterator<Item = NextAction>,
        engine_tx: Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<EngineRequest>>>>,
    ) -> Self {
        Self {
            actions: Mutex::new(actions.into_iter().collect()),
            step_statuses: Mutex::new(Vec::new()),
            completed: Mutex::new(None),
            available_log: Mutex::new(Vec::new()),
            engine_tx,
            container_exits: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl crate::data::message::UserMessageSink for CapturingFrontend {
    fn write_message(&mut self, _msg: crate::data::message::UserMessage) {}
    fn replay_queued(&mut self) {}
}

impl WorkflowFrontend for CapturingFrontend {
    fn show_workflow_control_board(
        &mut self,
        _state: &WorkflowState,
        available: &AvailableActions,
    ) -> Result<NextAction, EngineError> {
        self.available_log.lock().unwrap().push(available.clone());
        let action = self
            .actions
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(NextAction::Pause);
        Ok(action)
    }

    fn confirm_resume(&mut self, _: &ResumeMismatch) -> Result<bool, EngineError> {
        Ok(true)
    }

    fn report_step_status(&mut self, step: &WorkflowStep, status: WorkflowStepStatus) {
        self.step_statuses
            .lock()
            .unwrap()
            .push((step.name.clone(), status));
    }

    fn yolo_countdown_tick(
        &mut self,
        _step_name: &str,
        _remaining: Duration,
        _total: Duration,
    ) -> Result<YoloTickOutcome, EngineError> {
        Ok(YoloTickOutcome::Cancel)
    }

    fn report_workflow_completed(&mut self, outcome: &WorkflowOutcome) {
        *self.completed.lock().unwrap() = Some(outcome.clone());
    }

    fn report_container_exited(&mut self, exit_code: i32) {
        self.container_exits.lock().unwrap().push(exit_code);
    }

    fn attach_engine(&mut self, handles: EngineHandles) {
        if let Some(tx) = handles.requests {
            *self.engine_tx.lock().unwrap() = Some(tx);
        }
    }
}

fn make_capturing_engine(
    session: &Session,
    workflow: Workflow,
    factory: BlockingFactory,
    actions: impl IntoIterator<Item = NextAction>,
    engine_tx: Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<EngineRequest>>>>,
) -> (WorkflowEngine, Arc<Mutex<Vec<i32>>>) {
    let frontend = CapturingFrontend::new(actions, engine_tx);
    let container_exits = frontend.container_exits.clone();
    let engine = WorkflowEngine::new(
        session,
        WorkflowSpec::new(workflow).with_work_item_context(None),
        WorkflowEngineDeps {
            frontend: Box::new(frontend),
            agent_factory: Box::new(factory),
        },
    )
    .unwrap();
    (engine, container_exits)
}

// ── MockBackgroundContainer ───────────────────────────────────────────────

struct MockBackgroundContainer {
    /// Pre-programmed results: (stdout, stderr, exit_code).
    results: Mutex<VecDeque<(String, String, i32)>>,
    /// Recorded commands (in call order).
    calls: Mutex<Vec<String>>,
    /// Number of times a fresh container was handed out — exercised by
    /// per-step-container assertions (WI-0082).
    container_handouts: Mutex<usize>,
}

impl MockBackgroundContainer {
    /// All execs succeed with empty output.
    fn always_success() -> Self {
        Self {
            results: Mutex::new(VecDeque::new()),
            calls: Mutex::new(Vec::new()),
            container_handouts: Mutex::new(0),
        }
    }

    /// Provide an explicit sequence of (stdout, stderr, exit_code) results.
    fn with_results(results: impl IntoIterator<Item = (String, String, i32)>) -> Self {
        Self {
            results: Mutex::new(results.into_iter().collect()),
            calls: Mutex::new(Vec::new()),
            container_handouts: Mutex::new(0),
        }
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    fn handouts(&self) -> usize {
        *self.container_handouts.lock().unwrap()
    }

    /// Build a factory closure for `WorkflowEngine::run_phase`
    /// that records one container handout per step and
    /// delegates exec calls back to this mock. Tests that previously
    /// passed `&mock` directly can now pass `mock.factory()`.
    fn factory<'a>(
        self: &'a Arc<Self>,
    ) -> impl FnMut(
        usize,
    ) -> Result<
        Box<dyn crate::engine::agent_runtime::background::AgentExec>,
        EngineError,
    > + 'a {
        move |_idx| {
            *self.container_handouts.lock().unwrap() += 1;
            Ok(Box::new(SharedMockExec(Arc::clone(self))))
        }
    }
}

/// Trampoline that lets the test factory hand out fresh `Box<dyn
/// AgentExec>` values while keeping all recorded state in the single
/// shared `MockBackgroundContainer`.
struct SharedMockExec(Arc<MockBackgroundContainer>);

impl crate::engine::agent_runtime::background::AgentExec for SharedMockExec {
    fn exec(
        &self,
        command: &str,
        env: Option<&std::collections::HashMap<String, String>>,
    ) -> Result<
        crate::engine::agent_runtime::background::ExecOutput,
        crate::engine::error::EngineError,
    > {
        self.0.exec(command, env)
    }
}

impl crate::engine::agent_runtime::background::AgentExec for MockBackgroundContainer {
    fn exec(
        &self,
        command: &str,
        _env: Option<&std::collections::HashMap<String, String>>,
    ) -> Result<
        crate::engine::agent_runtime::background::ExecOutput,
        crate::engine::error::EngineError,
    > {
        self.calls.lock().unwrap().push(command.to_string());
        let (stdout, stderr, exit_code) = self
            .results
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| ("".into(), "".into(), 0));
        Ok(crate::engine::agent_runtime::background::ExecOutput {
            stdout,
            stderr,
            exit_code,
        })
    }
}
