//! `WorkflowFrontend` trait — defined by Layer 1, implemented by Layer 3.
//!
//! Engine-driven: the engine calls these methods to command the frontend.
//! The frontend is a pure I/O layer — it renders what the engine tells it
//! and collects user input when the engine asks for it.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::broadcast;

use crate::data::message::UserMessageSink;
use crate::data::workflow_definition::WorkflowStep;
use crate::data::workflow_state::{PhaseKind, WorkflowState};
use crate::engine::agent_runtime::execution::StuckEvent;
use crate::engine::agent_runtime::frontend::AgentIo;
use crate::engine::error::EngineError;
use crate::engine::workflow::actions::{
    AvailableActions, CountdownKind, NextAction, ResumeMismatch, StepOutput, WorkflowOutcome,
    WorkflowStepProgressInfo, WorkflowStepStatus, YoloTickOutcome,
};
use crate::engine::workflow::EngineRequest;

/// Per-workflow frontend the engine uses for every Q&A and status report.
///
/// The engine treats CLI, TUI, and API implementations identically; the
/// engine never knows which is on the other side.
pub trait WorkflowFrontend: UserMessageSink + Send {
    // === Engine-driven display commands (blocking) ===

    /// Engine tells frontend to show the Workflow Control Board with these
    /// actions. Frontend collects user input and returns the chosen action.
    /// This is a BLOCKING call — the engine waits for the user's choice.
    fn show_workflow_control_board(
        &mut self,
        state: &WorkflowState,
        available: &AvailableActions,
    ) -> Result<NextAction, EngineError>;

    /// Engine tells frontend to update the yolo countdown display.
    /// Called repeatedly (every ~100ms) with the remaining time.
    /// Frontend returns whether to Continue, Cancel, or AdvanceNow.
    fn yolo_countdown_tick(
        &mut self,
        step_name: &str,
        remaining: Duration,
        total: Duration,
    ) -> Result<YoloTickOutcome, EngineError>;

    /// Engine tells frontend: yolo countdown just started for this step.
    /// Frontend should show the countdown dialog (active tab) or flash
    /// the tab header yellow/purple (background tab).
    ///
    /// `kind` says what expiry will do — advance past a stuck container, or
    /// retry a step that failed (WI-0115 §3) — so a frontend that narrates the
    /// countdown in words can narrate the right ones. Frontends that only show
    /// a clock can ignore it.
    fn yolo_countdown_started(&mut self, _step_name: &str, _kind: CountdownKind) {}

    /// Engine tells frontend: yolo countdown finished (expired, cancelled,
    /// or step recovered). Frontend dismisses dialog / resets tab style.
    fn yolo_countdown_finished(&mut self, _step_name: &str) {}

    // === Status reporting (fire-and-forget) ===

    fn report_step_status(&mut self, step: &WorkflowStep, status: WorkflowStepStatus);

    fn report_step_output(&mut self, _step: &WorkflowStep, _output: StepOutput) {}

    fn report_workflow_completed(&mut self, outcome: &WorkflowOutcome);

    /// Called by the engine before each step and before any user-input prompt.
    /// The engine controls call ordering; the frontend renders the table.
    fn report_workflow_progress(&mut self, _steps: &[WorkflowStepProgressInfo]) {}

    /// Called by the engine after resolving the step's agent/model but before
    /// the container launches.
    fn report_step_interactive_launch(
        &mut self,
        _step: &WorkflowStep,
        _agent: &str,
        _model: Option<&str>,
    ) {
    }

    /// One observation from a `poll_ci` phase step.
    ///
    /// The poller used to compose its own narration and the engine pushed it
    /// through `write_message` (WI 0114 F-45: Layer 1 authoring transcript
    /// text). The default writes `event`'s `Display` at `event.level()` —
    /// byte-identical to the line the engine wrote before — so no frontend's
    /// output changes until it overrides this.
    fn report_ci_poll(&mut self, event: &crate::data::ci_poll_event::CiPollEvent) {
        self.write_message(crate::data::message::UserMessage {
            level: event.level(),
            text: event.to_string(),
        });
    }

    /// Called by the engine the moment the current container has actually
    /// terminated — either it exited on its own (agent quit, crash, startup
    /// grace kill) or the engine killed it (yolo advance, WCB action).
    ///
    /// Contract: this fires ONLY on real container death. It is never called
    /// for a stuck-but-alive container or while a yolo countdown is still
    /// running. `exit_code` is the container's exit code when known, or
    /// [`KILLED_EXIT_CODE`](crate::engine::agent_runtime::execution::KILLED_EXIT_CODE)
    /// when the engine killed it without waiting for the real code.
    fn report_container_exited(&mut self, _exit_code: i32) {}

    // === Capabilities ===

    /// Whether a human is on the other side of this frontend and can be asked
    /// what to do when a step fails.
    ///
    /// `true` puts the engine on the interactive recovery path: it opens the
    /// Workflow Control Board with the failure attached and waits for a
    /// restart / back / next / abort decision. `false` — the squad daemon, the
    /// API server, a `--non-interactive` CLI run — puts it on the unattended
    /// path: one yolo countdown, one automatic retry, then the workflow fails
    /// (WI-0115 §3).
    ///
    /// Defaults to `false`: a frontend that has not opted in must never be
    /// blocked on a question nobody can answer.
    fn supports_interactive_recovery(&self) -> bool {
        false
    }

    // === User decisions (blocking) ===

    fn confirm_resume(&mut self, mismatch: &ResumeMismatch) -> Result<bool, EngineError>;

    // === Channel setup ===

    /// The engine hands the frontend the channels it has just created.
    ///
    /// Before WI 0114 F-45 this was four `set_*` setters, each with its own
    /// default no-op, and a frontend that wanted two of them implemented two
    /// methods with the same shape. [`EngineHandles`] says which step the
    /// channels belong to and carries only the ones the engine actually
    /// created at that moment; see its field docs for when each is `Some`.
    ///
    /// Default is a no-op: a frontend that routes none of these — the CLI's
    /// plain workflow frontend, the API, the squad daemon — implements
    /// nothing.
    fn attach_engine(&mut self, _handles: EngineHandles) {}

    // === Setup/Teardown phase output (fire-and-forget, default no-ops) ===
    //
    // One set of five, keyed by `PhaseKind`. Before F-34 there were ten
    // methods — an `on_setup_*` and an `on_teardown_*` for each event — and
    // every frontend wrote the same body twice.

    fn on_phase_step_started(&mut self, _kind: PhaseKind, _description: &str) {}
    fn on_phase_step_output(&mut self, _kind: PhaseKind, _line: &str) {}
    fn on_phase_step_completed(&mut self, _kind: PhaseKind, _description: &str) {}
    fn on_phase_step_failed(
        &mut self,
        _kind: PhaseKind,
        _description: &str,
        _exit_code: i32,
        _stderr: &str,
    ) {
    }
    /// Step failed and an `on_failure` agent is about to run. Emitted
    /// once per remediation attempt before the agent launches; the step
    /// will be retried once the agent finishes.
    fn on_phase_step_fixing(
        &mut self,
        _kind: PhaseKind,
        _description: &str,
        _attempt: u32,
        _of: u32,
    ) {
    }

    // === Parallel-group commands (WI-0096) ===
    //
    // All default no-ops. The single-step path never calls these; frontends
    // that don't render multi-container UX simply ignore them. The engine owns
    // every scheduling/concurrency decision — these callbacks are pure
    // presentation notifications.

    /// Engine is launching multiple parallel containers for this group.
    /// `step_names` is the ordered list of all steps in this parallel batch
    /// (including queued ones that are not yet running).
    fn report_parallel_group_started(&mut self, _step_names: &[String]) {}

    /// One container in a parallel group has started running.
    fn report_parallel_step_launched(
        &mut self,
        _step_name: &str,
        _agent: &str,
        _model: Option<&str>,
    ) {
    }

    /// The engine learned a parallel step's actual container name (right
    /// after the container launched). Frontends use it for per-container
    /// stats polling and status-bar display.
    fn report_parallel_step_container(&mut self, _step_name: &str, _container_name: &str) {}

    /// One container in a parallel group has exited.
    /// `evict` — the frontend should remove the status bar for this step
    /// entirely (not replace it with a grey summary bar).
    fn report_parallel_step_exited(&mut self, _step_name: &str, _exit_code: i32) {}

    /// A queued step in this parallel group has started (because a slot freed up).
    fn report_parallel_step_dequeued(
        &mut self,
        _step_name: &str,
        _agent: &str,
        _model: Option<&str>,
    ) {
    }

    /// The parallel group has fully drained; all steps completed.
    fn report_parallel_group_finished(&mut self) {}

    /// Per-step stuck notification for a parallel container.
    fn report_parallel_step_stuck(&mut self, _step_name: &str) {}
    fn report_parallel_step_unstuck(&mut self, _step_name: &str) {}

    /// Per-step yolo countdown updates.
    fn parallel_step_yolo_countdown_started(&mut self, _step_name: &str) {}
    fn parallel_step_yolo_countdown_tick(
        &mut self,
        _step_name: &str,
        _remaining: Duration,
        _total: Duration,
    ) -> Result<YoloTickOutcome, EngineError> {
        Ok(YoloTickOutcome::Continue)
    }
    fn parallel_step_yolo_countdown_finished(&mut self, _step_name: &str) {}
}

/// The engine-side channels a frontend may attach to, handed over in one
/// [`WorkflowFrontend::attach_engine`] call.
///
/// `step` distinguishes the two moments this arrives: the workflow-level
/// handover right after the engine builds its request channel and at each
/// single-step launch (`None`), and the per-step handover for one container
/// in a parallel group (`Some(name)`). Every channel field is independently
/// optional, because the engine has different ones to give at each moment.
#[derive(Default)]
pub struct EngineHandles {
    /// The parallel step these channels belong to, or `None` for the
    /// workflow-level channels.
    pub step: Option<String>,
    /// The engine's request channel. The frontend stores the sender so the
    /// TUI event loop can route Ctrl-W requests to this engine instance.
    /// `Some` once, right after the engine creates the channel.
    pub requests: Option<tokio::sync::mpsc::UnboundedSender<EngineRequest>>,
    /// The broadcast channel from a launched container's stuck detector; the
    /// TUI subscribes to it for tab colouring. CLI and API frontends ignore
    /// it. `Some` at each container launch.
    pub stuck: Option<Arc<broadcast::Sender<StuckEvent>>>,
    /// A parallel step's I/O channels. Only ever `Some` alongside
    /// `step: Some(_)`.
    pub io: Option<AgentIo>,
}

impl EngineHandles {
    /// The workflow-level request channel.
    pub fn requests(tx: tokio::sync::mpsc::UnboundedSender<EngineRequest>) -> Self {
        Self {
            requests: Some(tx),
            ..Self::default()
        }
    }

    /// The stuck channel of the single-step container just launched.
    pub fn stuck(sender: Arc<broadcast::Sender<StuckEvent>>) -> Self {
        Self {
            stuck: Some(sender),
            ..Self::default()
        }
    }

    /// The stuck channel of one parallel step's container.
    pub fn step_stuck(step: impl Into<String>, sender: Arc<broadcast::Sender<StuckEvent>>) -> Self {
        Self {
            step: Some(step.into()),
            stuck: Some(sender),
            ..Self::default()
        }
    }

    /// One parallel step's I/O channels.
    pub fn step_io(step: impl Into<String>, io: AgentIo) -> Self {
        Self {
            step: Some(step.into()),
            io: Some(io),
            ..Self::default()
        }
    }
}

/// Forward a shared, mutex-guarded frontend to the frontend it wraps.
///
/// Layer 2 shares one frontend between the workflow engine and its execution
/// factory as `Arc<Mutex<Box<dyn …>>>`. Before F-36 each command hand-wrote a
/// forwarding proxy struct for that handle; those proxies had to restate every
/// method, and a method left out silently fell through to the trait's default
/// no-op instead of reaching the real frontend. This impl forwards **every**
/// method — including the defaulted ones — so an override on the inner
/// frontend is never lost.
impl<F: WorkflowFrontend + ?Sized> WorkflowFrontend for std::sync::Arc<std::sync::Mutex<Box<F>>> {
    fn show_workflow_control_board(
        &mut self,
        state: &WorkflowState,
        available: &AvailableActions,
    ) -> Result<NextAction, EngineError> {
        self.lock()
            .unwrap()
            .show_workflow_control_board(state, available)
    }

    fn yolo_countdown_tick(
        &mut self,
        step_name: &str,
        remaining: Duration,
        total: Duration,
    ) -> Result<YoloTickOutcome, EngineError> {
        self.lock()
            .unwrap()
            .yolo_countdown_tick(step_name, remaining, total)
    }

    fn yolo_countdown_started(&mut self, step_name: &str, kind: CountdownKind) {
        self.lock().unwrap().yolo_countdown_started(step_name, kind);
    }

    fn yolo_countdown_finished(&mut self, step_name: &str) {
        self.lock().unwrap().yolo_countdown_finished(step_name);
    }

    fn report_step_status(&mut self, step: &WorkflowStep, status: WorkflowStepStatus) {
        self.lock().unwrap().report_step_status(step, status);
    }

    fn report_step_output(&mut self, step: &WorkflowStep, output: StepOutput) {
        self.lock().unwrap().report_step_output(step, output);
    }

    fn report_workflow_completed(&mut self, outcome: &WorkflowOutcome) {
        self.lock().unwrap().report_workflow_completed(outcome);
    }

    fn report_workflow_progress(&mut self, steps: &[WorkflowStepProgressInfo]) {
        self.lock().unwrap().report_workflow_progress(steps);
    }

    fn report_step_interactive_launch(
        &mut self,
        step: &WorkflowStep,
        agent: &str,
        model: Option<&str>,
    ) {
        self.lock()
            .unwrap()
            .report_step_interactive_launch(step, agent, model);
    }

    fn report_container_exited(&mut self, exit_code: i32) {
        self.lock().unwrap().report_container_exited(exit_code);
    }

    fn report_ci_poll(&mut self, event: &crate::data::ci_poll_event::CiPollEvent) {
        self.lock().unwrap().report_ci_poll(event);
    }

    fn supports_interactive_recovery(&self) -> bool {
        self.lock().unwrap().supports_interactive_recovery()
    }

    fn confirm_resume(&mut self, mismatch: &ResumeMismatch) -> Result<bool, EngineError> {
        self.lock().unwrap().confirm_resume(mismatch)
    }

    fn attach_engine(&mut self, handles: EngineHandles) {
        self.lock().unwrap().attach_engine(handles);
    }

    fn on_phase_step_started(&mut self, kind: PhaseKind, description: &str) {
        self.lock()
            .unwrap()
            .on_phase_step_started(kind, description);
    }

    fn on_phase_step_output(&mut self, kind: PhaseKind, line: &str) {
        self.lock().unwrap().on_phase_step_output(kind, line);
    }

    fn on_phase_step_completed(&mut self, kind: PhaseKind, description: &str) {
        self.lock()
            .unwrap()
            .on_phase_step_completed(kind, description);
    }

    fn on_phase_step_failed(
        &mut self,
        kind: PhaseKind,
        description: &str,
        exit_code: i32,
        stderr: &str,
    ) {
        self.lock()
            .unwrap()
            .on_phase_step_failed(kind, description, exit_code, stderr);
    }

    fn on_phase_step_fixing(&mut self, kind: PhaseKind, description: &str, attempt: u32, of: u32) {
        self.lock()
            .unwrap()
            .on_phase_step_fixing(kind, description, attempt, of);
    }

    fn report_parallel_group_started(&mut self, step_names: &[String]) {
        self.lock()
            .unwrap()
            .report_parallel_group_started(step_names);
    }

    fn report_parallel_step_launched(&mut self, step_name: &str, agent: &str, model: Option<&str>) {
        self.lock()
            .unwrap()
            .report_parallel_step_launched(step_name, agent, model);
    }

    fn report_parallel_step_container(&mut self, step_name: &str, container_name: &str) {
        self.lock()
            .unwrap()
            .report_parallel_step_container(step_name, container_name);
    }

    fn report_parallel_step_exited(&mut self, step_name: &str, exit_code: i32) {
        self.lock()
            .unwrap()
            .report_parallel_step_exited(step_name, exit_code);
    }

    fn report_parallel_step_dequeued(&mut self, step_name: &str, agent: &str, model: Option<&str>) {
        self.lock()
            .unwrap()
            .report_parallel_step_dequeued(step_name, agent, model);
    }

    fn report_parallel_group_finished(&mut self) {
        self.lock().unwrap().report_parallel_group_finished();
    }

    fn report_parallel_step_stuck(&mut self, step_name: &str) {
        self.lock().unwrap().report_parallel_step_stuck(step_name);
    }

    fn report_parallel_step_unstuck(&mut self, step_name: &str) {
        self.lock().unwrap().report_parallel_step_unstuck(step_name);
    }

    fn parallel_step_yolo_countdown_started(&mut self, step_name: &str) {
        self.lock()
            .unwrap()
            .parallel_step_yolo_countdown_started(step_name);
    }

    fn parallel_step_yolo_countdown_tick(
        &mut self,
        step_name: &str,
        remaining: Duration,
        total: Duration,
    ) -> Result<YoloTickOutcome, EngineError> {
        self.lock()
            .unwrap()
            .parallel_step_yolo_countdown_tick(step_name, remaining, total)
    }

    fn parallel_step_yolo_countdown_finished(&mut self, step_name: &str) {
        self.lock()
            .unwrap()
            .parallel_step_yolo_countdown_finished(step_name);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::ci_poll_event::CiPollEvent;
    use crate::data::message::{MessageLevel, RecordingMessageSink, UserMessage};

    /// A frontend that implements only the required methods, so
    /// `report_ci_poll` and `attach_engine` run their defaults.
    #[derive(Default)]
    struct DefaultOnlyFrontend(RecordingMessageSink);

    impl UserMessageSink for DefaultOnlyFrontend {
        fn write_message(&mut self, message: UserMessage) {
            self.0.write_message(message);
        }
        fn replay_queued(&mut self) {}
    }

    impl WorkflowFrontend for DefaultOnlyFrontend {
        fn show_workflow_control_board(
            &mut self,
            _state: &WorkflowState,
            _available: &AvailableActions,
        ) -> Result<NextAction, EngineError> {
            unreachable!("not exercised")
        }
        fn yolo_countdown_tick(
            &mut self,
            _step_name: &str,
            _remaining: Duration,
            _total: Duration,
        ) -> Result<YoloTickOutcome, EngineError> {
            Ok(YoloTickOutcome::Continue)
        }
        fn report_step_status(&mut self, _step: &WorkflowStep, _status: WorkflowStepStatus) {}
        fn report_workflow_completed(&mut self, _outcome: &WorkflowOutcome) {}
        fn confirm_resume(&mut self, _mismatch: &ResumeMismatch) -> Result<bool, EngineError> {
            Ok(false)
        }
    }

    /// F-45's contract: the default `report_ci_poll` writes the identical line
    /// the workflow engine used to compose from `(PollMessage, String)`, at
    /// the same level — so a frontend that has not opted in shows exactly the
    /// same output.
    #[test]
    fn the_default_report_ci_poll_writes_the_pre_f45_narration() {
        let cases: &[(CiPollEvent, &str, MessageLevel)] = &[
            (
                CiPollEvent::Attempt { attempt: 1, of: 5 },
                "Polling CI (attempt 1/5)...",
                MessageLevel::Info,
            ),
            (CiPollEvent::Passed, "CI passed", MessageLevel::Info),
            (
                CiPollEvent::StillRunning,
                "CI still running",
                MessageLevel::Info,
            ),
            (
                CiPollEvent::NoRunYet,
                "No CI run found yet (may not have been created); will retry",
                MessageLevel::Info,
            ),
            (
                CiPollEvent::Failed {
                    detail: "unit-tests".into(),
                },
                "CI failed: unit-tests",
                MessageLevel::Warning,
            ),
        ];
        for (event, text, level) in cases {
            let mut fe = DefaultOnlyFrontend::default();
            fe.report_ci_poll(event);
            let messages = fe.0.all();
            assert_eq!(messages.len(), 1, "one line per event, for {event:?}");
            assert_eq!(&messages[0].text, text);
            assert_eq!(&messages[0].level, level);
        }
    }

    /// `attach_engine`'s default is a no-op that writes nothing, exactly as
    /// the four `set_*` defaults it replaced did.
    #[test]
    fn the_default_attach_engine_writes_nothing() {
        let mut fe = DefaultOnlyFrontend::default();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<EngineRequest>();
        fe.attach_engine(EngineHandles::requests(tx));
        let (sender, _rx) = broadcast::channel::<StuckEvent>(4);
        fe.attach_engine(EngineHandles::step_stuck("a", Arc::new(sender)));
        assert!(fe.0.all().is_empty());
    }

    /// The constructors label the handover correctly: only the per-step ones
    /// carry a step name, which is what tells a frontend whether the channels
    /// are the tab's or one parallel container's.
    #[test]
    fn only_the_per_step_constructors_name_a_step() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<EngineRequest>();
        assert!(EngineHandles::requests(tx).step.is_none());

        let (sender, _rx) = broadcast::channel::<StuckEvent>(4);
        assert!(EngineHandles::stuck(Arc::new(sender)).step.is_none());

        let (sender, _rx) = broadcast::channel::<StuckEvent>(4);
        let handles = EngineHandles::step_stuck("build", Arc::new(sender));
        assert_eq!(handles.step.as_deref(), Some("build"));
        assert!(handles.stuck.is_some() && handles.requests.is_none());
    }
}
