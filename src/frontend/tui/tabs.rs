//! Per-tab state.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use ratatui::layout::Rect;

use crate::command::dispatch::CommandOutcome;
use crate::command::error::CommandError;
use crate::data::session::Session;
use crate::engine::container::instance::ContainerStats;
use crate::frontend::tui::dialogs::{DialogRequest, DialogResponse};
use crate::frontend::tui::user_message::SharedStatusLog;

/// How long a tab can produce no PTY output before being marked "stuck".
/// The tab color flips to yellow and a warning glyph is added to the tab
/// label, so the user knows to check on it.
pub const STUCK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

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
/// reports `ContainerStatus::Running { container_name }`. The TUI event
/// loop reads this to populate `ContainerInfo.container_name` for stats
/// polling.
pub type SharedContainerName = Arc<Mutex<Option<String>>>;

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
/// to send Ctrl-W, StepStuck, and StepUnstuck requests.
pub type SharedEngineTx =
    Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<crate::engine::workflow::EngineRequest>>>>;

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
/// Coordinates are stored in vt100 cell space (0-based against the parser
/// grid), not raw terminal coords. The renderer publishes
/// `Tab::container_inner_area` so `handle_mouse_event` can subtract the
/// overlay's screen offset before recording these.
#[derive(Debug, Clone)]
pub struct TextSelection {
    pub start_col: u16,
    pub start_row: u16,
    pub end_col: u16,
    pub end_row: u16,
    /// Snapshot of the vt100 grid at selection-start time. Each cell is the
    /// printable contents of that position (or `" "` for empties), so the
    /// copied text reflects what the user *saw* when they started the drag,
    /// not the grid's current values (which mutate with live PTY output).
    pub snapshot: Vec<Vec<String>>,
}

/// Live container metadata, populated while a containerized command runs.
#[derive(Debug, Clone)]
pub struct ContainerInfo {
    pub agent_display_name: String,
    pub container_name: String,
    pub start_time: Instant,
    pub latest_stats: Option<ContainerStats>,
    /// History of `(cpu_percent, memory_mb)` samples for averaging in the
    /// post-exit summary bar.
    pub stats_history: Vec<(f64, f64)>,
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

/// Tab state — one per open tab.
pub struct Tab {
    pub session: Session,
    pub execution_phase: ExecutionPhase,
    pub vt100_parser: vt100::Parser,
    pub container_window_state: ContainerWindowState,
    /// How many lines from the bottom to skip in the vt100 scrollback when
    /// the container is Maximized. 0 = follow live output.
    pub container_scroll_offset: usize,
    /// Live container metadata, populated while a containerized command runs.
    pub container_info: Option<ContainerInfo>,
    /// Summary of the last container session, shown in a dashed-border bar
    /// below the exec window after the container exits.
    pub last_container_summary: Option<LastContainerSummary>,
    /// Inner content rect of the container overlay, refreshed each frame by
    /// the renderer. Used by the mouse handler to translate raw terminal
    /// coords into vt100 cell coords.
    pub container_inner_area: Option<Rect>,
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
    pub last_output_time: Option<Instant>,
    /// Last time the user touched this tab (key press, mouse). Used together
    /// with `last_output_time` to suppress stuck detection while the user is
    /// actively engaged.
    pub last_user_activity_time: Option<Instant>,

    // ── Async command plumbing ───────────────────────────────────────────
    /// Event loop drains container stdout/stderr into the vt100 parser.
    pub container_stdout_rx: Option<tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>>,
    /// Event loop forwards keystrokes to the container stdin.
    pub container_stdin_tx: Option<tokio::sync::mpsc::UnboundedSender<Vec<u8>>>,
    /// Event loop forwards terminal resizes to the container's PTY master.
    pub container_resize_tx: Option<tokio::sync::mpsc::UnboundedSender<(u16, u16)>>,
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
    /// Shared stdin sender slot for workflow step transitions.
    pub stdin_tx_shared: SharedStdinTx,
    /// Shared resize sender slot for workflow step transitions.
    pub resize_tx_shared: SharedResizeTx,
    /// Shared control board sender for mid-step WCB requests.
    pub engine_tx_shared: SharedEngineTx,
    /// Shared active worktree path: set by the worktree-lifecycle frontend
    /// after a worktree is created/resumed, cleared after the workflow
    /// finalize step. Drives the "Using worktree: <path>" bottom-bar line.
    pub active_worktree_path: SharedActiveWorktreePath,
    /// Live TUI context for the status command. The event loop refreshes this
    /// on every tick; `TuiCommandFrontend` reads from it on each watch
    /// iteration so the status table always reflects current tab state.
    pub tui_context_shared: SharedTuiContext,
}

impl Tab {
    pub fn new(session: Session) -> Self {
        let scrollback = session.effective_config().scrollback_lines();
        Self {
            session,
            execution_phase: ExecutionPhase::Idle,
            vt100_parser: vt100::Parser::new(24, 80, scrollback),
            container_window_state: ContainerWindowState::Hidden,
            container_scroll_offset: 0,
            container_info: None,
            last_container_summary: None,
            container_inner_area: None,
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
            last_output_time: None,
            last_user_activity_time: None,
            container_stdout_rx: None,
            container_stdin_tx: None,
            container_resize_tx: None,
            command_result_rx: None,
            dialog_request_rx: None,
            dialog_response_tx: None,
            pty_reset_flag: Arc::new(AtomicBool::new(false)),
            container_name_shared: Arc::new(Mutex::new(None)),
            stdin_tx_shared: Arc::new(Mutex::new(None)),
            resize_tx_shared: Arc::new(Mutex::new(None)),
            engine_tx_shared: Arc::new(Mutex::new(None)),
            active_worktree_path: Arc::new(Mutex::new(None)),
            tui_context_shared: Arc::new(Mutex::new(
                crate::command::commands::status::StatusCommandTuiContext::default(),
            )),
        }
    }

    /// Recompute the `stuck` flag based on `last_output_time` vs. now.
    ///
    /// A tab is considered stuck when:
    /// - it is currently Running
    /// - a container is open (Maximized or Minimized)
    /// - no PTY bytes have arrived for `STUCK_TIMEOUT`
    /// - if this is the active tab, the user hasn't touched it for at least
    ///   `STUCK_TIMEOUT` either (so we don't flag a tab the user is plainly
    ///   working in).
    pub fn recompute_stuck(&mut self, is_active: bool) {
        let was_stuck = self.stuck;
        self.stuck = self.is_stuck(is_active, STUCK_TIMEOUT);
        if !was_stuck && self.stuck {
            // Cosmetic: nothing else; the tab color picks this up.
        }
    }

    fn is_stuck(&self, is_active: bool, timeout: std::time::Duration) -> bool {
        if !matches!(self.execution_phase, ExecutionPhase::Running { .. }) {
            return false;
        }
        if self.container_window_state == ContainerWindowState::Hidden {
            return false;
        }
        let output_stale = self
            .last_output_time
            .map(|t| t.elapsed() >= timeout)
            .unwrap_or(false);
        if !output_stale {
            return false;
        }
        if is_active {
            // Active tab: don't flag while the user is actively typing.
            if let Some(activity) = self.last_user_activity_time {
                if activity.elapsed() < timeout {
                    return false;
                }
            }
        }
        true
    }

    /// Stamp `last_user_activity_time` to suppress stuck detection while the
    /// user is engaged. Called on key/mouse events.
    pub fn record_user_activity(&mut self) {
        self.last_user_activity_time = Some(Instant::now());
    }

    /// Activate the container overlay for a fresh PTY container session.
    ///
    /// Resizes the vt100 parser to the container's inner area, resets
    /// scrollback, and records `ContainerInfo` so the title bar can show the
    /// agent name and live stats.
    pub fn start_container(
        &mut self,
        agent_display_name: String,
        container_name: String,
        cols: u16,
        rows: u16,
    ) {
        self.container_window_state = ContainerWindowState::Maximized;
        self.container_scroll_offset = 0;
        self.vt100_parser = vt100::Parser::new(
            rows,
            cols,
            self.session.effective_config().scrollback_lines(),
        );
        self.last_container_summary = None;
        self.mouse_selection = None;
        self.last_output_time = Some(Instant::now());
        self.container_info = Some(ContainerInfo {
            agent_display_name,
            container_name,
            start_time: Instant::now(),
            latest_stats: None,
            stats_history: Vec::new(),
        });
    }

    /// Project name for display in the tab bar. Truncated to 14 chars + `…`
    /// when the cwd's basename is longer.
    pub fn project_name(&self) -> String {
        let name = self
            .session
            .working_dir()
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("?")
            .to_string();
        truncate_with_ellipsis(&name, 14)
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

        let prefix = if self.stuck && self.is_stuck(is_active, STUCK_TIMEOUT) {
            "\u{26a0}\u{fe0f} "
        } else {
            ""
        };
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

    /// Drain pending container output into the vt100 parser.
    ///
    /// Auto-opens the container overlay to Maximized the first time bytes
    /// arrive so the user sees the PTY output immediately without having to
    /// manually cycle with Ctrl+M. Also ensures the parser is sized to match
    /// the current terminal dimensions (prevents the PTY rendering at 80x24
    /// until the first resize event).
    ///
    /// Between workflow steps the engine sets `pty_reset_flag`, which causes
    /// this method to reinitialize the vt100 parser (clearing the old step's
    /// terminal content) before processing the new step's output.
    pub fn drain_container_output(&mut self) {
        if let Some(ref mut rx) = self.container_stdout_rx {
            // Check if the engine signalled a PTY reset (workflow step transition).
            if self.pty_reset_flag.swap(false, Ordering::Relaxed) {
                let (rows, cols) = self.vt100_parser.screen().size();
                self.vt100_parser = vt100::Parser::new(
                    rows,
                    cols,
                    self.session.effective_config().scrollback_lines(),
                );
                self.container_scroll_offset = 0;
                self.mouse_selection = None;
            }

            let mut received_any = false;
            while let Ok(bytes) = rx.try_recv() {
                self.vt100_parser.process(&bytes);
                self.last_output_time = Some(Instant::now());
                received_any = true;
            }
            if received_any && self.container_window_state == ContainerWindowState::Hidden {
                if let Ok((cols, rows)) = crossterm::terminal::size() {
                    let (inner_cols, inner_rows) =
                        crate::frontend::tui::compute_container_inner_size(cols, rows);
                    self.vt100_parser
                        .screen_mut()
                        .set_size(inner_rows, inner_cols);
                    if let Some(ref tx) = self.container_resize_tx {
                        let _ = tx.send((inner_cols, inner_rows));
                    }
                }
                self.container_window_state = ContainerWindowState::Maximized;
            }
        }
    }

    /// Tear down the container overlay state. Called when a containerized
    /// command finishes (exit, error, or task drop). Captures
    /// `LastContainerSummary` from `container_info` (if any) so the post-exit
    /// summary bar can show averaged stats and the exit code.
    fn close_container_overlay(&mut self, exit_code: i32) {
        if self.container_window_state != ContainerWindowState::Hidden {
            if let Some(info) = self.container_info.take() {
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
                    agent_display_name: info.agent_display_name,
                    container_name: info.container_name,
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
        self.last_output_time = None;
        self.stuck = false;
    }

    /// Check if the command task has completed; update execution phase.
    ///
    /// Closes the container overlay on completion so the user regains full
    /// keyboard control without having to manually cycle Ctrl+M.
    pub fn poll_command_completion(&mut self) {
        if let Some(ref rx) = self.command_result_rx {
            match rx.try_recv() {
                Ok(Ok(_outcome)) => {
                    let cmd_name = match &self.execution_phase {
                        ExecutionPhase::Running { command } => command.clone(),
                        _ => String::new(),
                    };
                    if let Ok(mut log) = self.status_log.lock() {
                        log.push(crate::frontend::tui::user_message::StatusLogEntry {
                            level: crate::engine::message::MessageLevel::Success,
                            text: format!("Command '{}' completed successfully.", cmd_name),
                        });
                    }
                    self.execution_phase = ExecutionPhase::Done {
                        command: cmd_name,
                        exit_code: 0,
                    };
                    self.close_container_overlay(0);
                    self.command_result_rx = None;
                    self.container_stdout_rx = None;
                    self.container_stdin_tx = None;
                    self.container_resize_tx = None;
                }
                Ok(Err(err)) => {
                    let cmd_name = match &self.execution_phase {
                        ExecutionPhase::Running { command } => command.clone(),
                        _ => String::new(),
                    };
                    let err_msg = format!("{err}");
                    if let Ok(mut log) = self.status_log.lock() {
                        log.push(crate::frontend::tui::user_message::StatusLogEntry {
                            level: crate::engine::message::MessageLevel::Error,
                            text: format!("Command '{}' failed: {}", cmd_name, err_msg),
                        });
                    }
                    self.execution_phase = ExecutionPhase::Error {
                        command: cmd_name,
                        message: err_msg,
                    };
                    self.close_container_overlay(-1);
                    self.command_result_rx = None;
                    self.container_stdout_rx = None;
                    self.container_stdin_tx = None;
                    self.container_resize_tx = None;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    // Still running — nothing to do.
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    // Command task dropped without sending a result.
                    let cmd_name = match &self.execution_phase {
                        ExecutionPhase::Running { command } => command.clone(),
                        _ => String::new(),
                    };
                    let err_msg = "command task dropped unexpectedly".to_string();
                    if let Ok(mut log) = self.status_log.lock() {
                        log.push(crate::frontend::tui::user_message::StatusLogEntry {
                            level: crate::engine::message::MessageLevel::Error,
                            text: format!("Command '{}' failed: {}", cmd_name, err_msg),
                        });
                    }
                    self.execution_phase = ExecutionPhase::Error {
                        command: cmd_name,
                        message: err_msg,
                    };
                    self.close_container_overlay(-1);
                    self.command_result_rx = None;
                    self.container_stdout_rx = None;
                    self.container_stdin_tx = None;
                    self.container_resize_tx = None;
                }
            }
        }
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
/// Glyphs and text mirror old amux exactly:
/// - Idle → `" amux "`
/// - Running → `" ● running: {cmd} "`  (U+25CF)
/// - Done (exit 0) → `" ✓ done: {cmd} "`  (U+2713)
/// - Done (non-zero exit) → `" ✗ error: {cmd} (exit N) "`  (U+2717)
/// - Error → `" ✗ error: {cmd} "`
pub fn phase_label(phase: &ExecutionPhase) -> String {
    match phase {
        ExecutionPhase::Idle => " amux ".to_string(),
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
            tab.vt100_parser.process(s.as_bytes());
        }
        // Probe depth.
        let depth = {
            let screen = tab.vt100_parser.screen_mut();
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
        let screen = tab.vt100_parser.screen_mut();
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
        assert_eq!(phase_label(&ExecutionPhase::Idle), " amux ");
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
}
