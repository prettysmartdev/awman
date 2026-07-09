//! Per-tab state.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use ratatui::layout::Rect;

use crate::command::dispatch::CommandOutcome;
use crate::command::error::CommandError;
use crate::data::session::Session;
use crate::engine::agent_runtime::execution::{AgentStats, StuckEvent};
use crate::frontend::tui::dialogs::{DialogRequest, DialogResponse};
use crate::frontend::tui::git_sidebar::{
    start_git_diff_poll_task, GitSidebarState, SharedGitDiffSummary,
};
use crate::frontend::tui::user_message::SharedStatusLog;

/// Per-tab execution lifecycle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecutionPhase {
    Idle,
    Running { command: String },
    Done { command: String, exit_code: i32 },
    Error { command: String, message: String },
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

/// Current workflow view state (visible when a workflow is running).
#[derive(Debug, Clone, Default)]
pub struct WorkflowViewState {
    pub steps: Vec<WorkflowStepView>,
    pub current_step: Option<String>,
    /// Effective `maxConcurrentAgents` for the running workflow (WI-0096 §11),
    /// set by the frontend when the engine reports a parallel group start.
    /// `None` means unlimited — the strip caps parallel rows at the legacy 3
    /// and renders no "queued" markers, so behavior is unchanged.
    pub max_concurrent: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct WorkflowStepView {
    pub name: String,
    pub status: String,
    /// Resolved agent (e.g. `"claude"`) — fed by `report_workflow_progress`.
    pub agent: Option<String>,
    /// Optional resolved model.
    pub model: Option<String>,
    /// Steps this one waits on. Drives the column-grouping in the strip
    /// renderer (steps with the same sorted `depends_on` set sit in the
    /// same topological column).
    pub depends_on: Vec<String>,
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

/// One running container. This is THE container representation — a plain
/// containerized command (`chat`, `exec prompt`) is simply a tab with one
/// slot, and a parallel workflow group is a tab with N of them (WI-0096).
///
/// Each slot owns its own PTY parser, terminal-mode flags, stats, and I/O
/// channels. The slot at `Tab::focused_slot_idx` renders maximized; the
/// others render as stacked minimized status bars.
pub struct ContainerSlot {
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

/// Tab state — one per open tab.
pub struct Tab {
    pub session: Session,
    pub execution_phase: ExecutionPhase,
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
    /// Shared workflow view state. The engine's `WorkflowFrontend` impl
    /// writes here on `report_workflow_progress` / `report_step_status`;
    /// the renderer reads from here when drawing the workflow strip.
    pub workflow_state: SharedWorkflowViewState,
    /// Shared yolo countdown state. Updated by `yolo_countdown_tick` on the
    /// engine side; rendered as a non-modal overlay (avoids the dialog-spam
    /// that a per-tick `ask_dialog` would cause).
    pub yolo_state: SharedYoloState,
    /// Shared cancel flag for yolo countdown. TUI event loop sets this on
    /// Esc; `yolo_countdown_tick` reads + clears it.
    pub yolo_cancel_flag: SharedYoloCancelFlag,
    pub status_log: SharedStatusLog,
    pub status_log_collapsed: bool,
    pub status_dashboard: SharedStatusDashboard,
    pub scroll_offset: usize,
    pub workflow_strip_scroll_offset: usize,
    pub last_strip_rect: Option<Rect>,
    pub mouse_selection: Option<TextSelection>,
    pub workflow_agent_fallbacks: HashMap<String, String>,
    pub is_remote: bool,
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
    /// Shared queue of slot lifecycle events published by the workflow
    /// frontend; drained each tick to maintain `container_slots`.
    pub container_slot_events: SharedContainerSlotEvents,
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
    /// Shared flag: workflow frontend sets this to signal the TUI to reset the
    /// vt100 parser between workflow steps.
    pub pty_reset_flag: SharedPtyResetFlag,
    /// Shared container name: set by the container frontend when the engine
    /// reports the running container's name.
    pub container_name_shared: SharedContainerName,
    /// Shared container exit code: set by the workflow frontend when the
    /// engine reports a mid-workflow container has actually terminated.
    pub container_exit_shared: SharedContainerExitCode,
    /// Shared stdin sender slot for workflow step transitions.
    pub stdin_tx_shared: SharedStdinTx,
    /// Shared resize sender slot for workflow step transitions.
    pub resize_tx_shared: SharedResizeTx,
    /// Shared control board sender for mid-step WCB requests.
    pub engine_tx_shared: SharedEngineTx,
    /// Shared stuck sender from the container engine. The event loop
    /// subscribes from it when a new sender appears.
    pub stuck_sender_shared: SharedStuckSender,
    /// Shared active worktree path: set by the worktree-lifecycle frontend
    /// after a worktree is created/resumed, cleared after the workflow
    /// finalize step. Drives the "Using worktree: <path>" bottom-bar line.
    pub active_worktree_path: SharedActiveWorktreePath,
    /// Live TUI context for the status command. The event loop refreshes this
    /// on every tick; `TuiCommandFrontend` reads from it on each watch
    /// iteration so the status table always reflects current tab state.
    pub tui_context_shared: SharedTuiContext,

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
    pub fn new(session: Session) -> Self {
        let git_root = session.git_root().to_path_buf();
        let mut tab = Self {
            session,
            execution_phase: ExecutionPhase::Idle,
            container_window_state: ContainerWindowState::Hidden,
            container_scroll_offset: 0,
            last_container_summary: None,
            container_inner_area: None,
            container_rendered: false,
            exec_inner_area: None,
            exec_window_grid: Vec::new(),
            workflow_state: Arc::new(Mutex::new(None)),
            yolo_state: Arc::new(Mutex::new(None)),
            yolo_cancel_flag: Arc::new(AtomicBool::new(false)),
            status_log: Arc::new(Mutex::new(Vec::new())),
            status_log_collapsed: false,
            status_dashboard: Arc::new(Mutex::new(None)),
            scroll_offset: 0,
            workflow_strip_scroll_offset: 0,
            last_strip_rect: None,
            mouse_selection: None,
            workflow_agent_fallbacks: HashMap::new(),
            is_remote: false,
            output_lines: Vec::new(),
            stuck: false,
            yolo_mode: false,
            stuck_rx: None,
            container_slots: Vec::new(),
            focused_slot_idx: 0,
            dormant_slots: Vec::new(),
            container_slot_events: Arc::new(Mutex::new(std::collections::VecDeque::new())),
            suppress_container_auto_open: false,
            command_result_rx: None,
            dialog_request_rx: None,
            dialog_response_tx: None,
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
            git_sidebar_state: GitSidebarState::Closed,
            git_diff_summary: Arc::new(Mutex::new(None)),
            git_poll_handle: None,
            git_poll_cancel: None,
            git_poll_root: None,
        };
        // Start polling against the session git root. Once a worktree is
        // created, `refresh_git_poll` (called each tick) restarts the task
        // pointed at the worktree path.
        tab.start_git_poll(git_root);
        tab
    }

    /// (Re)start the git diff poll task against `root`, cancelling any existing
    /// task first. No-ops (leaving the summary untouched) when called outside a
    /// tokio runtime, e.g. in synchronous unit tests.
    fn start_git_poll(&mut self, root: std::path::PathBuf) {
        // Cancel and drop the previous task, if any.
        if let Some(cancel) = self.git_poll_cancel.take() {
            cancel.cancel();
        }
        if let Some(handle) = self.git_poll_handle.take() {
            handle.abort();
        }

        // `tokio::spawn` panics without a runtime; skip gracefully so
        // non-async tests can construct tabs.
        if tokio::runtime::Handle::try_current().is_err() {
            self.git_poll_root = Some(root);
            return;
        }

        let cancel = tokio_util::sync::CancellationToken::new();
        let handle =
            start_git_diff_poll_task(root.clone(), self.git_diff_summary.clone(), cancel.clone());
        self.git_poll_cancel = Some(cancel);
        self.git_poll_handle = Some(handle);
        self.git_poll_root = Some(root);
    }

    /// Restart the poll task if the tab's effective working directory changed.
    /// The effective root is the active worktree path when set, otherwise the
    /// session git root. Called each tick from `tick_all_tabs`.
    pub fn refresh_git_poll(&mut self) {
        let desired = self
            .active_worktree_path
            .lock()
            .ok()
            .and_then(|g| g.clone())
            .unwrap_or_else(|| self.session.git_root().to_path_buf());
        if self.git_poll_root.as_ref() != Some(&desired) {
            self.start_git_poll(desired);
        }
    }

    /// Drain pending stuck events from the broadcast channel and update
    /// the `stuck` flag for tab coloring.
    pub fn drain_stuck_events(&mut self) {
        // Pick up a new stuck sender from the engine if available.
        if let Ok(mut guard) = self.stuck_sender_shared.lock() {
            if let Some(sender) = guard.take() {
                self.stuck_rx = Some(sender.subscribe());
            }
        }
        if let Some(ref mut rx) = self.stuck_rx {
            while let Ok(event) = rx.try_recv() {
                match event {
                    StuckEvent::Stuck => self.stuck = true,
                    StuckEvent::Unstuck => self.stuck = false,
                    // Bridge already killed the container; clear the stuck
                    // flag because the step is failing rather than blocked.
                    StuckEvent::StartupGraceExpired => self.stuck = false,
                }
            }
        }

        // WI-0096 §9: while a parallel group is active (the sequential
        // backbone is stashed in `dormant_slots`), aggregate the stuck /
        // yolo indicators across the group's slots for tab coloring.
        // Outside a group the broadcast channel above and the spawn-time
        // yolo flag drive the indicators, exactly as for plain commands.
        if !self.dormant_slots.is_empty() {
            self.stuck = self.container_slots.iter().any(|s| s.stuck);
            self.yolo_mode = self.container_slots.iter().any(|s| s.yolo_mode);
        }
    }

    /// Number of active (non-exited) container slots. `1` for plain
    /// containerized commands, `N` during a parallel workflow group, `0`
    /// while nothing containerized runs.
    pub fn active_slot_count(&self) -> usize {
        self.container_slots.len()
    }

    /// Whether the tab is showing more than one container. Ctrl-S slot
    /// switching only activates here.
    pub fn has_multiple_slots(&self) -> bool {
        self.container_slots.len() > 1
    }

    /// Whether the container overlay is currently covering the execution
    /// window: a slot exists and the display is Maximized. Minimized shows
    /// every slot as a status bar (no overlay); Hidden shows nothing. Key
    /// routing, mouse routing, and the execution-window renderer must all
    /// agree with `render_frame` on this.
    pub fn container_overlay_active(&self) -> bool {
        self.container_window_state == ContainerWindowState::Maximized
            && !self.container_slots.is_empty()
    }

    /// Mutable access to the currently-focused container slot. `None` while
    /// nothing containerized is running.
    pub fn focused_slot_mut(&mut self) -> Option<&mut ContainerSlot> {
        self.container_slots.get_mut(self.focused_slot_idx)
    }

    /// Immutable counterpart to [`focused_slot_mut`](Self::focused_slot_mut).
    pub fn focused_slot(&self) -> Option<&ContainerSlot> {
        self.container_slots.get(self.focused_slot_idx)
    }

    /// Focused slot's vt100 parser. Panics when no slot exists — test-only
    /// convenience for feeding PTY bytes and probing the grid.
    #[cfg(test)]
    pub fn focused_parser_mut(&mut self) -> &mut vt100::Parser {
        let idx = self.focused_slot_idx;
        &mut self.container_slots[idx].vt100_parser
    }

    /// Drain the shared parallel-slot event queue and update `container_slots`
    /// accordingly (WI-0096 §12). Events are processed in the order the engine
    /// emitted them, so an "exited then dequeued" pair in the same tick evicts
    /// the old slot before adding the new one — keeping the net slot count
    /// correct.
    pub fn drain_container_slot_events(&mut self) {
        let events: Vec<ContainerSlotEvent> = match self.container_slot_events.lock() {
            Ok(mut q) => q.drain(..).collect(),
            Err(_) => return,
        };
        for event in events {
            self.apply_container_slot_event(event);
        }
    }

    fn apply_container_slot_event(&mut self, event: ContainerSlotEvent) {
        match event {
            ContainerSlotEvent::GroupStarted => {
                // Stash the sequential backbone slot(s) for the duration of
                // the group. Their stdout receivers must stay alive: later
                // sequential steps send through the same persistent channel.
                self.dormant_slots.append(&mut self.container_slots);
                self.focused_slot_idx = 0;
                // Evict any lingering summary bar (e.g. killed leader or the
                // previous group's last exited container) and unblock
                // auto-open so the first output from the new group's
                // containers immediately maximizes the window.
                self.last_container_summary = None;
                self.suppress_container_auto_open = false;
            }
            ContainerSlotEvent::Launched {
                step_name,
                agent,
                io,
                ..
            } => {
                if self
                    .container_slots
                    .iter()
                    .any(|s| s.step_name == step_name)
                {
                    return;
                }
                let scrollback = self.session.effective_config().scrollback_lines();
                let mut slot = ContainerSlot::new(step_name, agent, scrollback);
                if let Some(io) = io {
                    slot.container_stdout_rx = Some(io.stdout_rx);
                    slot.container_stdin_tx = Some(io.stdin_tx);
                    slot.container_resize_tx = Some(io.resize_tx);
                }
                // Size the fresh 80x24 parser to the real overlay dimensions
                // immediately and push the size to the container's PTY, so
                // the agent never lays out against the default grid. The
                // per-tick sync in `tick_all_tabs` keeps it correct after.
                let size = self
                    .container_inner_area
                    .map(|r| (r.width, r.height))
                    .or_else(|| {
                        crossterm::terminal::size().ok().map(|(tc, tr)| {
                            let sidebar = crate::frontend::tui::git_sidebar::sidebar_width(
                                tc,
                                self.git_sidebar_state,
                            );
                            crate::frontend::tui::compute_container_inner_size(
                                tc.saturating_sub(sidebar),
                                tr,
                            )
                        })
                    });
                if let Some((cols, rows)) = size {
                    slot.vt100_parser.screen_mut().set_size(rows, cols);
                    if let Some(ref tx) = slot.container_resize_tx {
                        let _ = tx.send((cols, rows));
                    }
                }
                self.container_slots.push(slot);
            }
            ContainerSlotEvent::ContainerName {
                step_name,
                container_name,
            } => {
                if let Some(info) = self
                    .slot_mut(&step_name)
                    .and_then(|s| s.container_info.as_mut())
                {
                    info.container_name = container_name;
                    info.latest_stats = None;
                }
            }
            ContainerSlotEvent::Exited { step_name } => self.evict_slot(&step_name),
            ContainerSlotEvent::Stuck { step_name } => {
                if let Some(slot) = self.slot_mut(&step_name) {
                    slot.stuck = true;
                }
            }
            ContainerSlotEvent::Unstuck { step_name } => {
                if let Some(slot) = self.slot_mut(&step_name) {
                    slot.stuck = false;
                }
            }
            ContainerSlotEvent::YoloStarted {
                step_name,
                cancel_flag,
            } => {
                if let Some(slot) = self.slot_mut(&step_name) {
                    slot.yolo_mode = true;
                    slot.yolo_cancel_flag = cancel_flag;
                }
            }
            ContainerSlotEvent::YoloTick {
                step_name,
                remaining_secs,
            } => {
                if let Some(slot) = self.slot_mut(&step_name) {
                    if let Ok(mut guard) = slot.yolo_state.lock() {
                        *guard = Some(YoloState {
                            step_name,
                            remaining_secs,
                        });
                    }
                }
            }
            ContainerSlotEvent::YoloFinished { step_name } => {
                if let Some(slot) = self.slot_mut(&step_name) {
                    slot.yolo_mode = false;
                    if let Ok(mut guard) = slot.yolo_state.lock() {
                        *guard = None;
                    }
                }
            }
            ContainerSlotEvent::GroupFinished => {
                // Drop the group's slots and restore the sequential backbone
                // so the next sequential step's output (sent through the
                // persistent command-level channel) has a slot to land in.
                self.container_slots.clear();
                self.container_slots.append(&mut self.dormant_slots);
                self.focused_slot_idx = 0;
            }
        }
    }

    /// Drain pending PTY output from every slot into its vt100 parser.
    ///
    /// Auto-opens the container overlay to Maximized the first time bytes
    /// arrive so the user sees the PTY output immediately without having to
    /// manually cycle with Ctrl+M.
    ///
    /// Between sequential workflow steps the engine sets `pty_reset_flag`,
    /// which reinitializes the focused slot's parser (clearing the previous
    /// step's terminal content) before the new step's output is processed.
    pub fn drain_container_output(&mut self) {
        if self.pty_reset_flag.swap(false, Ordering::Relaxed) {
            let scrollback = self.session.effective_config().scrollback_lines();
            let focused_idx = self.focused_slot_idx;
            if let Some(slot) = self.container_slots.get_mut(focused_idx) {
                let (rows, cols) = slot.vt100_parser.screen().size();
                slot.vt100_parser = vt100::Parser::new(rows, cols, scrollback);
                slot.agent_alt_screen = false;
                slot.agent_alternate_scroll = false;
                slot.region_scroll.reset();
            }
            self.container_scroll_offset = 0;
            self.container_rendered = false;
            self.mouse_selection = None;
            // A new step's container is launching — allow auto-open again.
            self.suppress_container_auto_open = false;
        }

        let mut received_any = false;
        for slot in &mut self.container_slots {
            let Some(rx) = slot.container_stdout_rx.as_mut() else {
                continue;
            };
            while let Ok(bytes) = rx.try_recv() {
                let filtered = strip_alternate_screen_sequences(&bytes);
                if let Some(on) = filtered.alt_screen {
                    slot.agent_alt_screen = on;
                }
                if let Some(on) = filtered.alternate_scroll {
                    slot.agent_alternate_scroll = on;
                }
                slot.region_scroll
                    .process(&mut slot.vt100_parser, &filtered.bytes);
                received_any = true;
            }
        }
        if received_any
            && !self.suppress_container_auto_open
            && self.container_window_state == ContainerWindowState::Hidden
        {
            self.container_window_state = ContainerWindowState::Maximized;
        }
    }

    fn slot_mut(&mut self, step_name: &str) -> Option<&mut ContainerSlot> {
        self.container_slots
            .iter_mut()
            .find(|s| s.step_name == step_name)
    }

    /// Remove the slot for `step_name` with NO grey summary bar (WI-0096 §12)
    /// and recompute `focused_slot_idx`: if a slot before the focused one is
    /// removed, shift the index down; if the focused slot itself exits, advance
    /// to the next live slot (wrapping). When no slots remain, reset the index
    /// and let the container window go Hidden.
    fn evict_slot(&mut self, step_name: &str) {
        let Some(pos) = self
            .container_slots
            .iter()
            .position(|s| s.step_name == step_name)
        else {
            return;
        };
        self.container_slots.remove(pos);
        let len = self.container_slots.len();
        if len == 0 {
            self.focused_slot_idx = 0;
            self.container_window_state = ContainerWindowState::Hidden;
            return;
        }
        if pos < self.focused_slot_idx {
            self.focused_slot_idx -= 1;
        } else if pos == self.focused_slot_idx {
            // Removing shifts the next live slot into `pos`; wrap if it was the
            // last slot.
            if self.focused_slot_idx >= len {
                self.focused_slot_idx = 0;
            }
        }
    }

    /// Advance `focused_slot_idx` to the next active slot (WI-0096 §6, Ctrl-S).
    /// No-op with zero or one slot. Returns the newly-focused slot's resize
    /// sender (if any) so the caller can push the current terminal size to it.
    pub fn cycle_focused_slot(&mut self) {
        if self.container_slots.len() <= 1 {
            return;
        }
        self.focused_slot_idx = (self.focused_slot_idx + 1) % self.container_slots.len();
        self.mouse_selection = None;
        // The scrollback offset belongs to the overlay's current content;
        // start the rotated-in slot at its live view.
        self.container_scroll_offset = 0;
        // Un-minimize so the rotated container becomes visible.
        if self.container_window_state == ContainerWindowState::Minimized {
            self.container_window_state = ContainerWindowState::Maximized;
        }
    }

    /// Install a fresh single container slot for a newly-spawned command,
    /// replacing any previous slots. The slot's parser is sized to
    /// `cols`x`rows` (the computed overlay inner size); I/O channels are
    /// attached by the caller via [`focused_slot_mut`](Self::focused_slot_mut).
    ///
    /// This is the N==1 case of the unified slot model: a plain containerized
    /// command is simply a one-slot group.
    pub fn start_container(
        &mut self,
        agent_display_name: String,
        container_name: String,
        cols: u16,
        rows: u16,
    ) {
        self.container_scroll_offset = 0;
        self.container_rendered = false;
        self.last_container_summary = None;
        self.mouse_selection = None;
        let scrollback = self.session.effective_config().scrollback_lines();
        let mut slot = ContainerSlot::new(String::new(), agent_display_name, scrollback);
        slot.vt100_parser.screen_mut().set_size(rows, cols);
        if let Some(info) = slot.container_info.as_mut() {
            info.container_name = container_name;
        }
        self.container_slots.clear();
        self.dormant_slots.clear();
        self.focused_slot_idx = 0;
        self.container_slots.push(slot);
    }

    /// Project name for the tab title, truncated to fit a `tab_width`-wide
    /// tab cell. The rendered title is `" ➡ {name} "` inside the two border
    /// cells — 6 chars of chrome in the active variant, which sizing always
    /// reserves — so the name may use `tab_width - 6` chars. Wide tabs show
    /// more of a long project name instead of clipping at a fixed length;
    /// pass `u16::MAX` to measure the untruncated name.
    pub fn project_name(&self, tab_width: u16) -> String {
        let name = self
            .session
            .working_dir()
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("?")
            .to_string();
        let max = (tab_width as usize).saturating_sub(6).max(1);
        truncate_with_ellipsis(&name, max)
    }

    /// Yolo countdown label for background tabs: alternates emoji + countdown.
    /// Returns `None` when no yolo countdown is active.
    pub fn background_yolo_label(&self, tab_width: u16) -> Option<String> {
        let state = self.yolo_state.lock().ok()?.as_ref()?.clone();
        let label = if state.remaining_secs % 2 == 0 {
            format!("\u{26a0}\u{fe0f}  yolo in {}", state.remaining_secs)
        } else {
            format!("\u{1f918} yolo in {}", state.remaining_secs)
        };
        let max_chars = tab_width.saturating_sub(4) as usize;
        let truncated = if label.chars().count() > max_chars && max_chars > 1 {
            let t: String = label.chars().take(max_chars - 1).collect();
            format!("{}\u{2026}", t)
        } else {
            label
        };
        Some(truncated)
    }

    /// Subcommand label rendered inside the tab cell (NOT in the title).
    /// Empty while Idle. Prepended with `⚠️ ` while stuck. Truncated to fit
    /// `tab_width - 4` chars (2 borders + 2 padding spaces).
    /// For background tabs with an active yolo countdown, shows the countdown
    /// label instead.
    pub fn tab_subcommand_label(&self, tab_width: u16, is_active: bool) -> String {
        if !is_active {
            if let Some(label) = self.background_yolo_label(tab_width) {
                return label;
            }
        }
        let cmd = match &self.execution_phase {
            ExecutionPhase::Idle => return String::new(),
            ExecutionPhase::Running { command }
            | ExecutionPhase::Done { command, .. }
            | ExecutionPhase::Error { command, .. } => command.as_str(),
        };

        // When a workflow is active, append step info: "exec workflow: step (N/M)"
        let workflow_suffix = self.workflow_step_suffix();
        let display = if workflow_suffix.is_empty() {
            cmd.to_string()
        } else {
            format!("{}: {}", cmd, workflow_suffix)
        };

        let prefix = if self.stuck { "\u{26a0}\u{fe0f} " } else { "" };
        let prefix_chars = prefix.chars().count();
        let max_chars = (tab_width as usize).saturating_sub(4);
        let cmd_max = max_chars.saturating_sub(prefix_chars);
        let cmd_str = if display.chars().count() > cmd_max && cmd_max > 1 {
            let truncated: String = display.chars().take(cmd_max - 1).collect();
            format!("{}\u{2026}", truncated)
        } else {
            display
        };
        format!("{}{}", prefix, cmd_str)
    }

    /// Build a workflow step suffix like "implement (2/5)" for the tab label.
    /// Returns empty string when no workflow is active or has no steps.
    fn workflow_step_suffix(&self) -> String {
        let guard = match self.workflow_state.lock() {
            Ok(g) => g,
            Err(_) => return String::new(),
        };
        let view = match guard.as_ref() {
            Some(v) if !v.steps.is_empty() => v,
            _ => return String::new(),
        };
        let total = view.steps.len();
        let done_count = view.steps.iter().filter(|s| s.status == "done").count();
        let current_name = view.current_step.as_deref().unwrap_or_else(|| {
            view.steps
                .iter()
                .find(|s| s.status == "running")
                .map(|s| s.name.as_str())
                .unwrap_or("")
        });
        if current_name.is_empty() {
            // Workflow finished or not yet started
            let completed = done_count == total;
            if completed {
                return format!("done ({}/{})", total, total);
            }
            return String::new();
        }
        let step_index = view
            .steps
            .iter()
            .position(|s| s.name == current_name)
            .map(|i| i + 1)
            .unwrap_or(0);
        format!("{} ({}/{})", current_name, step_index, total)
    }

    /// Push the container's terminal contents to the status log when the
    /// overlay never got a frame on screen (the agent exited within one
    /// event-loop tick of producing its first output). Without this, a
    /// fast-failing launch's error output is parsed into the vt100 grid and
    /// then discarded before the user ever sees it.
    fn surface_unseen_container_output(&mut self) {
        if self.container_rendered {
            return;
        }
        let Some(slot) = self.focused_slot() else {
            return;
        };
        let contents = slot.vt100_parser.screen().contents();
        let lines: Vec<&str> = contents.lines().filter(|l| !l.trim().is_empty()).collect();
        if lines.is_empty() {
            return;
        }
        if let Ok(mut log) = self.status_log.lock() {
            log.push(crate::frontend::tui::user_message::StatusLogEntry {
                level: crate::data::message::MessageLevel::Warning,
                text: "Agent exited before its output could be displayed; captured output:"
                    .to_string(),
            });
            for line in lines {
                log.push(crate::frontend::tui::user_message::StatusLogEntry {
                    level: crate::data::message::MessageLevel::Info,
                    text: format!("  {line}"),
                });
            }
        }
    }

    /// Tear down the container overlay state. Called when a containerized
    /// command finishes (exit, error, or task drop). Captures
    /// `LastContainerSummary` from the focused slot's `ContainerInfo` so the
    /// post-exit summary bar can show averaged stats and the exit code. The
    /// slot itself is left in place — mid-workflow closes reuse its info for
    /// the next step's stats polling and title; `poll_command_completion`
    /// clears the slots when the whole command is over.
    fn close_container_overlay(&mut self, exit_code: i32) {
        self.surface_unseen_container_output();
        if self.container_window_state != ContainerWindowState::Hidden {
            if let Some(info) = self.focused_slot().and_then(|s| s.container_info.as_ref()) {
                let elapsed = info.start_time.elapsed().as_secs();
                let (avg_cpu, avg_memory) = if info.stats_history.is_empty() {
                    ("n/a".to_string(), "n/a".to_string())
                } else {
                    let count = info.stats_history.len() as f64;
                    let cpu_avg: f64 =
                        info.stats_history.iter().map(|(c, _)| c).sum::<f64>() / count;
                    let mem_avg: f64 =
                        info.stats_history.iter().map(|(_, m)| m).sum::<f64>() / count;
                    (format!("{:.1}%", cpu_avg), format!("{:.0}MiB", mem_avg))
                };
                self.last_container_summary = Some(LastContainerSummary {
                    agent_display_name: info.agent_display_name.clone(),
                    container_name: info.container_name.clone(),
                    avg_cpu,
                    avg_memory,
                    total_time: format_duration(elapsed),
                    exit_code,
                });
            }
        }
        self.container_window_state = ContainerWindowState::Hidden;
        self.container_inner_area = None;
        self.mouse_selection = None;
        self.container_scroll_offset = 0;
        for slot in &mut self.container_slots {
            slot.agent_alt_screen = false;
            slot.agent_alternate_scroll = false;
        }
        self.stuck = false;
        self.stuck_rx = None;
    }

    /// Close the container window as soon as the engine reports that a
    /// mid-workflow container has actually terminated (killed by awman —
    /// e.g. after a yolo countdown — or the user quit the agent process).
    ///
    /// The engine only publishes the exit code on real container death,
    /// never for a stuck-but-alive container or while a yolo countdown is
    /// still running, so taking the slot is the whole gate. The summary bar
    /// is left behind (captured by `close_container_overlay`), and the slot
    /// (with its `ContainerInfo`) is kept alive because later workflow steps
    /// reuse it for stats polling and the overlay title.
    pub fn poll_container_exit(&mut self) {
        let exit_code = self
            .container_exit_shared
            .lock()
            .ok()
            .and_then(|mut guard| guard.take());
        let Some(exit_code) = exit_code else {
            return;
        };
        // Pull any final bytes into the parser first so the captured
        // scrollback (and `surface_unseen_container_output`) is complete.
        self.drain_container_output();
        self.close_container_overlay(exit_code);
        // Late bytes still in flight from the dead container must not
        // re-open the window; cleared when the next container launches.
        self.suppress_container_auto_open = true;
    }

    /// Check if the command task has completed; update execution phase.
    ///
    /// Closes the container overlay on completion so the user regains full
    /// keyboard control without having to manually cycle Ctrl+M.
    pub fn poll_command_completion(&mut self) {
        if let Some(ref rx) = self.command_result_rx {
            match rx.try_recv() {
                Ok(Ok(outcome)) => {
                    let cmd_name = match &self.execution_phase {
                        ExecutionPhase::Running { command } => command.clone(),
                        _ => String::new(),
                    };
                    // Agent-session commands carry the agent's real exit code;
                    // reflect it instead of unconditionally reporting success.
                    let exit_code = match &outcome {
                        CommandOutcome::Chat(o) => o.exit_code.unwrap_or(0),
                        CommandOutcome::ExecPrompt(o) => o.exit_code.unwrap_or(0),
                        _ => 0,
                    };
                    if let Ok(mut log) = self.status_log.lock() {
                        if exit_code == 0 {
                            log.push(crate::frontend::tui::user_message::StatusLogEntry {
                                level: crate::data::message::MessageLevel::Success,
                                text: format!("Command '{}' completed successfully.", cmd_name),
                            });
                        } else {
                            log.push(crate::frontend::tui::user_message::StatusLogEntry {
                                level: crate::data::message::MessageLevel::Error,
                                text: format!(
                                    "Command '{}' finished: agent exited with code {}.",
                                    cmd_name, exit_code
                                ),
                            });
                        }
                    }
                    self.execution_phase = ExecutionPhase::Done {
                        command: cmd_name,
                        exit_code,
                    };
                    self.close_container_overlay(exit_code);
                    // The command is over — drop every slot (and any dormant
                    // backbone) so the tab returns to the no-container state.
                    self.clear_container_slots();
                }
                Ok(Err(err)) => {
                    let cmd_name = match &self.execution_phase {
                        ExecutionPhase::Running { command } => command.clone(),
                        _ => String::new(),
                    };
                    let err_msg = format!("{err}");
                    if let Ok(mut log) = self.status_log.lock() {
                        log.push(crate::frontend::tui::user_message::StatusLogEntry {
                            level: crate::data::message::MessageLevel::Error,
                            text: format!("Command '{}' failed: {}", cmd_name, err_msg),
                        });
                    }
                    self.execution_phase = ExecutionPhase::Error {
                        command: cmd_name,
                        message: err_msg,
                    };
                    self.close_container_overlay(-1);
                    self.clear_container_slots();
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    // Still running — nothing to do.
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    // Command task dropped without sending a result — in
                    // practice this means the task panicked. The panic hook
                    // records the backtrace; point the user at it.
                    let cmd_name = match &self.execution_phase {
                        ExecutionPhase::Running { command } => command.clone(),
                        _ => String::new(),
                    };
                    let err_msg = match crate::frontend::tui::panic_log_path() {
                        Some(path) => format!(
                            "command task ended unexpectedly (likely a panic — see {})",
                            path.display()
                        ),
                        None => "command task ended unexpectedly (likely a panic)".to_string(),
                    };
                    if let Ok(mut log) = self.status_log.lock() {
                        log.push(crate::frontend::tui::user_message::StatusLogEntry {
                            level: crate::data::message::MessageLevel::Error,
                            text: format!("Command '{}' failed: {}", cmd_name, err_msg),
                        });
                    }
                    self.execution_phase = ExecutionPhase::Error {
                        command: cmd_name,
                        message: err_msg,
                    };
                    self.close_container_overlay(-1);
                    self.clear_container_slots();
                }
            }
        }
    }

    /// Drop every container slot (active and dormant) and the command result
    /// channel. Called when the command task is over in any way — the tab
    /// returns to the no-container state.
    fn clear_container_slots(&mut self) {
        self.container_slots.clear();
        self.dormant_slots.clear();
        self.focused_slot_idx = 0;
        self.command_result_rx = None;
    }

    pub fn subcommand_label(&self) -> &str {
        match &self.execution_phase {
            ExecutionPhase::Idle => "",
            ExecutionPhase::Running { command } => command.as_str(),
            ExecutionPhase::Done { command, .. } => command.as_str(),
            ExecutionPhase::Error { command, .. } => command.as_str(),
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
    if let Ok(guard) = tab.yolo_state.lock() {
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
    if tab.is_remote {
        return Color::Magenta;
    }
    match &tab.execution_phase {
        ExecutionPhase::Error { .. } => Color::Red,
        ExecutionPhase::Running { .. } => {
            if tab.container_window_state != ContainerWindowState::Hidden {
                Color::Green
            } else {
                Color::Blue
            }
        }
        ExecutionPhase::Idle | ExecutionPhase::Done { .. } => Color::DarkGray,
    }
}

/// Execution window border color based on phase and focus.
pub fn window_border_color(phase: &ExecutionPhase, focused: bool) -> ratatui::style::Color {
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
                Color::Green
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::session::{Session, SessionOpenOptions, StaticGitRootResolver};

    fn make_test_session() -> Session {
        let tmp = tempfile::tempdir().unwrap();
        let resolver = StaticGitRootResolver::new(tmp.path());
        Session::open(
            tmp.path().to_path_buf(),
            &resolver,
            SessionOpenOptions::default(),
        )
        .unwrap()
    }

    fn make_tab() -> Tab {
        Tab::new(make_test_session())
    }

    /// Install a single container slot (as `spawn_command` would) and return
    /// a sender feeding its PTY output channel.
    fn attach_slot_stdout(tab: &mut Tab) -> tokio::sync::mpsc::UnboundedSender<Vec<u8>> {
        if tab.container_slots.is_empty() {
            tab.start_container("claude".into(), String::new(), 80, 24);
        }
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        tab.focused_slot_mut().unwrap().container_stdout_rx = Some(rx);
        tx
    }

    /// Tab whose working-dir basename is `name`, for project_name tests.
    /// Returns the TempDir so the directory outlives `Session::open`.
    fn make_named_tab(name: &str) -> (Tab, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let resolver = StaticGitRootResolver::new(&dir);
        let session = Session::open(dir.clone(), &resolver, SessionOpenOptions::default()).unwrap();
        (Tab::new(session), tmp)
    }

    // ── project_name width behavior ─────────────────────────────────────────

    #[test]
    fn project_name_at_minimum_tab_width_matches_historical_cap() {
        // 20 cols is the minimum tab width; 6 chars of title chrome leave 14
        // for the name — the old fixed truncation limit.
        let (tab, _tmp) = make_named_tab("a-very-long-project-directory-name");
        let out = tab.project_name(20);
        assert_eq!(out.chars().count(), 14);
        assert!(out.ends_with('\u{2026}'), "clipped name must mark: {out}");
    }

    #[test]
    fn project_name_uses_extra_space_in_wide_tabs() {
        let name = "a-very-long-project-directory-name";
        let (tab, _tmp) = make_named_tab(name);
        assert_eq!(
            tab.project_name(name.chars().count() as u16 + 6),
            name,
            "a tab wide enough for the full name must not truncate it"
        );
        let out = tab.project_name(30);
        assert_eq!(
            out.chars().count(),
            24,
            "a 30-col tab must show 24 chars of the name, not clip at 14: {out}"
        );
        assert!(out.ends_with('\u{2026}'));
    }

    // ── git sidebar state ──────────────────────────────────────────────────

    #[test]
    fn new_tab_git_sidebar_is_closed() {
        let tab = make_tab();
        assert_eq!(tab.git_sidebar_state, GitSidebarState::Closed);
    }

    #[test]
    fn new_tab_git_diff_summary_is_none() {
        let tab = make_tab();
        assert!(
            tab.git_diff_summary.lock().unwrap().is_none(),
            "a fresh tab has no diff summary until the poll task populates it"
        );
    }

    // ── mid-workflow container exit (poll_container_exit) ─────────────────

    #[test]
    fn container_exit_report_closes_window_and_leaves_summary() {
        let mut tab = make_tab();
        tab.start_container("claude".into(), "awman-abc".into(), 80, 24);
        tab.container_window_state = ContainerWindowState::Maximized;
        tab.container_rendered = true; // pretend a frame made it to screen
        *tab.container_exit_shared.lock().unwrap() = Some(137);

        tab.poll_container_exit();

        assert_eq!(tab.container_window_state, ContainerWindowState::Hidden);
        let summary = tab
            .last_container_summary
            .as_ref()
            .expect("closing on container exit must capture the summary bar");
        assert_eq!(summary.exit_code, 137);
        assert_eq!(summary.container_name, "awman-abc");
        assert!(
            tab.focused_slot()
                .and_then(|s| s.container_info.as_ref())
                .is_some(),
            "the slot's container_info must survive so later workflow steps keep stats polling"
        );
        assert!(
            tab.container_exit_shared.lock().unwrap().is_none(),
            "the exit slot is consumed"
        );
    }

    #[test]
    fn poll_container_exit_is_noop_without_a_reported_exit() {
        let mut tab = make_tab();
        tab.start_container("claude".into(), "awman-abc".into(), 80, 24);
        tab.container_window_state = ContainerWindowState::Maximized;

        tab.poll_container_exit();

        // No exit was reported (stuck container / yolo countdown running /
        // container alive) — the window must stay open.
        assert_eq!(tab.container_window_state, ContainerWindowState::Maximized);
        assert!(tab.last_container_summary.is_none());
    }

    #[test]
    fn late_bytes_after_container_exit_do_not_reopen_window() {
        let mut tab = make_tab();
        tab.start_container("claude".into(), "awman-abc".into(), 80, 24);
        tab.container_window_state = ContainerWindowState::Maximized;
        tab.container_rendered = true;
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        tab.focused_slot_mut().unwrap().container_stdout_rx = Some(rx);

        *tab.container_exit_shared.lock().unwrap() = Some(0);
        tab.poll_container_exit();
        assert_eq!(tab.container_window_state, ContainerWindowState::Hidden);

        // Bytes still in flight from the dead container arrive afterwards.
        tx.send(b"leftover output".to_vec()).unwrap();
        tab.drain_container_output();
        assert_eq!(
            tab.container_window_state,
            ContainerWindowState::Hidden,
            "a dead container's late bytes must not resurrect the window"
        );

        // The next step launches: the engine sets pty_reset_flag, after which
        // fresh output auto-opens the window again.
        tab.pty_reset_flag.store(true, Ordering::Relaxed);
        tx.send(b"next step output".to_vec()).unwrap();
        tab.drain_container_output();
        assert_eq!(tab.container_window_state, ContainerWindowState::Maximized);
    }

    // ── GroupStarted resets stuck summary bar and unblocks auto-open ──────

    #[test]
    fn group_started_evicts_summary_bar_and_unblocks_auto_open() {
        let mut tab = make_tab();
        tab.start_container("claude".into(), "awman-leader".into(), 80, 24);
        tab.container_window_state = ContainerWindowState::Maximized;
        tab.container_rendered = true;

        // Leader is killed — leaves a red summary bar and suppresses auto-open.
        *tab.container_exit_shared.lock().unwrap() = Some(137);
        tab.poll_container_exit();
        assert_eq!(tab.container_window_state, ContainerWindowState::Hidden);
        assert!(tab.last_container_summary.is_some());
        assert!(tab.suppress_container_auto_open);

        // Engine fires GroupStarted for the first parallel group.
        tab.apply_container_slot_event(ContainerSlotEvent::GroupStarted);
        assert!(
            tab.last_container_summary.is_none(),
            "GroupStarted must evict the stuck summary bar"
        );
        assert!(
            !tab.suppress_container_auto_open,
            "GroupStarted must unblock auto-open for new group containers"
        );

        // First container in the new group launches and produces output.
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        tab.apply_container_slot_event(ContainerSlotEvent::Launched {
            step_name: "build".to_string(),
            agent: "claude".to_string(),
            model: None,
            io: Some(crate::frontend::tui::tabs::ContainerSlotIo {
                stdout_rx: rx,
                stdin_tx: tokio::sync::mpsc::unbounded_channel().0,
                resize_tx: tokio::sync::mpsc::unbounded_channel().0,
            }),
        });
        tx.send(b"building...".to_vec()).unwrap();
        tab.drain_container_output();
        assert_eq!(
            tab.container_window_state,
            ContainerWindowState::Maximized,
            "first output from the new group must auto-open the window"
        );
    }

    #[test]
    fn container_window_cycles() {
        assert_eq!(
            ContainerWindowState::Hidden.cycle(),
            ContainerWindowState::Maximized
        );
        assert_eq!(
            ContainerWindowState::Minimized.cycle(),
            ContainerWindowState::Maximized
        );
        assert_eq!(
            ContainerWindowState::Maximized.cycle(),
            ContainerWindowState::Minimized
        );
    }

    /// Reproduces TUI-3: vt100 0.15.2's `Grid::visible_rows()` panicked in
    /// debug builds when `scrollback_offset > rows_len` (an unchecked
    /// `rows_len - scrollback_offset` subtraction). vt100-ctt 0.17 fixes
    /// the panic with `saturating_sub`, so we can scroll the full
    /// configured scrollback depth (5000 lines by default) without
    /// hitting an arithmetic overflow.
    #[test]
    fn deep_scroll_past_screen_rows_does_not_panic() {
        let mut tab = make_tab();
        tab.start_container("agent".into(), "container".into(), 80, 24);
        // Feed enough lines that the vt100 scrollback grows well past the
        // screen height. Each "line\n" becomes one row of scrollback.
        for i in 0..500 {
            let s = format!("line {i}\r\n");
            tab.focused_parser_mut().process(s.as_bytes());
        }
        // Probe depth.
        let depth = {
            let screen = tab.focused_parser_mut().screen_mut();
            screen.set_scrollback(usize::MAX);
            let d = screen.scrollback();
            screen.set_scrollback(0);
            d
        };
        assert!(
            depth > 24,
            "test setup: scrollback depth must exceed screen height; got {depth}"
        );
        // Set offset to a value much larger than screen_rows. Pre-fix
        // (vt100 0.15.2) this would panic in debug; vt100-ctt 0.17 must
        // handle it safely.
        let screen = tab.focused_parser_mut().screen_mut();
        screen.set_scrollback(depth);
        let eff = screen.scrollback();
        assert_eq!(
            eff, depth,
            "set_scrollback must clamp to depth, not screen_rows"
        );
        // Reading cells at this offset must not panic.
        let _ = screen.cell(0, 0);
        let _ = screen.cell(23, 79);
        screen.set_scrollback(0);
    }

    // ── truncate_with_ellipsis ─────────────────────────────────────────────────

    #[test]
    fn truncate_with_ellipsis_no_change_when_short() {
        assert_eq!(truncate_with_ellipsis("hello", 14), "hello");
    }

    #[test]
    fn truncate_with_ellipsis_at_limit() {
        // Exactly 14 chars: no ellipsis.
        assert_eq!(
            truncate_with_ellipsis("aaaaaaaaaaaaaa", 14),
            "aaaaaaaaaaaaaa"
        );
    }

    #[test]
    fn truncate_with_ellipsis_when_too_long() {
        let s = "aaaaaaaaaaaaaaaaaa"; // 18 chars
        let result = truncate_with_ellipsis(s, 14);
        assert!(result.ends_with('\u{2026}'));
        assert_eq!(result.chars().count(), 14);
    }

    // ── tab_subcommand_label ───────────────────────────────────────────────────

    #[test]
    fn tab_subcommand_label_idle_is_empty() {
        let tab = make_tab();
        assert_eq!(tab.tab_subcommand_label(20, true), "");
    }

    #[test]
    fn tab_subcommand_label_running_returns_command() {
        let mut tab = make_tab();
        tab.execution_phase = ExecutionPhase::Running {
            command: "chat".into(),
        };
        assert_eq!(tab.tab_subcommand_label(20, true), "chat");
    }

    #[test]
    fn tab_subcommand_label_truncates_to_fit_cell() {
        let mut tab = make_tab();
        tab.execution_phase = ExecutionPhase::Running {
            command: "very-long-subcommand-name".into(),
        };
        // tab_width=10 → max_chars=6; truncated to 5 chars + …
        let label = tab.tab_subcommand_label(10, true);
        assert!(label.ends_with('\u{2026}'));
        assert!(label.chars().count() <= 6);
    }

    // ── compute_tab_bar_width ──────────────────────────────────────────────────

    #[test]
    fn tab_bar_width_single_tab_uses_min_when_content_small() {
        // 1 tab, content 5 → natural = max(7, 20) = 20, fits in 200.
        assert_eq!(compute_tab_bar_width(1, 200, 5), 20);
    }

    #[test]
    fn tab_bar_width_single_tab_uses_natural_when_fits() {
        // 1 tab, content 80 → natural = 82, fits in 100.
        assert_eq!(compute_tab_bar_width(1, 100, 80), 82);
    }

    #[test]
    fn tab_bar_width_two_tabs_shrinks_when_overflow() {
        // 2 tabs, content 90 → natural = 92, total = 184 > 100. Shrink: 100/2 = 50.
        assert_eq!(compute_tab_bar_width(2, 100, 90), 50);
    }

    #[test]
    fn tab_bar_width_three_tabs_shrinks_when_overflow() {
        // 3 tabs, content 90 → natural = 92, total = 276 > 100. Shrink: 100/3 = 33.
        assert_eq!(compute_tab_bar_width(3, 100, 90), 33);
    }

    #[test]
    fn tab_bar_width_four_tabs_uses_min_when_content_small() {
        // 4 tabs, content 10 → natural = max(12, 20) = 20, total = 80 ≤ 100.
        assert_eq!(compute_tab_bar_width(4, 100, 10), 20);
    }

    #[test]
    fn tab_bar_width_zero_tabs() {
        assert_eq!(compute_tab_bar_width(0, 100, 5), 0);
    }

    // ── phase_label ───────────────────────────────────────────────────────────

    #[test]
    fn phase_label_idle() {
        assert_eq!(phase_label(&ExecutionPhase::Idle), " awman ");
    }

    #[test]
    fn phase_label_running() {
        let label = phase_label(&ExecutionPhase::Running {
            command: "chat".into(),
        });
        assert!(label.contains("running"));
        assert!(label.contains("chat"));
    }

    #[test]
    fn phase_label_done_exit_zero_shows_checkmark() {
        let label = phase_label(&ExecutionPhase::Done {
            command: "chat".into(),
            exit_code: 0,
        });
        assert!(label.contains('✓'), "exit-0 done must use checkmark");
        assert!(label.contains("done"));
        assert!(label.contains("chat"));
    }

    #[test]
    fn phase_label_done_nonzero_exit_shows_cross_and_code() {
        let label = phase_label(&ExecutionPhase::Done {
            command: "chat".into(),
            exit_code: 1,
        });
        assert!(label.contains('✗'), "non-zero exit must use cross");
        assert!(label.contains("exit 1"));
        assert!(label.contains("chat"));
    }

    #[test]
    fn phase_label_error_shows_cross_and_command() {
        let label = phase_label(&ExecutionPhase::Error {
            command: "ready".into(),
            message: "something broke".into(),
        });
        assert!(label.contains('✗'));
        assert!(label.contains("error"));
        assert!(label.contains("ready"));
    }

    // ── window_border_color matrix ────────────────────────────────────────────

    #[test]
    fn window_border_color_error_always_red() {
        use ratatui::style::Color;
        let phase = ExecutionPhase::Error {
            command: "x".into(),
            message: "y".into(),
        };
        assert_eq!(window_border_color(&phase, true), Color::Red);
        assert_eq!(window_border_color(&phase, false), Color::Red);
    }

    #[test]
    fn window_border_color_running_focused_is_blue() {
        use ratatui::style::Color;
        let phase = ExecutionPhase::Running {
            command: "x".into(),
        };
        assert_eq!(window_border_color(&phase, true), Color::Blue);
    }

    #[test]
    fn window_border_color_running_unfocused_is_gray() {
        use ratatui::style::Color;
        let phase = ExecutionPhase::Running {
            command: "x".into(),
        };
        assert_eq!(window_border_color(&phase, false), Color::Gray);
    }

    #[test]
    fn window_border_color_done_focused_is_green() {
        use ratatui::style::Color;
        let phase = ExecutionPhase::Done {
            command: "x".into(),
            exit_code: 0,
        };
        assert_eq!(window_border_color(&phase, true), Color::Green);
    }

    #[test]
    fn window_border_color_done_unfocused_is_gray() {
        use ratatui::style::Color;
        let phase = ExecutionPhase::Done {
            command: "x".into(),
            exit_code: 0,
        };
        assert_eq!(window_border_color(&phase, false), Color::Gray);
    }

    #[test]
    fn window_border_color_idle_is_dark_gray_regardless_of_focus() {
        use ratatui::style::Color;
        assert_eq!(
            window_border_color(&ExecutionPhase::Idle, true),
            Color::DarkGray
        );
        assert_eq!(
            window_border_color(&ExecutionPhase::Idle, false),
            Color::DarkGray
        );
    }

    // ── tab_color ─────────────────────────────────────────────────────────────

    #[test]
    fn tab_color_stuck_is_yellow() {
        use ratatui::style::Color;
        let mut tab = make_tab();
        tab.stuck = true;
        assert_eq!(tab_color(&tab), Color::Yellow);
    }

    #[test]
    fn tab_color_remote_is_magenta() {
        use ratatui::style::Color;
        let mut tab = make_tab();
        tab.is_remote = true;
        assert_eq!(tab_color(&tab), Color::Magenta);
    }

    #[test]
    fn tab_color_stuck_takes_priority_over_remote() {
        use ratatui::style::Color;
        let mut tab = make_tab();
        tab.stuck = true;
        tab.is_remote = true;
        assert_eq!(tab_color(&tab), Color::Yellow);
    }

    #[test]
    fn tab_color_error_is_red() {
        use ratatui::style::Color;
        let mut tab = make_tab();
        tab.execution_phase = ExecutionPhase::Error {
            command: "chat".into(),
            message: "oops".into(),
        };
        assert_eq!(tab_color(&tab), Color::Red);
    }

    #[test]
    fn tab_color_running_with_pty_container_visible_is_green() {
        use ratatui::style::Color;
        let mut tab = make_tab();
        tab.execution_phase = ExecutionPhase::Running {
            command: "chat".into(),
        };
        tab.container_window_state = ContainerWindowState::Minimized;
        assert_eq!(tab_color(&tab), Color::Green);
    }

    #[test]
    fn tab_color_running_maximized_container_is_green() {
        use ratatui::style::Color;
        let mut tab = make_tab();
        tab.execution_phase = ExecutionPhase::Running {
            command: "chat".into(),
        };
        tab.container_window_state = ContainerWindowState::Maximized;
        assert_eq!(tab_color(&tab), Color::Green);
    }

    #[test]
    fn tab_color_running_no_container_is_blue() {
        use ratatui::style::Color;
        let mut tab = make_tab();
        tab.execution_phase = ExecutionPhase::Running {
            command: "chat".into(),
        };
        tab.container_window_state = ContainerWindowState::Hidden;
        assert_eq!(tab_color(&tab), Color::Blue);
    }

    #[test]
    fn tab_color_idle_is_dark_gray() {
        use ratatui::style::Color;
        let tab = make_tab();
        assert_eq!(tab_color(&tab), Color::DarkGray);
    }

    #[test]
    fn tab_color_done_is_dark_gray() {
        use ratatui::style::Color;
        let mut tab = make_tab();
        tab.execution_phase = ExecutionPhase::Done {
            command: "chat".into(),
            exit_code: 0,
        };
        assert_eq!(tab_color(&tab), Color::DarkGray);
    }

    // ── strip_alternate_screen_sequences ─────────────────────────────

    #[test]
    fn strip_alt_screen_removes_1049h() {
        let input = b"hello\x1b[?1049hworld";
        let out = strip_alternate_screen_sequences(input);
        assert_eq!(out.bytes, b"helloworld");
        assert_eq!(out.alt_screen, Some(true));
    }

    #[test]
    fn strip_alt_screen_removes_1049l() {
        let input = b"\x1b[?1049lafter";
        let out = strip_alternate_screen_sequences(input);
        assert_eq!(out.bytes, b"after");
        assert_eq!(out.alt_screen, Some(false));
    }

    #[test]
    fn strip_alt_screen_removes_47h_and_47l() {
        let input = b"a\x1b[?47hb\x1b[?47lc";
        let out = strip_alternate_screen_sequences(input);
        assert_eq!(out.bytes, b"abc");
        // Last toggle in the chunk wins.
        assert_eq!(out.alt_screen, Some(false));
    }

    #[test]
    fn strip_alt_screen_removes_1047h() {
        let input = b"\x1b[?1047hx";
        let out = strip_alternate_screen_sequences(input);
        assert_eq!(out.bytes, b"x");
        assert_eq!(out.alt_screen, Some(true));
    }

    #[test]
    fn strip_alt_screen_preserves_other_escapes() {
        let input = b"\x1b[31mred\x1b[0m";
        let out = strip_alternate_screen_sequences(input);
        assert_eq!(out.bytes, input.to_vec());
        assert_eq!(out.alt_screen, None);
        assert_eq!(out.alternate_scroll, None);
    }

    #[test]
    fn strip_alt_screen_passthrough_no_sequences() {
        let input = b"plain text without escapes";
        let out = strip_alternate_screen_sequences(input);
        assert_eq!(out.bytes, input.to_vec());
        assert_eq!(out.alt_screen, None);
        assert_eq!(out.alternate_scroll, None);
    }

    #[test]
    fn strip_alt_screen_empty_input() {
        let out = strip_alternate_screen_sequences(b"");
        assert!(out.bytes.is_empty());
        assert_eq!(out.alt_screen, None);
        assert_eq!(out.alternate_scroll, None);
    }

    #[test]
    fn strip_alt_screen_consecutive_sequences() {
        let input = b"\x1b[?1049h\x1b[?1049l";
        let out = strip_alternate_screen_sequences(input);
        assert!(out.bytes.is_empty());
        assert_eq!(out.alt_screen, Some(false));
    }

    #[test]
    fn strip_observes_alternate_scroll_enable_without_stripping() {
        // codex's alt-screen entry: CSI ?1049h then CSI ?1007h. The 1049
        // must be stripped, the 1007 observed but left in the stream.
        let input = b"\x1b[?1049h\x1b[?1007h";
        let out = strip_alternate_screen_sequences(input);
        assert_eq!(out.bytes, b"\x1b[?1007h");
        assert_eq!(out.alt_screen, Some(true));
        assert_eq!(out.alternate_scroll, Some(true));
    }

    #[test]
    fn strip_observes_alternate_scroll_disable() {
        // codex's alt-screen exit: CSI ?1007l then CSI ?1049l.
        let input = b"\x1b[?1007l\x1b[?1049l";
        let out = strip_alternate_screen_sequences(input);
        assert_eq!(out.bytes, b"\x1b[?1007l");
        assert_eq!(out.alt_screen, Some(false));
        assert_eq!(out.alternate_scroll, Some(false));
    }

    #[test]
    fn drain_container_output_tracks_alt_screen_and_alternate_scroll() {
        let mut tab = make_tab();
        let tx = attach_slot_stdout(&mut tab);

        tx.send(b"\x1b[?1049h\x1b[?1007h".to_vec()).unwrap();
        tab.drain_container_output();
        let slot = tab.focused_slot().unwrap();
        assert!(slot.agent_alt_screen, "1049h must set agent_alt_screen");
        assert!(
            slot.agent_alternate_scroll,
            "1007h must set agent_alternate_scroll"
        );

        tx.send(b"\x1b[?1007l\x1b[?1049l".to_vec()).unwrap();
        tab.drain_container_output();
        let slot = tab.focused_slot().unwrap();
        assert!(!slot.agent_alt_screen, "1049l must clear agent_alt_screen");
        assert!(
            !slot.agent_alternate_scroll,
            "1007l must clear agent_alternate_scroll"
        );
    }

    #[test]
    fn codex_inline_history_insertion_lands_in_scrollback() {
        // Reproduces codex's inline-viewport history insertion
        // (codex-rs/tui/src/insert_history.rs): a scroll region anchored at
        // the top of the screen ending above the inline viewport, the cursor
        // parked on the region's bottom row, and one "\r\n" + line per
        // history entry. Each newline scrolls the region; the rows pushed
        // off the top of the screen must accumulate in vt100 scrollback so
        // mouse-wheel scrollback has something to show. Relies on the
        // RegionScrollEmulator in the drain pipeline (vt100 alone discards
        // these rows).
        let mut tab = make_tab(); // 24x80 parser
        let tx = attach_slot_stdout(&mut tab);
        // Steady-state: the overlay is already open and the parser sized
        // (start_container). Skips drain's auto-open branch.
        tab.container_window_state = ContainerWindowState::Maximized;

        // Viewport occupies the bottom 6 rows (0-based top = row 18), so the
        // scroll region is 1-based rows 1..18.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"\x1b[1;18r"); // DECSTBM, top-anchored
        bytes.extend_from_slice(b"\x1b[18;1H"); // cursor to region bottom
        for i in 0..30 {
            bytes.extend_from_slice(format!("\r\nhistory line {i}").as_bytes());
        }
        bytes.extend_from_slice(b"\x1b[r"); // reset region
        tx.send(bytes).unwrap();
        tab.drain_container_output();

        let screen = tab.focused_parser_mut().screen_mut();
        screen.set_scrollback(usize::MAX);
        let depth = screen.scrollback();
        assert!(
            depth >= 12,
            "30 lines through an 18-row region must overflow into scrollback \
             (got depth {depth})"
        );
        let scrolled_back = screen.contents();
        screen.set_scrollback(0);
        assert!(
            scrolled_back.contains("history line 0"),
            "earliest history line must be reachable in scrollback"
        );
    }

    // ── Agent exit-code reporting and fast-exit output capture ──────────

    fn finish_with_chat_outcome(tab: &mut Tab, exit_code: Option<i32>) {
        let (result_tx, result_rx) =
            std::sync::mpsc::channel::<Result<CommandOutcome, CommandError>>();
        tab.command_result_rx = Some(result_rx);
        tab.execution_phase = ExecutionPhase::Running {
            command: "chat".into(),
        };
        result_tx
            .send(Ok(CommandOutcome::Chat(
                crate::command::commands::chat::ChatOutcome {
                    agent: Some("claude".into()),
                    exit_code,
                },
            )))
            .unwrap();
        tab.poll_command_completion();
    }

    fn log_texts(tab: &Tab) -> Vec<(crate::data::message::MessageLevel, String)> {
        tab.status_log
            .lock()
            .unwrap()
            .iter()
            .map(|e| (e.level, e.text.clone()))
            .collect()
    }

    #[test]
    fn poll_completion_reports_nonzero_agent_exit_code() {
        let mut tab = make_tab();
        finish_with_chat_outcome(&mut tab, Some(2));

        assert!(
            matches!(
                tab.execution_phase,
                ExecutionPhase::Done { exit_code: 2, .. }
            ),
            "Done phase must carry the agent's exit code: {:?}",
            tab.execution_phase
        );
        let logs = log_texts(&tab);
        assert!(
            logs.iter().any(|(level, text)| {
                *level == crate::data::message::MessageLevel::Error
                    && text.contains("agent exited with code 2")
            }),
            "non-zero agent exit must be reported as an Error, got: {logs:?}"
        );
        assert!(
            !logs
                .iter()
                .any(|(_, text)| text.contains("completed successfully")),
            "non-zero agent exit must not be reported as success: {logs:?}"
        );
    }

    #[test]
    fn poll_completion_zero_exit_reports_success() {
        let mut tab = make_tab();
        finish_with_chat_outcome(&mut tab, Some(0));

        let logs = log_texts(&tab);
        assert!(
            logs.iter().any(|(level, text)| {
                *level == crate::data::message::MessageLevel::Success
                    && text.contains("completed successfully")
            }),
            "clean agent exit keeps the success message: {logs:?}"
        );
    }

    #[test]
    fn unrendered_container_output_is_surfaced_to_status_log() {
        let mut tab = make_tab();
        let tx = attach_slot_stdout(&mut tab);

        // The agent prints an error and dies before the renderer draws a
        // single frame — drain opens the overlay, poll closes it in the same
        // tick, container_rendered stays false.
        tx.send(b"ERROR: unknown flag: --workspace-dir\r\n".to_vec())
            .unwrap();
        tab.drain_container_output();
        assert!(!tab.container_rendered);
        finish_with_chat_outcome(&mut tab, Some(1));

        let logs = log_texts(&tab);
        assert!(
            logs.iter()
                .any(|(_, text)| text.contains("before its output could be displayed")),
            "must announce the captured-output replay: {logs:?}"
        );
        assert!(
            logs.iter()
                .any(|(_, text)| text.contains("ERROR: unknown flag: --workspace-dir")),
            "the agent's dying words must land in the status log: {logs:?}"
        );
    }

    #[test]
    fn rendered_container_output_is_not_duplicated_into_status_log() {
        let mut tab = make_tab();
        let tx = attach_slot_stdout(&mut tab);

        tx.send(b"normal session output\r\n".to_vec()).unwrap();
        tab.drain_container_output();
        // The renderer drew the overlay at least once.
        tab.container_rendered = true;
        finish_with_chat_outcome(&mut tab, Some(0));

        let logs = log_texts(&tab);
        assert!(
            !logs
                .iter()
                .any(|(_, text)| text.contains("normal session output")),
            "output the user already saw must not be replayed: {logs:?}"
        );
    }

    // ── WI-0096 parallel-slot behavior ───────────────────────────────────────

    fn slot(name: &str) -> ContainerSlot {
        ContainerSlot::new(name.to_string(), "claude".to_string(), 1000)
    }

    #[test]
    fn container_slots_aggregate_stuck_and_yolo_flags() {
        let mut tab = make_tab();
        tab.container_slots.push(slot("a"));
        tab.container_slots.push(slot("b"));
        // Slot-flag aggregation only runs while a parallel group is active,
        // marked by the stashed sequential backbone.
        tab.dormant_slots.push(slot(""));

        // All slots clear → aggregate is false.
        tab.drain_stuck_events();
        assert!(!tab.stuck);
        assert!(!tab.yolo_mode);

        // Any slot stuck → aggregate stuck is true.
        tab.container_slots[1].stuck = true;
        tab.drain_stuck_events();
        assert!(tab.stuck, "any stuck slot makes the tab aggregate stuck");
        assert!(!tab.yolo_mode);

        // Clear stuck, set yolo on the other slot → aggregate yolo is true.
        tab.container_slots[1].stuck = false;
        tab.container_slots[0].yolo_mode = true;
        tab.drain_stuck_events();
        assert!(!tab.stuck);
        assert!(tab.yolo_mode, "any yolo slot makes the tab aggregate yolo");

        // Everything clear again → both false.
        tab.container_slots[0].yolo_mode = false;
        tab.drain_stuck_events();
        assert!(!tab.stuck);
        assert!(!tab.yolo_mode);
    }

    #[test]
    fn evicting_focused_slot_advances_focus_to_next_live_slot() {
        let mut tab = make_tab();
        tab.container_slots.push(slot("a"));
        tab.container_slots.push(slot("b"));
        tab.container_slots.push(slot("c"));
        tab.focused_slot_idx = 1; // focus "b"

        tab.container_slot_events
            .lock()
            .unwrap()
            .push_back(ContainerSlotEvent::Exited {
                step_name: "b".to_string(),
            });
        tab.drain_container_slot_events();

        assert_eq!(tab.active_slot_count(), 2);
        assert!(
            !tab.container_slots.iter().any(|s| s.step_name == "b"),
            "the exited slot must be gone"
        );
        assert_eq!(tab.focused_slot_idx, 1);
        assert_eq!(
            tab.focused_slot().unwrap().step_name,
            "c",
            "focus advances to the slot that shifted into the freed index"
        );
    }

    #[test]
    fn evicting_slot_before_focused_shifts_index_down() {
        let mut tab = make_tab();
        tab.container_slots.push(slot("a"));
        tab.container_slots.push(slot("b"));
        tab.container_slots.push(slot("c"));
        tab.focused_slot_idx = 2; // focus "c"

        tab.container_slot_events
            .lock()
            .unwrap()
            .push_back(ContainerSlotEvent::Exited {
                step_name: "a".to_string(),
            });
        tab.drain_container_slot_events();

        assert_eq!(tab.active_slot_count(), 2);
        assert_eq!(tab.focused_slot_idx, 1, "index shifts down by one");
        assert_eq!(
            tab.focused_slot().unwrap().step_name,
            "c",
            "the same slot stays focused after the shift"
        );
    }

    #[test]
    fn evicting_last_slot_hides_the_container_window() {
        let mut tab = make_tab();
        tab.container_slots.push(slot("a"));
        tab.container_window_state = ContainerWindowState::Maximized;

        tab.container_slot_events
            .lock()
            .unwrap()
            .push_back(ContainerSlotEvent::Exited {
                step_name: "a".to_string(),
            });
        tab.drain_container_slot_events();

        assert_eq!(tab.active_slot_count(), 0);
        assert_eq!(tab.focused_slot_idx, 0);
        assert_eq!(tab.container_window_state, ContainerWindowState::Hidden);
    }

    #[test]
    fn cycle_focused_slot_advances_cyclically_through_three_slots() {
        let mut tab = make_tab();
        tab.container_slots.push(slot("a"));
        tab.container_slots.push(slot("b"));
        tab.container_slots.push(slot("c"));

        assert_eq!(tab.focused_slot_idx, 0);
        tab.cycle_focused_slot();
        assert_eq!(tab.focused_slot_idx, 1);
        tab.cycle_focused_slot();
        assert_eq!(tab.focused_slot_idx, 2);
        tab.cycle_focused_slot();
        assert_eq!(
            tab.focused_slot_idx, 0,
            "one full cycle of three slots returns to slot 0"
        );
    }

    #[test]
    fn cycle_focused_slot_is_noop_with_a_single_slot() {
        let mut tab = make_tab();
        tab.container_slots.push(slot("a"));
        tab.cycle_focused_slot();
        assert_eq!(tab.focused_slot_idx, 0);
    }

    #[test]
    fn cycle_focused_slot_resets_scrollback_to_live_view() {
        let mut tab = make_tab();
        tab.container_slots.push(slot("a"));
        tab.container_slots.push(slot("b"));
        tab.container_scroll_offset = 42;
        tab.cycle_focused_slot();
        assert_eq!(
            tab.container_scroll_offset, 0,
            "the rotated-in slot must start at its live view"
        );
    }

    #[test]
    fn launched_slot_parser_is_sized_to_the_overlay_not_80x24() {
        let mut tab = make_tab();
        // The renderer published the overlay's inner rect on a prior frame.
        tab.container_inner_area = Some(ratatui::layout::Rect::new(1, 1, 150, 40));

        let (resize_tx, mut resize_rx) = tokio::sync::mpsc::unbounded_channel::<(u16, u16)>();
        let (_stdout_tx, stdout_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let (stdin_tx, _stdin_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        tab.container_slot_events
            .lock()
            .unwrap()
            .push_back(ContainerSlotEvent::Launched {
                step_name: "build".to_string(),
                agent: "claude".to_string(),
                model: None,
                io: Some(ContainerSlotIo {
                    stdout_rx,
                    stdin_tx,
                    resize_tx,
                }),
            });
        tab.drain_container_slot_events();

        let (rows, cols) = tab.container_slots[0].vt100_parser.screen().size();
        assert_eq!(
            (cols, rows),
            (150, 40),
            "the fresh slot parser must match the overlay, not the 80x24 default"
        );
        assert_eq!(
            resize_rx.try_recv().ok(),
            Some((150, 40)),
            "the container PTY must receive the real size at launch"
        );
    }

    #[test]
    fn container_name_event_updates_the_matching_slot() {
        let mut tab = make_tab();
        tab.container_slots.push(slot("build"));
        tab.container_slots.push(slot("test"));

        tab.container_slot_events
            .lock()
            .unwrap()
            .push_back(ContainerSlotEvent::ContainerName {
                step_name: "test".to_string(),
                container_name: "awman-test-4242".to_string(),
            });
        tab.drain_container_slot_events();

        assert_eq!(
            tab.container_slots[1]
                .container_info
                .as_ref()
                .unwrap()
                .container_name,
            "awman-test-4242"
        );
        assert!(
            tab.container_slots[0]
                .container_info
                .as_ref()
                .unwrap()
                .container_name
                .is_empty(),
            "the other slot's name must be untouched"
        );
    }

    #[test]
    fn parallel_output_tracks_per_slot_alt_screen_flags() {
        let mut tab = make_tab();
        let mut s = slot("build");
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        s.container_stdout_rx = Some(rx);
        tab.container_slots.push(s);
        tab.container_slots.push(slot("test"));

        // Agent enables the alternate screen and alternate scroll (codex-style).
        tx.send(b"\x1b[?1049h\x1b[?1007h".to_vec()).unwrap();
        tab.drain_container_output();

        assert!(tab.container_slots[0].agent_alt_screen);
        assert!(tab.container_slots[0].agent_alternate_scroll);
        assert!(
            !tab.container_slots[1].agent_alt_screen,
            "flags are per-slot, not shared"
        );
    }

    #[test]
    fn first_parallel_output_auto_opens_the_container_window() {
        let mut tab = make_tab();
        let mut s = slot("build");
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        s.container_stdout_rx = Some(rx);
        tab.container_slots.push(s);
        assert_eq!(tab.container_window_state, ContainerWindowState::Hidden);

        tx.send(b"hello from the agent".to_vec()).unwrap();
        tab.drain_container_output();

        assert_eq!(
            tab.container_window_state,
            ContainerWindowState::Maximized,
            "a workflow starting directly with a parallel group must open the overlay"
        );
    }

    #[test]
    fn container_overlay_active_requires_a_slot_and_maximized() {
        let mut tab = make_tab();
        // No slots: never active, regardless of window state.
        tab.container_window_state = ContainerWindowState::Maximized;
        assert!(!tab.container_overlay_active());

        // With a slot: only Maximized shows the overlay; Minimized renders
        // every slot as a status bar instead.
        tab.container_slots.push(slot("a"));
        assert!(tab.container_overlay_active());
        tab.container_window_state = ContainerWindowState::Minimized;
        assert!(!tab.container_overlay_active());
        tab.container_window_state = ContainerWindowState::Hidden;
        assert!(!tab.container_overlay_active());
    }

    #[test]
    fn group_started_stashes_backbone_and_group_finished_restores_it() {
        let mut tab = make_tab();
        // The command-level backbone slot (as spawn_command installs it).
        tab.start_container("claude".into(), "awman-backbone".into(), 80, 24);

        // Parallel group starts: the backbone goes dormant, group slots join.
        {
            let mut q = tab.container_slot_events.lock().unwrap();
            q.push_back(ContainerSlotEvent::GroupStarted);
            q.push_back(ContainerSlotEvent::Launched {
                step_name: "a".into(),
                agent: "claude".into(),
                model: None,
                io: None,
            });
            q.push_back(ContainerSlotEvent::Launched {
                step_name: "b".into(),
                agent: "codex".into(),
                model: None,
                io: None,
            });
        }
        tab.drain_container_slot_events();
        assert_eq!(tab.active_slot_count(), 2, "only the group slots display");
        assert_eq!(tab.dormant_slots.len(), 1, "the backbone is stashed");

        // Group drains and finishes: the backbone is restored.
        {
            let mut q = tab.container_slot_events.lock().unwrap();
            q.push_back(ContainerSlotEvent::Exited {
                step_name: "a".into(),
            });
            q.push_back(ContainerSlotEvent::Exited {
                step_name: "b".into(),
            });
            q.push_back(ContainerSlotEvent::GroupFinished);
        }
        tab.drain_container_slot_events();
        assert_eq!(tab.active_slot_count(), 1);
        assert!(tab.dormant_slots.is_empty());
        assert_eq!(
            tab.focused_slot()
                .and_then(|s| s.container_info.as_ref())
                .map(|i| i.container_name.as_str()),
            Some("awman-backbone"),
            "the restored slot is the original backbone"
        );
    }

    #[test]
    fn yolo_started_shares_cancel_flag_and_tick_updates_slot_state() {
        let mut tab = make_tab();
        tab.container_slots.push(slot("a"));
        tab.container_slots.push(slot("b"));

        let cancel_flag: SharedYoloCancelFlag = Arc::new(AtomicBool::new(false));
        {
            let mut q = tab.container_slot_events.lock().unwrap();
            q.push_back(ContainerSlotEvent::YoloStarted {
                step_name: "b".into(),
                cancel_flag: cancel_flag.clone(),
            });
            q.push_back(ContainerSlotEvent::YoloTick {
                step_name: "b".into(),
                remaining_secs: 42,
            });
        }
        tab.drain_container_slot_events();

        let b = tab
            .container_slots
            .iter()
            .find(|s| s.step_name == "b")
            .unwrap();
        assert!(b.yolo_mode, "yolo_mode set on the ticking slot only");
        assert_eq!(
            b.yolo_state
                .lock()
                .unwrap()
                .as_ref()
                .map(|s| s.remaining_secs),
            Some(42)
        );
        // The slot's cancel flag is the SAME Arc the engine-side frontend
        // holds, so setting it here (as Esc does) is visible to the engine's
        // next `parallel_step_yolo_countdown_tick` check.
        assert!(Arc::ptr_eq(&b.yolo_cancel_flag, &cancel_flag));

        let a = tab
            .container_slots
            .iter()
            .find(|s| s.step_name == "a")
            .unwrap();
        assert!(!a.yolo_mode, "sibling slot is untouched");
        assert!(a.yolo_state.lock().unwrap().is_none());

        // Finishing clears both the flag and the displayed countdown.
        {
            let mut q = tab.container_slot_events.lock().unwrap();
            q.push_back(ContainerSlotEvent::YoloFinished {
                step_name: "b".into(),
            });
        }
        tab.drain_container_slot_events();
        let b = tab
            .container_slots
            .iter()
            .find(|s| s.step_name == "b")
            .unwrap();
        assert!(!b.yolo_mode);
        assert!(b.yolo_state.lock().unwrap().is_none());
    }
}
