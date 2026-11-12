//! `InstanceCommandFrontend`: the `CommandFrontend` + per-command frontend
//! implementation that runs on a tab's command thread and produces the feeds in
//! [`super::feeds`] (dialog requests, container slot events, workflow
//! presentation, status lines). It is the former
//! `frontend::tui::command_frontend::TuiCommandFrontend`, moved to Layer 2 so
//! the builtin TUI and the daemon host share one producer (WI 0142 D2; the TUI
//! re-exports it under its old name). Per-command trait impls live in
//! [`super::per_command`].
//!
//! Constructed from a `ParsedCommandBoxInput` (produced by
//! `Dispatch::parse_command_box_input`). Flag/argument extraction delegates to
//! the input's typed `ParsedArgs`; Layer 2 has already validated every value.
//! Interactive Q&A methods send a typed `DialogRequest` down the dialog channel
//! and block until the presenting frontend answers.
//!
//! The few things the producer needs from a presenter — the wording/glyph
//! helpers that Layer 3 owns, the host-output byte filter, and the agent
//! window's PTY size — come through [`InstancePresentation`], supplied by the
//! frontend that builds the instance. Nothing here draws.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::command::dispatch::catalogue::FrontendKind;
use crate::command::dispatch::parsed_input::ParsedCommandBoxInput;
use crate::command::dispatch::CommandFrontend;
use crate::command::error::CommandError;
use crate::command::instance::feeds::{
    ContainerLifecycle, SharedAgentsDashboard, SharedTuiContext, SharedWorkflowPresentation,
    SharedYoloCancelFlag, SharedYoloState, TabCommandFeeds,
};
use crate::command::instance::feeds::{SharedStatusLog, StatusLogSink};
use crate::data::dialog::{DialogRequest, DialogResponse};
use crate::data::message::{UserMessage, UserMessageSink};
use crate::engine::agent_runtime::frontend::AgentIo;

/// TUI frontend struct. Implements every per-command frontend trait.
///
/// Dialog channels use `std::sync::mpsc` so that the blocking `recv()` in
/// `ask_dialog` parks the OS thread rather than stalling a tokio worker —
/// the engine trait methods are synchronous, so this is the correct
/// blocking strategy.
///
/// Container I/O channels (stdout/stdin/resize) are bundled into a
/// `AgentIo` and detached lazily by the engine via `take_io`.
/// The TUI populates these channels from `App::spawn_command`; the engine's
/// container backend drains them against a real PTY master.
pub struct InstanceCommandFrontend {
    pub(crate) worker_events: Option<crate::command::worker::events::ForegroundRecorder>,
    pub(crate) command_bytes: super::output::CommandBytes,
    input_available: bool,
    pub(crate) wake: Option<Arc<tokio::sync::Notify>>,

    pub(crate) attach_exit_code: std::sync::Arc<std::sync::atomic::AtomicI32>,
    pub(crate) attach_hangup: tokio_util::sync::CancellationToken,
    parsed: ParsedCommandBoxInput,
    pub(crate) messages: StatusLogSink,
    pub(crate) pty_active: bool,
    pub(crate) dialog_tx: std::sync::mpsc::Sender<DialogRequest>,
    pub(crate) dialog_rx: Mutex<std::sync::mpsc::Receiver<DialogResponse>>,
    pub(crate) container_io: Option<AgentIo>,
    pub(crate) status_log: SharedStatusLog,
    pub(crate) workflow: SharedWorkflowPresentation,
    pub(crate) container: ContainerLifecycle,
    pub(crate) yolo_state: SharedYoloState,
    pub(crate) yolo_cancel_flag: SharedYoloCancelFlag,
    /// Why the running sequential countdown exists (dialog wording).
    pub(crate) countdown_kind: crate::engine::workflow::actions::CountdownKind,
    /// Output sender for the current sequential launch only.
    pub(crate) stdout_tx: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    /// The `agents status` dashboard slot. The command writes each frame
    /// here; the TUI renderer reads it to display a `Table` widget.
    pub(crate) agents_dashboard: SharedAgentsDashboard,
    /// Live TUI context shared with the event loop. The event loop refreshes
    /// this on every tick; `agents status --watch` reads it each iteration.
    pub(crate) tui_context_shared: SharedTuiContext,
    /// Active-tab suppression inputs (last user input, latest agent status).
    pub(crate) signals: crate::command::instance::feeds::AgentSignals,
    /// The tab's command-cancel token: ends an `agents status --watch` loop
    /// or an `agents attach` session (new command, tab closed, Esc).
    pub(crate) command_cancel: tokio_util::sync::CancellationToken,
    pub(crate) squad_attach_session:
        Option<std::sync::Arc<crate::command::instance::feeds::SquadAttachSession>>,
    pub(crate) squad_attach_reachable: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    pub(crate) squad_attach_metadata: HashMap<String, (String, Option<String>)>,
    pub(crate) parallel_group_active: bool,
    pub(crate) pending_step_slot_io:
        HashMap<String, crate::command::instance::feeds::ContainerSlotIo>,
    /// Per-step cancel flags for in-flight parallel yolo countdowns, keyed by
    /// step name. Created in `parallel_step_yolo_countdown_started`, shared
    /// with the slot via `ContainerSlotEvent::YoloStarted` so the TUI event
    /// loop can request cancellation (Esc on the per-slot countdown modal);
    /// checked and removed in `parallel_step_yolo_countdown_tick` /
    /// `_finished`.
    pub(crate) pending_parallel_yolo_cancel: HashMap<String, SharedYoloCancelFlag>,
    /// Field name of the most recent config-dialog edit, so the re-presented
    /// table reopens with that row selected (the `config show` edit loop
    /// presents the dialog again after every save).
    pub(crate) last_config_edit_field: Option<String>,
    /// The window title for the setup/teardown step now running, set when it
    /// starts. A step on the filtered background stream (one holding a token
    /// grant) has no PTY, so its redacted output lines are written into the
    /// container window under this title instead of the status log.
    pub(crate) phase_stream_title: Option<String>,
    /// Whether the current phase step's filtered output has readied the
    /// container window; cleared when the step ends or a PTY launch takes the
    /// window over.
    pub(crate) phase_stream_open: bool,
    pub(crate) host_output_policy: crate::data::terminal::HostOutputPolicy,
    /// Presenter-supplied wording, byte filtering and PTY sizing.
    pub(crate) presentation: Arc<dyn InstancePresentation>,
    /// The tab's agent-window PTY size as its presenter last reported it
    /// (D8). Every launch this command makes — the first, a retry, a
    /// parallel peer, an attach — starts at this size.
    pub(crate) pty_size: crate::command::instance::feeds::SharedPtySize,
    /// Which frontend this command runs for (`Tui` for the builtin TUI).
    kind: FrontendKind,
    /// Set when an engine instance hosts the command (WI 0142): host
    /// processes that need a terminal get a daemon PTY exposed as a hub
    /// terminal instead of the process TTY.
    pub(crate) host_terminals: Option<HostTerminals>,
}

/// Where a hosted command opens daemon PTYs for host processes
/// (`DialogRequest::TerminalRelease { terminal_id: Some(..) }`): the hub and
/// the owner the terminal is registered under (the command's tab).
#[derive(Clone)]
pub struct HostTerminals {
    pub hub: Arc<crate::engine::terminal_hub::TerminalHub>,
    pub owner: crate::data::terminal::TerminalOwnerInfo,
}

// `SquadCommandFrontend` for `InstanceCommandFrontend` (interview dialogs) lives in
// `per_command::squad`, alongside the other per-command TUI frontend impls.

/// The two ends of a command's dialog conversation: requests out to the TUI
/// event loop, responses back from it. They are created as a pair and are
/// useless apart, so they are passed as one value.
pub struct DialogChannels {
    pub tx: std::sync::mpsc::Sender<DialogRequest>,
    pub rx: std::sync::mpsc::Receiver<DialogResponse>,
}

impl DialogChannels {
    pub fn new(
        tx: std::sync::mpsc::Sender<DialogRequest>,
        rx: std::sync::mpsc::Receiver<DialogResponse>,
    ) -> Self {
        Self { tx, rx }
    }
}

impl InstanceCommandFrontend {
    /// Active-tab suppression for an agent's `idle`/`done` report: hold back
    /// the dialog or countdown while the user has typed or scrolled on this
    /// tab within `agents.stuckTimeout`, so an interactive step does not
    /// pop a dialog on every turn. The engine has already decided what a
    /// report means; this only delays showing it. `blocked`/`error` never
    /// come through here. Gives up early if the agent resumes (a `working`,
    /// `blocked` or `error` report) or the command is cancelled.
    pub(crate) fn wait_for_quiet_user(&self, step_name: &str) {
        while self.should_defer_for_step(step_name) {
            if self.command_cancel.is_cancelled() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
    }

    /// Hold a done/idle action only for recent activity in that step's slot.
    /// The tab's aggregate status can belong to a different parallel agent.
    pub(crate) fn should_defer_for_step(&self, step_name: &str) -> bool {
        use crate::data::program_status::ProgramState;
        let status = self
            .signals
            .step_status(step_name)
            .unwrap_or_else(|| self.signals.status());
        self.signals.user_active_within_step(
            crate::engine::container::timing::DEFAULT_STUCK_TIMEOUT,
            step_name,
        ) && (!self.parallel_group_active
            || self.signals.last_user_step().as_deref() == Some(step_name))
            && status.is_some_and(|s| matches!(s.state, ProgramState::Done | ProgramState::Idle))
    }

    pub(crate) fn squad_attach_has_explicit_target(&self) -> bool {
        // squad attach now resolves the task's hub terminal; the legacy
        // container-selection flag is no longer part of its catalogue.
        false
    }

    /// Build the frontend for one command run in one tab.
    ///
    /// `shared` carries lifecycle owners intact into the command frontend.
    pub fn new(
        parsed: ParsedCommandBoxInput,
        dialogs: DialogChannels,
        container_io: AgentIo,
        shared: TabCommandFeeds,
    ) -> Self {
        let TabCommandFeeds {
            command_bytes,
            workflow,
            container,
            yolo_state,
            yolo_cancel_flag,
            status_log,
            agents_dashboard,
            tui_context_shared,
            signals,
            command_cancel,
        } = shared;
        let DialogChannels {
            tx: dialog_tx,
            rx: dialog_rx,
        } = dialogs;
        let stdout_tx = container_io.stdout.clone();
        // Later launches start at the size the first launch was given until
        // the presenter reports another (`with_pty_size` shares the tab's).
        let pty_size = crate::command::instance::feeds::SharedPtySize::new(
            container_io
                .initial_size
                .unwrap_or(crate::command::instance::feeds::SharedPtySize::DEFAULT),
        );
        Self {
            command_bytes,
            input_available: true,
            wake: None,

            parsed,
            messages: StatusLogSink::new(status_log.clone()),
            pty_active: false,
            dialog_tx,
            dialog_rx: Mutex::new(dialog_rx),
            worker_events: None,
            container_io: Some(container_io),
            status_log,
            workflow,
            container,
            yolo_state,
            yolo_cancel_flag,
            countdown_kind: Default::default(),
            stdout_tx,
            agents_dashboard,
            tui_context_shared,
            signals,
            command_cancel,
            squad_attach_session: None,
            attach_exit_code: Default::default(),
            attach_hangup: Default::default(),
            squad_attach_reachable: None,
            squad_attach_metadata: HashMap::new(),
            parallel_group_active: false,
            pending_step_slot_io: HashMap::new(),
            pending_parallel_yolo_cancel: HashMap::new(),
            last_config_edit_field: None,
            phase_stream_title: None,
            phase_stream_open: false,
            host_output_policy: crate::data::terminal::HostOutputPolicy::default(),
            presentation: Arc::new(PlainPresentation),
            pty_size,
            kind: FrontendKind::Tui,
            host_terminals: None,
        }
    }

    /// Each sequential launch owns a complete set of channels. Publishing them
    /// in the lifecycle queue orders the old exit before the new parser/I/O.
    pub(crate) fn recreate_container_io(&mut self) {
        self.launch_sequential_container(None);
    }

    pub(crate) fn launch_sequential_container(&mut self, title: Option<String>) {
        let (stdout_tx, stdout_rx) = tokio::sync::mpsc::unbounded_channel();
        let (stdin_tx, stdin_rx) = tokio::sync::mpsc::unbounded_channel();
        let (resize_tx, resize_rx) = tokio::sync::mpsc::unbounded_channel();
        let initial_size = self.pty_size.get();
        self.container_io = Some(AgentIo {
            stdout: stdout_tx.clone(),
            stderr: stdout_tx.clone(),
            stdin_tx: stdin_tx.clone(),
            stdin_rx,
            resize: Some(resize_rx),
            initial_size: Some(initial_size),
        });
        self.stdout_tx = stdout_tx;
        self.container.launch(
            title,
            crate::command::instance::feeds::ContainerSlotIo {
                stdout_rx,
                stdin_tx,
                resize_tx,
            },
        );
    }

    pub(crate) fn recreate_parallel_container_io(&mut self, step_name: &str) {
        let (stdout_tx, stdout_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let (stdin_tx, stdin_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let stdin_tx_for_engine = stdin_tx.clone();
        let (resize_tx, resize_rx) = tokio::sync::mpsc::unbounded_channel::<(u16, u16)>();

        let initial_size = self.pty_size.get();

        self.container_io = Some(AgentIo {
            stdout: stdout_tx.clone(),
            stderr: stdout_tx,
            stdin_tx: stdin_tx_for_engine,
            stdin_rx,
            resize: Some(resize_rx),
            initial_size: Some(initial_size),
        });
        self.pending_step_slot_io.insert(
            step_name.to_string(),
            crate::command::instance::feeds::ContainerSlotIo {
                stdout_rx,
                stdin_tx,
                resize_tx,
            },
        );
    }

    /// Push a parallel-slot lifecycle event into the shared queue for the
    /// TUI event loop to drain (WI-0096).
    pub(crate) fn push_container_slot_event(
        &self,
        event: crate::command::instance::feeds::ContainerSlotEvent,
    ) {
        self.container.push(event);
    }

    /// Send a dialog request and block waiting for the response.
    ///
    /// This uses `std::sync::mpsc::Receiver::recv()` which blocks the OS
    /// thread. Since engine trait methods are synchronous this is correct —
    /// no tokio executor is blocked.
    pub(crate) fn ask_dialog(
        &self,
        request: DialogRequest,
    ) -> Result<DialogResponse, CommandError> {
        let _ = self.dialog_tx.send(request);
        if let Some(wake) = &self.wake {
            wake.notify_one();
        }
        self.dialog_rx
            .lock()
            .map_err(|_| CommandError::Aborted)?
            .recv()
            .map_err(|_| CommandError::Aborted)
    }

    /// Present a workflow board opened because of the agent's own report
    /// (WI 0141). If the agent resumes (`working` or a clear) before the
    /// user has touched the board, it is closed for them and answered
    /// `Dismissed`, whether it is visible or still queued on a background
    /// tab. Any key or scroll on the tab after it opened keeps it.
    pub(crate) fn ask_report_dialog(
        &self,
        step_name: &str,
        request: DialogRequest,
    ) -> Result<DialogResponse, CommandError> {
        // A request left over from an earlier board must not close this one.
        self.signals.take_report_dialog_dismiss();
        let opened = std::time::Instant::now();

        let _ = self.dialog_tx.send(request);
        if let Some(wake) = &self.wake {
            wake.notify_one();
        }
        let rx = self.dialog_rx.lock().map_err(|_| CommandError::Aborted)?;
        let mut dismiss_requested = false;
        // The tab learns of reports asynchronously: only a resume seen
        // after the reported problem/done itself reached the tab counts.
        let mut seen_report = !self.agent_resumed(step_name);
        loop {
            match rx.recv_timeout(std::time::Duration::from_millis(100)) {
                Ok(response) => return Ok(response),
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(CommandError::Aborted)
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            }
            if dismiss_requested || self.command_cancel.is_cancelled() {
                continue;
            }
            let resumed = self.agent_resumed(step_name);
            if !resumed {
                seen_report = true;
                continue;
            }
            let touched = self.signals.last_user_input().is_some_and(|t| t >= opened)
                && if self.parallel_group_active {
                    self.signals.last_user_step().as_deref() == Some(step_name)
                } else {
                    true
                };
            if seen_report && !touched {
                self.signals.request_report_dialog_dismiss();
                dismiss_requested = true;
            }
        }
    }

    /// Whether the agent's latest report is `working` or a clear: a board
    /// opened for an earlier report is stale.
    pub(crate) fn agent_resumed(&self, step_name: &str) -> bool {
        use crate::data::program_status::ProgramState;
        self.signals
            .step_status(step_name)
            .unwrap_or_else(|| self.signals.status())
            .is_none_or(|s| s.state == ProgramState::Working)
    }

    /// Draw a `Prompt<D>` as a `KindSelect` dialog and map the answer back.
    ///
    /// The one place the TUI turns a prompt into a modal: the title and the
    /// keyed labels come from `command::prompts`, and a dismissal means
    /// `default_on_dismiss` — or `Aborted` when the prompt has none (F-19).
    /// Nothing here is copy, and no `D` is constructed from a string.
    pub(crate) fn pick_from_prompt<D: Clone>(
        &self,
        prompt: &crate::data::prompt::Prompt<D>,
    ) -> Result<D, CommandError> {
        let response = self.ask_dialog(DialogRequest::KindSelect {
            title: prompt.title.clone(),
            options: prompt
                .choices
                .iter()
                .map(|choice| (choice.key.to_string(), choice.label.clone()))
                .collect(),
        })?;
        let answer = match response {
            DialogResponse::Char(key) => prompt.answer_for_key(key),
            DialogResponse::Index(index) => prompt.answer_at(index),
            _ => None,
        };
        match answer.or_else(|| prompt.default_on_dismiss.clone()) {
            Some(value) => Ok(value),
            None => Err(CommandError::Aborted),
        }
    }

    /// Draw a `Prompt<D>` as a `Custom` dialog — title, body, and one line per
    /// choice under its own hotkey — and map the answer back. For questions
    /// whose body carries the substance (confirmations), which a
    /// `KindSelect` would drop. A dismissal means `default_on_dismiss`, and
    /// `Aborted` when the prompt has none, exactly as in `pick_from_prompt`.
    pub(crate) fn pick_from_keyed_prompt<D: Clone>(
        &self,
        prompt: &crate::data::prompt::Prompt<D>,
    ) -> Result<D, CommandError> {
        let response = self.ask_dialog(DialogRequest::Custom {
            title: prompt.title.clone(),
            body: prompt.body.clone(),
            keys: prompt
                .choices
                .iter()
                .map(|choice| (choice.key, choice.label.clone()))
                .collect(),
        })?;
        let answer = match response {
            DialogResponse::Char(key) => prompt.answer_for_key(key),
            _ => None,
        };
        match answer.or_else(|| prompt.default_on_dismiss.clone()) {
            Some(value) => Ok(value),
            None => Err(CommandError::Aborted),
        }
    }
}

impl InstanceCommandFrontend {
    pub fn with_input_available(mut self, available: bool) -> Self {
        self.input_available = available;
        self
    }
    pub(crate) fn is_cli(&self) -> bool {
        matches!(
            self.kind,
            FrontendKind::Cli
                | FrontendKind::Relayed {
                    origin: crate::command::dispatch::catalogue::RelayOrigin::Cli
                }
        )
    }
    pub(crate) fn is_headless_cli(&self) -> bool {
        self.is_cli()
            && (!self.input_available
                || self.parsed.args.flag_bool("headless") == Some(true)
                || self.parsed.args.flag_bool("json") == Some(true))
    }
    pub(crate) fn cli_defaults(&self) -> Option<crate::command::headless::HeadlessDefaults> {
        self.is_headless_cli()
            .then(crate::command::headless::HeadlessDefaults::cli)
    }
    pub fn with_wake(mut self, wake: Arc<tokio::sync::Notify>) -> Self {
        self.messages = self.messages.with_wake(wake.clone());
        self.wake = Some(wake);
        self
    }

    /// The frontend kind Dispatch applies policy for (default `Tui`).
    pub fn with_kind(mut self, kind: FrontendKind) -> Self {
        self.kind = kind;
        if self.is_cli() {
            self.command_bytes.enable();
            self.messages = self.messages.with_output(self.command_bytes.clone());
        }
        self
    }

    /// The presenter's wording, host-output filter and PTY sizing (default
    /// [`PlainPresentation`]).
    pub fn with_presentation(mut self, presentation: Arc<dyn InstancePresentation>) -> Self {
        self.presentation = presentation;
        self
    }

    /// Share the tab's presenter-reported PTY size with this command.
    pub fn with_pty_size(
        mut self,
        pty_size: crate::command::instance::feeds::SharedPtySize,
    ) -> Self {
        self.pty_size = pty_size;
        self
    }

    /// Run host processes that need a terminal in daemon PTYs registered
    /// with `terminals.hub` (engine-hosted commands only).
    pub fn with_host_terminals(mut self, terminals: Option<HostTerminals>) -> Self {
        self.host_terminals = terminals;
        self
    }
}

// ─── Presentation hooks ───────────────────────────────────────────────────

/// A byte filter applied to an attached terminal's output before it reaches
/// the presenter's terminal emulator (Layer 3's `HostOutputFilter`).
pub trait HostByteFilter: Send {
    fn feed(&mut self, chunk: &[u8], out: &mut Vec<u8>);
    fn finish(&mut self, out: &mut Vec<u8>);
}

/// What the producer asks of the frontend presenting its tab. Layer 3 owns
/// wording, glyphs and box drawing (grand architecture: engines and commands
/// return facts, frontends draw), so the few places where the producer writes
/// presenter text into the status log or a dialog payload delegate here. The
/// builtin TUI supplies its own implementation; [`PlainPresentation`] is the
/// fallback for hosts that have not registered one.
pub trait InstancePresentation: Send + Sync {
    /// The ready-summary table as status-log lines (`ready`, `remote`).
    fn ready_summary(&self, summary: &crate::data::ready_summary::ReadySummary) -> String;
    /// A table (`agents list`).
    fn table(&self, headers: &[&str], rows: &[Vec<String>]) -> String;
    /// Short agent-status label for the workflow overview (`blocked: permission`).
    fn status_short_label(&self, status: &crate::data::program_status::EffectiveStatus) -> String;
    /// One-line agent-status sentence for a control-board detail line.
    fn status_line(&self, status: &crate::data::program_status::EffectiveStatus) -> String;
    /// A fresh host-output filter for one attached terminal stream.
    fn host_output_filter(
        &self,
        policy: crate::data::terminal::HostOutputPolicy,
    ) -> Box<dyn HostByteFilter>;
}

/// Undecorated fallback presentation: plain text and pass-through bytes.
/// Used where no presenter registered one (unit tests,
/// hosts without a renderer); the builtin TUI never runs on it.
#[derive(Debug, Clone, Copy, Default)]
pub struct PlainPresentation;

struct PassThrough;

impl HostByteFilter for PassThrough {
    fn feed(&mut self, chunk: &[u8], out: &mut Vec<u8>) {
        out.extend_from_slice(chunk);
    }
    fn finish(&mut self, _out: &mut Vec<u8>) {}
}

impl InstancePresentation for PlainPresentation {
    fn ready_summary(&self, summary: &crate::data::ready_summary::ReadySummary) -> String {
        let mut out = format!("Ready Summary ({})", summary.runtime_name);
        for (label, status) in summary.rows() {
            out.push_str(&format!("\n{label}: {status:?}"));
        }
        out
    }

    fn table(&self, headers: &[&str], rows: &[Vec<String>]) -> String {
        let mut out = headers.join("  ");
        for row in rows {
            out.push('\n');
            out.push_str(&row.join("  "));
        }
        out
    }

    fn status_short_label(&self, status: &crate::data::program_status::EffectiveStatus) -> String {
        use crate::data::program_status::ProgramState;
        match (status.state, status.kind) {
            (ProgramState::Blocked, Some(kind)) => format!("blocked: {}", kind.as_str()),
            (state, _) => state.as_str().to_string(),
        }
    }

    fn status_line(&self, status: &crate::data::program_status::EffectiveStatus) -> String {
        let head = self.status_short_label(status);
        match status.msg.as_deref().filter(|m| !m.is_empty()) {
            Some(msg) => format!("{head}: {msg}"),
            None => head,
        }
    }

    fn host_output_filter(
        &self,
        _policy: crate::data::terminal::HostOutputPolicy,
    ) -> Box<dyn HostByteFilter> {
        Box::new(PassThrough)
    }
}

// ─── UserMessageSink ──────────────────────────────────────────────────────

/// F-45: takes the default `command_started`, so the `$ git …` echo line
/// is byte-identical to the one `run_git_logged` composed before.
impl crate::engine::git::GitFrontend for InstanceCommandFrontend {}

impl UserMessageSink for InstanceCommandFrontend {
    fn write_message(&mut self, msg: UserMessage) {
        self.messages.write_message(msg);
    }

    fn replay_queued(&mut self) {
        self.messages.replay_queued();
    }
}

// ─── CommandFrontend ──────────────────────────────────────────────────────

impl CommandFrontend for InstanceCommandFrontend {
    fn pick_queue_item(
        &mut self,
        items: &[crate::command::daemon_api::worker_pull::QueueRow],
    ) -> Result<Option<String>, CommandError> {
        let answer = self.ask_dialog(DialogRequest::KindSelect {
            title: "Pick a work item".into(),
            options: items
                .iter()
                .map(|row| (row.id.clone(), format!("{} · {}", row.title, row.server)))
                .collect(),
        })?;
        Ok(match answer {
            DialogResponse::Index(index) => items.get(index).map(|row| row.id.clone()),
            _ => None,
        })
    }
    fn confirm_queue_checkout(
        &mut self,
        prompt: &crate::data::prompt::Prompt<crate::command::worker::queue_source::CheckoutChoice>,
    ) -> crate::command::worker::queue_source::CheckoutChoice {
        self.pick_from_keyed_prompt(prompt)
            .unwrap_or(crate::command::worker::queue_source::CheckoutChoice::FreshClone)
    }
    fn bind_worker_run(
        &mut self,
        input: ParsedCommandBoxInput,
        events: crate::command::commands::api_server::event_bus::EventBusSender,
        writer: crate::data::fs::api_command_log::CommandStateWriter,
    ) -> Result<(), CommandError> {
        self.parsed = input;
        self.worker_events = Some(crate::command::worker::events::ForegroundRecorder::new(
            events,
            writer,
            self.parsed.path == ["exec", "prompt"],
        ));
        Ok(())
    }
    fn input_available(&self) -> bool {
        self.input_available
    }
    fn kind(&self) -> FrontendKind {
        self.kind
    }
    fn set_host_output_policy(&mut self, policy: crate::data::terminal::HostOutputPolicy) {
        self.host_output_policy = policy;
    }
    fn flag_bool(&self, _command_path: &[&str], flag: &str) -> Result<Option<bool>, CommandError> {
        Ok(self.parsed.args.flag_bool(flag))
    }

    fn flag_string(
        &self,
        _command_path: &[&str],
        flag: &str,
    ) -> Result<Option<String>, CommandError> {
        Ok(self.parsed.args.flag_string(flag))
    }

    fn flag_strings(
        &self,
        _command_path: &[&str],
        flag: &str,
    ) -> Result<Vec<String>, CommandError> {
        Ok(self.parsed.args.flag_strings(flag))
    }

    fn flag_path(
        &self,
        _command_path: &[&str],
        flag: &str,
    ) -> Result<Option<PathBuf>, CommandError> {
        Ok(self.parsed.args.flag_path(flag))
    }

    fn flag_enum(
        &self,
        _command_path: &[&str],
        flag: &str,
    ) -> Result<Option<String>, CommandError> {
        Ok(self.parsed.args.flag_enum(flag))
    }

    fn flag_u16(&self, _command_path: &[&str], flag: &str) -> Result<Option<u16>, CommandError> {
        Ok(self.parsed.args.flag_u16(flag))
    }

    fn flag_usize(
        &self,
        _command_path: &[&str],
        flag: &str,
    ) -> Result<Option<usize>, CommandError> {
        Ok(self.parsed.args.flag_usize(flag))
    }

    fn argument(&self, _command_path: &[&str], name: &str) -> Result<Option<String>, CommandError> {
        Ok(self.parsed.args.argument(name))
    }

    fn arguments(&self, _command_path: &[&str], name: &str) -> Result<Vec<String>, CommandError> {
        Ok(self.parsed.args.arguments(name))
    }
}
