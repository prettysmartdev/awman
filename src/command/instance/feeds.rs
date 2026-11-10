//! The command-owned feeds a tab's presenter consumes (WI 0142 Part A §5,
//! design D9): `TabCommandFeeds` (formerly `frontend::tui::tabs::TabSharedState`,
//! re-exported there under its old name), `ContainerSlotEvent`,
//! `ContainerSlotIo`, `ContainerLifecycle`, `SharedWorkflowPresentation`,
//! `AgentSignals`, the yolo/status/dashboard/ACP feeds and `TabFeeds` (the
//! per-tab consumer bundle).
//!
//! Layer 2: these carry channel ends and shared handles, so they are not wire
//! data; `data::instance_snapshot` holds their projections. The producer
//! ([`super::frontend::InstanceCommandFrontend`]) writes them on the command
//! thread; a presenting frontend (the TUI `Tab`) drains them on its own thread.

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::command::dispatch::CommandOutcome;
use crate::command::error::CommandError;
use crate::data::dialog::{DialogRequest, DialogResponse};
use crate::data::message::{UserMessage, UserMessageSink};
use crate::data::program_status::EffectiveStatus;
use crate::data::workflow_state::WorkflowState;
use crate::engine::acp::{PermissionRequest, SessionUpdate};
use crate::engine::agent_runtime::execution::StuckEvent;
use crate::engine::git::GitDiffSummary;
use tokio_util::sync::CancellationToken;

pub use crate::data::instance_snapshot::{
    StatusLine, StatusLine as StatusLogEntry, StepViewStatus, WorkflowStepKind, WorkflowStepView,
    WorkflowViewState, YoloState,
};

/// Shared diff summary written by a tab's background git poll and read by
/// the presenter (sidebar, status-bar `+X -Y`).
pub type SharedGitDiffSummary = Arc<Mutex<Option<GitDiffSummary>>>;

/// Workflow identity and its presentation metadata are published together.
/// A command owns one handle; reset replaces it instead of clearing a live
/// producer's fields. Scroll offsets and dialog input remain on the UI thread.
#[derive(Clone, Default)]
pub struct SharedWorkflowPresentation(Arc<Mutex<WorkflowPresentation>>);

#[derive(Clone, Default)]
pub struct WorkflowPresentation {
    pub view: Option<WorkflowViewState>,
    pub invocation_id: Option<uuid::Uuid>,
    pub context_path: Option<std::path::PathBuf>,
    pub worktree_path: Option<std::path::PathBuf>,
}

impl SharedWorkflowPresentation {
    pub fn snapshot(&self) -> WorkflowPresentation {
        self.0.lock().map(|state| state.clone()).unwrap_or_default()
    }

    pub fn update_view(&self, update: impl FnOnce(&mut Option<WorkflowViewState>)) {
        if let Ok(mut state) = self.0.lock() {
            update(&mut state.view);
        }
    }

    pub fn publish_remote(&self, invocation_id: uuid::Uuid, view: WorkflowViewState) {
        if let Ok(mut state) = self.0.lock() {
            if state.invocation_id != Some(invocation_id) {
                state.context_path = None;
                state.worktree_path = None;
            }
            state.invocation_id = Some(invocation_id);
            state.view = Some(view);
        }
    }

    pub fn set_context_path(&self, path: Option<std::path::PathBuf>) {
        if let Ok(mut state) = self.0.lock() {
            state.context_path = path;
        }
    }

    pub fn set_worktree_path(&self, path: Option<std::path::PathBuf>) {
        if let Ok(mut state) = self.0.lock() {
            state.worktree_path = path;
        }
    }
}

/// The latest `agents status` frame, rendered as a `Table` widget.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentsDashboardData {
    pub overview: crate::command::commands::agents::AgentsOverview,
    /// True while a `--watch` loop is refreshing this slot; drives the
    /// footer ("Refreshing every 2s" vs. the collection time).
    pub watching: bool,
}

/// Cross-thread shared dashboard slot. `agents status` writes here; the TUI
/// renderer reads it to display a `Table` widget.
pub type SharedAgentsDashboard = Arc<Mutex<Option<AgentsDashboardData>>>;

/// Cross-thread shared yolo-countdown state. The engine ticks it every 100ms
/// while a yolo countdown is active; the renderer reads it to display the
/// "Auto-advancing in Ns" non-modal overlay.
pub type SharedYoloState = Arc<Mutex<Option<YoloState>>>;

/// Shared flag: TUI event loop sets this to `true` when the user presses
/// Esc during a yolo countdown. `yolo_countdown_tick` checks it and
/// returns `Cancel` when set, then resets the flag.
pub type SharedYoloCancelFlag = Arc<AtomicBool>;

/// What the engine-facing frontend needs to know about the tab to apply
/// active-tab suppression: when the user last typed or scrolled, and the
/// agent's latest reported status. Frontends hold no policy; the engine acts
/// on every report it receives, and this only lets the TUI hold back the
/// *dialog* until the user has stopped interacting.
#[derive(Clone, Default)]
pub struct AgentSignals(Arc<Mutex<AgentSignalsInner>>);

#[derive(Default)]
struct AgentSignalsInner {
    last_user_input: Option<Instant>,
    status: Option<EffectiveStatus>,
    /// The command asked the App to close its report-opened board: the
    /// agent resumed (`working` / clear) before the user touched it.
    dismiss_report_dialog: bool,
}

impl AgentSignals {
    /// Record a key press or scroll on this tab.
    pub fn touch_user(&self) {
        if let Ok(mut g) = self.0.lock() {
            g.last_user_input = Some(Instant::now());
        }
    }

    /// Whether the user typed or scrolled within the last `window`.
    pub fn user_active_within(&self, window: std::time::Duration) -> bool {
        self.0
            .lock()
            .ok()
            .and_then(|g| g.last_user_input)
            .is_some_and(|t| t.elapsed() < window)
    }

    pub fn set_status(&self, status: Option<EffectiveStatus>) {
        if let Ok(mut g) = self.0.lock() {
            g.status = status;
        }
    }

    pub fn status(&self) -> Option<EffectiveStatus> {
        self.0.lock().ok().and_then(|g| g.status.clone())
    }

    /// When the user last typed or scrolled on this tab (dialog keys count).
    pub fn last_user_input(&self) -> Option<Instant> {
        self.0.lock().ok().and_then(|g| g.last_user_input)
    }

    /// Ask the App to close this tab's report-opened workflow board (shown
    /// or still queued) and answer it `Dismissed`.
    pub fn request_report_dialog_dismiss(&self) {
        if let Ok(mut g) = self.0.lock() {
            g.dismiss_report_dialog = true;
        }
    }

    /// Take a pending [`Self::request_report_dialog_dismiss`].
    pub fn take_report_dialog_dismiss(&self) -> bool {
        self.0
            .lock()
            .map(|mut g| std::mem::take(&mut g.dismiss_report_dialog))
            .unwrap_or(false)
    }
}

/// Shared TUI context for `agents status`. The event loop refreshes this on
/// every tick so the watch loop always sees live tab data.
pub type SharedTuiContext = Arc<Mutex<crate::command::commands::agents::AgentsTuiContext>>;

/// Upper bound on the rendered ACP update history kept per slot. Once full,
/// the oldest entry is dropped as new ones arrive — a plain ring buffer.
/// There is no raw terminal scrollback to fall back on for an ACP window, so
/// this cap is the only backstop against unbounded growth.
pub const ACP_HISTORY_LIMIT: usize = 2000;

/// The producer half of an ACP (Agent Client Protocol) agent window.
///
/// Unlike a stdio slot there is no raw terminal byte stream and no vt100
/// grid: the agent speaks structured [`SessionUpdate`] frames, which are kept
/// here as a bounded history. A pending permission request is parked here
/// while its modal is open.
///
/// The feed is shared (`Arc<Mutex<…>>`, see [`SharedAcpFeed`]) between the
/// ACP frontend running on the engine task, which appends updates in
/// `render_update`, and every view presenting the slot, which reads the
/// history to draw the window on its next frame. Per-view navigation (the
/// list scroll offset) is presenter state and lives with each view, never
/// here, so two views of one slot scroll independently (WI 0142 D9).
#[derive(Debug, Default)]
pub struct AcpFeed {
    /// Rendered update history, oldest first. Bounded to [`ACP_HISTORY_LIMIT`]
    /// entries (a ring buffer).
    pub history: std::collections::VecDeque<SessionUpdate>,
    /// The permission request currently awaiting a user decision, if any. Set
    /// when the modal opens and cleared when it resolves.
    pub pending_permission: Option<PermissionRequest>,
}

impl AcpFeed {
    /// Append an update, enforcing the [`ACP_HISTORY_LIMIT`] ring-buffer cap.
    pub fn push_update(&mut self, update: SessionUpdate) {
        self.history.push_back(update);
        while self.history.len() > ACP_HISTORY_LIMIT {
            self.history.pop_front();
        }
    }
}

/// Cross-thread handle to an ACP window's [`AcpFeed`]. Held by both the
/// presenting slot (UI thread) and the ACP frontend (engine task).
pub type SharedAcpFeed = Arc<Mutex<AcpFeed>>;

/// Former names of [`AcpFeed`] / [`SharedAcpFeed`], kept for existing callers.
pub type AcpSlotState = AcpFeed;
pub type SharedAcpState = SharedAcpFeed;

/// The agent window's PTY size `(cols, rows)` for one tab, as its presenting
/// frontend last reported it (D8: the frontend chooses the size; the engine
/// never knows its layout). The instance keeps one per tab and hands a clone
/// to every command it runs there, so each later launch — sequential,
/// parallel, attach — starts at the presenter's current size rather than a
/// size the engine process guessed from its own terminal.
#[derive(Debug, Clone)]
pub struct SharedPtySize(Arc<Mutex<(u16, u16)>>);

impl SharedPtySize {
    /// The size used until a presenter reports one.
    pub const DEFAULT: (u16, u16) = (80, 24);

    pub fn new(size: (u16, u16)) -> Self {
        Self(Arc::new(Mutex::new(size)))
    }

    pub fn get(&self) -> (u16, u16) {
        match self.0.lock() {
            Ok(size) => *size,
            Err(poisoned) => *poisoned.into_inner(),
        }
    }

    pub fn set(&self, size: (u16, u16)) {
        match self.0.lock() {
            Ok(mut current) => *current = size,
            Err(poisoned) => *poisoned.into_inner() = size,
        }
    }
}

impl Default for SharedPtySize {
    fn default() -> Self {
        Self::new(Self::DEFAULT)
    }
}

pub struct ContainerSlotIo {
    pub stdout_rx: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
    pub stdin_tx: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    pub resize_tx: tokio::sync::mpsc::UnboundedSender<(u16, u16)>,
}

/// Lifecycle event published by the workflow frontend (engine thread) and
/// drained by the TUI event loop to maintain `Tab::container_slots`
/// (WI-0096 §12). Kept in a shared queue rather than mutating `Tab` directly
/// because the frontend runs on the engine's tokio task while `Tab` lives on
/// the TUI thread.
pub enum ContainerSlotEvent {
    /// Replace the sequential backbone's identity and all of its I/O at once.
    SequentialLaunched {
        launch_id: uuid::Uuid,
        title: Option<String>,
        io: ContainerSlotIo,
    },
    SequentialRunning {
        launch_id: uuid::Uuid,
        container_name: String,
    },
    SequentialExited {
        launch_id: uuid::Uuid,
        exit_code: i32,
    },
    EngineAttached {
        launch_id: uuid::Uuid,
        requests:
            Option<tokio::sync::mpsc::UnboundedSender<crate::engine::workflow::EngineRequest>>,
        stuck: Option<tokio::sync::broadcast::Receiver<StuckEvent>>,
    },
    /// An asynchronous attach waiter may finish after the same step was reattached.
    AttachedExited {
        container_name: String,
        step_name: String,
    },
    /// A parallel group is starting: the sequential "backbone" slot (the
    /// command-level container plumbing that sequential steps reuse) goes
    /// dormant while the group's per-step slots take over the display.
    GroupStarted,
    /// A container in the group started running (initial launch or dequeued).
    Launched {
        step_name: String,
        agent: String,
        model: Option<String>,
        io: Option<ContainerSlotIo>,
    },
    /// The engine learned the step's actual container name (published right
    /// after launch). Drives per-slot stats polling and the stats title.
    ContainerName {
        step_name: String,
        container_name: String,
    },
    /// A container exited — evict its slot with no grey summary bar.
    Exited { step_name: String },
    /// A container's stuck timer fired (yolo off).
    Stuck { step_name: String },
    /// A stuck container recovered.
    Unstuck { step_name: String },
    /// Why a parallel slot's countdown runs (drives the dialog wording).
    CountdownKind {
        step_name: String,
        kind: crate::engine::workflow::actions::CountdownKind,
    },
    /// A container's yolo countdown started. `cancel_flag` is the same
    /// `Arc` the engine-side frontend checks each tick — stashed on the slot
    /// so the TUI event loop can request cancellation (Esc on the per-slot
    /// countdown modal) without a lookup back into the engine thread.
    YoloStarted {
        step_name: String,
        cancel_flag: SharedYoloCancelFlag,
    },
    /// A per-second countdown update for a slot's yolo timer, mirroring the
    /// sequential path's `yolo_countdown_tick`. Drives both the minimized-bar
    /// countdown text and the per-slot modal shown when the slot is focused.
    YoloTick {
        step_name: String,
        remaining_secs: u64,
    },
    /// A container's yolo countdown ended (cancelled, expired, or advanced).
    YoloFinished { step_name: String },
    /// The whole group drained; clear any remaining group slots and restore
    /// the dormant sequential backbone.
    GroupFinished,
}

/// Shared queue of [`ContainerSlotEvent`]s. Mirrors the other `SharedXxx`
/// slots: the workflow frontend pushes, the event loop drains.
pub type SharedContainerSlotEvents = Arc<Mutex<std::collections::VecDeque<ContainerSlotEvent>>>;

/// Ordered container transitions for one command. A proxy keeps the launch id
/// it was given, so late Running/Exited callbacks cannot relabel a newer slot.
#[derive(Clone)]
pub struct ContainerLifecycle {
    #[cfg(any(test, feature = "test-support"))]
    trace: Arc<Mutex<Option<Arc<super::trace::OutcomeTrace>>>>,
    pub events: SharedContainerSlotEvents,
    launch_id: uuid::Uuid,
}

impl Default for ContainerLifecycle {
    fn default() -> Self {
        Self {
            #[cfg(any(test, feature = "test-support"))]
            trace: Default::default(),
            events: Arc::new(Mutex::new(std::collections::VecDeque::new())),
            launch_id: uuid::Uuid::new_v4(),
        }
    }
}

impl ContainerLifecycle {
    #[cfg(any(test, feature = "test-support"))]
    pub fn set_trace(&self, trace: Arc<super::trace::OutcomeTrace>) {
        *self.trace.lock().unwrap() = Some(trace);
    }
    pub fn launch_id(&self) -> uuid::Uuid {
        self.launch_id
    }

    pub fn push(&self, event: ContainerSlotEvent) {
        #[cfg(any(test, feature = "test-support"))]
        if let Some(trace) = self.trace.lock().unwrap().as_ref() {
            let value = match &event {
                ContainerSlotEvent::SequentialRunning { container_name, .. } => {
                    Some(serde_json::json!({"event":"running","container":container_name}))
                }
                ContainerSlotEvent::SequentialExited { exit_code, .. } => {
                    Some(serde_json::json!({"event":"exited","exit_code":exit_code}))
                }
                ContainerSlotEvent::SequentialLaunched { title, .. } => {
                    Some(serde_json::json!({"event":"launched","title":title}))
                }
                ContainerSlotEvent::Launched {
                    step_name,
                    agent,
                    model,
                    ..
                } => Some(
                    serde_json::json!({"event":"step_launched","step":step_name,"agent":agent,"model":model}),
                ),
                ContainerSlotEvent::ContainerName {
                    step_name,
                    container_name,
                } => Some(
                    serde_json::json!({"event":"step_running","step":step_name,"container":container_name}),
                ),
                ContainerSlotEvent::Exited { step_name } => {
                    Some(serde_json::json!({"event":"step_exited","step":step_name}))
                }
                _ => None,
            };
            if let Some(value) = value {
                trace.record("container", value);
            }
        }
        if let Ok(mut queue) = self.events.lock() {
            queue.push_back(event);
        }
    }

    pub fn launch(&mut self, title: Option<String>, io: ContainerSlotIo) {
        self.launch_id = uuid::Uuid::new_v4();
        self.push(ContainerSlotEvent::SequentialLaunched {
            launch_id: self.launch_id,
            title,
            io,
        });
    }

    pub fn running(&self, container_name: String) {
        self.push(ContainerSlotEvent::SequentialRunning {
            launch_id: self.launch_id,
            container_name,
        });
    }

    pub fn exited(&self, exit_code: i32) {
        self.push(ContainerSlotEvent::SequentialExited {
            launch_id: self.launch_id,
            exit_code,
        });
    }

    pub fn attach_engine(&self, handles: crate::engine::workflow::frontend::EngineHandles) {
        if handles.step.is_none() {
            self.push(ContainerSlotEvent::EngineAttached {
                launch_id: self.launch_id,
                requests: handles.requests,
                // Subscribe now so events before the next UI tick are retained.
                stuck: handles.stuck.map(|sender| sender.subscribe()),
            });
        }
    }
}

/// Command-owned workflow and container lifecycles, plus independent feeds
/// (formerly `TabSharedState`; the TUI re-exports it under that name).
/// Reset replaces producer handles, isolating the next command from late writes.
#[derive(Clone)]
pub struct TabCommandFeeds {
    pub command_bytes: super::output::CommandBytes,
    pub workflow: SharedWorkflowPresentation,
    pub container: ContainerLifecycle,
    pub yolo_state: SharedYoloState,
    pub yolo_cancel_flag: SharedYoloCancelFlag,
    pub status_log: SharedStatusLog,
    pub agents_dashboard: SharedAgentsDashboard,
    pub tui_context_shared: SharedTuiContext,
    pub signals: AgentSignals,
    pub command_cancel: tokio_util::sync::CancellationToken,
}

impl TabCommandFeeds {
    pub fn new() -> Self {
        Self {
            command_bytes: Default::default(),
            workflow: SharedWorkflowPresentation::default(),
            container: ContainerLifecycle::default(),
            yolo_state: Arc::new(Mutex::new(None)),
            yolo_cancel_flag: Arc::new(AtomicBool::new(false)),
            status_log: Arc::new(Mutex::new(Vec::new())),
            agents_dashboard: Arc::new(Mutex::new(None)),
            tui_context_shared: Arc::new(Mutex::new(
                crate::command::commands::agents::AgentsTuiContext::default(),
            )),
            signals: AgentSignals::default(),
            command_cancel: tokio_util::sync::CancellationToken::new(),
        }
    }

    #[cfg(test)]
    pub fn for_tests() -> Self {
        Self::new()
    }

    pub fn reset_for_new_command(&mut self) {
        self.command_cancel.cancel();
        let context = self.tui_context_shared.clone();
        let signals = self.signals.clone();
        *self = Self::new();
        self.tui_context_shared = context;
        signals.set_status(None);
        self.signals = signals;
    }

    /// Detaching drops command-owned presentation feeds while retaining the
    /// tab's log and dashboard. Old publishers keep disconnected handles.
    pub fn detach(&mut self) {
        self.command_cancel.cancel();
        self.command_cancel = tokio_util::sync::CancellationToken::new();
        self.workflow = SharedWorkflowPresentation::default();
        self.container = ContainerLifecycle::default();
        self.yolo_state = Arc::new(Mutex::new(None));
        self.yolo_cancel_flag = Arc::new(AtomicBool::new(false));
    }
}

impl Default for TabCommandFeeds {
    fn default() -> Self {
        Self::new()
    }
}

/// The consumer ends of one command's channels, held by the presenting tab
/// (the TUI `Tab`, local backend) or by a daemon-side relay. Channel types are
/// exactly the ones `Tab` held before the WI 0142 split.
pub struct TabFeeds {
    pub command_result_rx: std::sync::mpsc::Receiver<Result<CommandOutcome, CommandError>>,
    pub dialog_request_rx: std::sync::mpsc::Receiver<DialogRequest>,
    pub dialog_response_tx: std::sync::mpsc::Sender<DialogResponse>,
}

// ─── Status log ───────────────────────────────────────────────────────────

/// The tab's status log: the command appends, the presenter draws.
pub type SharedStatusLog = Arc<Mutex<Vec<StatusLine>>>;

/// [`UserMessageSink`] that appends to a [`SharedStatusLog`]. Formerly
/// `frontend::tui::user_message::TuiUserMessageSink` (re-exported there).
#[derive(Clone)]
pub struct StatusLogSink {
    #[cfg(any(test, feature = "test-support"))]
    trace: Option<Arc<super::trace::OutcomeTrace>>,
    output: Option<super::output::CommandBytes>,
    log: SharedStatusLog,
    wake: Option<Arc<tokio::sync::Notify>>,
}

impl StatusLogSink {
    pub fn new(log: SharedStatusLog) -> Self {
        Self {
            log,
            wake: None,
            output: None,
            #[cfg(any(test, feature = "test-support"))]
            trace: None,
        }
    }
    pub fn with_wake(mut self, wake: Arc<tokio::sync::Notify>) -> Self {
        self.wake = Some(wake);
        self
    }
}

impl StatusLogSink {
    #[cfg(any(test, feature = "test-support"))]
    pub fn with_trace(mut self, trace: Arc<super::trace::OutcomeTrace>) -> Self {
        self.trace = Some(trace);
        self
    }
    pub fn with_output(mut self, output: super::output::CommandBytes) -> Self {
        self.output = Some(output);
        self
    }
}

impl UserMessageSink for StatusLogSink {
    fn write_message(&mut self, msg: UserMessage) {
        #[cfg(any(test, feature = "test-support"))]
        if let Some(trace) = &self.trace {
            trace.record(
                "status",
                serde_json::json!({"level":msg.level,"text":msg.text}),
            );
        }
        if let Some(output) = &self.output {
            output.message(&msg);
        }
        if let Ok(mut log) = self.log.lock() {
            log.push(StatusLine {
                level: msg.level,
                text: msg.text,
            });
        }
        if let Some(wake) = &self.wake {
            wake.notify_one();
        }
    }

    fn replay_queued(&mut self) {
        // Messages are appended immediately; nothing is queued.
    }
}

// ─── Squad attach ─────────────────────────────────────────────────────────

/// Cancellation state shared by a squad tab and its command frontend: the
/// tab's root token and the token of the attach session now running. Holds no
/// UI state (formerly `frontend::tui::squad_attach::SquadAttachSession`).
pub struct SquadAttachSession {
    root_cancel: CancellationToken,
    current: Mutex<Option<(String, CancellationToken)>>,
}

impl SquadAttachSession {
    pub fn new(root_cancel: CancellationToken) -> Self {
        Self {
            root_cancel,
            current: Mutex::new(None),
        }
    }

    pub fn begin(&self, task: &str) -> CancellationToken {
        let _ = self.end();
        let cancel = self.root_cancel.child_token();
        if let Ok(mut current) = self.current.lock() {
            *current = Some((task.to_string(), cancel.clone()));
        }
        cancel
    }

    pub fn end(&self) -> Option<String> {
        let current = self.current.lock().ok()?.take();
        current.map(|(task, cancel)| {
            cancel.cancel();
            task
        })
    }

    pub fn task(&self) -> Option<String> {
        self.current
            .lock()
            .ok()
            .and_then(|current| current.as_ref().map(|(task, _)| task.clone()))
    }

    pub(crate) fn cancel_token(&self) -> Option<CancellationToken> {
        self.current
            .lock()
            .ok()
            .and_then(|current| current.as_ref().map(|(_, cancel)| cancel.clone()))
    }
}

/// What a squad tab hands the command it runs: the attach session and the
/// daemon-reachable flag its card grid maintains.
#[derive(Clone)]
pub struct SharedSquadAttach {
    pub session: Arc<SquadAttachSession>,
    pub daemon_reachable: Arc<AtomicBool>,
}

// ─── Workflow view projection ─────────────────────────────────────────────

/// Project a Layer 0 [`WorkflowState`] (a squad or remote run) into the
/// overview's [`WorkflowViewState`]. Pure data conversion; formerly in
/// `frontend::tui::workflow_view` (re-exported there).
pub fn workflow_state_to_view_state(state: &WorkflowState) -> WorkflowViewState {
    let mut steps: Vec<WorkflowStepView> = Vec::new();

    for ps in &state.setup_step_states {
        steps.push(WorkflowStepView {
            name: ps.description.clone(),
            status: StepViewStatus::of_phase_step_status(&ps.status),
            agent: None,
            model: None,
            depends_on: Vec::new(),
            kind: WorkflowStepKind::Setup,
        });
    }

    for info in &state.steps {
        let status = state
            .step_states
            .get(&info.name)
            .map(StepViewStatus::of_step_state)
            .unwrap_or(StepViewStatus::Pending);
        steps.push(WorkflowStepView {
            name: info.name.clone(),
            status,
            agent: info.agent.clone(),
            model: info.model.clone(),
            depends_on: info.depends_on.clone(),
            kind: WorkflowStepKind::Agent,
        });
    }

    for ps in &state.teardown_step_states {
        steps.push(WorkflowStepView {
            name: ps.description.clone(),
            status: StepViewStatus::of_phase_step_status(&ps.status),
            agent: None,
            model: None,
            depends_on: Vec::new(),
            kind: WorkflowStepKind::Teardown,
        });
    }

    let current_step = state.current_step_index.and_then(|idx| {
        let setup_len = state.setup_step_states.len();
        state
            .steps
            .get(idx)
            .map(|s| s.name.clone())
            .or_else(|| steps.get(idx + setup_len).map(|s| s.name.clone()))
    });

    WorkflowViewState {
        steps,
        current_step,
        max_concurrent: None,
        reported: Default::default(),
    }
}
