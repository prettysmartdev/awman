//! `TuiCommandFrontend` — the single Layer 3 struct implementing every
//! per-command frontend trait for the TUI execution mode.
//!
//! Constructed from a `ParsedCommandBoxInput` (produced by
//! `Dispatch::parse_command_box_input`). Flag/argument extraction delegates to
//! the input's typed `ParsedArgs`; Layer 2 has already validated every value. Interactive Q&A methods open modal dialogs
//! via the dialog channel and block until the user responds.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use crate::command::dispatch::parsed_input::ParsedCommandBoxInput;
use crate::command::dispatch::CommandFrontend;
use crate::command::error::CommandError;
use crate::data::message::{UserMessage, UserMessageSink};
use crate::engine::agent_runtime::frontend::AgentIo;
use crate::frontend::tui::dialogs::{DialogRequest, DialogResponse};
use crate::frontend::tui::tabs::{
    ContainerLifecycle, SharedAgentsDashboard, SharedTuiContext, SharedWorkflowPresentation,
    SharedYoloCancelFlag, SharedYoloState, TabSharedState,
};
use crate::frontend::tui::user_message::{SharedStatusLog, TuiUserMessageSink};

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
pub struct TuiCommandFrontend {
    pub(crate) attach_exit_code: std::sync::Arc<std::sync::atomic::AtomicI32>,
    pub(crate) attach_hangup: tokio_util::sync::CancellationToken,
    parsed: ParsedCommandBoxInput,
    pub(crate) messages: TuiUserMessageSink,
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
    pub(crate) signals: crate::frontend::tui::tabs::AgentSignals,
    /// The tab's command-cancel token: ends an `agents status --watch` loop
    /// or an `agents attach` session (new command, tab closed, Esc).
    pub(crate) command_cancel: tokio_util::sync::CancellationToken,
    pub(crate) squad_attach_session:
        Option<std::sync::Arc<crate::frontend::tui::squad_attach::SquadAttachSession>>,
    pub(crate) squad_attach_reachable: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    pub(crate) squad_attach_metadata: HashMap<String, (String, Option<String>)>,
    pub(crate) parallel_group_active: bool,
    pub(crate) pending_step_slot_io: HashMap<String, crate::frontend::tui::tabs::ContainerSlotIo>,
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
}

// `SquadCommandFrontend` for `TuiCommandFrontend` (interview dialogs) lives in
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

impl TuiCommandFrontend {
    /// Active-tab suppression for an agent's `idle`/`done` report: hold back
    /// the dialog or countdown while the user has typed or scrolled on this
    /// tab within `agents.stuckTimeout`, so an interactive step does not
    /// pop a dialog on every turn. The engine has already decided what a
    /// report means; this only delays showing it. `blocked`/`error` never
    /// come through here. Gives up early if the agent resumes (a `working`,
    /// `blocked` or `error` report) or the command is cancelled.
    pub(crate) fn wait_for_quiet_user(&self) {
        while self.should_defer_for_user() {
            if self.command_cancel.is_cancelled() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
    }

    /// Whether an `idle`/`done` dialog should still wait: the user interacted
    /// with this tab within `agents.stuckTimeout` and the agent has not moved
    /// on to `working`, `blocked` or `error` (which are never deferred).
    pub(crate) fn should_defer_for_user(&self) -> bool {
        use crate::data::program_status::ProgramState;
        let window = crate::engine::container::timing::DEFAULT_STUCK_TIMEOUT;
        if !self.signals.user_active_within(window) {
            return false;
        }
        let resumed = self
            .signals
            .status()
            .is_some_and(|s| !matches!(s.state, ProgramState::Done | ProgramState::Idle));
        !resumed
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
        shared: TabSharedState,
    ) -> Self {
        let TabSharedState {
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
        Self {
            parsed,
            messages: TuiUserMessageSink::new(status_log.clone()),
            pty_active: false,
            dialog_tx,
            dialog_rx: Mutex::new(dialog_rx),
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
        let initial_size = crossterm::terminal::size()
            .map(|(cols, rows)| {
                crate::frontend::tui::event_loop::compute_container_inner_size(cols, rows)
            })
            .unwrap_or((80, 24));
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
            crate::frontend::tui::tabs::ContainerSlotIo {
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

        let initial_size = match crossterm::terminal::size() {
            Ok((cols, rows)) => {
                crate::frontend::tui::event_loop::compute_container_inner_size(cols, rows)
            }
            Err(_) => (80u16, 24u16),
        };

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
            crate::frontend::tui::tabs::ContainerSlotIo {
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
        event: crate::frontend::tui::tabs::ContainerSlotEvent,
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
        request: DialogRequest,
    ) -> Result<DialogResponse, CommandError> {
        // A request left over from an earlier board must not close this one.
        self.signals.take_report_dialog_dismiss();
        let opened = std::time::Instant::now();
        let _ = self.dialog_tx.send(request);
        let rx = self.dialog_rx.lock().map_err(|_| CommandError::Aborted)?;
        let mut dismiss_requested = false;
        // The tab learns of reports asynchronously: only a resume seen
        // after the reported problem/done itself reached the tab counts.
        let mut seen_report = !self.agent_resumed();
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
            let resumed = self.agent_resumed();
            if !resumed {
                seen_report = true;
                continue;
            }
            let touched = self.signals.last_user_input().is_some_and(|t| t >= opened);
            if seen_report && !touched {
                self.signals.request_report_dialog_dismiss();
                dismiss_requested = true;
            }
        }
    }

    /// Whether the agent's latest report is `working` or a clear: a board
    /// opened for an earlier report is stale.
    pub(crate) fn agent_resumed(&self) -> bool {
        use crate::data::program_status::ProgramState;
        self.signals
            .status()
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

// ─── UserMessageSink ──────────────────────────────────────────────────────

/// F-45: takes the default `command_started`, so the `$ git …` echo line
/// is byte-identical to the one `run_git_logged` composed before.
impl crate::engine::git::GitFrontend for TuiCommandFrontend {}

impl UserMessageSink for TuiCommandFrontend {
    fn write_message(&mut self, msg: UserMessage) {
        self.messages.write_message(msg);
    }

    fn replay_queued(&mut self) {
        self.messages.replay_queued();
    }
}

// ─── CommandFrontend ──────────────────────────────────────────────────────

impl CommandFrontend for TuiCommandFrontend {
    fn kind(&self) -> crate::command::dispatch::catalogue::FrontendKind {
        crate::command::dispatch::catalogue::FrontendKind::Tui
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

#[cfg(test)]
mod tests {
    use crate::command::dispatch::catalogue::CommandCatalogue;
    use crate::command::dispatch::parsed_input::parse;
    use crate::command::dispatch::projections::raw_args::shared_cases::{
        assert_reads, classification, CASES, ERROR_CASES,
    };
    use crate::frontend::tui::tests::test_command_frontend;

    fn command_line(path: &[&str], args: &[&str]) -> String {
        shell_words::join(path.iter().chain(args))
    }

    #[test]
    fn typed_input_reaches_every_accessor_unchanged() {
        for case in CASES {
            let raw = command_line(case.path, case.args);
            let parsed = parse(&raw, CommandCatalogue::get())
                .unwrap_or_else(|e| panic!("{}: {e}", case.name));
            assert_eq!(parsed.path, case.canonical, "{}", case.name);
            let (frontend, _requests, _responses) =
                test_command_frontend(case.canonical, parsed.args);
            assert_reads(&frontend, case, case.reads);
            assert_reads(&frontend, case, case.tui_reads);
        }
    }

    #[test]
    fn the_box_rejects_shared_error_cases_with_their_classification() {
        for (path, args, want) in ERROR_CASES {
            let raw = command_line(path, args);
            let error = parse(&raw, CommandCatalogue::get()).expect_err(&raw);
            assert_eq!(classification(&error), *want, "{raw}");
        }
    }

    #[test]
    fn squad_attach_resolves_the_task_and_refuses_the_removed_container_flag() {
        let attach = |raw: &str| {
            let parsed = parse(raw, CommandCatalogue::get()).unwrap();
            test_command_frontend(&["squad", "attach"], parsed.args).0
        };
        assert!(!attach("squad attach nightly").squad_attach_has_explicit_target());
        assert!(parse(
            "squad attach nightly --container c1",
            CommandCatalogue::get()
        )
        .is_err());
    }
}
