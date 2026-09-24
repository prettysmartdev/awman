//! Per-tab state.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use ratatui::layout::Rect;

use crate::command::dispatch::CommandOutcome;
use crate::command::error::CommandError;
use crate::data::session::{CommandStatus, Session, SessionId, SessionState};
use crate::engine::acp::{PermissionRequest, SessionUpdate};
use crate::engine::agent_runtime::execution::{AgentStats, StuckEvent};
use crate::engine::git::{GitDiffSummary, GitEngine};
use crate::frontend::tui::dialogs::{DialogRequest, DialogResponse};
use crate::frontend::tui::git_sidebar::{start_git_diff_poll_task, GitSidebarState};
use crate::frontend::tui::user_message::SharedStatusLog;

mod container_slots;
mod git_poll;
mod labels;
mod overlay_lifecycle;
pub mod squad_state;
#[cfg(test)]
mod tests;

use squad_state::SquadTabState;

pub type SharedGitDiffSummary = Arc<Mutex<Option<GitDiffSummary>>>;

/// Per-tab execution lifecycle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecutionPhase {
    Idle,
    Running { command: String },
    Done { command: String, exit_code: i32 },
    Error { command: String, message: String },
}

impl ExecutionPhase {
    /// How a session's recorded command reads in the tab bar and the
    /// execution window.
    ///
    /// The phase is a *view* of `SessionState::current_command`, which Layer 2
    /// writes for every frontend (decision Q3, WI 0114 F-22). The TUI used to
    /// keep its own copy and set it in four places, so the tab could disagree
    /// with the session about what was running.
    pub fn of_session_state(state: &SessionState) -> Self {
        let Some(cmd) = state.current_command.as_ref() else {
            return Self::Idle;
        };
        match &cmd.status {
            CommandStatus::Pending | CommandStatus::Running => Self::Running {
                command: cmd.subcommand.clone(),
            },
            CommandStatus::Done => Self::Done {
                command: cmd.subcommand.clone(),
                exit_code: cmd.exit_code.unwrap_or(0),
            },
            CommandStatus::Error(message) => Self::Error {
                command: cmd.subcommand.clone(),
                message: message.clone(),
            },
        }
    }
}

/// Container overlay window state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainerWindowState {
    Hidden,
    Minimized,
    Maximized,
}

impl ContainerWindowState {
    pub fn cycle(self) -> Self {
        match self {
            Self::Hidden => Self::Maximized,
            Self::Minimized => Self::Maximized,
            Self::Maximized => Self::Minimized,
        }
    }
}

/// Workflow Overview display mode.
///
/// The overview defaults to `Minimized`: exactly one 3-row box per topological
/// stage, so it never costs the execution window more than 3 rows. A stage
/// with a single step draws that step's normal box; a parallel stage draws a
/// `N steps…` summary box in the stage's aggregate status colour.
///
/// `Maximized` draws every step of every stage as its own box — full name,
/// agent/model label, status colour, nothing rolled up. It grows into the
/// space the frame can spare between the tab bar and the command box, ahead of
/// the execution window and the container status bars — but it never displaces
/// a maximized container PTY, which keeps its own share of the body.
///
/// Toggled with `Ctrl-O` ("overview"), independently of the container PTY's
/// own `Ctrl-M` min/max ([`ContainerWindowState`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WorkflowOverviewState {
    #[default]
    Minimized,
    Maximized,
}

impl WorkflowOverviewState {
    pub fn toggle(self) -> Self {
        match self {
            Self::Minimized => Self::Maximized,
            Self::Maximized => Self::Minimized,
        }
    }

    pub fn is_maximized(self) -> bool {
        matches!(self, Self::Maximized)
    }
}

/// Current workflow view state (visible when a workflow is running).
#[derive(Debug, Clone, Default)]
pub struct WorkflowViewState {
    pub steps: Vec<WorkflowStepView>,
    pub current_step: Option<String>,
    /// Effective `maxConcurrentAgents` for the running workflow (WI-0096 §11),
    /// set by the frontend when the engine reports a parallel group start.
    /// `None` means unlimited — the overview caps parallel rows at the legacy 3
    /// and renders no "queued" markers, so behavior is unchanged.
    pub max_concurrent: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct WorkflowStepView {
    pub name: String,
    pub status: StepViewStatus,
    /// Resolved agent (e.g. `"claude"`) — fed by `report_workflow_progress`.
    pub agent: Option<String>,
    /// Optional resolved model.
    pub model: Option<String>,
    /// Steps this one waits on. Drives the column-grouping in the overview
    /// renderer (steps with the same sorted `depends_on` set sit in the
    /// same topological column).
    pub depends_on: Vec<String>,
    /// Which phase this step belongs to. `Setup`/`Teardown` steps get their
    /// own dedicated first/last column in the overview rather than being
    /// grouped by `depends_on` topology alongside `Agent` steps.
    pub kind: WorkflowStepKind,
}

/// How one step of a workflow reads in the Workflow Overview.
///
/// The Layer 0 run carries two different status enums — `StepState` for agent
/// steps and `PhaseStepStatus` for setup/teardown steps — that the overview
/// renders identically. This is the one classification it renders from, and
/// both conversions ([`StepViewStatus::of_step_state`],
/// [`StepViewStatus::of_phase_step_status`]) are exhaustive, so a new Layer 0
/// variant is a compile error rather than a step that silently draws as
/// pending. Until WI 0114 F-22 this was a `String` matched on in four places.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepViewStatus {
    /// Not started.
    Pending,
    /// Executing now.
    Running,
    /// An `on_failure` remediation is running for this step.
    Fixing,
    /// Finished successfully.
    Done,
    /// Finished with a failure.
    Error,
    /// Abandoned before it could finish.
    Cancelled,
    /// Never ran because it was not needed.
    Skipped,
}

impl StepViewStatus {
    /// Whether the step will not run again.
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Done | Self::Cancelled | Self::Skipped)
    }

    /// How an agent step's Layer 0 state reads.
    pub fn of_step_state(state: &crate::data::workflow_state::StepState) -> Self {
        use crate::data::workflow_state::StepState;
        match state {
            StepState::Pending => Self::Pending,
            StepState::Running { .. } => Self::Running,
            StepState::Succeeded => Self::Done,
            StepState::Failed { .. } => Self::Error,
            StepState::Cancelled => Self::Cancelled,
            StepState::Skipped => Self::Skipped,
        }
    }

    /// How a setup/teardown step's Layer 0 state reads.
    pub fn of_phase_step_status(status: &crate::data::workflow_state::PhaseStepStatus) -> Self {
        use crate::data::workflow_state::PhaseStepStatus;
        match status {
            PhaseStepStatus::Pending => Self::Pending,
            PhaseStepStatus::Running => Self::Running,
            PhaseStepStatus::Succeeded => Self::Done,
            PhaseStepStatus::Failed { .. } => Self::Error,
            PhaseStepStatus::Remediating { .. } => Self::Fixing,
        }
    }
}

/// The phase a [`WorkflowStepView`] belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkflowStepKind {
    /// A `setup:` step, run once before the main workflow steps.
    Setup,
    /// An ordinary workflow step, grouped into columns by `depends_on`.
    Agent,
    /// A `teardown:` step, run once after the main workflow steps.
    Teardown,
}

/// Cross-thread shared workflow view state.
///
/// `WorkflowFrontend` (engine-driven, in a tokio task) writes to it; the TUI
/// renderer reads from it. Mirrors the pattern used by `SharedStatusLog`.
pub type SharedWorkflowViewState = Arc<Mutex<Option<WorkflowViewState>>>;

/// Snapshot of the status dashboard for TUI table rendering.
#[derive(Debug, Clone)]
pub struct StatusDashboardData {
    pub containers: Vec<crate::command::commands::status::StatusContainerRow>,
    pub tip: String,
}

/// Cross-thread shared status dashboard data. The status command writes here;
/// the TUI renderer reads it to display a proper `Table` widget.
pub type SharedStatusDashboard = Arc<Mutex<Option<StatusDashboardData>>>;

/// Cross-thread shared yolo-countdown state. The engine ticks it every 100ms
/// while a yolo countdown is active; the renderer reads it to display the
/// "Auto-advancing in Ns" non-modal overlay.
pub type SharedYoloState = Arc<Mutex<Option<YoloState>>>;

/// Shared flag: TUI event loop sets this to `true` when the user presses
/// Esc during a yolo countdown. `yolo_countdown_tick` checks it and
/// returns `Cancel` when set, then resets the flag.
pub type SharedYoloCancelFlag = Arc<AtomicBool>;

/// Shared flag set by the workflow frontend to signal the TUI event loop
/// to reset the vt100 parser before the next step's PTY output arrives.
pub type SharedPtyResetFlag = Arc<AtomicBool>;

/// Shared container name. Set by the container frontend when the engine
/// reports `AgentStatus::Running { container_name }`. The TUI event
/// loop reads this to populate `ContainerInfo.container_name` for stats
/// polling.
pub type SharedContainerName = Arc<Mutex<Option<String>>>;

/// Shared container exit code. Set by the workflow frontend when the engine
/// reports `report_container_exited` — the step's container has actually
/// terminated (killed by awman or the agent process exited). The TUI event
/// loop takes it and closes the container window, leaving the summary bar.
pub type SharedContainerExitCode = Arc<Mutex<Option<i32>>>;

/// Shared active-worktree path. Set by the worktree-lifecycle frontend on
/// `report_worktree_created` and cleared on the post-workflow report
/// (kept/discarded). The renderer reads this so the bottom-bar context
/// line can show "Using worktree: <path>" while a workflow runs in a
/// worktree even though the tab's session is rooted at the main repo.
pub type SharedActiveWorktreePath = Arc<Mutex<Option<std::path::PathBuf>>>;

/// Shared stdin sender slot. When a workflow step transition creates fresh
/// stdin channels, the new sender is published here so the TUI event loop
/// can swap `tab.container_stdin_tx` to the new one.
pub type SharedStdinTx = Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<Vec<u8>>>>>;

/// Shared resize sender slot, same pattern as `SharedStdinTx`.
pub type SharedResizeTx = Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<(u16, u16)>>>>;

/// Shared engine sender. The engine creates the channel and publishes
/// the sender via `set_engine_sender`; the TUI event loop reads it
/// to send Ctrl-W requests.
pub type SharedEngineTx =
    Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<crate::engine::workflow::EngineRequest>>>>;

/// Shared stuck sender. The engine publishes the container's stuck
/// broadcast sender via `set_stuck_sender`; the TUI event loop subscribes
/// from it for tab-coloring (stuck indicator).
pub type SharedStuckSender = Arc<Mutex<Option<Arc<tokio::sync::broadcast::Sender<StuckEvent>>>>>;

/// Shared TUI context for the status command. The event loop refreshes this
/// on every tick so the status watch loop always sees live tab data.
pub type SharedTuiContext = Arc<Mutex<crate::command::commands::status::StatusCommandTuiContext>>;

#[derive(Debug, Clone)]
pub struct YoloState {
    pub step_name: String,
    pub remaining_secs: u64,
}

/// Mouse text selection.
///
/// Coordinates are stored in window cell space (0-based against the window
/// the selection started in — the maximized container's vt100 grid or the
/// execution window's inner area), not raw terminal coords. The renderer
/// publishes `Tab::container_inner_area` / `Tab::exec_inner_area` so
/// `handle_mouse_event` can subtract the window's screen offset before
/// recording these.
#[derive(Debug, Clone)]
pub struct TextSelection {
    pub start_col: u16,
    pub start_row: u16,
    pub end_col: u16,
    pub end_row: u16,
    /// Snapshot of the window's text grid at selection-start time. Each cell
    /// is the printable contents of that position (or `" "` for empties), so
    /// the copied text reflects what the user *saw* when they started the
    /// drag, not the window's current values (which mutate with live output).
    pub snapshot: Vec<Vec<String>>,
}

/// Live container metadata, populated while a containerized command runs.
#[derive(Debug, Clone)]
pub struct ContainerInfo {
    pub agent_display_name: String,
    pub container_name: String,
    pub start_time: Instant,
    pub latest_stats: Option<AgentStats>,
    /// History of `(cpu_percent, memory_mb)` samples for averaging in the
    /// post-exit summary bar.
    pub stats_history: Vec<(f64, f64)>,
    /// Whether the active runtime is sandbox-class (e.g.
    /// `docker-sbx-experimental`) rather than container-class. Drives the
    /// overlay title — "(sandboxed)" vs "(containerized)".
    pub sandboxed: bool,
}

/// Summary captured after a containerized command exits, displayed in a
/// dashed-border bar below the execution window until the next command starts.
#[derive(Debug, Clone)]
pub struct LastContainerSummary {
    pub agent_display_name: String,
    pub container_name: String,
    pub avg_cpu: String,
    pub avg_memory: String,
    pub total_time: String,
    pub exit_code: i32,
}

/// Upper bound on the rendered ACP update history kept per slot. Once full,
/// the oldest entry is dropped as new ones arrive — a plain ring buffer.
/// There is no raw terminal scrollback to fall back on for an ACP window, so
/// this cap is the only backstop against unbounded growth.
pub const ACP_HISTORY_LIMIT: usize = 2000;

/// Rendered state for an ACP (Agent Client Protocol) agent window.
///
/// Unlike a stdio slot there is no raw terminal byte stream and no vt100
/// grid: the agent speaks structured [`SessionUpdate`] frames, which are kept
/// here as a bounded history and drawn as a scrollable list. A pending
/// permission request is parked here while its modal is open.
///
/// The state is shared (`Arc<Mutex<…>>`, see [`SharedAcpState`]) between the
/// TUI render thread, which reads the history to draw the window, and the ACP
/// frontend running on the engine task, which appends updates in
/// `render_update`. This mirrors the other engine→TUI shared handles
/// (`SharedStatusLog`, `SharedYoloState`): the render loop repaints every
/// tick, so an append is picked up on the next frame with no explicit redraw
/// signal.
#[derive(Debug, Default)]
pub struct AcpSlotState {
    /// Rendered update history, oldest first. Bounded to [`ACP_HISTORY_LIMIT`]
    /// entries (a ring buffer).
    pub history: std::collections::VecDeque<SessionUpdate>,
    /// The permission request currently awaiting a user decision, if any. Set
    /// when the modal opens and cleared when it resolves.
    pub pending_permission: Option<PermissionRequest>,
    /// Lines-from-bottom scroll offset into the rendered history. `0` follows
    /// the latest updates; increasing it scrolls toward older output. This is
    /// the ACP window's own simple list scroll — it never touches the
    /// vt100 scrollback path stdio slots use.
    pub scroll_offset: usize,
}

impl AcpSlotState {
    /// Append an update, enforcing the [`ACP_HISTORY_LIMIT`] ring-buffer cap.
    pub fn push_update(&mut self, update: SessionUpdate) {
        self.history.push_back(update);
        while self.history.len() > ACP_HISTORY_LIMIT {
            self.history.pop_front();
        }
    }
}

/// Cross-thread handle to an ACP window's [`AcpSlotState`]. Held by both the
/// [`ContainerSlot`] (TUI thread) and the ACP frontend (engine task).
pub type SharedAcpState = Arc<Mutex<AcpSlotState>>;

/// Which kind of agent window a [`ContainerSlot`] hosts.
///
/// A **stdio** slot bridges a real PTY/piped byte stream into the vt100 grid,
/// using the `vt100_parser`, `region_scroll`, terminal-mode flags, and I/O
/// channels on [`ContainerSlot`]. An **ACP** slot has no raw terminal stream:
/// its structured updates live in the shared [`AcpSlotState`] instead, and the
/// vt100 fields on the slot stay inert.
///
/// Keeping both behind one `ContainerSlot` type is what lets a parallel
/// workflow group mix stdio and ACP windows in a single `container_slots`
/// vec, so `focused_slot()` and the minimized-bar iteration stay uniform over
/// one slot type.
#[derive(Debug, Clone)]
pub enum AgentWindowKind {
    Stdio,
    Acp(SharedAcpState),
}

/// One running container. This is THE container representation — a plain
/// containerized command (`chat`, `exec prompt`) is simply a tab with one
/// slot, and a parallel workflow group is a tab with N of them (WI-0096).
///
/// Each slot owns its own PTY parser, terminal-mode flags, stats, and I/O
/// channels. The slot at `Tab::focused_slot_idx` renders maximized; the
/// others render as stacked minimized status bars. A slot's [`AgentWindowKind`]
/// selects whether it is driven by that stdio/PTY machinery or by a structured
/// ACP update stream (in which case the vt100 fields stay inert).
pub struct ContainerSlot {
    /// Whether this slot is a stdio (PTY/vt100) window or an ACP window.
    /// Stdio is the default; ACP slots carry their shared render state here.
    pub kind: AgentWindowKind,
    /// Workflow step this container runs, or empty for non-workflow
    /// commands and sequential workflow steps (whose step name comes from
    /// the workflow view state instead).
    pub step_name: String,
    pub vt100_parser: vt100::Parser,
    pub region_scroll: crate::frontend::tui::region_scroll::RegionScrollEmulator,
    pub container_info: Option<ContainerInfo>,
    pub container_stdout_rx: Option<tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>>,
    pub container_stdin_tx: Option<tokio::sync::mpsc::UnboundedSender<Vec<u8>>>,
    pub container_resize_tx: Option<tokio::sync::mpsc::UnboundedSender<(u16, u16)>>,
    /// Whether the agent has requested the alternate screen buffer. Tracked
    /// here (not via the vt100 parser) because `drain_container_output`
    /// strips alternate-screen sequences before the parser sees them.
    pub agent_alt_screen: bool,
    /// Whether the agent has enabled "alternate scroll" mode (DECSET 1007),
    /// tracked from the raw PTY output for the same reason.
    pub agent_alternate_scroll: bool,
    pub stuck: bool,
    pub yolo_mode: bool,
    pub yolo_state: SharedYoloState,
    pub yolo_cancel_flag: SharedYoloCancelFlag,
    pub stuck_rx: Option<tokio::sync::broadcast::Receiver<StuckEvent>>,
}

pub struct ContainerSlotIo {
    pub stdout_rx: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
    pub stdin_tx: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    pub resize_tx: tokio::sync::mpsc::UnboundedSender<(u16, u16)>,
}

impl ContainerSlot {
    /// Create a fresh slot for a newly-launched container. The PTY parser
    /// starts at 80x24 and is sized to the real overlay dimensions as soon
    /// as they are known; live I/O channels are attached by the caller.
    pub fn new(step_name: String, agent_display_name: String, scrollback: usize) -> Self {
        Self {
            kind: AgentWindowKind::Stdio,
            step_name,
            vt100_parser: vt100::Parser::new(24, 80, scrollback),
            region_scroll: crate::frontend::tui::region_scroll::RegionScrollEmulator::new(),
            container_info: Some(ContainerInfo {
                agent_display_name,
                container_name: String::new(),
                start_time: Instant::now(),
                latest_stats: None,
                stats_history: Vec::new(),
                sandboxed: false,
            }),
            container_stdout_rx: None,
            container_stdin_tx: None,
            container_resize_tx: None,
            agent_alt_screen: false,
            agent_alternate_scroll: false,
            stuck: false,
            yolo_mode: false,
            yolo_state: Arc::new(Mutex::new(None)),
            yolo_cancel_flag: Arc::new(AtomicBool::new(false)),
            stuck_rx: None,
        }
    }

    /// Create a fresh ACP window slot. The vt100 fields exist but stay inert
    /// (no PTY stream feeds them); the window renders from `state` instead.
    /// `state` is the same handle the ACP frontend appends updates to, so the
    /// caller shares one `Arc` between the slot and the frontend.
    pub fn new_acp(step_name: String, agent_display_name: String, state: SharedAcpState) -> Self {
        let mut slot = Self::new(step_name, agent_display_name, 0);
        slot.kind = AgentWindowKind::Acp(state);
        slot
    }

    /// Whether this slot hosts an ACP window (rather than a stdio/PTY one).
    pub fn is_acp(&self) -> bool {
        matches!(self.kind, AgentWindowKind::Acp(_))
    }

    /// The shared ACP render state, when this is an ACP slot.
    pub fn acp_state(&self) -> Option<&SharedAcpState> {
        match &self.kind {
            AgentWindowKind::Acp(state) => Some(state),
            AgentWindowKind::Stdio => None,
        }
    }

    /// Agent display name for the minimized bar, falling back to "agent".
    pub fn agent_name(&self) -> &str {
        self.container_info
            .as_ref()
            .map(|i| i.agent_display_name.as_str())
            .unwrap_or("agent")
    }

    /// Elapsed run time for the minimized bar.
    pub fn elapsed_secs(&self) -> u64 {
        self.container_info
            .as_ref()
            .map(|i| i.start_time.elapsed().as_secs())
            .unwrap_or(0)
    }
}

/// Lifecycle event published by the workflow frontend (engine thread) and
/// drained by the TUI event loop to maintain `Tab::container_slots`
/// (WI-0096 §12). Kept in a shared queue rather than mutating `Tab` directly
/// because the frontend runs on the engine's tokio task while `Tab` lives on
/// the TUI thread.
pub enum ContainerSlotEvent {
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

/// The cross-thread slots a [`Tab`] shares with the command thread running
/// against it.
///
/// Every field is an `Arc` handle: the tab keeps one end, the
/// `TuiCommandFrontend` built for a command keeps the other, and both observe
/// the same value. They travel together — a frontend that received some of
/// them and not others would render against a tab it is only half wired to —
/// so they are one clonable bundle rather than sixteen constructor
/// parameters.
#[derive(Clone)]
pub struct TabSharedState {
    /// Workflow view state written by the engine's `WorkflowFrontend` impl and
    /// read by the Workflow Overview renderer.
    pub workflow_state: SharedWorkflowViewState,
    /// Invocation id of the latest remote workflow snapshot, when available.
    pub workflow_invocation_id: Arc<Mutex<Option<uuid::Uuid>>>,
    /// Yolo countdown state, rendered as a non-modal overlay.
    pub yolo_state: SharedYoloState,
    /// Cancel flag for the yolo countdown; set on Esc, read and cleared by
    /// `yolo_countdown_tick`.
    pub yolo_cancel_flag: SharedYoloCancelFlag,
    /// The tab's status log, appended to by `TuiUserMessageSink`.
    pub status_log: SharedStatusLog,
    /// Structured `status` dashboard rows, rendered as a `Table` widget.
    pub status_dashboard: SharedStatusDashboard,
    /// Queue of container-slot lifecycle events, drained each tick.
    pub container_slot_events: SharedContainerSlotEvents,
    /// Signals the event loop to reset the vt100 parser between steps.
    pub pty_reset_flag: SharedPtyResetFlag,
    /// Name of the running container, published by the container frontend.
    pub container_name_shared: SharedContainerName,
    /// Exit code of a mid-workflow container that actually terminated.
    pub container_exit_shared: SharedContainerExitCode,
    /// Stdin sender slot, republished on each workflow step transition.
    pub stdin_tx_shared: SharedStdinTx,
    /// Resize sender slot, same pattern as `stdin_tx_shared`.
    pub resize_tx_shared: SharedResizeTx,
    /// Engine request sender, published by the engine for Ctrl-W.
    pub engine_tx_shared: SharedEngineTx,
    /// Stuck-event broadcast sender, published by the container engine.
    pub stuck_sender_shared: SharedStuckSender,
    /// Active worktree path, driving the bottom-bar context line.
    pub active_worktree_path: SharedActiveWorktreePath,
    /// Live TUI context refreshed each tick for `status --watch`.
    pub tui_context_shared: SharedTuiContext,
}

impl TabSharedState {
    /// A fresh, empty set of slots. Every tab starts with its own.
    pub fn new() -> Self {
        Self {
            workflow_state: Arc::new(Mutex::new(None)),
            workflow_invocation_id: Arc::new(Mutex::new(None)),
            yolo_state: Arc::new(Mutex::new(None)),
            yolo_cancel_flag: Arc::new(AtomicBool::new(false)),
            status_log: Arc::new(Mutex::new(Vec::new())),
            status_dashboard: Arc::new(Mutex::new(None)),
            container_slot_events: Arc::new(Mutex::new(std::collections::VecDeque::new())),
            pty_reset_flag: Arc::new(AtomicBool::new(false)),
            container_name_shared: Arc::new(Mutex::new(None)),
            container_exit_shared: Arc::new(Mutex::new(None)),
            stdin_tx_shared: Arc::new(Mutex::new(None)),
            resize_tx_shared: Arc::new(Mutex::new(None)),
            engine_tx_shared: Arc::new(Mutex::new(None)),
            stuck_sender_shared: Arc::new(Mutex::new(None)),
            active_worktree_path: Arc::new(Mutex::new(None)),
            tui_context_shared: Arc::new(Mutex::new(
                crate::command::commands::status::StatusCommandTuiContext::default(),
            )),
        }
    }

    /// Fresh slots for a test that builds a `TuiCommandFrontend` without a
    /// `Tab`. Identical to [`TabSharedState::new`]; named so the call sites
    /// read as fixtures rather than as production wiring.
    #[cfg(test)]
    pub fn for_tests() -> Self {
        Self::new()
    }

    /// Replace the per-command slots before a new command is spawned into the
    /// tab, so a stale container name, exit code or I/O sender from the
    /// previous command cannot be observed by the new one. The log, workflow
    /// view and dashboard slots persist for the life of the tab.
    pub fn reset_for_new_command(&mut self) {
        self.container_name_shared = Arc::new(Mutex::new(None));
        self.container_exit_shared = Arc::new(Mutex::new(None));
        self.stdin_tx_shared = Arc::new(Mutex::new(None));
        self.resize_tx_shared = Arc::new(Mutex::new(None));
        self.engine_tx_shared = Arc::new(Mutex::new(None));
    }
}

impl Default for TabSharedState {
    fn default() -> Self {
        Self::new()
    }
}

/// Tab state — one per open tab.
pub struct Tab {
    /// Identity of the manager-owned session backing this tab. The session
    /// snapshot below remains the pre-F-22 view data; WI 0114 moves that view
    /// state out of Tab without changing ownership again.
    pub session_id: SessionId,
    pub session: Session,
    pub(crate) git_engine: Arc<GitEngine>,
    /// Derived from `SessionState::current_command` by
    /// [`Tab::refresh_from_session`] each tick. Never assigned directly:
    /// whatever starts or ends a command records that on the session, and this
    /// follows.
    pub execution_phase: ExecutionPhase,
    /// Derived from `SessionState::current_workflow` each tick — the live run's
    /// summary, mirrored by `WorkflowEngine::persist`.
    pub current_workflow: Option<crate::data::workflow_state::WorkflowSummary>,
    /// Derived from `SessionState::current_container` each tick.
    pub current_container: Option<crate::data::session::AgentHandle>,

    pub container_window_state: ContainerWindowState,
    /// How many lines from the bottom to skip in the focused slot's vt100
    /// scrollback when the container is Maximized. 0 = follow live output.
    pub container_scroll_offset: usize,
    /// Summary of the last container session, shown in a dashed-border bar
    /// below the exec window after the container exits.
    pub last_container_summary: Option<LastContainerSummary>,
    /// Inner content rect of the container overlay, refreshed each frame by
    /// the renderer. Used by the mouse handler to translate raw terminal
    /// coords into vt100 cell coords.
    pub container_inner_area: Option<Rect>,
    /// Whether the container overlay has been drawn at least once this
    /// session. Set by the renderer when it draws the maximized overlay;
    /// cleared at command start. When the agent exits before any frame was
    /// drawn (fast-failing launch), `close_container_overlay` dumps the
    /// captured terminal contents to the status log instead of silently
    /// discarding them.
    pub container_rendered: bool,
    /// Inner content rect of the execution window, refreshed each frame by
    /// the renderer while the container overlay is not Maximized. Used by
    /// the mouse handler to translate raw terminal coords into execution
    /// window cell coords when starting a text selection there.
    pub exec_inner_area: Option<Rect>,
    /// Visible text grid of the execution window, refreshed each frame by
    /// the renderer while the container overlay is not Maximized. Cloned
    /// into `TextSelection::snapshot` at selection-start so the copied text
    /// reflects what the user saw (the window content shifts as new
    /// status-log lines arrive).
    pub exec_window_grid: Vec<Vec<String>>,
    /// Cross-thread slots shared with the command thread (see
    /// [`TabSharedState`]). Handed to the `TuiCommandFrontend` as one
    /// bundle when a command is spawned into this tab.
    pub shared: TabSharedState,
    pub status_log_collapsed: bool,
    pub scroll_offset: usize,
    pub workflow_overview_scroll_offset: usize,
    pub workflow_overview_hscroll_offset: usize,
    pub workflow_overview_hscroll_follow: bool,
    /// Whether the Workflow Overview shows one box per stage (the default) or
    /// every parallel step of every stage. Toggled with `Ctrl-O`, independently
    /// of the container PTY's `Ctrl-M` min/max.
    pub workflow_overview_state: WorkflowOverviewState,
    pub last_overview_rect: Option<Rect>,
    pub last_overview_hlayout: Option<crate::frontend::tui::workflow_view::HorizontalLayout>,
    /// Last remote workflow invocation rendered on this tab.
    pub last_workflow_invocation_id: Option<uuid::Uuid>,
    pub mouse_selection: Option<TextSelection>,
    /// Fixed tab kind. A squad tab is not bound to a project directory and
    /// renders squad content in place of the execution window. Never toggled
    /// after construction.
    ///
    /// Remoteness is *not* a field here: it is a property of the session
    /// (`SessionType::Remote`), read through it. `Tab::is_remote` was a `bool`
    /// that nothing outside the initialiser and two tests ever set, so the
    /// magenta remote tab colour could not appear (WI 0114 F-22).
    pub is_squad: bool,
    /// Squad sub-view state (selection, polled tasks, daemon reachability).
    /// `Some` exactly when `is_squad`.
    pub squad: Option<SquadTabState>,
    pub output_lines: Vec<String>,
    pub stuck: bool,
    pub yolo_mode: bool,
    /// Broadcast receiver for stuck/unstuck events from the container engine.
    /// Drained non-blockingly in `tick_all_tabs` for tab coloring.
    pub stuck_rx: Option<tokio::sync::broadcast::Receiver<StuckEvent>>,

    // ── Container slots ──────────────────────────────────────────────────
    /// The tab's running containers. A plain containerized command is one
    /// slot; a parallel workflow group is N of them. Empty while nothing
    /// containerized is running. The slot at `focused_slot_idx` renders
    /// maximized; the others render as stacked minimized status bars.
    pub container_slots: Vec<ContainerSlot>,
    /// Index into `container_slots` of the Maximized (focused) slot. Cycled
    /// by Ctrl-S. Always `0` with a single slot.
    pub focused_slot_idx: usize,
    /// The sequential "backbone" slot(s), stashed while a parallel workflow
    /// group runs. Sequential steps reuse the command-level stdout channel,
    /// so the slot holding its receiver must stay alive across the group
    /// and is restored when the group finishes. Non-empty exactly while a
    /// parallel group is active.
    pub dormant_slots: Vec<ContainerSlot>,
    /// Set after a mid-workflow container exit closes the window: PTY bytes
    /// that were still in flight from the dead container must not re-open it
    /// via `drain_container_output`'s auto-open branch. Cleared when the next
    /// container launches (new command, step transition, or a fresh
    /// `Running { container_name }` report).
    pub suppress_container_auto_open: bool,

    // ── Async command plumbing ───────────────────────────────────────────
    /// Receives the command outcome once the spawned task finishes.
    pub command_result_rx: Option<std::sync::mpsc::Receiver<Result<CommandOutcome, CommandError>>>,
    /// Event loop polls for dialog requests from the command thread.
    pub dialog_request_rx: Option<std::sync::mpsc::Receiver<DialogRequest>>,
    /// Event loop sends dialog responses back to the command thread.
    pub dialog_response_tx: Option<std::sync::mpsc::Sender<DialogResponse>>,

    // ── Git sidebar ──────────────────────────────────────────────────────
    /// Whether the git sidebar is open. Toggled by Ctrl-G.
    pub git_sidebar_state: GitSidebarState,
    /// Shared diff summary written by the background poll task and read by the
    /// renderer for the sidebar and the status-bar `+X -Y` summary.
    pub git_diff_summary: SharedGitDiffSummary,
    /// Handle to the background poll task, aborted on `Drop`.
    git_poll_handle: Option<tokio::task::JoinHandle<()>>,
    /// Cancellation token for the current poll task; triggered before a
    /// restart (worktree change) and on `Drop`.
    git_poll_cancel: Option<tokio_util::sync::CancellationToken>,
    /// The directory the poll task is currently watching. Compared against the
    /// desired root each tick so the task restarts when the worktree changes.
    git_poll_root: Option<std::path::PathBuf>,
}

impl Drop for Tab {
    fn drop(&mut self) {
        // Stop the background git poll task so it doesn't outlive the tab.
        if let Some(cancel) = self.git_poll_cancel.take() {
            cancel.cancel();
        }
        if let Some(handle) = self.git_poll_handle.take() {
            handle.abort();
        }
    }
}

impl Tab {
    /// Apply a manual one-column scroll while the overview overflows, and
    /// detach follow mode. The next render clamps the requested offset, and
    /// re-attaches follow once the view is scrolled all the way right with
    /// the running stage in view (see `hscroll_follow_reattaches`).
    pub(crate) fn scroll_workflow_overview_horizontal(&mut self, right: bool) {
        if !self
            .last_overview_hlayout
            .is_some_and(|layout| layout.overflows())
        {
            return;
        }
        self.workflow_overview_hscroll_offset = if right {
            self.workflow_overview_hscroll_offset.saturating_add(1)
        } else {
            self.workflow_overview_hscroll_offset.saturating_sub(1)
        };
        self.workflow_overview_hscroll_follow = false;
    }

    pub fn new(session: Session) -> Self {
        Self::new_with_git_engine(session, Arc::new(GitEngine::new()))
    }

    pub fn new_with_git_engine(session: Session, git_engine: Arc<GitEngine>) -> Self {
        let git_root = session.git_root().to_path_buf();
        let mut tab = Self::new_inner(session, git_engine);
        // Start polling against the session git root. Once a worktree is
        // created, `refresh_git_poll` (called each tick) restarts the task
        // pointed at the worktree path.
        tab.start_git_poll(git_root);
        tab
    }

    /// Construct the singleton squad tab. Unlike [`Tab::new`] this starts **no**
    /// git poll: the synthetic session is rooted at the squad storage root,
    /// which has no meaningful diff. The caller must not auto-spawn a startup
    /// command into this tab.
    pub fn new_squad(session: Session) -> Self {
        let mut tab = Self::new_inner(session, Arc::new(GitEngine::new()));
        tab.is_squad = true;
        tab.squad = Some(SquadTabState::new());
        tab
    }

    /// The tab's cross-thread slots, cloned for the command thread that is
    /// about to run against this tab.
    pub fn shared(&self) -> TabSharedState {
        self.shared.clone()
    }

    /// Adopt the manager-owned session's in-flight state as this tab's view,
    /// once per tick (decision Q3, WI 0114 F-22).
    ///
    /// Everything derived here is written by Layer 2 (`Dispatch::run_command`)
    /// or Layer 1 (`WorkflowEngine::persist`), never by the tab. The tab keeps
    /// the derived values as plain fields rather than re-deriving them in
    /// every renderer, because rendering happens many times per tick and this
    /// runs once.
    /// Adopt a terminal phase this tab observed, so the frame being drawn is
    /// already right.
    ///
    /// A view update only. Layer 2 has recorded the same outcome on the
    /// session before the result reaches this tab, and
    /// `refresh_from_session` re-derives `execution_phase` from there on the
    /// next tick — including for a command that panicked, which
    /// `Dispatch`'s own guard records (decision Q3, WI 0114 F-22). The tab
    /// writes nothing back.
    pub(crate) fn record_terminal_phase(&mut self, phase: ExecutionPhase) {
        self.execution_phase = phase;
    }

    pub fn refresh_from_session(&mut self, session: &Session) {
        self.session = session.clone();
        self.execution_phase = ExecutionPhase::of_session_state(session.state());
        self.current_workflow = session.state().current_workflow.clone();
        self.current_container = session.state().current_container.clone();
    }

    /// Shared field initialisation for [`Tab::new`] and [`Tab::new_squad`].
    /// Starts **no** poll and spawns nothing; the caller decides whether a git
    /// poll runs (normal tab) or not (squad tab).
    fn new_inner(session: Session, git_engine: Arc<GitEngine>) -> Self {
        Self {
            session_id: session.id(),
            session,
            git_engine,
            execution_phase: ExecutionPhase::Idle,
            current_workflow: None,
            current_container: None,
            container_window_state: ContainerWindowState::Hidden,
            container_scroll_offset: 0,
            last_container_summary: None,
            container_inner_area: None,
            container_rendered: false,
            exec_inner_area: None,
            exec_window_grid: Vec::new(),
            shared: TabSharedState::new(),
            status_log_collapsed: false,
            scroll_offset: 0,
            workflow_overview_scroll_offset: 0,
            workflow_overview_hscroll_offset: 0,
            workflow_overview_hscroll_follow: true,
            workflow_overview_state: WorkflowOverviewState::Minimized,
            last_overview_rect: None,
            last_overview_hlayout: None,
            last_workflow_invocation_id: None,
            mouse_selection: None,
            is_squad: false,
            squad: None,
            output_lines: Vec::new(),
            stuck: false,
            yolo_mode: false,
            stuck_rx: None,
            container_slots: Vec::new(),
            focused_slot_idx: 0,
            dormant_slots: Vec::new(),
            suppress_container_auto_open: false,
            command_result_rx: None,
            dialog_request_rx: None,
            dialog_response_tx: None,
            git_sidebar_state: GitSidebarState::Closed,
            git_diff_summary: Arc::new(Mutex::new(None)),
            git_poll_handle: None,
            git_poll_cancel: None,
            git_poll_root: None,
        }
    }
}

/// Truncate a string to at most `max` characters; if longer, replace the
/// trailing characters with `…`.
fn truncate_with_ellipsis(s: &str, max: usize) -> String {
    if s.chars().count() > max {
        let trunc: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{}\u{2026}", trunc)
    } else {
        s.to_string()
    }
}

/// Format an elapsed-seconds count as a short human duration:
/// `"42s"` < 60s, `"7m"` < 1h, `"2h 15m"` otherwise.
pub fn format_duration(secs: u64) -> String {
    if secs < 60 {
        format!("{}s", secs)
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else {
        let h = secs / 3600;
        let m = (secs % 3600) / 60;
        format!("{}h {}m", h, m)
    }
}

/// Tab color based on execution state.
pub fn tab_color(tab: &Tab) -> ratatui::style::Color {
    use ratatui::style::Color;
    // Yolo countdown in progress: alternate yellow/magenta each second so
    // background tabs flash visibly, matching old-amux behavior.
    if let Ok(guard) = tab.shared.yolo_state.lock() {
        if let Some(ref state) = *guard {
            return if state.remaining_secs % 2 == 0 {
                Color::Yellow
            } else {
                Color::Magenta
            };
        }
    }
    if tab.stuck {
        return Color::Yellow;
    }
    // Fixed tab kinds. Both win over execution-phase colouring and both yield
    // to the stuck / yolo indicators above, which are transient run signals.
    if tab.is_squad {
        return Color::Cyan;
    }
    if tab.session.session_type().is_remote() {
        return Color::Magenta;
    }
    match &tab.execution_phase {
        ExecutionPhase::Error { .. } => Color::Red,
        ExecutionPhase::Running { .. } => {
            if tab.container_window_state != ContainerWindowState::Hidden {
                // The container-visible "running" color is the agent-window
                // identity color: purple when the focused slot is an ACP
                // window, green for a stdio one. The Blue/Red/DarkGray phase
                // states below and above are unchanged.
                if tab.focused_slot().is_some_and(|s| s.is_acp()) {
                    crate::frontend::tui::acp_view::ACP_BORDER_COLOR
                } else {
                    Color::Green
                }
            } else {
                Color::Blue
            }
        }
        ExecutionPhase::Idle | ExecutionPhase::Done { .. } => Color::DarkGray,
    }
}

/// Execution window border color based on phase and focus.
///
/// `acp` is `true` when the focused agent slot is an ACP window: the only
/// green state (focused + Done) then becomes the ACP identity color, matching
/// [`tab_color`]. The Blue/Gray/Red/DarkGray phase states are unchanged — the
/// stdio window's appearance is identical (callers pass `acp = false`).
pub fn window_border_color(
    phase: &ExecutionPhase,
    focused: bool,
    acp: bool,
) -> ratatui::style::Color {
    use ratatui::style::Color;
    match phase {
        ExecutionPhase::Error { .. } => Color::Red,
        ExecutionPhase::Running { .. } => {
            if focused {
                Color::Blue
            } else {
                Color::Gray
            }
        }
        ExecutionPhase::Done { .. } => {
            if focused {
                if acp {
                    crate::frontend::tui::acp_view::ACP_BORDER_COLOR
                } else {
                    Color::Green
                }
            } else {
                Color::Gray
            }
        }
        ExecutionPhase::Idle => Color::DarkGray,
    }
}

/// Phase label shown in the execution window border.
///
/// Glyphs and text mirror old awman exactly:
/// - Idle → `" awman "`
/// - Running → `" ● running: {cmd} "`  (U+25CF)
/// - Done (exit 0) → `" ✓ done: {cmd} "`  (U+2713)
/// - Done (non-zero exit) → `" ✗ error: {cmd} (exit N) "`  (U+2717)
/// - Error → `" ✗ error: {cmd} "`
pub fn phase_label(phase: &ExecutionPhase) -> String {
    match phase {
        ExecutionPhase::Idle => " awman ".to_string(),
        ExecutionPhase::Running { command } => format!(" \u{25cf} running: {command} "),
        ExecutionPhase::Done { command, exit_code } if *exit_code == 0 => {
            format!(" \u{2713} done: {command} ")
        }
        ExecutionPhase::Done { command, exit_code } => {
            format!(" \u{2717} error: {command} (exit {exit_code}) ")
        }
        ExecutionPhase::Error { command, .. } => format!(" \u{2717} error: {command} "),
    }
}

/// Compute the width of each tab in the tab bar.
///
/// Dynamic sizing:
/// - **Natural**: the widest "untruncated content" across all tabs (project
///   name title vs. subcommand body) plus 2 cells for the borders, with a
///   minimum of 20 (double the old minimum). Tabs grow as wide as needed
///   to fit their content.
/// - **Budget**: when all tabs fit within the area width at their natural
///   size, use the natural size. When they don't fit, shrink to share the
///   full width equally (`area_width / n`).
///
/// Tabs never shrink below 12 cells (enough for a truncated label + ellipsis).
pub fn compute_tab_bar_width(num_tabs: usize, area_width: u16, max_natural_content: u16) -> u16 {
    if num_tabs == 0 || area_width == 0 {
        return 0;
    }
    let n = num_tabs as u16;
    let min_tab_width: u16 = 20;
    let natural = (max_natural_content + 2).max(min_tab_width);
    let total_natural = natural.saturating_mul(n);
    if total_natural <= area_width {
        natural
    } else {
        (area_width / n).max(12)
    }
}

/// Output of [`strip_alternate_screen_sequences`]: the filtered bytes plus
/// the last private-mode toggles observed in the chunk (if any), so the tab
/// can track terminal state the vt100 parser never sees (alternate screen,
/// which is stripped) or ignores (alternate scroll, mode 1007).
struct StrippedOutput {
    bytes: Vec<u8>,
    /// Last alternate-screen toggle in the chunk: `Some(true)` = entered,
    /// `Some(false)` = left, `None` = no toggle seen.
    alt_screen: Option<bool>,
    /// Last alternate-scroll (DECSET/DECRST 1007) toggle in the chunk.
    alternate_scroll: Option<bool>,
}

/// Strip DEC Private Mode Set/Reset sequences that toggle the alternate
/// screen buffer.  Agents running inside the container (e.g. Claude Code
/// in TUI mode) send these, which switches the vt100 parser to an
/// alternate grid with zero scrollback — breaking mouse-wheel scrollback.
/// By filtering these sequences the parser stays on the primary grid and
/// scrollback accumulates normally.
///
/// Recognised sequences (single-parameter forms):
///   ESC[?1049h / ESC[?1049l   (alternate screen + save/restore cursor)
///   ESC[?47h   / ESC[?47l     (alternate screen, legacy)
///   ESC[?1047h / ESC[?1047l   (alternate screen, xterm)
///
/// Additionally *observes* (without stripping) the alternate-scroll mode:
///   ESC[?1007h / ESC[?1007l   (wheel → arrow keys while on alt screen)
///
/// Both observations are reported in [`StrippedOutput`] so the tab can
/// reconstruct the agent's intended terminal state.
fn strip_alternate_screen_sequences(input: &[u8]) -> StrippedOutput {
    const ALT_ON: &[&[u8]] = &[b"\x1b[?1049h", b"\x1b[?47h", b"\x1b[?1047h"];
    const ALT_OFF: &[&[u8]] = &[b"\x1b[?1049l", b"\x1b[?47l", b"\x1b[?1047l"];
    const ALT_SCROLL_ON: &[u8] = b"\x1b[?1007h";
    const ALT_SCROLL_OFF: &[u8] = b"\x1b[?1007l";

    let mut out = Vec::with_capacity(input.len());
    let mut alt_screen = None;
    let mut alternate_scroll = None;
    let mut i = 0;
    while i < input.len() {
        if input[i] == 0x1b {
            if let Some(seq) = ALT_ON.iter().find(|s| input[i..].starts_with(s)) {
                alt_screen = Some(true);
                i += seq.len();
                continue;
            }
            if let Some(seq) = ALT_OFF.iter().find(|s| input[i..].starts_with(s)) {
                alt_screen = Some(false);
                i += seq.len();
                continue;
            }
            if input[i..].starts_with(ALT_SCROLL_ON) {
                alternate_scroll = Some(true);
                out.extend_from_slice(ALT_SCROLL_ON);
                i += ALT_SCROLL_ON.len();
                continue;
            }
            if input[i..].starts_with(ALT_SCROLL_OFF) {
                alternate_scroll = Some(false);
                out.extend_from_slice(ALT_SCROLL_OFF);
                i += ALT_SCROLL_OFF.len();
                continue;
            }
        }
        out.push(input[i]);
        i += 1;
    }
    StrippedOutput {
        bytes: out,
        alt_screen,
        alternate_scroll,
    }
}
