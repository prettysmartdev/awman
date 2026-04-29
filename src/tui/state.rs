use crate::commands::claws::ClawsAuditCtx;
use crate::commands::status::TuiTabInfo;
use crate::config::{GlobalConfig, RepoConfig};
use crate::runtime::{ContainerStats, HostSettings, parse_cpu_percent, parse_memory_mb};
use crate::tui::pty::PtySession;
use crate::workflow::{StepStatus, WorkflowState};
use ratatui::layout::Rect;
use ratatui::style::Color;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::sync::mpsc::Receiver;
use tracing;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

/// Duration of container output inactivity before a tab is considered "stuck".
pub const STUCK_TIMEOUT: Duration = Duration::from_secs(10);

/// After the user dismisses the auto-advance dialog with Esc, wait this long
/// before showing it again for the same stuck episode.
pub const STUCK_DIALOG_BACKOFF: Duration = Duration::from_secs(60);

/// In yolo mode, the countdown duration before automatically advancing a stuck workflow step.
pub const YOLO_COUNTDOWN_DURATION: Duration = Duration::from_secs(60);

/// Permanent binding of a TUI tab to a remote headless session.
#[derive(Debug, Clone, PartialEq)]
pub struct RemoteTabBinding {
    /// Full URL of the remote headless host (e.g. "http://1.2.3.4:9876").
    pub remote_addr: String,
    /// Session ID on the remote host.
    pub session_id: String,
    /// Resolved API key (if any) for authenticating with the remote host.
    pub api_key: Option<String>,
    /// Hostname portion extracted from `remote_addr` for display in the tab bar.
    pub display_host: String,
}

impl RemoteTabBinding {
    /// Create a new binding, extracting `display_host` from the URL.
    pub fn new(remote_addr: String, session_id: String, api_key: Option<String>) -> Self {
        let display_host = extract_display_host(&remote_addr);
        Self {
            remote_addr,
            session_id,
            api_key,
            display_host,
        }
    }
}

/// Extract host:port from a URL for display purposes.
pub fn extract_display_host(url: &str) -> String {
    // Strip scheme prefix.
    let without_scheme = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
        .unwrap_or(url);
    // Strip trailing path/slash.
    let host_port = without_scheme.split('/').next().unwrap_or(without_scheme);
    host_port.to_string()
}

/// Which widget currently receives keyboard input.
#[derive(Debug, Clone, PartialEq)]
pub enum Focus {
    CommandBox,
    ExecutionWindow,
}

/// Lifecycle of the currently running (or last run) command.
#[derive(Debug, Clone, PartialEq)]
pub enum ExecutionPhase {
    /// No command has run yet (or previous output has been cleared).
    Idle,
    /// A command is running; output is live.
    Running { command: String },
    /// Command completed successfully; window is read-only.
    Done { command: String },
    /// Command exited with a non-zero status.
    Error { command: String, exit_code: i32 },
}

/// State for the config show/edit modal dialog.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigDialogState {
    /// Index into `ALL_FIELDS` of the currently selected row.
    pub selected_row: usize,
    /// Which scope column is selected: 0 = Global, 1 = Repo.
    pub selected_col: usize,
    /// Whether the selected cell is in edit mode.
    pub edit_mode: bool,
    /// Text being edited in the current cell.
    pub edit_value: String,
    /// Byte cursor position within `edit_value`.
    pub edit_cursor: usize,
    /// Git root, if the dialog was opened inside a git repo.
    pub git_root: Option<PathBuf>,
    /// Snapshot of the global config (refreshed after each save).
    pub global_config: GlobalConfig,
    /// Snapshot of the repo config (refreshed after each save).
    pub repo_config: RepoConfig,
    /// Error from the last save attempt (cleared on next edit).
    pub error_msg: Option<String>,
}

/// An overlay modal dialog, if any.
#[derive(Debug, Clone, PartialEq)]
pub enum Dialog {
    None,
    QuitConfirm,
    /// Prompts user for new tab's working directory, optionally showing remote sessions.
    NewTabDirectory {
        input: String,
        /// None = not yet fetched or no remote configured; Some(Ok(sessions)) = fetched;
        /// Some(Err(msg)) = fetch failed.
        remote_sessions: Option<Result<Vec<crate::commands::remote::RemoteSessionEntry>, String>>,
        /// Index of the currently selected item in the remote sessions list (or "create new").
        remote_selected_idx: Option<usize>,
        /// Whether focus is in the workdir field (true) or the remote sessions list (false).
        focus_workdir: bool,
    },
    /// Create a new remote session: enter directory, optionally pick from saved dirs.
    NewRemoteSession {
        /// Remote address we're creating the session on.
        remote_addr: String,
        /// API key for the remote host.
        api_key: Option<String>,
        /// Text input for the working directory.
        dir_input: String,
        /// Saved directories from config.
        saved_dirs: Vec<String>,
        /// Currently selected saved dir index (None = text input focused).
        saved_selected_idx: Option<usize>,
        /// Whether focus is on the text input (true) or saved dirs list (false).
        focus_input: bool,
        /// Error from the last session-creation attempt (shown inline; cleared on next attempt).
        creation_error: Option<String>,
    },
    /// Close current tab (when idle + multiple tabs).
    CloseTabConfirm,
    /// Ask whether to mount the Git root or just CWD.
    MountScope { git_root: PathBuf, cwd: PathBuf },
    /// Ask whether to mount agent credentials (and save the decision).
    /// Retained for completeness but currently unused (auto-passthrough).
    #[allow(dead_code)]
    AgentAuth { agent: String, git_root: PathBuf },
    /// Step 1 of `new`: select work item kind (Feature/Bug/Task/Enhancement).
    NewKindSelect { interview: bool },
    /// Step 2 of `new`: enter title. The kind has already been chosen.
    NewTitleInput {
        kind: crate::commands::new::WorkItemKind,
        /// Current title text being typed.
        title: String,
        interview: bool,
    },
    /// Interview summary input: large freeform text box.
    NewInterviewSummary {
        kind: crate::commands::new::WorkItemKind,
        title: String,
        work_item_number: u32,
        /// The text being typed.
        summary: String,
        /// Byte offset of the cursor in `summary`.
        cursor_pos: usize,
    },
    /// Claws wizard: ask if user has already forked nanoclaw.
    ClawsReadyHasForked,
    /// Claws wizard: enter GitHub username (if already forked).
    ClawsReadyUsernameInput { username: String },
    /// Claws wizard: confirm Docker socket access warning.
    ClawsReadyDockerSocketWarning,
    /// Claws wizard: confirm launching the audit agent (shown when Dockerfile.dev matches template).
    ClawsAuditConfirm,
    /// Claws subsequent run: offer to restart a found stopped container.
    /// Shows container details and a y/n prompt.
    ClawsReadyOfferRestartStopped {
        container_id: String,
        name: String,
        created: String,
    },
    /// Claws subsequent run: offer to start the stopped container.
    ClawsReadyOfferStart,
    /// Claws subsequent run: container restart failed — offer to delete and start fresh.
    ClawsRestartFailedOfferFresh { container_id: String },
    /// Claws wizard: clone failed with permission denied; collect sudo password for retry.
    ClawsReadySudoConfirm {
        /// The sudo password being entered (displayed as '*').
        password: String,
    },
    /// Workflow step completed: ask user to advance to the next step or abort.
    WorkflowStepConfirm {
        /// Name of the step that just completed.
        completed_step: String,
        /// Names of the next ready step(s).
        next_steps: Vec<String>,
    },
    /// Workflow step failed: ask user to retry or abort.
    WorkflowStepError {
        /// Name of the step that failed.
        failed_step: String,
        /// Error message.
        error: String,
    },
    /// Workflow control board: opened with Ctrl+W during a running workflow.
    WorkflowControlBoard {
        /// Name of the currently running step.
        current_step: String,
        /// Optional error message (e.g. "No previous step to return to").
        error: Option<String>,
    },
    /// Yolo mode: countdown dialog shown when a workflow step is stuck.
    /// When the countdown expires the step is automatically advanced.
    /// Timing is read from `TabState.yolo_countdown_started_at` (the single
    /// authoritative source) rather than stored here.
    WorkflowYoloCountdown {
        /// Name of the currently running step.
        current_step: String,
    },
    /// Confirm cancellation of the running workflow execution (y/n).
    /// On confirmation: kills the container, reverts the step to Pending, returns tab to idle.
    WorkflowCancelConfirm,
    /// After `implement --worktree` completes: ask whether to merge, discard, or keep the branch.
    WorktreeMergePrompt {
        branch: String,
        worktree_path: PathBuf,
        git_root: PathBuf,
        had_error: bool,
    },
    /// Worktree has uncommitted files — prompt user to enter a commit message before merging.
    WorktreeCommitPrompt {
        branch: String,
        worktree_path: PathBuf,
        git_root: PathBuf,
        /// Lines from `git status --porcelain` to show the user.
        uncommitted_files: Vec<String>,
        /// Commit message being typed.
        message: String,
        /// Byte offset of the cursor in `message`.
        cursor_pos: usize,
    },
    /// Confirm squash-merge of the worktree branch into the current HEAD.
    WorktreeMergeConfirm {
        branch: String,
        worktree_path: PathBuf,
        git_root: PathBuf,
    },
    /// Confirm deletion of the worktree directory and branch after a successful merge.
    WorktreeDeleteConfirm {
        branch: String,
        worktree_path: PathBuf,
        git_root: PathBuf,
    },
    /// Before creating a worktree: main branch has uncommitted files.
    /// Options: (c)ommit, (u)se last commit, (a)bort.
    WorktreePreCommitWarning {
        uncommitted_files: Vec<String>,
    },
    /// Before creating a worktree: enter a commit message to commit main branch changes.
    WorktreePreCommitMessage {
        uncommitted_files: Vec<String>,
        message: String,
        cursor_pos: usize,
    },
    /// Full-screen config view/edit dialog (triggered by `config show` in the TUI command input).
    ConfigShow(ConfigDialogState),
    /// Ready: legacy single-file Dockerfile.dev layout detected — ask whether to migrate
    /// to the modular layout (separate project + agent Dockerfiles).
    ReadyLegacyMigration {
        agent_name: String,
    },
    /// Ready: Dockerfile.dev is identical to the default project template — ask whether to
    /// launch the audit container to customise it for this project's toolchain.
    ReadyTemplateAuditConfirm,
    /// Init: ask whether to run the agent audit container after creating project files.
    InitAuditConfirm {
        agent: crate::cli::Agent,
        aspec: bool,
        replace_aspec: bool,
    },
    /// Init: `--aspec` was passed and the aspec folder already exists —
    /// ask whether to replace it with fresh templates.
    InitReplaceAspec {
        agent: crate::cli::Agent,
    },
    /// Init: ask whether to configure a work items directory.
    InitWorkItemsConfirm {
        agent: crate::cli::Agent,
        aspec: bool,
        replace_aspec: bool,
        run_audit: bool,
    },
    /// Init: collect the work items directory path (text input).
    InitWorkItemsDirInput {
        agent: crate::cli::Agent,
        aspec: bool,
        replace_aspec: bool,
        run_audit: bool,
        input: String,
    },
    /// Init: collect the work item template path (optional text input).
    InitWorkItemsTemplateInput {
        agent: crate::cli::Agent,
        aspec: bool,
        replace_aspec: bool,
        run_audit: bool,
        dir: String,
        input: String,
    },
    /// Agent setup: agent Dockerfile is missing or image not built — ask whether to set up.
    AgentSetupConfirm {
        /// The agent name that needs setup.
        agent: String,
        /// The configured default agent name, used to offer a fallback when the user declines.
        default_agent: String,
        /// `true` when triggered by a workflow step (`implement --workflow`);
        /// `false` when triggered by `chat` or a non-workflow `implement`.
        from_workflow: bool,
        /// `true` when the Dockerfile exists but the image is not built;
        /// `false` when the Dockerfile itself is missing.
        image_only: bool,
    },
    /// Remote run: show a picker to choose a session from the remote host.
    RemoteSessionPicker {
        /// Sessions fetched from the remote host.
        sessions: Vec<crate::commands::remote::RemoteSessionEntry>,
        /// Currently highlighted index.
        selected: usize,
        /// The resolved remote address.
        remote_addr: String,
        /// The passthrough command to run in the selected session.
        command: Vec<String>,
        /// Whether to stream logs after submission.
        follow: bool,
    },
    /// Remote session start: show a picker of saved directories.
    RemoteSavedDirPicker {
        /// Saved directories from config.
        dirs: Vec<String>,
        /// Currently highlighted index.
        selected: usize,
        /// The resolved remote address.
        remote_addr: String,
    },
    /// Remote session start: offer to save the newly used directory to config.
    RemoteSaveDirConfirm {
        /// The directory that was used.
        dir: String,
        /// The remote address (needed to complete the flow).
        remote_addr: String,
    },
    /// Remote session kill: show a picker to choose which session to kill.
    RemoteSessionKillPicker {
        /// Sessions fetched from the remote host.
        sessions: Vec<crate::commands::remote::RemoteSessionEntry>,
        /// Currently highlighted index.
        selected: usize,
        /// The resolved remote address.
        remote_addr: String,
    },
}

/// Tracks which command is waiting for dialog answers (mount scope, auth).
#[derive(Debug, Clone, PartialEq)]
pub enum PendingCommand {
    None,
    Ready {
        refresh: bool,
        build: bool,
        no_cache: bool,
        non_interactive: bool,
        allow_docker: bool,
        /// Pre-collected answer to the legacy migration dialog.
        /// `None` = no legacy layout detected (or dialog not yet shown).
        /// `Some(true)` = user accepted migration.
        /// `Some(false)` = user declined migration (keep legacy layout).
        migrate_decision: Option<bool>,
        /// Pre-collected answer to the template-audit dialog.
        /// `None` = no template match detected (or dialog not yet shown).
        /// `Some(true)` = user accepted running the audit.
        /// `Some(false)` = user declined running the audit.
        template_audit_decision: Option<bool>,
    },
    Implement {
        /// Override the configured agent for this session.
        agent: Option<String>,
        /// Override the model used by the agent.
        model: Option<String>,
        work_item: u32,
        non_interactive: bool,
        plan: bool,
        allow_docker: bool,
        /// Optional workflow file path for multi-step execution.
        workflow: Option<PathBuf>,
        /// Run in an isolated Git worktree.
        worktree: bool,
        /// Mount host ~/.ssh read-only into the container.
        mount_ssh: bool,
        /// Enable fully autonomous mode (--dangerously-skip-permissions + auto-advance).
        yolo: bool,
        /// Enable auto permission mode (--permission-mode auto, no auto-advance).
        auto: bool,
        /// Raw `--overlay` flag value from the TUI command input (comma-separated overlay spec).
        overlay: Option<String>,
    },
    Chat {
        /// Override the configured agent for this session.
        agent: Option<String>,
        /// Override the model used by the agent.
        model: Option<String>,
        non_interactive: bool,
        plan: bool,
        allow_docker: bool,
        /// Mount host ~/.ssh read-only into the container.
        mount_ssh: bool,
        /// Enable fully autonomous mode (--dangerously-skip-permissions).
        yolo: bool,
        /// Enable auto permission mode (--permission-mode auto).
        auto: bool,
        /// Raw `--overlay` flag value from the TUI command input (comma-separated overlay spec).
        overlay: Option<String>,
    },
    ClawsReady,
    /// specs amend: run amend agent for a work item.
    SpecsAmend {
        work_item: u32,
        allow_docker: bool,
    },
    /// specs new --interview: run interview agent after file creation.
    SpecsNewInterview {
        work_item_number: u32,
        kind: crate::commands::new::WorkItemKind,
        title: String,
        summary: String,
        allow_docker: bool,
    },
    /// exec prompt: send a one-shot prompt to the agent.
    ExecPrompt {
        prompt: String,
        agent: Option<String>,
        model: Option<String>,
        non_interactive: bool,
        plan: bool,
        allow_docker: bool,
        mount_ssh: bool,
        yolo: bool,
        auto: bool,
        /// Raw `--overlay` flag value from the TUI command input (comma-separated overlay spec).
        overlay: Option<String>,
    },
    /// exec workflow: run a workflow file (optionally with a work item).
    ExecWorkflow {
        workflow: PathBuf,
        work_item: Option<u32>,
        agent: Option<String>,
        model: Option<String>,
        non_interactive: bool,
        plan: bool,
        allow_docker: bool,
        worktree: bool,
        mount_ssh: bool,
        yolo: bool,
        auto: bool,
        /// Raw `--overlay` flag value from the TUI command input (comma-separated overlay spec).
        overlay: Option<String>,
    },
    /// remote run: execute a command on the remote host.
    RemoteRun {
        remote_addr: String,
        session_id: String,
        command: Vec<String>,
        follow: bool,
        api_key: Option<String>,
    },
    /// remote session start: create a new session on the remote host.
    RemoteSessionStart {
        remote_addr: String,
        dir: String,
        api_key: Option<String>,
    },
    /// remote session kill: close a session on the remote host.
    RemoteSessionKill {
        remote_addr: String,
        session_id: String,
        api_key: Option<String>,
    },
}

/// Which phase of the multi-step claws workflow is active in the TUI.
#[derive(Debug, Clone, PartialEq)]
pub enum ClawsPhase {
    /// Not running a claws workflow.
    Inactive,
    /// Container start-only task is running (used by `claws ready` when container stopped).
    Setup,
    /// Initial image build text task is running (downloads Dockerfile.nanoclaw + builds image).
    PreAudit,
    /// Post-build: /setup dialog + docker socket dialog + container launch + detached audit exec.
    PostAudit,
}

/// Which phase of the multi-step `ready` / `init` audit workflow is active in the TUI.
///
/// Both commands split into three phases so the audit agent runs in a foreground
/// PTY container window rather than being captured in the background:
///   1. Pre-audit text task (image builds, Q&A)
///   2. PTY audit container (foreground, interactive)
///   3. Post-audit text task (image rebuild, summary)
#[derive(Debug, Clone, PartialEq)]
pub enum AuditPhase {
    /// No audit workflow is in progress.
    Inactive,
    /// `ready` pre-audit text task is running; handoff arrives via `ready_audit_handoff_rx`.
    ReadyPreAudit,
    /// `ready` PTY audit container is running; handoff is in `ready_audit_handoff`.
    ReadyAuditPty,
    /// `ready` post-audit text task is running.
    ReadyPostAudit,
    /// `init` pre-audit text task is running; handoff arrives via `init_audit_handoff_rx`.
    InitPreAudit,
    /// `init` PTY audit container is running; handoff is in `init_audit_handoff`.
    InitAuditPty,
    /// `init` post-audit text task is running.
    InitPostAudit,
    /// An agent Dockerfile is being downloaded and built; re-launch pending command on completion.
    AgentSetupBuild,
}

/// State of the container overlay window.
#[derive(Debug, Clone, PartialEq)]
pub enum ContainerWindowState {
    /// No container window is visible.
    Hidden,
    /// Container window is open and capturing all keyboard input.
    Maximized,
    /// Container window is collapsed to a 1-line bar below the outer window.
    Minimized,
}

/// Metadata about the currently running (or most recently run) container.
#[derive(Debug, Clone)]
pub struct ContainerInfo {
    pub container_name: String,
    pub agent_display_name: String,
    pub start_time: Instant,
    pub latest_stats: Option<ContainerStats>,
    /// History of (cpu%, memory_mb) samples for averaging.
    pub stats_history: Vec<(f64, f64)>,
}

/// Summary of a completed container session, displayed after the container exits.
#[derive(Debug, Clone)]
pub struct LastContainerSummary {
    pub agent_display_name: String,
    pub container_name: String,
    pub avg_cpu: String,
    pub avg_memory: String,
    pub total_time: String,
    pub exit_code: i32,
}

/// Human-readable display name for an agent.
/// Delegates to `Agent::display_name` so the TUI and CLI always agree.
pub fn agent_display_name(agent: &str) -> &str {
    use crate::cli::Agent;
    Agent::all()
        .iter()
        .find(|a| a.as_str() == agent)
        .map_or(agent, |a| a.display_name())
}

/// Format a duration in seconds into a human-readable string (e.g. "5s", "12m", "1h 23m").
pub fn format_duration(secs: u64) -> String {
    if secs < 60 {
        format!("{}s", secs)
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else {
        let h = secs / 3600;
        let m = (secs % 3600) / 60;
        if m == 0 {
            format!("{}h", h)
        } else {
            format!("{}h {}m", h, m)
        }
    }
}


/// Per-tab application state for the TUI event loop.
pub struct TabState {
    /// Working directory for this tab.
    pub cwd: PathBuf,
    pub focus: Focus,
    pub phase: ExecutionPhase,
    pub dialog: Dialog,

    // --- Command input box ---
    /// Current text in the command input box.
    pub input: String,
    /// Cursor position (byte offset).
    pub cursor_col: usize,
    /// Autocomplete suggestions for the current input.
    pub suggestions: Vec<String>,
    /// Error message to display below the command box (cleared on next keypress).
    pub input_error: Option<String>,

    // --- Execution window ---
    /// Output lines received from the running command (ANSI stripped).
    pub output_lines: Vec<String>,
    /// How many lines from the bottom to skip (for post-run scrolling).
    pub scroll_offset: usize,

    // --- Live PTY session (Some only while Running with a PTY process) ---
    pub pty: Option<PtySession>,
    pub pty_rx: Option<Receiver<crate::tui::pty::PtyEvent>>,
    /// Accumulates the current incomplete line from PTY output.
    /// Handles `\r` (carriage return) by clearing the buffer so subsequent
    /// characters overwrite from the start — this is how terminal spinners
    /// and progress indicators work.
    pub pty_line_buffer: String,
    /// When true, the last entry in `output_lines` is a "live" (unfinalised)
    /// line that should be updated in-place rather than appended to.
    pub pty_live_line: bool,
    /// When true, the previous chunk ended with `\r` and we haven't yet seen
    /// the next byte to decide if it's `\r\n` (newline) or bare `\r` (overwrite).
    pub pty_pending_cr: bool,

    // --- Channels for text-based command output (init/ready) ---
    pub output_rx: UnboundedReceiver<String>,
    /// Cloned into OutputSink::Channel when launching non-PTY commands.
    pub output_tx: UnboundedSender<String>,
    /// Fires once when the current non-PTY command exits.
    pub exit_rx: Option<tokio::sync::oneshot::Receiver<i32>>,

    // --- Pending TUI state before launching a command (used by dialogs) ---
    pub pending_command: PendingCommand,
    pub pending_mount_path: Option<PathBuf>,

    // --- Container window state ---
    /// Whether the container overlay window is visible (and in what state).
    pub container_window: ContainerWindowState,
    /// How many lines from the bottom to skip when rendering the container's
    /// vt100 scrollback (mouse-wheel scrolling). 0 = live view (auto-follow).
    pub container_scroll_offset: usize,
    /// Metadata about the currently running container.
    pub container_info: Option<ContainerInfo>,
    /// VT100 terminal emulator for rendering container output with full ANSI support
    /// (colors, bold, cursor positioning, tabs, etc.). Replaces plain-text line buffer.
    pub vt100_parser: Option<vt100::Parser>,
    /// Summary of the last container session (shown after container exits).
    pub last_container_summary: Option<LastContainerSummary>,
    /// Receives Docker stats from the background polling task.
    pub stats_rx: Option<UnboundedReceiver<ContainerStats>>,

    /// Host settings mounted into the container (sanitized config files in a temp dir).
    /// Held here so the temp dir lives as long as the container runs; dropped on finish.
    pub host_settings: Option<HostSettings>,

    /// Directory overlays resolved from config + env (no CLI flags in TUI mode).
    /// Resolved once when the git root is determined and applied to `host_settings`
    /// whenever it is (re-)created.
    pub resolved_overlays: Vec<crate::overlays::directory::DirectoryOverlay>,

    /// Number of scrollback lines for the vt100 parser. Loaded from config before
    /// `start_container` is called; defaults to `crate::config::DEFAULT_SCROLLBACK_LINES`.
    pub terminal_scrollback_lines: usize,

    // --- Terminal text selection state ---
    /// Anchor cell of the in-progress or completed text selection, in vt100 screen
    /// coordinates (row, col). Set on mouse button down; cleared on Esc or new container.
    pub terminal_selection_start: Option<(u16, u16)>,
    /// Current end cell of the text selection, in vt100 screen coordinates (row, col).
    /// Extended on mouse drag; finalized on mouse button up.
    pub terminal_selection_end: Option<(u16, u16)>,
    /// Snapshot of the vt100 cell contents captured at MouseDown. Used for text extraction
    /// on copy, isolating the selection from live output that may shift cell contents.
    pub terminal_selection_snapshot: Option<Vec<Vec<String>>>,

    /// Inner content area of the container window, updated each frame by the renderer.
    /// Used to convert mouse terminal coordinates into vt100 cell (row, col) positions.
    pub container_inner_area: Option<Rect>,

    /// Timestamp of the most recent PTY byte received from the running container.
    /// `None` when no container is active. Used to detect stuck (idle) containers:
    /// if the elapsed time exceeds `STUCK_TIMEOUT`, `is_stuck()` returns `true`
    /// and the tab turns yellow.
    pub last_output_time: Option<Instant>,

    // --- Ready / init audit phase split state ---
    /// Which phase of the ready/init audit workflow is active.
    pub audit_phase: AuditPhase,
    /// Handoff produced by the `ready` pre-audit text task; consumed to launch the PTY.
    /// Also retained during `ReadyAuditPty` so post-audit has `ctx`, `opts`, `summary`.
    pub ready_audit_handoff: Option<crate::commands::ready_flow::ReadyAuditHandoff>,
    /// Receives the handoff from the `ready` pre-audit background task.
    pub ready_audit_handoff_rx: Option<tokio::sync::oneshot::Receiver<crate::commands::ready_flow::ReadyAuditHandoff>>,
    /// Handoff produced by the `init` pre-audit text task; consumed to launch the PTY.
    pub init_audit_handoff: Option<crate::commands::init_flow::InitAuditHandoff>,
    /// Receives the handoff from the `init` pre-audit background task.
    pub init_audit_handoff_rx: Option<tokio::sync::oneshot::Receiver<crate::commands::init_flow::InitAuditHandoff>>,

    // --- Claws wizard state ---
    /// Which phase of the claws workflow is active.
    pub claws_phase: ClawsPhase,
    /// Container ID received from the claws setup task; consumed when attaching.
    pub claws_container_id: Option<String>,
    /// Receives the container ID from the claws setup task when it completes.
    pub claws_container_id_rx: Option<tokio::sync::oneshot::Receiver<String>>,
    /// GitHub username entered during the claws first-run wizard.
    pub claws_wizard_username: Option<String>,
    /// Whether the user indicated they have already forked nanoclaw.
    pub claws_wizard_already_forked: bool,
    /// Receives a unit signal from the background clone task when it encounters
    /// a permission-denied error and needs the user's permission to use sudo.
    pub claws_sudo_request_rx: Option<tokio::sync::oneshot::Receiver<()>>,
    /// Sends the user's sudo password (Some = accepted with password, None = declined) to the clone task.
    pub claws_sudo_response_tx: Option<tokio::sync::oneshot::Sender<Option<String>>>,
    /// Receives a unit signal from the background build task when it needs the user to
    /// accept docker socket access (shown after the image rebuild completes).
    pub claws_docker_accept_request_rx: Option<tokio::sync::oneshot::Receiver<()>>,
    /// Sends the user's docker-socket acceptance (true = accepted, false = declined) to the build task.
    pub claws_docker_accept_response_tx: Option<tokio::sync::oneshot::Sender<bool>>,
    /// Context produced by the pre-audit background task, consumed to launch the audit PTY.
    /// Also stored across the Audit phase so the post-audit task can access env_vars etc.
    pub claws_audit_ctx: Option<ClawsAuditCtx>,
    /// Receives the audit context from the pre-audit background task.
    pub claws_audit_ctx_rx: Option<tokio::sync::oneshot::Receiver<ClawsAuditCtx>>,
    /// When true, attach to the nanoclaw container after it is started.
    /// Set by `claws chat` when the container is not running.
    pub claws_attach_after_start: bool,
    /// Container ID of a stopped container that we tried (and failed) to restart.
    /// Stored so the error-recovery dialog can offer to delete it and start fresh.
    pub claws_restarting_container_id: Option<String>,

    /// Cancels a running `status --watch` loop in the TUI.
    ///
    /// `start_command` sends on this channel (if present) before starting any new
    /// command, stopping the background watch task so stale status output does not
    /// overwrite the new command's output.
    pub status_watch_cancel_tx: Option<tokio::sync::oneshot::Sender<()>>,

    // --- Multi-step workflow state ---
    /// Active workflow state for the current `implement --workflow` run.
    /// `None` when no workflow is active.
    pub workflow: Option<WorkflowState>,
    /// Name of the workflow step currently executing (while `workflow` is `Some`).
    pub workflow_current_step: Option<String>,
    /// Git root path captured when the workflow was launched (needed for state persistence).
    pub workflow_git_root: Option<PathBuf>,
    /// Resolved per-step agent map: step_name → effective agent name.
    /// Built during workflow initialization using per-step `Agent:` fields and the
    /// config default. Used to determine "same container" eligibility between steps.
    pub workflow_step_agents: std::collections::HashMap<String, String>,
    /// Fallback decisions made during workflow pre-flight: declined_agent → default_agent.
    /// When `AgentSetupConfirm` is declined for a non-default agent, the user is offered
    /// a fallback to the default; accepted fallbacks are stored here so pre-flight can
    /// substitute the default for all steps that referenced the declined agent.
    pub workflow_agent_fallbacks: std::collections::HashMap<String, String>,
    /// Set to `true` once the `WorkflowControlBoard` dialog has been auto-opened for the
    /// current stuck episode, preventing it from re-opening on every subsequent tick.
    /// Reset to `false` by `acknowledge_stuck()` and `finish_command()`.
    pub workflow_stuck_dialog_opened: bool,
    /// Timestamp of the last time the user dismissed the `WorkflowControlBoard` dialog
    /// with Esc. While within `STUCK_DIALOG_BACKOFF` of this instant the dialog will not
    /// be auto-opened again, even if the tab remains stuck.
    pub workflow_stuck_dialog_dismissed_at: Option<Instant>,

    // --- Worktree state (set when --worktree is active) ---
    /// The branch name created for this worktree session.
    pub worktree_branch: Option<String>,
    /// The path to the active worktree directory.
    pub worktree_active_path: Option<PathBuf>,
    /// The git root captured when the worktree was created.
    pub worktree_git_root: Option<PathBuf>,
    /// When `true`, skip the uncommitted-files pre-check on the next worktree creation.
    /// Set by the pre-commit warning dialog when the user chooses "use last commit".
    pub worktree_skip_precommit_check: bool,

    // --- Workflow launch context (persisted so step-advancement uses identical settings) ---
    /// Resolved `~/.ssh` path when `--mount-ssh` was passed for this workflow.
    /// `None` when SSH mounting was not requested.
    pub workflow_ssh_dir: Option<PathBuf>,
    /// Mount path used for the first workflow step. This is the worktree path when
    /// `--worktree` is active, or the pending mount path otherwise. Every subsequent
    /// step must use the same path so the container sees a consistent filesystem.
    pub workflow_mount_path: Option<PathBuf>,
    /// Whether `--allow-docker` was passed for this workflow session.
    pub workflow_allow_docker: bool,

    // --- Yolo/auto mode state ---
    /// When `true`, the agent was launched with `--yolo` (fully autonomous mode).
    pub yolo_mode: bool,
    /// When `true`, the agent was launched with `--auto` (--permission-mode auto).
    /// Unlike yolo_mode, auto_mode does not trigger auto-advance in workflows.
    pub auto_mode: bool,
    /// Resolved `yoloDisallowedTools` list for the current session.
    /// Empty when neither yolo nor auto mode is active, or no tools are configured.
    pub yolo_disallowed_tools: Vec<String>,
    /// When `true`, the stuck-dialog auto-popup is disabled for the current workflow step.
    /// Set by pressing `d` in the `WorkflowControlBoard` dialog; reset when the step changes.
    pub auto_workflow_disabled_for_step: bool,
    /// Set to `true` by `tick_all()` when a yolo countdown dialog expires.
    /// The event loop reads this flag and dispatches the appropriate workflow-advance action.
    pub yolo_countdown_expired: bool,

    /// Timestamp of the most recent user keypress or mouse interaction on this tab.
    /// `None` until the first interaction. Used by `is_stuck(true)` to suppress stuck
    /// detection while the user is actively engaged with the active tab.
    pub last_user_activity_time: Option<Instant>,

    /// Single authoritative timestamp for the yolo countdown timer.
    /// Set by `tick_all()` when a tab (active or background) first becomes stuck in yolo
    /// mode. Cleared when the countdown expires, when new output arrives, or when the
    /// active tab is no longer stuck due to user activity.
    /// Dialog rendering reads this value rather than any field inside the dialog variant.
    pub yolo_countdown_started_at: Option<Instant>,

    /// Session ID of the last successfully started/used remote session on this tab.
    /// Used as the default session when `remote run` is invoked without --session.
    pub last_remote_session_id: Option<String>,

    /// If set, this tab is bound to a remote headless session for its lifetime.
    /// All commands are sent to this host/session via the headless API.
    pub remote_binding: Option<RemoteTabBinding>,

    /// Receives the result of a background remote sessions fetch (for the new-tab dialog).
    pub remote_sessions_fetch_rx: Option<tokio::sync::oneshot::Receiver<Result<Vec<crate::commands::remote::RemoteSessionEntry>, String>>>,

    /// Receives workflow state updates from a remote polling task.
    pub remote_workflow_rx: Option<UnboundedReceiver<crate::workflow::WorkflowState>>,
    /// Command ID of the currently running remote command (for workflow polling).
    pub remote_command_id: Option<String>,
}

impl TabState {
    pub fn new(cwd: PathBuf) -> Self {
        let (output_tx, output_rx) = mpsc::unbounded_channel();
        Self {
            cwd,
            focus: Focus::CommandBox,
            phase: ExecutionPhase::Idle,
            dialog: Dialog::None,
            input: String::new(),
            cursor_col: 0,
            suggestions: Vec::new(),
            input_error: None,
            output_lines: Vec::new(),
            scroll_offset: 0,
            pty: None,
            pty_rx: None,
            pty_line_buffer: String::new(),
            pty_live_line: false,
            pty_pending_cr: false,
            output_rx,
            output_tx,
            exit_rx: None,
            pending_command: PendingCommand::None,
            pending_mount_path: None,
            container_window: ContainerWindowState::Hidden,
            container_scroll_offset: 0,
            container_info: None,
            vt100_parser: None,
            last_container_summary: None,
            stats_rx: None,
            host_settings: None,
            resolved_overlays: Vec::new(),
            terminal_scrollback_lines: crate::config::DEFAULT_SCROLLBACK_LINES,
            terminal_selection_start: None,
            terminal_selection_end: None,
            terminal_selection_snapshot: None,
            container_inner_area: None,
            audit_phase: AuditPhase::Inactive,
            ready_audit_handoff: None,
            ready_audit_handoff_rx: None,
            init_audit_handoff: None,
            init_audit_handoff_rx: None,
            claws_phase: ClawsPhase::Inactive,
            claws_container_id: None,
            claws_container_id_rx: None,
            claws_wizard_username: None,
            claws_wizard_already_forked: false,
            claws_sudo_request_rx: None,
            claws_sudo_response_tx: None,
            claws_docker_accept_request_rx: None,
            claws_docker_accept_response_tx: None,
            claws_audit_ctx: None,
            claws_audit_ctx_rx: None,
            claws_attach_after_start: false,
            claws_restarting_container_id: None,
            status_watch_cancel_tx: None,
            last_output_time: None,
            workflow: None,
            workflow_current_step: None,
            workflow_git_root: None,
            workflow_step_agents: std::collections::HashMap::new(),
            workflow_agent_fallbacks: std::collections::HashMap::new(),
            workflow_stuck_dialog_opened: false,
            workflow_stuck_dialog_dismissed_at: None,
            worktree_branch: None,
            worktree_active_path: None,
            worktree_git_root: None,
            worktree_skip_precommit_check: false,
            workflow_ssh_dir: None,
            workflow_mount_path: None,
            workflow_allow_docker: false,
            yolo_mode: false,
            auto_mode: false,
            yolo_disallowed_tools: Vec::new(),
            auto_workflow_disabled_for_step: false,
            yolo_countdown_expired: false,
            last_user_activity_time: None,
            yolo_countdown_started_at: None,
            last_remote_session_id: None,
            remote_binding: None,
            remote_sessions_fetch_rx: None,
            remote_workflow_rx: None,
            remote_command_id: None,
        }
    }

    /// Resolve overlays for the given git root (config + env only) and cache them.
    ///
    /// No-op if overlays have already been resolved for this tab.
    /// For commands that also accept `--overlay` flags, call `resolve_and_cache_overlays`
    /// instead, which always re-resolves including flag values.
    pub fn resolve_overlays_once(&mut self, git_root: &std::path::Path) -> anyhow::Result<()> {
        if self.resolved_overlays.is_empty() {
            self.resolved_overlays = crate::overlays::resolve_overlays(git_root, &[])?;
        }
        Ok(())
    }

    /// Resolve overlays including any per-command `--overlay` flag values and cache them.
    ///
    /// Always re-resolves (unlike `resolve_overlays_once`) so that per-command flags
    /// are incorporated even when overlays were previously cached from an earlier run.
    /// Returns an error if any flag value is malformed.
    pub fn resolve_and_cache_overlays(
        &mut self,
        git_root: &std::path::Path,
        raw_overlay_flags: &[String],
    ) -> anyhow::Result<()> {
        self.resolved_overlays = crate::overlays::resolve_overlays(git_root, raw_overlay_flags)?;
        Ok(())
    }

    /// Apply resolved_overlays to the current host_settings.
    ///
    /// Call this after setting `self.host_settings` so overlay mounts are included.
    pub fn apply_overlays_to_host_settings(&mut self) {
        if self.resolved_overlays.is_empty() {
            return;
        }
        match self.host_settings.as_mut() {
            Some(hs) => hs.set_overlays(self.resolved_overlays.clone()),
            None => {
                self.host_settings = Some(
                    HostSettings::overlays_only(self.resolved_overlays.clone()),
                );
            }
        }
    }

    /// Append a line to the execution window output.
    pub fn push_output(&mut self, line: impl Into<String>) {
        self.output_lines.push(line.into());
        // Auto-scroll to bottom while running.
        if matches!(self.phase, ExecutionPhase::Running { .. }) {
            self.scroll_offset = 0;
        }
    }

    /// Clear output and reset state for a fresh command execution.
    #[tracing::instrument(skip(self), fields(command = %command))]
    pub fn start_command(&mut self, command: String) {
        // Cancel any running status --watch loop so it doesn't overwrite the
        // new command's output.
        if let Some(tx) = self.status_watch_cancel_tx.take() {
            let _ = tx.send(());
        }
        self.output_lines.clear();
        self.scroll_offset = 0;
        self.pty_line_buffer.clear();
        self.pty_live_line = false;
        self.pty_pending_cr = false;
        self.phase = ExecutionPhase::Running { command };
        self.focus = Focus::ExecutionWindow;
        self.input_error = None;
        // For remote-bound tabs: clear stale workflow state from the previous
        // command so the workflow strip does not show out-of-date data while the
        // new command is starting up (before its first poll returns).
        // Dropping the receiver also causes any in-flight polling task to exit
        // cleanly on its next attempted send.
        if self.remote_binding.is_some() {
            self.workflow = None;
            self.remote_workflow_rx = None;
            self.remote_command_id = None;
        }
    }

    /// Activate the container window for a new PTY container session.
    ///
    /// `cols` and `rows` specify the inner dimensions of the container window
    /// (used to initialise the VT100 terminal emulator).
    #[tracing::instrument(skip(self), fields(container_name = %container_name, cols, rows))]
    pub fn start_container(
        &mut self,
        container_name: String,
        agent_display_name: String,
        cols: u16,
        rows: u16,
    ) {
        self.container_window = ContainerWindowState::Maximized;
        self.container_scroll_offset = 0;
        self.vt100_parser = Some(vt100::Parser::new(rows, cols, self.terminal_scrollback_lines));
        self.last_container_summary = None;
        self.terminal_selection_start = None;
        self.terminal_selection_end = None;
        self.terminal_selection_snapshot = None;
        self.last_output_time = Some(Instant::now());
        self.container_info = Some(ContainerInfo {
            container_name,
            agent_display_name,
            start_time: Instant::now(),
            latest_stats: None,
            stats_history: Vec::new(),
        });
    }

    /// Clear any active terminal text selection.
    pub fn clear_terminal_selection(&mut self) {
        self.terminal_selection_start = None;
        self.terminal_selection_end = None;
        self.terminal_selection_snapshot = None;
    }

    /// Transition to the next phase of a multi-step workflow (e.g. ready).
    /// Like `start_command` but preserves existing output instead of clearing it.
    pub fn continue_command(&mut self, command: String) {
        self.scroll_offset = 0;
        self.pty_line_buffer.clear();
        self.pty_live_line = false;
        self.pty_pending_cr = false;
        self.phase = ExecutionPhase::Running { command };
        self.focus = Focus::ExecutionWindow;
        self.input_error = None;
    }

    /// Transition to Done or Error based on exit code; re-enable input.
    #[tracing::instrument(skip(self), fields(exit_code))]
    pub fn finish_command(&mut self, exit_code: i32) {
        let command = match &self.phase {
            ExecutionPhase::Running { command } => command.clone(),
            _ => String::new(),
        };
        self.phase = if exit_code == 0 {
            ExecutionPhase::Done { command }
        } else {
            ExecutionPhase::Error { command, exit_code }
        };
        self.focus = Focus::CommandBox;
        self.pty = None;
        self.pty_rx = None;
        self.pty_line_buffer.clear();
        self.pty_live_line = false;
        self.pty_pending_cr = false;
        self.exit_rx = None;

        // Drop host settings only if no multi-phase workflow is in progress.
        // During claws setup, the text task completes before the PTY exec session starts —
        // host_settings must survive until the exec session ends.
        // Also preserve host_settings while a workflow step sequence or audit PTY is active.
        if self.claws_phase == ClawsPhase::Inactive
            && self.audit_phase == AuditPhase::Inactive
            && self.workflow.is_none()
        {
            self.host_settings = None;
        }

        // Clear the stuck-detection timer; the container is no longer running.
        self.last_output_time = None;
        self.workflow_stuck_dialog_opened = false;
        self.workflow_stuck_dialog_dismissed_at = None;
        self.auto_workflow_disabled_for_step = false;
        self.yolo_countdown_expired = false;
        self.yolo_countdown_started_at = None;
        // Close the yolo countdown dialog defensively: the container has exited so there
        // is nothing left to count down for.  In normal yolo+workflow runs the worktree
        // merge prompt or WorkflowStepConfirm will overwrite this anyway, but if
        // worktree_branch is somehow unset the dialog would otherwise persist on screen.
        if matches!(self.dialog, Dialog::WorkflowYoloCountdown { .. }) {
            self.dialog = Dialog::None;
        }

        // Close the container window and generate a summary if applicable.
        if self.container_window != ContainerWindowState::Hidden {
            if let Some(info) = self.container_info.take() {
                let elapsed = info.start_time.elapsed().as_secs();
                let (avg_cpu, avg_memory) = if info.stats_history.is_empty() {
                    ("n/a".to_string(), "n/a".to_string())
                } else {
                    let count = info.stats_history.len() as f64;
                    let cpu_avg: f64 = info.stats_history.iter().map(|(c, _)| c).sum::<f64>() / count;
                    let mem_avg: f64 = info.stats_history.iter().map(|(_, m)| m).sum::<f64>() / count;
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
            self.container_window = ContainerWindowState::Hidden;
            self.vt100_parser = None;
            self.stats_rx = None;
        }
    }

    /// Forcibly terminate the running process and return the tab to idle state.
    ///
    /// Used when the user cancels a workflow execution mid-step.  Preserves the
    /// `workflow` state (and output lines) so the user can resume later, but tears
    /// down the PTY channels so no further PTY exit events are processed and the
    /// container info / window are cleaned up.
    pub fn reset_to_idle(&mut self) {
        self.phase = ExecutionPhase::Idle;
        self.focus = Focus::CommandBox;
        // Drop PTY resources so tick() won't call finish_command when the container
        // exit event eventually arrives.
        self.pty = None;
        self.pty_rx = None;
        self.pty_line_buffer.clear();
        self.pty_live_line = false;
        self.pty_pending_cr = false;
        self.exit_rx = None;
        // Clear stuck-detection state.
        self.last_output_time = None;
        self.workflow_stuck_dialog_opened = false;
        self.workflow_stuck_dialog_dismissed_at = None;
        self.auto_workflow_disabled_for_step = false;
        self.yolo_countdown_expired = false;
        self.yolo_countdown_started_at = None;
        // Close any workflow-related dialogs that no longer apply.
        if matches!(
            self.dialog,
            Dialog::WorkflowYoloCountdown { .. }
                | Dialog::WorkflowControlBoard { .. }
                | Dialog::WorkflowCancelConfirm
        ) {
            self.dialog = Dialog::None;
        }
        // Hide the container window and release associated resources.
        self.container_window = ContainerWindowState::Hidden;
        self.vt100_parser = None;
        self.stats_rx = None;
        self.container_info = None;
    }

    /// Whether PTY output should be routed to the vt100 terminal emulator.
    pub fn pty_uses_container(&self) -> bool {
        self.container_window != ContainerWindowState::Hidden
    }

    /// Process raw PTY output bytes, handling carriage returns (`\r`) correctly.
    ///
    /// This method is used for the *outer* execution window (non-container output).
    /// Container output is routed through the vt100 parser instead.
    ///
    /// Terminal applications use `\r` (without `\n`) to move the cursor back to
    /// column 0 so the next output overwrites the current line — this is how
    /// spinners and progress indicators work. `\r\n` is treated as a newline.
    ///
    /// The method maintains `pty_line_buffer` (the current incomplete line) and
    /// a "live line" at the end of `output_lines` that is updated in-place until
    /// a `\n` finalises it.
    #[tracing::instrument(skip(self, bytes), fields(bytes_len = bytes.len()))]
    pub fn process_pty_data(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }

        // Process \r and \n from the raw bytes BEFORE stripping ANSI escapes,
        // because strip_ansi_escapes::strip removes \r characters.
        let mut i = 0;

        // Resolve a pending \r from the previous chunk.
        if self.pty_pending_cr {
            self.pty_pending_cr = false;
            if bytes[0] == b'\n' {
                // Previous \r + this \n → newline.
                self.finalise_pty_line();
                i = 1;
            } else {
                // Previous \r was a bare carriage return → move cursor to column 0.
                // Clear the buffer so subsequent content overwrites the current line.
                self.pty_line_buffer.clear();
            }
        }

        while i < bytes.len() {
            match bytes[i] {
                b'\r' => {
                    if i + 1 < bytes.len() {
                        if bytes[i + 1] == b'\n' {
                            // \r\n → newline
                            self.finalise_pty_line();
                            i += 2;
                        } else {
                            // Bare \r → move cursor to column 0. Clear the buffer
                            // so subsequent content overwrites the current line
                            // (this is how terminal spinners/progress bars work).
                            self.pty_line_buffer.clear();
                            i += 1;
                        }
                    } else {
                        // \r at the very end of the chunk — defer until next chunk
                        // so we can distinguish \r\n (newline) from bare \r (overwrite).
                        self.pty_pending_cr = true;
                        i += 1;
                    }
                }
                b'\n' => {
                    self.finalise_pty_line();
                    i += 1;
                }
                _ => {
                    // Collect a content segment (up to next \r or \n).
                    let start = i;
                    while i < bytes.len() && bytes[i] != b'\r' && bytes[i] != b'\n' {
                        i += 1;
                    }
                    // Strip ANSI escape sequences from the content segment only.
                    let segment = &bytes[start..i];
                    let stripped = strip_ansi_escapes::strip(segment);
                    let text = String::from_utf8_lossy(&stripped);
                    // Filter out remaining C0 control characters (BEL, BS, ESC
                    // fragments, etc.) that have zero display width but non-zero
                    // byte length — they cause scroll calculation mismatches.
                    for ch in text.chars() {
                        if ch >= ' ' {
                            self.pty_line_buffer.push(ch);
                        }
                    }
                }
            }
        }

        // Sync the live-line display with the current buffer contents.
        if !self.pty_line_buffer.is_empty() {
            if self.pty_live_line {
                if let Some(last) = self.output_lines.last_mut() {
                    *last = self.pty_line_buffer.clone();
                }
            } else {
                self.output_lines.push(self.pty_line_buffer.clone());
                self.pty_live_line = true;
            }
            // Auto-scroll to bottom while running.
            if matches!(self.phase, ExecutionPhase::Running { .. }) {
                self.scroll_offset = 0;
            }
        }
    }

    /// Finalise the current PTY line buffer: push it to `output_lines`
    /// (or update the existing live line) and reset the buffer.
    fn finalise_pty_line(&mut self) {
        let line = std::mem::take(&mut self.pty_line_buffer);
        if self.pty_live_line {
            if let Some(last) = self.output_lines.last_mut() {
                *last = line;
            }
        } else {
            self.output_lines.push(line);
        }
        self.pty_live_line = false;
    }

    /// Border color for the execution window based on current state and focus.
    ///
    /// Selected:   blue (running) | green (done/success) | red (done/error)
    /// Unselected: grey (idle/running/done) | red (error, persists when unselected)
    pub fn window_border_color(&self) -> Color {
        match (&self.phase, &self.focus) {
            (ExecutionPhase::Running { .. }, Focus::ExecutionWindow) => Color::Blue,
            (ExecutionPhase::Running { .. }, Focus::CommandBox) => Color::Gray,
            (ExecutionPhase::Done { .. }, Focus::ExecutionWindow) => Color::Green,
            (ExecutionPhase::Done { .. }, Focus::CommandBox) => Color::Gray,
            (ExecutionPhase::Error { .. }, _) => Color::Red,
            (ExecutionPhase::Idle, _) => Color::DarkGray,
        }
    }

    /// Returns `true` if the running container has produced no output for
    /// longer than [`STUCK_TIMEOUT`]. Only meaningful when a container is
    /// active; always `false` otherwise.
    ///
    /// When `is_active = true` (the tab is currently visible to the user),
    /// also returns `false` if the user has interacted with the tab within
    /// the last `STUCK_TIMEOUT` — suppressing stuck detection while the user
    /// is actively reading output.
    /// When `is_active = false`, only `last_output_time` is considered.
    pub fn is_stuck(&self, is_active: bool) -> bool {
        if !matches!(&self.phase, ExecutionPhase::Running { .. }) {
            return false;
        }
        if self.container_window == ContainerWindowState::Hidden {
            return false;
        }
        if !self.last_output_time.map(|t| t.elapsed() > STUCK_TIMEOUT).unwrap_or(false) {
            return false;
        }
        // Active-tab suppression: if the user interacted recently, not considered stuck.
        if is_active {
            if let Some(activity) = self.last_user_activity_time {
                if activity.elapsed() < STUCK_TIMEOUT {
                    return false;
                }
            }
        }
        true
    }

    /// Reset the stuck timer to now and clear the auto-open flag.
    /// Call this whenever the user interacts with this tab (switching to it,
    /// typing, mouse scroll, etc.) so the yellow warning colour is immediately
    /// cleared and any pending auto-open of the `WorkflowControlBoard` dialog
    /// is deferred for another full `STUCK_TIMEOUT` window.
    pub fn acknowledge_stuck(&mut self) {
        self.workflow_stuck_dialog_opened = false;
        self.workflow_stuck_dialog_dismissed_at = None;
        // Do not reset last_output_time while a yolo countdown is active.  Doing so
        // would make is_stuck() return false on the very next tick, causing the "active
        // and not stuck" branch in tick_all() to immediately close the dialog that was
        // just opened by the tab-switching code — defeating the preserved-remaining-time
        // feature.  When a countdown is running, is_stuck() is still suppressed via
        // last_user_activity_time (if the user typed) or last_output_time (once real new
        // output arrives), so this guard does not break any other stuck-clearing path.
        if self.last_output_time.is_some() && self.yolo_countdown_started_at.is_none() {
            self.last_output_time = Some(Instant::now());
        }
    }

    /// Record that the user has interacted with this tab (keypress, mouse scroll, etc.).
    /// Sets `last_user_activity_time` so that `is_stuck(true)` returns `false` for the
    /// next `STUCK_TIMEOUT` window, suppressing stuck indicators while the user is active.
    /// Distinct from `acknowledge_stuck()`: that method resets the output-based timer;
    /// this one records the user's intent to suppress stuck detection.
    pub fn record_user_activity(&mut self) {
        self.last_user_activity_time = Some(Instant::now());
    }

    /// Record that the user dismissed the `WorkflowControlBoard` dialog with Esc.
    /// The dialog will not auto-open again for `STUCK_DIALOG_BACKOFF` (60 s), after
    /// which it becomes eligible to re-open if the tab is still stuck.
    pub fn dismiss_stuck_dialog(&mut self) {
        self.dialog = Dialog::None;
        self.workflow_stuck_dialog_opened = false;
        self.workflow_stuck_dialog_dismissed_at = Some(Instant::now());
    }

    /// Returns `Some(next_agent)` when the next ready step uses a different agent
    /// than the currently running step, making "continue in same container" invalid.
    /// Returns `None` when it is safe to reuse the container (same agent or no next step).
    pub fn next_step_different_agent(&self) -> Option<String> {
        let current_step = self.workflow_current_step.as_deref()?;
        let current_agent = self.workflow_step_agents.get(current_step)?;

        let wf = self.workflow.as_ref()?;
        let mut wf_clone = wf.clone();
        wf_clone.set_status(current_step, crate::workflow::StepStatus::Done);
        let next_ready = wf_clone.next_ready();
        let next_step = next_ready.first()?;

        let next_agent = self.workflow_step_agents.get(next_step.as_str())?;
        if next_agent != current_agent {
            Some(next_agent.clone())
        } else {
            None
        }
    }

    /// Returns `true` if the currently running workflow step is the last one —
    /// i.e. marking it Done would leave no further ready steps.
    pub fn is_last_workflow_step(&self) -> bool {
        let wf = match &self.workflow {
            Some(w) => w,
            None => return false,
        };
        let current = match &self.workflow_current_step {
            Some(s) => s.as_str(),
            None => return false,
        };
        let mut wf_clone = wf.clone();
        wf_clone.set_status(current, StepStatus::Done);
        wf_clone.next_ready().is_empty()
    }

    /// Returns the yolo countdown color for a background tab, alternating each second.
    /// `None` when `yolo_countdown_started_at` is not set.
    /// Even elapsed seconds → `Color::Yellow`; odd elapsed seconds → `Color::Magenta`.
    pub fn background_yolo_color(&self) -> Option<Color> {
        let started = self.yolo_countdown_started_at?;
        let secs_elapsed = started.elapsed().as_secs();
        if secs_elapsed % 2 == 0 {
            Some(Color::Yellow)
        } else {
            Some(Color::Magenta)
        }
    }

    /// Color for the tab indicator based on current phase and container state.
    /// When `is_active = false` and a yolo countdown is running, returns the
    /// alternating background yolo color instead of the normal color.
    pub fn tab_color(&self, is_active: bool) -> Color {
        // Background yolo countdown overrides normal color for background tabs.
        if !is_active {
            if let Some(color) = self.background_yolo_color() {
                return color;
            }
        }
        if self.is_stuck(is_active) {
            return Color::Yellow;
        }
        // Remote-bound tabs use purple (Magenta) unless stuck or in error.
        if self.remote_binding.is_some() {
            return match &self.phase {
                ExecutionPhase::Error { .. } => Color::Red,
                ExecutionPhase::Running { .. } => Color::Magenta,
                ExecutionPhase::Idle | ExecutionPhase::Done { .. } => Color::Magenta,
            };
        }
        match &self.phase {
            ExecutionPhase::Error { .. } => Color::Red,
            ExecutionPhase::Running { command } => {
                if self.claws_phase != ClawsPhase::Inactive || command.starts_with("claws") {
                    Color::Magenta
                } else if self.container_window != ContainerWindowState::Hidden {
                    Color::Green
                } else {
                    Color::Blue
                }
            }
            ExecutionPhase::Idle | ExecutionPhase::Done { .. } => Color::DarkGray,
        }
    }

    /// Project folder name for the tab border title (≤14 chars).
    pub fn tab_project_name(&self) -> String {
        // Remote-bound tabs show the display_host instead of the local project name.
        if let Some(ref binding) = self.remote_binding {
            let host = &binding.display_host;
            if host.chars().count() > 14 {
                let t: String = host.chars().take(13).collect();
                return format!("{}…", t);
            }
            return host.clone();
        }
        let name = self.cwd.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("?")
            .to_string();
        if name.chars().count() > 14 {
            let t: String = name.chars().take(13).collect();
            format!("{}…", t)
        } else {
            name
        }
    }

    /// Returns the yolo countdown label for a background tab.
    /// `None` when `yolo_countdown_started_at` is not set.
    /// Alternates between "⚠️  yolo in {N}" (yellow phase) and "🤘 yolo in {N}" (magenta phase),
    /// truncated to fit `tab_width` (total widget width including borders).
    pub fn background_yolo_label(&self, tab_width: u16) -> Option<String> {
        let started = self.yolo_countdown_started_at?;
        let elapsed = started.elapsed();
        let remaining = YOLO_COUNTDOWN_DURATION.saturating_sub(elapsed);
        let secs_remaining = remaining.as_secs();
        let secs_elapsed = elapsed.as_secs();
        let label = if secs_elapsed % 2 == 0 {
            format!("⚠️  yolo in {}", secs_remaining)
        } else {
            format!("🤘 yolo in {}", secs_remaining)
        };
        // Inner width: tab_width minus 2 borders minus 2 padding spaces.
        let max_chars = tab_width.saturating_sub(4) as usize;
        let truncated = if label.chars().count() > max_chars && max_chars > 1 {
            let t: String = label.chars().take(max_chars - 1).collect();
            format!("{}…", t)
        } else {
            label
        };
        Some(truncated)
    }

    /// Full subcommand shown inside the tab box, truncated if wider than the tab.
    /// `tab_width` is the total width of the tab widget (including borders).
    /// Empty string when idle. Prepends "⚠️ " when the tab is stuck.
    /// When `is_active = false` and a yolo countdown is running, returns the
    /// countdown label instead of the normal subcommand label.
    pub fn tab_subcommand_label(&self, tab_width: u16, is_active: bool) -> String {
        // Background yolo countdown overrides normal label for background tabs.
        if !is_active {
            if let Some(label) = self.background_yolo_label(tab_width) {
                return label;
            }
        }
        let cmd = match &self.phase {
            ExecutionPhase::Idle => {
                // Remote-bound tabs show "(ready)" when idle.
                if self.remote_binding.is_some() {
                    return "(ready)".to_string();
                }
                return String::new();
            }
            ExecutionPhase::Running { command }
            | ExecutionPhase::Done { command }
            | ExecutionPhase::Error { command, .. } => command.as_str(),
        };
        // Prepend warning prefix for stuck tabs.
        let prefix = if self.is_stuck(is_active) { "⚠️ " } else { "" };
        let prefix_chars = prefix.chars().count();
        // Inner width: tab_width minus 2 borders minus 2 padding spaces.
        let max_chars = tab_width.saturating_sub(4) as usize;
        let cmd_max = max_chars.saturating_sub(prefix_chars);
        let cmd_str = if cmd.chars().count() > cmd_max && cmd_max > 1 {
            let truncated: String = cmd.chars().take(cmd_max - 1).collect();
            format!("{}…", truncated)
        } else {
            cmd.to_string()
        };
        format!("{}{}", prefix, cmd_str)
    }

    /// Combined display name for the tab: "projname" or "projname | cmd".
    pub fn tab_display_name(&self) -> String {
        let proj = self.tab_project_name();
        let cmd = self.tab_subcommand_label(20, true);
        if cmd.is_empty() { proj } else { format!("{} | {}", proj, cmd) }
    }

    /// Poll all channels for new data; called once per event loop tick.
    pub fn tick(&mut self) {
        // Drain text command output.
        while let Ok(line) = self.output_rx.try_recv() {
            // Special marker sent by `status --watch` to clear the window before
            // rendering an updated snapshot. This makes the tables appear to update
            // in place even though the outer execution window does not support ANSI
            // cursor movement.
            if line == crate::commands::status::CLEAR_MARKER {
                self.output_lines.clear();
                self.scroll_offset = 0;
                continue;
            }
            // Split on newlines in case a single send contains multiple lines.
            for part in line.split('\n') {
                self.push_output(part.to_string());
            }
        }

        // Drain PTY output — collect events first to avoid a split borrow.
        let pty_events: Vec<crate::tui::pty::PtyEvent> = if let Some(ref rx) = self.pty_rx {
            let mut events = Vec::new();
            loop {
                match rx.try_recv() {
                    Ok(ev) => events.push(ev),
                    Err(_) => break,
                }
            }
            events
        } else {
            vec![]
        };
        for event in pty_events {
            match event {
                crate::tui::pty::PtyEvent::Data(bytes) => {
                    // Route container PTY data through the vt100 terminal emulator
                    // for full ANSI rendering. Non-container data goes through the
                    // plain-text line processor for the outer window.
                    if self.pty_uses_container() {
                        if let Some(ref mut parser) = self.vt100_parser {
                            parser.process(&bytes);
                        }
                        // Any output from the container resets the stuck timer.
                        self.last_output_time = Some(Instant::now());
                        // Cancel the yolo countdown and dialog: the agent is active again.
                        if matches!(self.dialog, Dialog::WorkflowYoloCountdown { .. }) {
                            self.dialog = Dialog::None;
                            self.workflow_stuck_dialog_opened = false;
                        }
                        // Also clear the authoritative countdown timer so the background
                        // tab bar returns to its normal color.
                        self.yolo_countdown_started_at = None;
                    } else {
                        self.process_pty_data(&bytes);
                    }
                }
                crate::tui::pty::PtyEvent::Exit(code) => {
                    self.finish_command(code);
                    // If a worktree was active, show the merge-or-discard dialog.
                    if let (Some(branch), Some(wt_path), Some(git_root)) = (
                        self.worktree_branch.clone(),
                        self.worktree_active_path.clone(),
                        self.worktree_git_root.clone(),
                    ) {
                        self.dialog = Dialog::WorktreeMergePrompt {
                            branch,
                            worktree_path: wt_path,
                            git_root,
                            had_error: code != 0,
                        };
                    }
                    break;
                }
            }
        }

        // Check non-PTY exit code.
        if let Some(ref mut rx) = self.exit_rx {
            if let Ok(code) = rx.try_recv() {
                self.finish_command(code);
                // If a worktree was active, show the merge-or-discard dialog.
                if let (Some(branch), Some(wt_path), Some(git_root)) = (
                    self.worktree_branch.clone(),
                    self.worktree_active_path.clone(),
                    self.worktree_git_root.clone(),
                ) {
                    self.dialog = Dialog::WorktreeMergePrompt {
                        branch,
                        worktree_path: wt_path,
                        git_root,
                        had_error: code != 0,
                    };
                }
            }
        }

        // Check for ready audit handoff from the pre-audit background task.
        if let Some(ref mut rx) = self.ready_audit_handoff_rx {
            if let Ok(handoff) = rx.try_recv() {
                self.ready_audit_handoff = Some(handoff);
                self.ready_audit_handoff_rx = None;
            }
        }

        // Check for init audit handoff from the pre-audit background task.
        if let Some(ref mut rx) = self.init_audit_handoff_rx {
            if let Ok(handoff) = rx.try_recv() {
                self.init_audit_handoff = Some(handoff);
                self.init_audit_handoff_rx = None;
            }
        }

        // Check for container ID from the claws setup task.
        if let Some(ref mut rx) = self.claws_container_id_rx {
            if let Ok(id) = rx.try_recv() {
                self.claws_container_id = Some(id);
            }
        }

        // Check for audit context from the pre-audit background task.
        if let Some(ref mut rx) = self.claws_audit_ctx_rx {
            if let Ok(ctx) = rx.try_recv() {
                self.claws_audit_ctx = Some(ctx);
                self.claws_audit_ctx_rx = None;
            }
        }

        // Check if the background clone task needs sudo permission.
        if let Some(ref mut rx) = self.claws_sudo_request_rx {
            if rx.try_recv().is_ok() {
                self.claws_sudo_request_rx = None;
                self.dialog = Dialog::ClawsReadySudoConfirm { password: String::new() };
            }
        }

        // Check if the background build task needs docker socket acceptance.
        if let Some(ref mut rx) = self.claws_docker_accept_request_rx {
            if rx.try_recv().is_ok() {
                self.claws_docker_accept_request_rx = None;
                self.dialog = Dialog::ClawsReadyDockerSocketWarning;
            }
        }

        // Drain Docker stats from the polling task.
        if let Some(ref mut rx) = self.stats_rx {
            while let Ok(stats) = rx.try_recv() {
                if let Some(ref mut info) = self.container_info {
                    let cpu = parse_cpu_percent(&stats.cpu_percent);
                    let mem = parse_memory_mb(&stats.memory);
                    info.stats_history.push((cpu, mem));
                    info.latest_stats = Some(stats);
                }
            }
        }

        // Check for remote sessions fetch result (new-tab dialog).
        if let Some(ref mut rx) = self.remote_sessions_fetch_rx {
            if let Ok(result) = rx.try_recv() {
                self.remote_sessions_fetch_rx = None;
                // Update the dialog if it's still the NewTabDirectory dialog.
                if let Dialog::NewTabDirectory { ref input, .. } = self.dialog {
                    let input = input.clone();
                    self.dialog = Dialog::NewTabDirectory {
                        input,
                        remote_sessions: Some(result),
                        remote_selected_idx: Some(0),
                        focus_workdir: true,
                    };
                }
            }
        }

        // Drain remote workflow state updates.
        if let Some(ref mut rx) = self.remote_workflow_rx {
            let mut latest: Option<WorkflowState> = None;
            while let Ok(state) = rx.try_recv() {
                latest = Some(state);
            }
            if let Some(state) = latest {
                let is_terminal = state.is_terminal();
                self.workflow = Some(state);
                if is_terminal {
                    // Stop polling — drop the receiver.
                    self.remote_workflow_rx = None;
                }
            }
        }
    }
}

/// Top-level application state: manages multiple tabs.
pub struct App {
    pub tabs: Vec<TabState>,
    pub active_tab_idx: usize,
    pub should_quit: bool,
    /// Live snapshot of tab→container associations, kept up-to-date by `tick_all()`.
    /// Shared with any running `status --watch` background task so the table reflects
    /// current state on every refresh rather than the state at command-start time.
    pub tui_tabs_shared: Arc<Mutex<Vec<TuiTabInfo>>>,
    /// Set to `true` after a TUI suspend/restore so the event loop calls
    /// `terminal.clear()` before the next draw, forcing a full re-render.
    pub needs_full_redraw: bool,
    /// Container runtime backend (Docker, Apple Containers, etc.).
    pub runtime: Arc<dyn crate::runtime::AgentRuntime>,
}

impl App {
    pub fn new(cwd: std::path::PathBuf) -> Self {
        Self::new_with_runtime(
            cwd,
            Arc::new(crate::runtime::DockerRuntime::new()),
        )
    }

    pub fn new_with_runtime(cwd: std::path::PathBuf, runtime: Arc<dyn crate::runtime::AgentRuntime>) -> Self {
        Self {
            tabs: vec![TabState::new(cwd)],
            active_tab_idx: 0,
            should_quit: false,
            tui_tabs_shared: Arc::new(Mutex::new(vec![])),
            needs_full_redraw: false,
            runtime,
        }
    }

    pub fn active_tab(&self) -> &TabState {
        &self.tabs[self.active_tab_idx]
    }

    pub fn active_tab_mut(&mut self) -> &mut TabState {
        &mut self.tabs[self.active_tab_idx]
    }

    /// Create a new tab immediately after the active tab. Returns the new tab index.
    pub fn create_tab(&mut self, cwd: std::path::PathBuf) -> usize {
        let new_idx = self.active_tab_idx + 1;
        self.tabs.insert(new_idx, TabState::new(cwd));
        new_idx
    }

    /// Close the tab at `idx`. Adjusts `active_tab_idx`.
    /// If only one tab remains, sets `should_quit`.
    pub fn close_tab(&mut self, idx: usize) {
        if self.tabs.len() <= 1 {
            self.should_quit = true;
            return;
        }
        self.tabs.remove(idx);
        if self.active_tab_idx >= self.tabs.len() {
            self.active_tab_idx = self.tabs.len() - 1;
        }
    }

    /// Call `tick()` on every tab so background PTY sessions stay live.
    /// Also refreshes the shared `tui_tabs_shared` snapshot so any running
    /// `status --watch` task sees up-to-date container associations and stuck state.
    pub fn tick_all(&mut self) {
        for tab in &mut self.tabs {
            tab.tick();
        }

        // Process stuck/yolo state transitions for all tabs.
        // Uses a captured index to avoid borrow-checker conflicts.
        let active = self.active_tab_idx;

        for (i, tab) in self.tabs.iter_mut().enumerate() {
            let is_active = i == active;
            let stuck = tab.is_stuck(is_active);

            // Active-tab: if no longer stuck (user activity suppression), close any open
            // yolo dialog and clear the countdown so the tab returns to its normal state.
            if is_active && !stuck {
                if matches!(tab.dialog, Dialog::WorkflowYoloCountdown { .. }) {
                    tab.dialog = Dialog::None;
                    tab.workflow_stuck_dialog_opened = false;
                    tab.yolo_countdown_started_at = None;
                }
            }

            if tab.yolo_mode && tab.workflow_current_step.is_some() {
                if stuck {
                    // Start the countdown timer if not already running and no expiry is
                    // pending consumption by the event loop.  Without this guard, the
                    // timer restarts on the very next tick after expiring for a background
                    // tab, because yolo_countdown_started_at is None and is_stuck() is
                    // still true while the container is being killed.
                    if tab.yolo_countdown_started_at.is_none() && !tab.yolo_countdown_expired {
                        tab.yolo_countdown_started_at = Some(Instant::now());
                    }

                    // Check if the countdown has expired → signal auto-advance.
                    if let Some(started) = tab.yolo_countdown_started_at {
                        if started.elapsed() >= YOLO_COUNTDOWN_DURATION {
                            tab.yolo_countdown_expired = true;
                            tab.yolo_countdown_started_at = None;
                            tab.dialog = Dialog::None;
                            tab.workflow_stuck_dialog_opened = false;
                            continue;
                        }
                    }

                    // Active tab only: open the dialog if not already open.
                    // Background tabs rely on tab-bar rendering for countdown feedback.
                    if is_active {
                        let backoff_elapsed = tab
                            .workflow_stuck_dialog_dismissed_at
                            .map(|t| t.elapsed() >= STUCK_DIALOG_BACKOFF)
                            .unwrap_or(true);
                        if tab.dialog == Dialog::None
                            && !tab.workflow_stuck_dialog_opened
                            && backoff_elapsed
                        {
                            let step = tab.workflow_current_step.clone().unwrap();
                            tab.dialog = Dialog::WorkflowYoloCountdown {
                                current_step: step,
                            };
                            tab.workflow_stuck_dialog_opened = true;
                        }
                    }
                } else {
                    // No longer stuck: reset countdown so a fresh one begins if it stalls again.
                    tab.yolo_countdown_started_at = None;
                }
            } else if !tab.yolo_mode && is_active && stuck && tab.workflow_current_step.is_some() {
                // Non-yolo active tab: open the WorkflowControlBoard unless suppressed.
                let backoff_elapsed = tab
                    .workflow_stuck_dialog_dismissed_at
                    .map(|t| t.elapsed() >= STUCK_DIALOG_BACKOFF)
                    .unwrap_or(true);
                if tab.dialog == Dialog::None
                    && !tab.workflow_stuck_dialog_opened
                    && backoff_elapsed
                    && !tab.auto_workflow_disabled_for_step
                {
                    let step = tab.workflow_current_step.clone().unwrap();
                    tab.dialog = Dialog::WorkflowControlBoard {
                        current_step: step,
                        error: None,
                    };
                    tab.workflow_stuck_dialog_opened = true;
                }
            }
        }

        let snapshot: Vec<TuiTabInfo> = self.tabs.iter().enumerate()
            .map(|(i, tab)| TuiTabInfo {
                tab_number: i + 1,
                container_name: tab.container_info.as_ref()
                    .map(|ci| ci.container_name.clone())
                    .unwrap_or_default(),
                is_stuck: tab.is_stuck(i == active),
            })
            .collect();
        if let Ok(mut guard) = self.tui_tabs_shared.lock() {
            *guard = snapshot;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new_tab() -> TabState {
        TabState::new(std::path::PathBuf::new())
    }

    #[test]
    fn window_border_color_blue_when_selected_and_running() {
        let mut tab = new_tab();
        tab.phase = ExecutionPhase::Running { command: "ready".into() };
        tab.focus = Focus::ExecutionWindow;
        assert_eq!(tab.window_border_color(), Color::Blue);
    }

    #[test]
    fn window_border_color_grey_when_unselected_running() {
        let mut tab = new_tab();
        tab.phase = ExecutionPhase::Running { command: "ready".into() };
        tab.focus = Focus::CommandBox;
        assert_eq!(tab.window_border_color(), Color::Gray);
    }

    #[test]
    fn window_border_color_green_when_selected_and_done() {
        let mut tab = new_tab();
        tab.phase = ExecutionPhase::Done { command: "ready".into() };
        tab.focus = Focus::ExecutionWindow;
        assert_eq!(tab.window_border_color(), Color::Green);
    }

    #[test]
    fn window_border_color_grey_when_unselected_done() {
        let mut tab = new_tab();
        tab.phase = ExecutionPhase::Done { command: "ready".into() };
        tab.focus = Focus::CommandBox;
        assert_eq!(tab.window_border_color(), Color::Gray);
    }

    #[test]
    fn window_border_color_red_on_error_selected() {
        let mut tab = new_tab();
        tab.phase = ExecutionPhase::Error { command: "ready".into(), exit_code: 1 };
        tab.focus = Focus::ExecutionWindow;
        assert_eq!(tab.window_border_color(), Color::Red);
    }

    #[test]
    fn window_border_color_red_on_error_unselected() {
        let mut tab = new_tab();
        tab.phase = ExecutionPhase::Error { command: "ready".into(), exit_code: 1 };
        tab.focus = Focus::CommandBox;
        assert_eq!(tab.window_border_color(), Color::Red);
    }

    #[test]
    fn start_command_clears_output_and_focuses_window() {
        let mut tab = new_tab();
        tab.output_lines.push("old line".into());
        tab.start_command("ready".into());
        assert!(tab.output_lines.is_empty());
        assert_eq!(tab.focus, Focus::ExecutionWindow);
        assert!(matches!(tab.phase, ExecutionPhase::Running { .. }));
    }

    #[test]
    fn continue_command_preserves_output() {
        let mut tab = new_tab();
        tab.output_lines.push("phase 1 output".into());
        tab.output_lines.push("more output".into());
        tab.continue_command("phase 2".into());
        // Output from previous phase must be preserved.
        assert_eq!(tab.output_lines.len(), 2);
        assert_eq!(tab.output_lines[0], "phase 1 output");
        assert!(matches!(tab.phase, ExecutionPhase::Running { .. }));
        assert_eq!(tab.focus, Focus::ExecutionWindow);
    }

    #[test]
    fn finish_command_zero_transitions_to_done() {
        let mut tab = new_tab();
        tab.phase = ExecutionPhase::Running { command: "init".into() };
        tab.finish_command(0);
        assert!(matches!(tab.phase, ExecutionPhase::Done { .. }));
        assert_eq!(tab.focus, Focus::CommandBox);
    }

    #[test]
    fn finish_command_nonzero_transitions_to_error() {
        let mut tab = new_tab();
        tab.phase = ExecutionPhase::Running { command: "ready".into() };
        tab.finish_command(1);
        assert!(matches!(tab.phase, ExecutionPhase::Error { exit_code: 1, .. }));
    }

    #[test]
    fn pty_data_newlines_create_separate_lines() {
        let mut tab = new_tab();
        tab.phase = ExecutionPhase::Running { command: "test".into() };
        tab.process_pty_data(b"Hello\nWorld\n");
        assert_eq!(tab.output_lines, vec!["Hello", "World"]);
        assert!(!tab.pty_live_line);
    }

    #[test]
    fn pty_data_cr_overwrites_current_line() {
        let mut tab = new_tab();
        tab.phase = ExecutionPhase::Running { command: "test".into() };
        // First chunk: spinner frame 1
        tab.process_pty_data(b"Thinking...");
        assert_eq!(tab.output_lines, vec!["Thinking..."]);
        assert!(tab.pty_live_line);

        // Second chunk: \r clears the buffer, "Done!" overwrites the live line
        tab.process_pty_data(b"\rDone!      ");
        assert_eq!(tab.output_lines, vec!["Done!      "]);
        assert!(tab.pty_live_line);

        // Newline finalises the line
        tab.process_pty_data(b"\n");
        assert_eq!(tab.output_lines, vec!["Done!      "]);
        assert!(!tab.pty_live_line);
    }

    #[test]
    fn pty_data_cr_lf_treated_as_newline() {
        let mut tab = new_tab();
        tab.phase = ExecutionPhase::Running { command: "test".into() };
        tab.process_pty_data(b"Hello\r\nWorld\r\n");
        assert_eq!(tab.output_lines, vec!["Hello", "World"]);
        assert!(!tab.pty_live_line);
    }

    #[test]
    fn pty_data_multiple_cr_in_one_chunk() {
        let mut tab = new_tab();
        tab.phase = ExecutionPhase::Running { command: "test".into() };
        // Multiple carriage returns in one chunk — each \r clears the buffer
        // so only the final frame survives (overwrite behavior).
        tab.process_pty_data(b"frame1\rframe2\rframe3\n");
        assert_eq!(tab.output_lines, vec!["frame3"]);
    }

    #[test]
    fn pty_data_cr_lf_split_across_chunks() {
        let mut tab = new_tab();
        tab.phase = ExecutionPhase::Running { command: "test".into() };
        // \r\n split: \r at end of chunk 1, \n at start of chunk 2.
        // Must be treated as a newline, NOT as bare \r (which would lose text).
        tab.process_pty_data(b"Hello\r");
        assert!(tab.pty_pending_cr, "should defer \\r at end of chunk");
        // The text should still be visible as a live line while pending.
        assert_eq!(tab.output_lines, vec!["Hello"]);

        tab.process_pty_data(b"\nWorld\r\n");
        assert!(!tab.pty_pending_cr);
        assert_eq!(tab.output_lines, vec!["Hello", "World"]);
        assert!(!tab.pty_live_line);
    }

    #[test]
    fn pty_data_cr_split_then_bare_cr() {
        let mut tab = new_tab();
        tab.phase = ExecutionPhase::Running { command: "test".into() };
        // \r at end of chunk, but next chunk does NOT start with \n → bare \r.
        tab.process_pty_data(b"old text\r");
        assert!(tab.pty_pending_cr);

        tab.process_pty_data(b"new text\n");
        assert!(!tab.pty_pending_cr);
        // bare \r clears the buffer, so "new text" overwrites "old text".
        assert_eq!(tab.output_lines, vec!["new text"]);
    }

    #[test]
    fn pty_data_empty_chunk_preserves_pending_cr() {
        let mut tab = new_tab();
        tab.phase = ExecutionPhase::Running { command: "test".into() };
        tab.process_pty_data(b"text\r");
        assert!(tab.pty_pending_cr);
        // Empty chunk should not resolve the pending \r.
        tab.process_pty_data(b"");
        assert!(tab.pty_pending_cr);
    }

    #[test]
    fn pty_data_control_chars_filtered() {
        let mut tab = new_tab();
        tab.phase = ExecutionPhase::Running { command: "test".into() };
        // BEL (0x07) and BS (0x08) should be filtered out of the line buffer.
        tab.process_pty_data(b"Hello\x07World\x08!\n");
        assert_eq!(tab.output_lines, vec!["HelloWorld!"]);
    }

    #[test]
    fn pty_data_tabs_stripped_by_ansi_strip() {
        let mut tab = new_tab();
        tab.phase = ExecutionPhase::Running { command: "test".into() };
        // strip_ansi_escapes also removes tabs; verify they don't cause issues.
        tab.process_pty_data(b"col1\tcol2\n");
        assert_eq!(tab.output_lines, vec!["col1col2"]);
    }

    // --- Container window tests ---

    #[test]
    fn container_window_starts_hidden() {
        let tab = new_tab();
        assert_eq!(tab.container_window, ContainerWindowState::Hidden);
        assert!(tab.container_info.is_none());
        assert!(tab.vt100_parser.is_none());
        assert!(tab.last_container_summary.is_none());
    }

    #[test]
    fn start_container_activates_window() {
        let mut tab = new_tab();
        tab.start_container("amux-test".into(), "Claude Code".into(), 78, 18);
        assert_eq!(tab.container_window, ContainerWindowState::Maximized);
        assert!(tab.container_info.is_some());
        assert!(tab.vt100_parser.is_some());
        let info = tab.container_info.as_ref().unwrap();
        assert_eq!(info.container_name, "amux-test");
        assert_eq!(info.agent_display_name, "Claude Code");
    }

    #[test]
    fn pty_data_routes_to_vt100_when_container_active() {
        let mut tab = new_tab();
        tab.phase = ExecutionPhase::Running { command: "implement 0001".into() };
        tab.start_container("amux-test".into(), "Claude Code".into(), 80, 24);

        // Feed data through the vt100 parser (simulating what tick() does).
        if let Some(ref mut parser) = tab.vt100_parser {
            parser.process(b"Hello from container\r\n");
        }

        // Output goes to vt100 screen, not outer window lines.
        let screen_text = tab.vt100_parser.as_ref().unwrap().screen().contents();
        assert!(
            screen_text.contains("Hello from container"),
            "vt100 screen should contain container output"
        );
        assert!(
            tab.output_lines.is_empty()
                || !tab.output_lines.iter().any(|l| l.contains("Hello from container")),
            "Outer window should not contain container output"
        );
    }

    #[test]
    fn pty_data_routes_to_outer_when_no_container() {
        let mut tab = new_tab();
        tab.phase = ExecutionPhase::Running { command: "test".into() };

        tab.process_pty_data(b"Hello outer\n");
        assert_eq!(tab.output_lines, vec!["Hello outer"]);
        assert!(tab.vt100_parser.is_none());
    }

    #[test]
    fn finish_command_closes_container_and_creates_summary() {
        let mut tab = new_tab();
        tab.phase = ExecutionPhase::Running { command: "implement 0001".into() };
        tab.start_container("amux-test".into(), "Claude Code".into(), 78, 18);

        tab.finish_command(0);

        assert_eq!(tab.container_window, ContainerWindowState::Hidden);
        assert!(tab.container_info.is_none());
        assert!(tab.vt100_parser.is_none());
        assert!(tab.last_container_summary.is_some());
        let summary = tab.last_container_summary.as_ref().unwrap();
        assert_eq!(summary.container_name, "amux-test");
        assert_eq!(summary.agent_display_name, "Claude Code");
        assert_eq!(summary.exit_code, 0);
    }

    #[test]
    fn finish_command_with_error_records_exit_code() {
        let mut tab = new_tab();
        tab.phase = ExecutionPhase::Running { command: "implement 0001".into() };
        tab.start_container("amux-test".into(), "Claude Code".into(), 78, 18);

        tab.finish_command(1);

        let summary = tab.last_container_summary.as_ref().unwrap();
        assert_eq!(summary.exit_code, 1);
    }

    #[test]
    fn start_container_clears_previous_summary() {
        let mut tab = new_tab();
        tab.last_container_summary = Some(LastContainerSummary {
            agent_display_name: "old".into(),
            container_name: "old".into(),
            avg_cpu: "0%".into(),
            avg_memory: "0MiB".into(),
            total_time: "0s".into(),
            exit_code: 0,
        });

        tab.start_container("amux-new".into(), "Claude Code".into(), 78, 18);
        assert!(tab.last_container_summary.is_none());
    }

    #[test]
    fn format_duration_seconds() {
        assert_eq!(format_duration(0), "0s");
        assert_eq!(format_duration(45), "45s");
    }

    #[test]
    fn format_duration_minutes() {
        assert_eq!(format_duration(60), "1m");
        assert_eq!(format_duration(120), "2m");
        assert_eq!(format_duration(3599), "59m");
    }

    #[test]
    fn format_duration_hours() {
        assert_eq!(format_duration(3600), "1h");
        assert_eq!(format_duration(5400), "1h 30m");
        assert_eq!(format_duration(7200), "2h");
    }

    #[test]
    fn agent_display_name_known_agents() {
        assert_eq!(agent_display_name("claude"), "Claude Code");
        assert_eq!(agent_display_name("codex"), "Codex");
        assert_eq!(agent_display_name("opencode"), "Opencode");
    }

    #[test]
    fn agent_display_name_unknown_returns_input() {
        assert_eq!(agent_display_name("custom-agent"), "custom-agent");
    }

    #[test]
    fn container_stats_history_used_for_averages() {
        let mut tab = new_tab();
        tab.phase = ExecutionPhase::Running { command: "implement 0001".into() };
        tab.start_container("amux-test".into(), "Claude Code".into(), 78, 18);

        // Simulate stats arriving
        if let Some(ref mut info) = tab.container_info {
            info.stats_history.push((5.0, 200.0));
            info.stats_history.push((10.0, 300.0));
        }

        tab.finish_command(0);

        let summary = tab.last_container_summary.as_ref().unwrap();
        assert_eq!(summary.avg_cpu, "7.5%");
        assert_eq!(summary.avg_memory, "250MiB");
    }

    #[test]
    fn container_scroll_offset_starts_at_zero() {
        let tab = new_tab();
        assert_eq!(tab.container_scroll_offset, 0);
    }

    #[test]
    fn start_container_resets_scroll_offset() {
        let mut tab = new_tab();
        tab.container_scroll_offset = 10;
        tab.start_container("test".into(), "Agent".into(), 80, 24);
        assert_eq!(tab.container_scroll_offset, 0);
    }

    #[test]
    fn claws_wizard_defaults_correct() {
        let tab = TabState::new(std::path::PathBuf::from("/tmp"));
        assert!(tab.claws_wizard_username.is_none());
        assert!(!tab.claws_wizard_already_forked);
        assert_eq!(tab.claws_phase, ClawsPhase::Inactive);
        assert!(tab.claws_container_id.is_none());
        assert!(tab.claws_sudo_request_rx.is_none());
        assert!(tab.claws_sudo_response_tx.is_none()); // channel for Option<String> (password or None)
    }

    #[test]
    fn tick_shows_sudo_confirm_dialog_when_request_received() {
        let mut tab = new_tab();
        let (sudo_tx, sudo_rx) = tokio::sync::oneshot::channel::<()>();
        tab.claws_sudo_request_rx = Some(sudo_rx);
        // Send the signal.
        sudo_tx.send(()).unwrap();
        tab.tick();
        assert_eq!(tab.dialog, Dialog::ClawsReadySudoConfirm { password: String::new() });
        assert!(tab.claws_sudo_request_rx.is_none(), "rx should be consumed after signal");
    }

    #[test]
    fn tick_does_not_show_sudo_dialog_when_no_signal() {
        let mut tab = new_tab();
        let (_sudo_tx, sudo_rx) = tokio::sync::oneshot::channel::<()>();
        tab.claws_sudo_request_rx = Some(sudo_rx);
        // Do NOT send the signal.
        tab.tick();
        assert_eq!(tab.dialog, Dialog::None);
    }

    #[test]
    fn pending_command_claws_ready() {
        let mut tab = new_tab();
        tab.pending_command = PendingCommand::ClawsReady;
        assert_eq!(tab.pending_command, PendingCommand::ClawsReady);
    }

    #[test]
    fn finish_command_does_not_leave_stale_scroll_offset() {
        let mut tab = new_tab();
        tab.phase = ExecutionPhase::Running { command: "implement 0001".into() };
        tab.start_container("test".into(), "Agent".into(), 80, 24);
        tab.container_scroll_offset = 15;
        tab.finish_command(0);
        // After finishing, container is hidden and scroll offset is irrelevant,
        // but it should be left at 0 for the next container session.
        // start_container resets it, so this just verifies no panic.
        assert_eq!(tab.container_window, ContainerWindowState::Hidden);
    }

    // --- tab_color tests ---

    #[test]
    fn tab_color_idle_is_dark_gray() {
        let tab = TabState::new(std::path::PathBuf::from("/tmp/proj"));
        assert_eq!(tab.tab_color(true), Color::DarkGray);
    }

    #[test]
    fn tab_color_running_no_container_is_blue() {
        let mut tab = TabState::new(std::path::PathBuf::from("/tmp/proj"));
        tab.phase = ExecutionPhase::Running { command: "chat".into() };
        assert_eq!(tab.tab_color(true), Color::Blue);
    }

    #[test]
    fn tab_color_running_with_container_is_green() {
        let mut tab = TabState::new(std::path::PathBuf::from("/tmp/proj"));
        tab.phase = ExecutionPhase::Running { command: "implement 0001".into() };
        tab.container_window = ContainerWindowState::Maximized;
        assert_eq!(tab.tab_color(true), Color::Green);
    }

    #[test]
    fn tab_color_error_is_red() {
        let mut tab = TabState::new(std::path::PathBuf::from("/tmp/proj"));
        tab.phase = ExecutionPhase::Error { command: "ready".into(), exit_code: 1 };
        assert_eq!(tab.tab_color(true), Color::Red);
    }

    #[test]
    fn tab_color_claws_command_no_container_is_magenta() {
        let mut tab = TabState::new(std::path::PathBuf::from("/tmp/proj"));
        tab.phase = ExecutionPhase::Running { command: "claws ready".into() };
        assert_eq!(tab.tab_color(true), Color::Magenta);
    }

    #[test]
    fn tab_color_claws_command_with_container_is_magenta() {
        let mut tab = TabState::new(std::path::PathBuf::from("/tmp/proj"));
        tab.phase = ExecutionPhase::Running { command: "claws ready (attached)".into() };
        tab.container_window = ContainerWindowState::Maximized;
        assert_eq!(tab.tab_color(true), Color::Magenta);
    }

    #[test]
    fn tab_color_claws_phase_active_is_magenta() {
        let mut tab = TabState::new(std::path::PathBuf::from("/tmp/proj"));
        tab.phase = ExecutionPhase::Running { command: "claws ready".into() };
        tab.claws_phase = ClawsPhase::Setup;
        assert_eq!(tab.tab_color(true), Color::Magenta);
    }

    #[test]
    fn tab_display_name_idle_shows_project() {
        let tab = TabState::new(std::path::PathBuf::from("/home/user/myproject"));
        assert_eq!(tab.tab_display_name(), "myproject");
    }

    #[test]
    fn tab_display_name_running_shows_command() {
        let mut tab = TabState::new(std::path::PathBuf::from("/home/user/proj"));
        tab.phase = ExecutionPhase::Running { command: "chat --plan".into() };
        // Full command shown: "proj | chat --plan"
        assert_eq!(tab.tab_display_name(), "proj | chat --plan");
    }

    #[test]
    fn tab_display_name_truncates_long_names() {
        let tab = TabState::new(std::path::PathBuf::from("/home/user/a-very-long-project-name"));
        // "a-very-long-pr…" should be 15 chars with ellipsis
        let name = tab.tab_display_name();
        assert!(name.chars().count() <= 14, "Name too long: {}", name);
    }

    #[test]
    fn app_new_creates_one_tab() {
        let app = App::new(std::path::PathBuf::from("/tmp"));
        assert_eq!(app.tabs.len(), 1);
        assert_eq!(app.active_tab_idx, 0);
        assert!(!app.should_quit);
    }

    #[test]
    fn app_create_tab_inserts_after_active() {
        let mut app = App::new(std::path::PathBuf::from("/tmp/a"));
        let new_idx = app.create_tab(std::path::PathBuf::from("/tmp/b"));
        assert_eq!(new_idx, 1);
        assert_eq!(app.tabs.len(), 2);
        assert_eq!(app.tabs[1].cwd, std::path::PathBuf::from("/tmp/b"));
    }

    #[test]
    fn app_close_tab_removes_and_adjusts_idx() {
        let mut app = App::new(std::path::PathBuf::from("/tmp/a"));
        app.create_tab(std::path::PathBuf::from("/tmp/b"));
        app.active_tab_idx = 1;
        app.close_tab(1);
        assert_eq!(app.tabs.len(), 1);
        assert_eq!(app.active_tab_idx, 0);
    }

    #[test]
    fn app_close_tab_single_tab_is_noop() {
        let mut app = App::new(std::path::PathBuf::from("/tmp"));
        app.close_tab(0);
        assert_eq!(app.tabs.len(), 1);
    }

    #[test]
    fn app_active_tab_returns_correct_tab() {
        let mut app = App::new(std::path::PathBuf::from("/tmp/a"));
        app.create_tab(std::path::PathBuf::from("/tmp/b"));
        app.active_tab_idx = 1;
        assert_eq!(app.active_tab().cwd, std::path::PathBuf::from("/tmp/b"));
    }

    // --- Stuck tab detection tests ---

    #[test]
    fn is_stuck_false_when_idle() {
        let tab = new_tab();
        assert!(!tab.is_stuck(false));
    }

    #[test]
    fn is_stuck_false_when_running_without_container() {
        let mut tab = new_tab();
        tab.phase = ExecutionPhase::Running { command: "init".into() };
        // No container → never stuck.
        assert!(!tab.is_stuck(false));
    }

    #[test]
    fn is_stuck_false_when_container_just_started() {
        let mut tab = new_tab();
        tab.phase = ExecutionPhase::Running { command: "implement 0001".into() };
        tab.start_container("amux-test".into(), "Claude Code".into(), 80, 24);
        // last_output_time was just set → not yet stuck.
        assert!(!tab.is_stuck(false));
    }

    #[test]
    fn is_stuck_true_when_container_silent_over_10s() {
        let mut tab = new_tab();
        tab.phase = ExecutionPhase::Running { command: "implement 0001".into() };
        tab.start_container("amux-test".into(), "Claude Code".into(), 80, 24);
        // Wind the clock back past the timeout.
        tab.last_output_time = Some(Instant::now() - (STUCK_TIMEOUT + Duration::from_secs(1)));
        assert!(tab.is_stuck(false));
    }

    #[test]
    fn is_stuck_false_exactly_at_timeout_boundary() {
        let mut tab = new_tab();
        tab.phase = ExecutionPhase::Running { command: "implement 0001".into() };
        tab.start_container("amux-test".into(), "Claude Code".into(), 80, 24);
        // 9 seconds elapsed — just under the 10s threshold.
        tab.last_output_time = Some(Instant::now() - Duration::from_secs(9));
        assert!(!tab.is_stuck(false));
    }

    #[test]
    fn tab_color_is_yellow_when_stuck() {
        let mut tab = new_tab();
        tab.phase = ExecutionPhase::Running { command: "implement 0001".into() };
        tab.start_container("amux-test".into(), "Claude Code".into(), 80, 24);
        tab.last_output_time = Some(Instant::now() - (STUCK_TIMEOUT + Duration::from_secs(1)));
        assert_eq!(tab.tab_color(true), Color::Yellow);
    }

    #[test]
    fn tab_color_not_yellow_after_acknowledge() {
        let mut tab = new_tab();
        tab.phase = ExecutionPhase::Running { command: "implement 0001".into() };
        tab.start_container("amux-test".into(), "Claude Code".into(), 80, 24);
        tab.last_output_time = Some(Instant::now() - (STUCK_TIMEOUT + Duration::from_secs(1)));
        assert_eq!(tab.tab_color(true), Color::Yellow);

        tab.acknowledge_stuck();
        // After acknowledging, last_output_time is reset to now → no longer stuck.
        assert_ne!(tab.tab_color(true), Color::Yellow);
        assert_eq!(tab.tab_color(true), Color::Green); // running + container = green
    }

    #[test]
    fn tab_subcommand_label_has_warning_prefix_when_stuck() {
        let mut tab = new_tab();
        tab.phase = ExecutionPhase::Running { command: "implement 0001".into() };
        tab.start_container("amux-test".into(), "Claude Code".into(), 80, 24);
        tab.last_output_time = Some(Instant::now() - (STUCK_TIMEOUT + Duration::from_secs(1)));

        let label = tab.tab_subcommand_label(30, true);
        assert!(
            label.contains("⚠️"),
            "expected warning emoji in stuck label, got: {:?}",
            label
        );
        assert!(label.contains("implement 0001"), "expected command in label, got: {:?}", label);
    }

    #[test]
    fn tab_subcommand_label_no_warning_prefix_when_not_stuck() {
        let mut tab = new_tab();
        tab.phase = ExecutionPhase::Running { command: "implement 0001".into() };
        let label = tab.tab_subcommand_label(30, true);
        assert!(!label.contains('⚠'), "expected no warning in non-stuck label, got: {:?}", label);
        assert_eq!(label, "implement 0001");
    }

    #[test]
    fn tab_subcommand_label_warning_prefix_after_acknowledge_is_gone() {
        let mut tab = new_tab();
        tab.phase = ExecutionPhase::Running { command: "implement 0001".into() };
        tab.start_container("amux-test".into(), "Claude Code".into(), 80, 24);
        tab.last_output_time = Some(Instant::now() - (STUCK_TIMEOUT + Duration::from_secs(1)));

        // Stuck → warning present.
        assert!(tab.tab_subcommand_label(30, true).contains('⚠'));

        // After acknowledgment → warning gone.
        tab.acknowledge_stuck();
        assert!(!tab.tab_subcommand_label(30, true).contains('⚠'));
    }

    #[test]
    fn acknowledge_stuck_is_noop_when_no_container() {
        let mut tab = new_tab();
        // last_output_time is None — the timer reset is skipped, but the
        // auto-open flag is still cleared (it is always reset unconditionally).
        tab.acknowledge_stuck();
        assert!(tab.last_output_time.is_none());
        assert!(!tab.workflow_stuck_dialog_opened);
    }

    #[test]
    fn start_container_initialises_last_output_time() {
        let mut tab = new_tab();
        assert!(tab.last_output_time.is_none());
        tab.start_container("amux-test".into(), "Claude Code".into(), 80, 24);
        assert!(tab.last_output_time.is_some());
    }

    #[test]
    fn finish_command_clears_last_output_time() {
        let mut tab = new_tab();
        tab.phase = ExecutionPhase::Running { command: "implement 0001".into() };
        tab.start_container("amux-test".into(), "Claude Code".into(), 80, 24);
        assert!(tab.last_output_time.is_some());

        tab.finish_command(0);
        assert!(tab.last_output_time.is_none());
    }

    #[test]
    fn is_stuck_false_after_finish_command() {
        let mut tab = new_tab();
        tab.phase = ExecutionPhase::Running { command: "implement 0001".into() };
        tab.start_container("amux-test".into(), "Claude Code".into(), 80, 24);
        tab.last_output_time = Some(Instant::now() - (STUCK_TIMEOUT + Duration::from_secs(5)));
        assert!(tab.is_stuck(false));

        tab.finish_command(0);
        assert!(!tab.is_stuck(false));
    }

    // --- Workflow auto-advance (0031) tests ---

    fn new_app() -> App {
        App::new(std::path::PathBuf::new())
    }

    /// Returns an App whose active tab is a running, stuck workflow tab.
    /// `start_container` sets `container_window = Maximized`; adjust after calling
    /// if a specific window state is needed.
    fn setup_stuck_workflow_app() -> App {
        let mut app = new_app();
        let tab = app.active_tab_mut();
        tab.phase = ExecutionPhase::Running { command: "implement 0001".into() };
        tab.start_container("amux-test".into(), "Claude Code".into(), 80, 24);
        tab.workflow_current_step = Some("step-one".to_string());
        // Wind the clock back so the tab is past the stuck threshold.
        tab.last_output_time = Some(Instant::now() - (STUCK_TIMEOUT + Duration::from_secs(1)));
        app
    }

    // --- Unit: threshold constant ---

    #[test]
    fn stuck_timeout_is_10s() {
        assert_eq!(STUCK_TIMEOUT, Duration::from_secs(10));
    }

    // --- Unit: workflow_stuck_dialog_opened field ---

    #[test]
    fn workflow_stuck_dialog_opened_initialises_false() {
        let tab = new_tab();
        assert!(!tab.workflow_stuck_dialog_opened);
    }

    #[test]
    fn finish_command_resets_workflow_stuck_dialog_opened() {
        let mut tab = new_tab();
        tab.phase = ExecutionPhase::Running { command: "implement 0001".into() };
        tab.workflow_stuck_dialog_opened = true;
        tab.finish_command(0);
        assert!(!tab.workflow_stuck_dialog_opened);
    }

    #[test]
    fn acknowledge_stuck_resets_workflow_stuck_dialog_opened() {
        let mut tab = new_tab();
        tab.workflow_stuck_dialog_opened = true;
        tab.acknowledge_stuck();
        assert!(!tab.workflow_stuck_dialog_opened);
    }

    // --- Integration: tick_all auto-open logic ---

    #[test]
    fn tick_all_opens_dialog_for_active_stuck_workflow_tab() {
        let mut app = setup_stuck_workflow_app();
        app.tick_all();
        match &app.active_tab().dialog {
            Dialog::WorkflowControlBoard { current_step, error } => {
                assert_eq!(current_step, "step-one");
                assert_eq!(*error, None);
            }
            other => panic!("expected WorkflowControlBoard, got {:?}", other),
        }
        assert!(app.active_tab().workflow_stuck_dialog_opened);
    }

    #[test]
    fn tick_all_does_not_reopen_dialog_if_flag_set() {
        let mut app = setup_stuck_workflow_app();
        // Simulate: dialog was already auto-opened once and then manually cleared.
        app.active_tab_mut().workflow_stuck_dialog_opened = true;
        app.active_tab_mut().dialog = Dialog::None;
        app.tick_all();
        assert_eq!(app.active_tab().dialog, Dialog::None);
    }

    #[test]
    fn tick_all_does_not_auto_open_for_background_stuck_workflow_tab() {
        let mut app = new_app();
        // Add a second tab and make it (index 1, inactive) a stuck workflow tab.
        app.tabs.push(TabState::new(std::path::PathBuf::new()));
        let tab1 = &mut app.tabs[1];
        tab1.phase = ExecutionPhase::Running { command: "implement 0001".into() };
        tab1.start_container("amux-test".into(), "Claude Code".into(), 80, 24);
        tab1.workflow_current_step = Some("step-one".to_string());
        tab1.last_output_time = Some(Instant::now() - (STUCK_TIMEOUT + Duration::from_secs(1)));
        // active_tab_idx stays 0.
        app.tick_all();
        assert_eq!(app.tabs[1].dialog, Dialog::None);
    }

    #[test]
    fn tick_all_does_not_auto_open_when_different_dialog_active() {
        let mut app = setup_stuck_workflow_app();
        app.active_tab_mut().dialog = Dialog::QuitConfirm;
        app.tick_all();
        assert_eq!(app.active_tab().dialog, Dialog::QuitConfirm);
    }

    #[test]
    fn tick_all_does_not_auto_open_for_stuck_non_workflow_containers() {
        let mut app = new_app();
        let tab = app.active_tab_mut();
        tab.phase = ExecutionPhase::Running { command: "implement 0001".into() };
        tab.start_container("amux-test".into(), "Claude Code".into(), 80, 24);
        tab.last_output_time = Some(Instant::now() - (STUCK_TIMEOUT + Duration::from_secs(1)));
        // workflow_current_step is None by default.
        app.tick_all();
        assert_eq!(app.active_tab().dialog, Dialog::None);
    }

    #[test]
    fn tick_all_auto_opens_dialog_even_when_container_maximized() {
        // setup_stuck_workflow_app already leaves container_window = Maximized
        // (set by start_container), so no extra setup needed.
        let mut app = setup_stuck_workflow_app();
        assert_eq!(app.active_tab().container_window, ContainerWindowState::Maximized);
        app.tick_all();
        assert!(
            matches!(app.active_tab().dialog, Dialog::WorkflowControlBoard { .. }),
            "auto-open must not be suppressed by Maximized container window"
        );
    }

    // --- End-to-end: deferred auto-open on tab switch ---

    #[test]
    fn switching_to_stuck_background_tab_triggers_dialog_on_next_tick() {
        let mut app = new_app();
        // Add a second tab and make it (index 1, inactive) a stuck workflow tab.
        app.tabs.push(TabState::new(std::path::PathBuf::new()));
        let tab1 = &mut app.tabs[1];
        tab1.phase = ExecutionPhase::Running { command: "implement 0001".into() };
        tab1.start_container("amux-test".into(), "Claude Code".into(), 80, 24);
        tab1.workflow_current_step = Some("step-one".to_string());
        tab1.last_output_time = Some(Instant::now() - (STUCK_TIMEOUT + Duration::from_secs(1)));

        // Confirm auto-open is deferred while tab 1 is not active.
        app.tick_all();
        assert_eq!(app.tabs[1].dialog, Dialog::None, "background tab must not auto-open");

        // Simulate switching to tab 1 (set index directly to isolate tick_all logic).
        app.active_tab_idx = 1;

        // On the next tick, tab 1 is now active and stuck → dialog opens.
        app.tick_all();
        assert!(
            matches!(app.active_tab().dialog, Dialog::WorkflowControlBoard { .. }),
            "expected WorkflowControlBoard after switching to stuck background tab"
        );
    }

    // --- Unit: dismiss_stuck_dialog / STUCK_DIALOG_BACKOFF ---

    #[test]
    fn stuck_dialog_backoff_is_60s() {
        assert_eq!(STUCK_DIALOG_BACKOFF, Duration::from_secs(60));
    }

    #[test]
    fn dismiss_stuck_dialog_clears_dialog_and_sets_dismissed_at() {
        let mut tab = new_tab();
        tab.dialog = Dialog::WorkflowControlBoard {
            current_step: "step-one".into(),
            error: None,
        };
        tab.workflow_stuck_dialog_opened = true;
        tab.dismiss_stuck_dialog();
        assert_eq!(tab.dialog, Dialog::None);
        assert!(!tab.workflow_stuck_dialog_opened);
        assert!(tab.workflow_stuck_dialog_dismissed_at.is_some());
    }

    #[test]
    fn finish_command_resets_workflow_stuck_dialog_dismissed_at() {
        let mut tab = new_tab();
        tab.phase = ExecutionPhase::Running { command: "implement 0001".into() };
        tab.workflow_stuck_dialog_dismissed_at = Some(Instant::now());
        tab.finish_command(0);
        assert!(tab.workflow_stuck_dialog_dismissed_at.is_none());
    }

    #[test]
    fn acknowledge_stuck_resets_workflow_stuck_dialog_dismissed_at() {
        let mut tab = new_tab();
        tab.workflow_stuck_dialog_dismissed_at = Some(Instant::now());
        tab.acknowledge_stuck();
        assert!(tab.workflow_stuck_dialog_dismissed_at.is_none());
    }

    #[test]
    fn tick_all_does_not_reopen_dialog_within_backoff_after_esc_dismiss() {
        let mut app = setup_stuck_workflow_app();
        // Simulate: user dismissed with Esc just now.
        app.active_tab_mut().workflow_stuck_dialog_dismissed_at = Some(Instant::now());
        app.tick_all();
        // Dialog must stay closed during backoff window.
        assert_eq!(app.active_tab().dialog, Dialog::None);
    }

    #[test]
    fn tick_all_reopens_dialog_after_backoff_expires() {
        let mut app = setup_stuck_workflow_app();
        // Simulate: user dismissed with Esc STUCK_DIALOG_BACKOFF ago.
        app.active_tab_mut().workflow_stuck_dialog_dismissed_at =
            Some(Instant::now() - STUCK_DIALOG_BACKOFF);
        app.tick_all();
        assert!(
            matches!(app.active_tab().dialog, Dialog::WorkflowControlBoard { .. }),
            "dialog must reopen once the 60 s backoff has elapsed"
        );
    }

    // ─── Yolo countdown tests ─────────────────────────────────────────────────────

    #[test]
    fn yolo_countdown_duration_constant_is_60s() {
        assert_eq!(YOLO_COUNTDOWN_DURATION, Duration::from_secs(60));
    }

    fn setup_yolo_stuck_workflow_app() -> App {
        let mut app = new_app();
        let tab = app.active_tab_mut();
        tab.phase = ExecutionPhase::Running { command: "implement 0001".into() };
        tab.start_container("amux-test".into(), "Claude Code".into(), 80, 24);
        tab.workflow_current_step = Some("step-one".to_string());
        tab.yolo_mode = true;
        tab.last_output_time = Some(Instant::now() - (STUCK_TIMEOUT + Duration::from_secs(1)));
        app
    }

    #[test]
    fn tick_all_yolo_opens_countdown_dialog_when_stuck() {
        let mut app = setup_yolo_stuck_workflow_app();
        app.tick_all();
        assert!(
            matches!(app.active_tab().dialog, Dialog::WorkflowYoloCountdown { .. }),
            "expected WorkflowYoloCountdown, got {:?}",
            app.active_tab().dialog
        );
        assert!(app.active_tab().workflow_stuck_dialog_opened);
    }

    #[test]
    fn tick_all_yolo_does_not_open_control_board() {
        let mut app = setup_yolo_stuck_workflow_app();
        app.tick_all();
        assert!(
            !matches!(app.active_tab().dialog, Dialog::WorkflowControlBoard { .. }),
            "yolo mode must never open WorkflowControlBoard"
        );
    }

    #[test]
    fn tick_all_yolo_sets_expired_flag_after_countdown() {
        let mut app = setup_yolo_stuck_workflow_app();
        // Place an already-expired countdown (set the authoritative timer back in time).
        app.active_tab_mut().yolo_countdown_started_at =
            Some(Instant::now() - YOLO_COUNTDOWN_DURATION);
        app.active_tab_mut().dialog = Dialog::WorkflowYoloCountdown {
            current_step: "step-one".to_string(),
        };
        app.active_tab_mut().workflow_stuck_dialog_opened = true;
        app.tick_all();
        assert!(
            app.active_tab().yolo_countdown_expired,
            "yolo_countdown_expired must be set when the countdown elapses"
        );
        assert_eq!(
            app.active_tab().dialog,
            Dialog::None,
            "countdown dialog must be closed after expiry"
        );
        assert!(!app.active_tab().workflow_stuck_dialog_opened);
    }

    #[test]
    fn finish_command_closes_yolo_countdown_dialog() {
        let mut tab = new_tab();
        tab.phase = ExecutionPhase::Running { command: "implement 0001".into() };
        tab.start_container("amux-test".into(), "Claude Code".into(), 80, 24);
        tab.yolo_countdown_started_at = Some(Instant::now());
        tab.dialog = Dialog::WorkflowYoloCountdown {
            current_step: "step-one".to_string(),
        };
        tab.finish_command(0);
        assert_eq!(
            tab.dialog,
            Dialog::None,
            "finish_command must close the yolo countdown dialog"
        );
    }

    #[test]
    fn tick_pty_output_closes_yolo_countdown_dialog() {
        use std::sync::mpsc;
        use crate::tui::pty::PtyEvent;

        let mut tab = new_tab();
        tab.phase = ExecutionPhase::Running { command: "implement 0001".into() };
        tab.start_container("amux-test".into(), "Claude Code".into(), 80, 24);
        tab.yolo_countdown_started_at = Some(Instant::now());
        tab.dialog = Dialog::WorkflowYoloCountdown {
            current_step: "step-one".to_string(),
        };
        tab.workflow_stuck_dialog_opened = true;

        // Wire a fake PTY channel and send one byte of data.
        let (tx, rx) = mpsc::channel::<PtyEvent>();
        tab.pty_rx = Some(rx);
        tx.send(PtyEvent::Data(b"x".to_vec())).unwrap();

        tab.tick();

        assert_eq!(
            tab.dialog,
            Dialog::None,
            "any PTY byte must close the yolo countdown dialog"
        );
        assert!(
            !tab.workflow_stuck_dialog_opened,
            "workflow_stuck_dialog_opened must be cleared when countdown is cancelled"
        );
    }

    // ─── auto_workflow_disabled_for_step tests ────────────────────────────────────

    #[test]
    fn auto_workflow_disabled_suppresses_control_board_auto_open() {
        let mut app = setup_stuck_workflow_app();
        app.active_tab_mut().auto_workflow_disabled_for_step = true;
        app.tick_all();
        assert_eq!(
            app.active_tab().dialog,
            Dialog::None,
            "auto_workflow_disabled_for_step must suppress auto-open of WorkflowControlBoard"
        );
    }

    #[test]
    fn finish_command_resets_auto_workflow_disabled_for_step() {
        let mut tab = new_tab();
        tab.phase = ExecutionPhase::Running { command: "implement 0001".into() };
        tab.auto_workflow_disabled_for_step = true;
        tab.finish_command(0);
        assert!(
            !tab.auto_workflow_disabled_for_step,
            "finish_command must reset auto_workflow_disabled_for_step"
        );
    }

    #[test]
    fn yolo_countdown_opens_even_when_auto_workflow_disabled() {
        // auto_workflow_disabled_for_step only affects the non-yolo code path;
        // the yolo countdown must still open regardless.
        let mut app = setup_yolo_stuck_workflow_app();
        app.active_tab_mut().auto_workflow_disabled_for_step = true;
        app.tick_all();
        assert!(
            matches!(app.active_tab().dialog, Dialog::WorkflowYoloCountdown { .. }),
            "yolo countdown must open even when auto_workflow_disabled_for_step is set"
        );
    }

    // ─── User activity suppression (0048) unit tests ──────────────────────────

    #[test]
    fn is_stuck_active_suppressed_when_user_recently_active() {
        let mut tab = new_tab();
        tab.phase = ExecutionPhase::Running { command: "implement 0001".into() };
        tab.start_container("amux-test".into(), "Claude Code".into(), 80, 24);
        // Output clock is past the stuck threshold.
        tab.last_output_time = Some(Instant::now() - (STUCK_TIMEOUT + Duration::from_secs(1)));
        // Record very recent user activity.
        tab.last_user_activity_time = Some(Instant::now());
        // Active-tab check must return false despite stale output.
        assert!(
            !tab.is_stuck(true),
            "is_stuck(true) must return false when user just interacted, even if output is stale"
        );
    }

    #[test]
    fn is_stuck_background_ignores_user_activity() {
        let mut tab = new_tab();
        tab.phase = ExecutionPhase::Running { command: "implement 0001".into() };
        tab.start_container("amux-test".into(), "Claude Code".into(), 80, 24);
        tab.last_output_time = Some(Instant::now() - (STUCK_TIMEOUT + Duration::from_secs(1)));
        // Very recent user activity — should be ignored for background tabs.
        tab.last_user_activity_time = Some(Instant::now());
        assert!(
            tab.is_stuck(false),
            "is_stuck(false) must return true based only on last_output_time, ignoring user activity"
        );
    }

    #[test]
    fn record_user_activity_sets_last_user_activity_time() {
        let mut tab = new_tab();
        assert!(tab.last_user_activity_time.is_none());
        tab.record_user_activity();
        let activity = tab.last_user_activity_time.expect("last_user_activity_time must be Some after record_user_activity");
        assert!(
            activity.elapsed() < Duration::from_secs(1),
            "last_user_activity_time must be recent"
        );
    }

    #[test]
    fn record_user_activity_does_not_affect_last_output_time() {
        let mut tab = new_tab();
        tab.phase = ExecutionPhase::Running { command: "implement 0001".into() };
        tab.start_container("amux-test".into(), "Claude Code".into(), 80, 24);
        let output_before = tab.last_output_time.expect("start_container sets last_output_time");
        tab.record_user_activity();
        assert_eq!(
            tab.last_output_time.unwrap(),
            output_before,
            "record_user_activity must not change last_output_time"
        );
    }

    // ─── Background yolo color and label (0048) unit tests ────────────────────

    #[test]
    fn background_yolo_color_none_when_countdown_not_started() {
        let tab = new_tab();
        assert!(
            tab.background_yolo_color().is_none(),
            "background_yolo_color must return None when yolo_countdown_started_at is not set"
        );
    }

    #[test]
    fn background_yolo_color_yellow_for_even_elapsed_seconds() {
        let mut tab = new_tab();
        // 2 elapsed seconds → even → Color::Yellow
        tab.yolo_countdown_started_at = Some(Instant::now() - Duration::from_secs(2));
        assert_eq!(
            tab.background_yolo_color(),
            Some(Color::Yellow),
            "background_yolo_color must return Yellow for even elapsed seconds"
        );
    }

    #[test]
    fn background_yolo_color_magenta_for_odd_elapsed_seconds() {
        let mut tab = new_tab();
        // 3 elapsed seconds → odd → Color::Magenta
        tab.yolo_countdown_started_at = Some(Instant::now() - Duration::from_secs(3));
        assert_eq!(
            tab.background_yolo_color(),
            Some(Color::Magenta),
            "background_yolo_color must return Magenta for odd elapsed seconds"
        );
    }

    #[test]
    fn background_yolo_label_none_when_countdown_not_started() {
        let tab = new_tab();
        assert!(
            tab.background_yolo_label(30).is_none(),
            "background_yolo_label must return None when yolo_countdown_started_at is not set"
        );
    }

    #[test]
    fn background_yolo_label_even_seconds_shows_warning_emoji_and_countdown() {
        let mut tab = new_tab();
        // 2 elapsed seconds → even phase → warning emoji; 58 seconds remaining.
        tab.yolo_countdown_started_at = Some(Instant::now() - Duration::from_secs(2));
        let label = tab.background_yolo_label(50).unwrap();
        assert!(label.contains('⚠'), "expected ⚠ emoji for even seconds, got: {:?}", label);
        assert!(label.contains("yolo in"), "expected 'yolo in' text, got: {:?}", label);
        // Allow 1s of timing slack: 57 or 58 seconds remaining.
        assert!(
            label.contains("58") || label.contains("57"),
            "expected ~58 s remaining in label, got: {:?}",
            label
        );
    }

    #[test]
    fn background_yolo_label_odd_seconds_shows_rock_emoji_and_countdown() {
        let mut tab = new_tab();
        // 3 elapsed seconds → odd phase → rock emoji; 57 seconds remaining.
        tab.yolo_countdown_started_at = Some(Instant::now() - Duration::from_secs(3));
        let label = tab.background_yolo_label(50).unwrap();
        assert!(label.contains('🤘'), "expected 🤘 emoji for odd seconds, got: {:?}", label);
        assert!(label.contains("yolo in"), "expected 'yolo in' text, got: {:?}", label);
        // Allow 1s of timing slack: 56 or 57 seconds remaining.
        assert!(
            label.contains("57") || label.contains("56"),
            "expected ~57 s remaining in label, got: {:?}",
            label
        );
    }

    #[test]
    fn background_yolo_label_countdown_value_decreases_with_elapsed_time() {
        let mut tab = new_tab();
        // 10 elapsed seconds → 50 seconds remaining.
        tab.yolo_countdown_started_at = Some(Instant::now() - Duration::from_secs(10));
        let label = tab.background_yolo_label(50).unwrap();
        // Allow 1s of timing slack: 49 or 50 seconds remaining.
        assert!(
            label.contains("50") || label.contains("49"),
            "expected ~50 s remaining after 10 s elapsed, got: {:?}",
            label
        );
    }

    // ─── tick_all background yolo countdown (0048) unit tests ─────────────────

    fn setup_background_yolo_tab_app() -> App {
        let mut app = new_app();
        app.tabs.push(TabState::new(std::path::PathBuf::new()));
        let tab1 = &mut app.tabs[1];
        tab1.phase = ExecutionPhase::Running { command: "implement 0001".into() };
        tab1.start_container("amux-test".into(), "Claude Code".into(), 80, 24);
        tab1.workflow_current_step = Some("step-one".to_string());
        tab1.yolo_mode = true;
        tab1.last_output_time =
            Some(Instant::now() - (STUCK_TIMEOUT + Duration::from_secs(1)));
        // active_tab_idx stays 0 → tab 1 is a background tab.
        app
    }

    #[test]
    fn tick_all_yolo_sets_countdown_started_at_for_background_tab() {
        let mut app = setup_background_yolo_tab_app();
        assert!(app.tabs[1].yolo_countdown_started_at.is_none());
        app.tick_all();
        assert!(
            app.tabs[1].yolo_countdown_started_at.is_some(),
            "tick_all must set yolo_countdown_started_at for a background stuck yolo tab"
        );
    }

    #[test]
    fn tick_all_yolo_does_not_reset_countdown_started_at_on_subsequent_ticks() {
        let mut app = setup_background_yolo_tab_app();
        app.tick_all();
        let first_start = app.tabs[1].yolo_countdown_started_at.unwrap();
        app.tick_all();
        let second_start = app.tabs[1].yolo_countdown_started_at.unwrap();
        assert_eq!(
            first_start, second_start,
            "yolo_countdown_started_at must not be reset on subsequent ticks while still stuck"
        );
    }

    #[test]
    fn tick_all_yolo_does_not_open_dialog_for_background_tab() {
        let mut app = setup_background_yolo_tab_app();
        app.tick_all();
        assert_eq!(
            app.tabs[1].dialog,
            Dialog::None,
            "tick_all must not open a dialog for a background yolo tab; the tab bar handles feedback"
        );
    }

    #[test]
    fn tick_all_yolo_sets_expired_and_clears_timer_for_background_tab() {
        let mut app = setup_background_yolo_tab_app();
        // Pre-set an already-expired countdown on the background tab.
        app.tabs[1].yolo_countdown_started_at =
            Some(Instant::now() - YOLO_COUNTDOWN_DURATION);
        app.tick_all();
        assert!(
            app.tabs[1].yolo_countdown_expired,
            "yolo_countdown_expired must be set when the countdown elapses for a background tab"
        );
        assert!(
            app.tabs[1].yolo_countdown_started_at.is_none(),
            "yolo_countdown_started_at must be cleared after countdown expires"
        );
        assert_eq!(
            app.tabs[1].dialog,
            Dialog::None,
            "no dialog must be opened for the expired background tab"
        );
    }

    #[test]
    fn tick_all_yolo_does_not_restart_timer_while_expiry_is_pending() {
        // After a background-tab countdown expires, tick_all must NOT restart the timer
        // while yolo_countdown_expired is still true (i.e. before the event loop has
        // consumed the flag and actually advanced the workflow).
        let mut app = setup_background_yolo_tab_app();
        // Simulate an already-expired countdown.
        app.tabs[1].yolo_countdown_started_at =
            Some(Instant::now() - YOLO_COUNTDOWN_DURATION);
        app.tick_all(); // sets expired=true, clears started_at
        assert!(app.tabs[1].yolo_countdown_expired);
        assert!(app.tabs[1].yolo_countdown_started_at.is_none());

        // Another tick while expired flag is still set — timer must NOT restart.
        app.tick_all();
        assert!(
            app.tabs[1].yolo_countdown_started_at.is_none(),
            "yolo_countdown_started_at must not be restarted while yolo_countdown_expired is pending"
        );
    }

    #[test]
    fn tick_all_clears_countdown_when_background_tab_no_longer_stuck() {
        let mut app = setup_background_yolo_tab_app();
        app.tick_all();
        assert!(app.tabs[1].yolo_countdown_started_at.is_some());
        // Simulate new output arriving — tab is no longer stuck.
        app.tabs[1].last_output_time = Some(Instant::now());
        app.tick_all();
        assert!(
            app.tabs[1].yolo_countdown_started_at.is_none(),
            "yolo_countdown_started_at must be cleared when the tab is no longer stuck"
        );
    }

    // ─── next_step_different_agent tests (work item 0052) ────────────────────

    /// Build a two-step WorkflowState (a → b) from scratch.
    fn make_two_step_workflow() -> crate::workflow::WorkflowState {
        let steps = vec![
            crate::workflow::parser::WorkflowStep {
                name: "a".to_string(),
                depends_on: vec![],
                prompt_template: "Step A".to_string(),
                agent: None,
                model: None,
            },
            crate::workflow::parser::WorkflowStep {
                name: "b".to_string(),
                depends_on: vec!["a".to_string()],
                prompt_template: "Step B".to_string(),
                agent: None,
                model: None,
            },
        ];
        crate::workflow::WorkflowState::new(None, steps, "hash".into(), Some(1), "wf".into())
    }

    #[test]
    fn next_step_different_agent_returns_none_when_same_agent() {
        let mut tab = new_tab();
        let wf = make_two_step_workflow();
        tab.workflow = Some(wf);
        tab.workflow_current_step = Some("a".to_string());
        // Both steps use the same agent.
        tab.workflow_step_agents.insert("a".to_string(), "claude".to_string());
        tab.workflow_step_agents.insert("b".to_string(), "claude".to_string());

        let result = tab.next_step_different_agent();
        assert!(
            result.is_none(),
            "next_step_different_agent must return None when both steps use the same agent; got: {:?}",
            result
        );
    }

    #[test]
    fn next_step_different_agent_returns_next_agent_when_different() {
        let mut tab = new_tab();
        let wf = make_two_step_workflow();
        tab.workflow = Some(wf);
        tab.workflow_current_step = Some("a".to_string());
        // Steps use different agents.
        tab.workflow_step_agents.insert("a".to_string(), "claude".to_string());
        tab.workflow_step_agents.insert("b".to_string(), "codex".to_string());

        let result = tab.next_step_different_agent();
        assert_eq!(
            result,
            Some("codex".to_string()),
            "next_step_different_agent must return the next step's agent when it differs"
        );
    }

    #[test]
    fn next_step_different_agent_returns_none_when_no_next_step() {
        let mut tab = new_tab();
        // Single-step workflow — no next step after "solo".
        let steps = vec![crate::workflow::parser::WorkflowStep {
            name: "solo".to_string(),
            depends_on: vec![],
            prompt_template: "Only step".to_string(),
            agent: None,
            model: None,
        }];
        let wf = crate::workflow::WorkflowState::new(None, steps, "hash".into(), Some(1), "wf".into());
        tab.workflow = Some(wf);
        tab.workflow_current_step = Some("solo".to_string());
        tab.workflow_step_agents.insert("solo".to_string(), "claude".to_string());

        let result = tab.next_step_different_agent();
        assert!(
            result.is_none(),
            "next_step_different_agent must return None when there is no next step"
        );
    }

    // ─── RemoteTabBinding and display_host helpers (work item 0061) ──────────

    #[test]
    fn remote_tab_binding_new_extracts_http_host_port() {
        let binding = RemoteTabBinding::new(
            "http://192.168.1.100:9876".to_string(),
            "sess-abc".to_string(),
            None,
        );
        assert_eq!(
            binding.display_host, "192.168.1.100:9876",
            "display_host must be host:port without scheme"
        );
    }

    #[test]
    fn remote_tab_binding_new_extracts_https_host_port() {
        let binding = RemoteTabBinding::new(
            "https://amux.example.com:443".to_string(),
            "sess-xyz".to_string(),
            Some("key123".to_string()),
        );
        assert_eq!(binding.display_host, "amux.example.com:443");
    }

    #[test]
    fn remote_tab_binding_new_stores_all_fields() {
        let binding = RemoteTabBinding::new(
            "http://10.0.0.1:8080".to_string(),
            "my-session".to_string(),
            Some("api-key-value".to_string()),
        );
        assert_eq!(binding.remote_addr, "http://10.0.0.1:8080");
        assert_eq!(binding.session_id, "my-session");
        assert_eq!(binding.api_key.as_deref(), Some("api-key-value"));
        assert_eq!(binding.display_host, "10.0.0.1:8080");
    }

    #[test]
    fn extract_display_host_strips_http_scheme() {
        assert_eq!(extract_display_host("http://host.example:9000"), "host.example:9000");
    }

    #[test]
    fn extract_display_host_strips_https_scheme() {
        assert_eq!(extract_display_host("https://secure.example:443"), "secure.example:443");
    }

    #[test]
    fn extract_display_host_strips_trailing_path() {
        assert_eq!(extract_display_host("http://host:9876/some/path"), "host:9876");
    }

    #[test]
    fn extract_display_host_no_scheme_returns_host_port_as_is() {
        assert_eq!(extract_display_host("host:9876"), "host:9876");
    }

    // ─── tab_color and tab_project_name for remote-bound tabs ────────────────

    #[test]
    fn tab_color_is_magenta_for_remote_bound_idle_tab() {
        let mut tab = new_tab();
        tab.remote_binding = Some(RemoteTabBinding {
            remote_addr: "http://1.2.3.4:9876".to_string(),
            session_id: "s1".to_string(),
            api_key: None,
            display_host: "1.2.3.4:9876".to_string(),
        });
        tab.phase = ExecutionPhase::Idle;
        assert_eq!(
            tab.tab_color(true),
            Color::Magenta,
            "remote-bound tab must be Magenta (active, idle)"
        );
        assert_eq!(
            tab.tab_color(false),
            Color::Magenta,
            "remote-bound tab must be Magenta (inactive, idle)"
        );
    }

    #[test]
    fn tab_color_is_magenta_for_remote_bound_running_tab() {
        let mut tab = new_tab();
        tab.remote_binding = Some(RemoteTabBinding {
            remote_addr: "http://1.2.3.4:9876".to_string(),
            session_id: "s1".to_string(),
            api_key: None,
            display_host: "1.2.3.4:9876".to_string(),
        });
        tab.phase = ExecutionPhase::Running { command: "implement 0001".to_string() };
        assert_eq!(
            tab.tab_color(true),
            Color::Magenta,
            "remote-bound tab must remain Magenta when running"
        );
    }

    #[test]
    fn tab_project_name_returns_display_host_for_remote_bound_tab() {
        let mut tab = new_tab();
        tab.remote_binding = Some(RemoteTabBinding {
            remote_addr: "http://10.0.0.5:8080".to_string(),
            session_id: "s2".to_string(),
            api_key: None,
            display_host: "10.0.0.5:8080".to_string(),
        });
        assert_eq!(
            tab.tab_project_name(),
            "10.0.0.5:8080",
            "tab project name must show display_host for remote-bound tabs"
        );
    }

    #[test]
    fn tab_project_name_truncates_long_remote_display_host() {
        let mut tab = new_tab();
        // display_host longer than 14 chars must be truncated with ellipsis.
        tab.remote_binding = Some(RemoteTabBinding {
            remote_addr: "http://very-long-hostname.example.com:9876".to_string(),
            session_id: "s3".to_string(),
            api_key: None,
            display_host: "very-long-hostname.example.com:9876".to_string(),
        });
        let name = tab.tab_project_name();
        assert!(
            name.chars().count() <= 14,
            "tab project name must be truncated to 14 chars; got: '{name}' ({} chars)",
            name.chars().count()
        );
        assert!(name.ends_with('…'), "truncated name must end with ellipsis; got: '{name}'");
    }

    // ─── tick() drains remote_sessions_fetch_rx and remote_workflow_rx ────────

    #[tokio::test]
    async fn tick_updates_dialog_with_fetched_remote_sessions_ok() {
        let mut tab = new_tab();
        tab.dialog = Dialog::NewTabDirectory {
            input: String::new(),
            remote_sessions: None,
            remote_selected_idx: None,
            focus_workdir: true,
        };

        let (tx, rx) = tokio::sync::oneshot::channel::<
            Result<Vec<crate::commands::remote::RemoteSessionEntry>, String>,
        >();
        tab.remote_sessions_fetch_rx = Some(rx);

        let sessions = vec![crate::commands::remote::RemoteSessionEntry {
            id: "abc12345".to_string(),
            workdir: "/workspace/myproj".to_string(),
        }];
        tx.send(Ok(sessions)).unwrap();

        tab.tick();

        match &tab.dialog {
            Dialog::NewTabDirectory { remote_sessions: Some(Ok(got)), .. } => {
                assert_eq!(got.len(), 1, "must have one session");
                assert_eq!(got[0].id, "abc12345");
                assert_eq!(got[0].workdir, "/workspace/myproj");
            }
            other => panic!(
                "expected NewTabDirectory with Ok sessions after tick; got: {:?}",
                other
            ),
        }
        assert!(
            tab.remote_sessions_fetch_rx.is_none(),
            "receiver must be cleared after receiving the result"
        );
    }

    #[tokio::test]
    async fn tick_updates_dialog_with_fetched_remote_sessions_err() {
        let mut tab = new_tab();
        tab.dialog = Dialog::NewTabDirectory {
            input: String::new(),
            remote_sessions: None,
            remote_selected_idx: None,
            focus_workdir: true,
        };

        let (tx, rx) = tokio::sync::oneshot::channel::<
            Result<Vec<crate::commands::remote::RemoteSessionEntry>, String>,
        >();
        tab.remote_sessions_fetch_rx = Some(rx);
        tx.send(Err("connection refused".to_string())).unwrap();

        tab.tick();

        match &tab.dialog {
            Dialog::NewTabDirectory { remote_sessions: Some(Err(msg)), .. } => {
                assert_eq!(
                    msg, "connection refused",
                    "error message must match"
                );
            }
            other => panic!(
                "expected NewTabDirectory with Err after tick; got: {:?}",
                other
            ),
        }
    }

    #[tokio::test]
    async fn tick_updates_workflow_from_remote_workflow_channel() {
        let mut tab = new_tab();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<WorkflowState>();
        tab.remote_workflow_rx = Some(rx);

        let wf = WorkflowState::new(
            None,
            vec![crate::workflow::parser::WorkflowStep {
                name: "poll-step".to_string(),
                depends_on: vec![],
                prompt_template: "check".to_string(),
                agent: None,
                model: None,
            }],
            "pollhash42".to_string(),
            None,
            "poll-wf".to_string(),
        );
        tx.send(wf.clone()).unwrap();

        tab.tick();

        assert!(
            tab.workflow.is_some(),
            "workflow must be set after receiving from remote_workflow_rx"
        );
        assert_eq!(
            tab.workflow.as_ref().unwrap().workflow_name,
            "poll-wf",
            "workflow_name must match the sent state"
        );
        assert!(
            tab.remote_workflow_rx.is_some(),
            "receiver must NOT be dropped while workflow is non-terminal"
        );
    }

    #[tokio::test]
    async fn tick_drops_remote_workflow_rx_when_state_is_terminal() {
        let mut tab = new_tab();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<WorkflowState>();
        tab.remote_workflow_rx = Some(rx);

        // Build a terminal workflow state (all steps Done).
        let mut wf = WorkflowState::new(
            None,
            vec![crate::workflow::parser::WorkflowStep {
                name: "terminal-step".to_string(),
                depends_on: vec![],
                prompt_template: "finish".to_string(),
                agent: None,
                model: None,
            }],
            "termhash".to_string(),
            None,
            "term-wf".to_string(),
        );
        wf.set_status("terminal-step", StepStatus::Done);
        assert!(wf.is_terminal(), "workflow must be terminal for this test");

        tx.send(wf).unwrap();
        tab.tick();

        assert!(
            tab.remote_workflow_rx.is_none(),
            "receiver must be dropped when workflow reaches a terminal state"
        );
        assert!(
            tab.workflow.is_some(),
            "workflow field must still hold the terminal state"
        );
    }
}
