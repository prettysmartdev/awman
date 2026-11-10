//! Per-tab state.
//!
//! WI 0142 split (`aspec/work-items/0142` §5): this module keeps the Layer 3
//! view of a tab — rendering, scroll, mouse, vt100 parsers and slot layout.
//! The command-owned feeds it drains (`TabSharedState`, `ContainerSlotEvent`,
//! `ContainerLifecycle`, …) live in Layer 2 (`command::instance::feeds`) and the
//! pure phase/view types in Layer 0 (`data::instance_snapshot`); both are
//! re-exported here under their established names.

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use ratatui::layout::Rect;

use crate::command::dispatch::CommandOutcome;
use crate::command::error::CommandError;
use crate::command::git_summary::GitSummaryPoller;
use crate::data::program_status::EffectiveStatus;
use crate::data::session::{Session, SessionId};
use crate::engine::agent_runtime::execution::{AgentStats, StuckEvent};
use crate::frontend::tui::dialogs::{DialogRequest, DialogResponse};
use crate::frontend::tui::git_sidebar::GitSidebarState;

pub use crate::command::instance::feeds::{
    AcpSlotState, AgentSignals, AgentsDashboardData, ContainerLifecycle, ContainerSlotEvent,
    ContainerSlotIo, SharedAcpState, SharedAgentsDashboard, SharedContainerSlotEvents,
    SharedGitDiffSummary, SharedTuiContext, SharedWorkflowPresentation, SharedYoloCancelFlag,
    SharedYoloState, TabCommandFeeds as TabSharedState, WorkflowPresentation, ACP_HISTORY_LIMIT,
};
pub use crate::data::instance_snapshot::{
    ContainerWindowState, ExecutionPhase, LastContainerSummary, StepViewStatus, WorkflowStepKind,
    WorkflowStepView, WorkflowViewState, YoloState,
};

mod container_slots;
mod git_poll;
mod labels;
mod overlay_lifecycle;
pub mod squad_state;
#[cfg(test)]
pub(crate) mod tests;

use squad_state::SquadTabState;

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
    /// Set when this container runs a workflow setup/teardown step instead of
    /// an agent: the step's title, shown in place of the agent name.
    pub phase_step_title: Option<String>,
}

impl ContainerInfo {
    /// Record one stats sample, unless it names a different container than
    /// this slot's (a sample that raced a relaunch).
    pub fn record_stats(&mut self, stats: AgentStats) {
        if !self.container_name.is_empty() && self.container_name != stats.name {
            return;
        }
        self.stats_history
            .push((stats.cpu_percent, stats.memory_mb));
        self.latest_stats = Some(stats);
    }

    /// What the container window, its minimized bar and the post-exit
    /// summary call this container: the setup/teardown step it runs, or
    /// else the agent's display name.
    pub fn display_name(&self) -> &str {
        self.phase_step_title
            .as_deref()
            .unwrap_or(&self.agent_display_name)
    }
}

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
    /// Identity of this launch, independent of its reusable workflow step name.
    pub launch_id: uuid::Uuid,
    /// Whether this slot is a stdio (PTY/vt100) window or an ACP window.
    /// Stdio is the default; ACP slots carry their shared render state here.
    pub kind: AgentWindowKind,
    /// Lines-from-bottom scroll offset into an ACP window's rendered
    /// history. `0` follows the latest updates; increasing it scrolls toward
    /// older output. View state of this presenter (the shared `AcpFeed` holds
    /// only the producer's history), and never touches the vt100 scrollback
    /// path stdio slots use. Unused for stdio slots.
    pub acp_scroll_offset: usize,
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
    /// The agent's own reported status (OSC 7501), or `None` when it does not
    /// report or has cleared its records. Presentation only: the engine decides
    /// what a report means.
    pub program_status: Option<EffectiveStatus>,
    /// When this slot last received output, for the "working · silent Nm" note.
    pub last_output_at: Instant,
    /// Why this slot's yolo countdown runs (parallel groups).
    pub countdown_kind: crate::engine::workflow::actions::CountdownKind,
    pub yolo_mode: bool,
    pub yolo_state: SharedYoloState,
    pub yolo_cancel_flag: SharedYoloCancelFlag,
    pub stuck_rx: Option<tokio::sync::broadcast::Receiver<StuckEvent>>,
}

impl ContainerSlot {
    /// Install one launch's channels as a unit; input and resize must target
    /// the same execution that supplies this slot's output.
    pub fn bind_io(&mut self, io: ContainerSlotIo) {
        self.container_stdout_rx = Some(io.stdout_rx);
        self.container_stdin_tx = Some(io.stdin_tx);
        self.container_resize_tx = Some(io.resize_tx);
    }

    /// Only lifecycle transitions assign runtime identity. Stats are observations
    /// and never supply a container name for input or later polling.
    pub fn set_runtime_name(&mut self, container_name: String) {
        if let Some(info) = self.container_info.as_mut() {
            info.container_name = container_name;
            info.latest_stats = None;
        }
    }

    /// Create a fresh slot for a newly-launched container. The PTY parser
    /// starts at 80x24 and is sized to the real overlay dimensions as soon
    /// as they are known; live I/O channels are attached by the caller.
    pub fn new(step_name: String, agent_display_name: String, scrollback: usize) -> Self {
        Self {
            launch_id: uuid::Uuid::new_v4(),
            kind: AgentWindowKind::Stdio,
            acp_scroll_offset: 0,
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
                phase_step_title: None,
            }),
            container_stdout_rx: None,
            container_stdin_tx: None,
            container_resize_tx: None,
            agent_alt_screen: false,
            agent_alternate_scroll: false,
            stuck: false,
            program_status: None,
            last_output_at: Instant::now(),
            countdown_kind: Default::default(),
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
            .map(|i| i.display_name())
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

/// Tab state — one per open tab.
pub struct Tab {
    /// Identity of the manager-owned session backing this tab. The session
    /// snapshot below remains the pre-F-22 view data; WI 0114 moves that view
    /// state out of Tab without changing ownership again.
    pub session_id: SessionId,
    pub session: Session,
    pub(crate) git_summary: GitSummaryPoller,
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
    /// The work-queue lease this tab runs (WI 0143), set when a picked item
    /// opens it. Closing the tab mid-run asks what to do with the lease.
    pub lease: Option<crate::frontend::tui::queue::TabLease>,
    /// Replaces the directory name in the tab bar (picked items show
    /// "title · server").
    pub label_override: Option<String>,
    /// Squad sub-view state (selection, polled tasks, daemon reachability).
    /// `Some` exactly when `is_squad`.
    pub squad: Option<SquadTabState>,
    pub output_lines: Vec<String>,
    pub stuck: bool,
    pub yolo_mode: bool,
    /// Broadcast receiver for stuck/unstuck events from the container engine.
    /// Drained non-blockingly in `tick_all_tabs` for tab coloring.
    pub engine_tx:
        Option<tokio::sync::mpsc::UnboundedSender<crate::engine::workflow::EngineRequest>>,

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
    /// The agent-window PTY size last reported to the instance for this tab
    /// (`App::report_pty_sizes`), so a size is reported once per change.
    pub reported_pty_size: Option<(u16, u16)>,
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
        self.shared.command_cancel.cancel();
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
    pub(crate) fn active_workflow_context_path(&self) -> Option<std::path::PathBuf> {
        if !matches!(self.execution_phase, ExecutionPhase::Running { .. }) {
            return None;
        }
        self.shared.workflow.snapshot().context_path
    }

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
        Self::new_with_git_summary(session, GitSummaryPoller::default())
    }

    pub fn new_with_git_summary(session: Session, git_summary: GitSummaryPoller) -> Self {
        let git_root = session.git_root().to_path_buf();
        let mut tab = Self::new_inner(session, git_summary);
        // Start polling against the session git root. Once a worktree is
        // created, `refresh_git_poll` (called each tick) restarts the task
        // pointed at the worktree path.
        tab.start_git_poll(git_root);
        tab
    }

    /// The view of a directory tab the instance opened. The tab presents the
    /// feeds the instance runs its commands with; the instance's startup run
    /// is returned for `App::bind_startup` to bind.
    pub fn from_opened(
        opened: crate::command::instance::OpenedTab,
    ) -> (Self, crate::command::instance::StartupRun) {
        let crate::command::instance::OpenedTab {
            session,
            git_summary,
            feeds,
            startup,
        } = opened;
        let mut tab = Self::new_with_git_summary(session, git_summary);
        tab.shared = feeds;
        (tab, startup)
    }

    /// Construct the singleton squad tab. Unlike [`Tab::new`] this starts **no**
    /// git poll: the synthetic session is rooted at the squad storage root,
    /// which has no meaningful diff. The caller must not auto-spawn a startup
    /// command into this tab.
    pub fn new_squad(session: Session) -> Self {
        let mut tab = Self::new_inner(session, GitSummaryPoller::default());
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
    fn new_inner(session: Session, git_summary: GitSummaryPoller) -> Self {
        Self {
            session_id: session.id(),
            session,
            git_summary,
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
            lease: None,
            label_override: None,
            squad: None,
            output_lines: Vec::new(),
            stuck: false,
            yolo_mode: false,
            engine_tx: None,
            container_slots: Vec::new(),
            focused_slot_idx: 0,
            dormant_slots: Vec::new(),
            suppress_container_auto_open: false,
            command_result_rx: None,
            dialog_request_rx: None,
            dialog_response_tx: None,
            git_sidebar_state: GitSidebarState::Closed,
            reported_pty_size: None,
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
    // background tabs flash visibly.
    if let Ok(guard) = tab.shared.yolo_state.lock() {
        if let Some(ref state) = *guard {
            return if state.remaining_secs % 2 == 0 {
                Color::Yellow
            } else {
                Color::Magenta
            };
        }
    }
    // A reporting agent is coloured by what it reported, in the TUI's usual
    // meanings: working is the running blue, done and idle green, blocked the
    // "needs you" yellow, error red. Reporting agents never get the
    // silence-based stuck colouring; a silent agent keeps the colours below.
    {
        use crate::data::program_status::ProgramState;
        match tab.program_status().map(|s| s.state) {
            Some(ProgramState::Working) => return Color::Blue,
            Some(ProgramState::Done | ProgramState::Idle) => return Color::Green,
            Some(ProgramState::Blocked) => return Color::Yellow,
            Some(ProgramState::Error) => return Color::Red,
            None if tab.stuck => return Color::Yellow,
            None => {}
        }
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
