//! Application state — the central TUI state object.
//!
//! `App` stores UI state only. All command execution delegates to `Dispatch`
//! and the per-command frontend trait chain.

use std::sync::Arc;

use tokio::sync::RwLock;

use crate::command::dispatch::catalogue::CommandCatalogue;
use crate::command::dispatch::parsed_input::ParsedCommandBoxInput;
use crate::command::dispatch::{CommandOutcome, Dispatch, Engines};
use crate::command::error::CommandError;
use crate::data::session::Session;
use crate::data::session_manager::SessionManager;
use crate::frontend::tui::command_frontend::TuiCommandFrontend;
use crate::frontend::tui::dialogs::{Dialog, DialogRequest, DialogResponse};
use crate::frontend::tui::tabs::{ExecutionPhase, Tab};
use crate::frontend::tui::text_edit::TextEdit;

/// Pull the `--agent` value out of a parsed command box input, falling back
/// to "Claude" (the default agent) when the flag is absent.
/// Used to seed `ContainerInfo.agent_display_name`.
fn agent_name_from_parsed(parsed: &ParsedCommandBoxInput) -> String {
    use crate::command::dispatch::parsed_input::FlagValue;
    if let Some(FlagValue::String(s)) = parsed.flags.get("agent") {
        return s.clone();
    }
    "Claude".to_string()
}

/// UI focus target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    CommandBox,
    ExecutionWindow,
}

/// Status bar state.
#[derive(Debug, Clone, Default)]
pub struct StatusBar {
    pub text: String,
}

/// Central TUI state. Contains NO business logic — only UI state.
pub struct App {
    pub tabs: Vec<Tab>,
    pub active_tab: usize,
    pub active_dialog: Option<Dialog>,
    pub focus: Focus,
    pub catalogue: &'static CommandCatalogue,
    pub engines: Engines,
    pub session_manager: Arc<RwLock<SessionManager>>,
    pub command_input: TextEdit,
    pub suggestion_row: Vec<String>,
    pub input_error: Option<String>,
    pub status_bar: StatusBar,
    pub should_quit: bool,
    pub needs_redraw: bool,
    pub command_dialog_active: bool,
    pub runtime_handle: tokio::runtime::Handle,
    pub session: Arc<RwLock<Session>>,
    /// Receiver for asynchronous container stats results.
    pub stats_rx: Option<std::sync::mpsc::Receiver<(usize, crate::engine::container::instance::ContainerStats)>>,
    /// Sender cloned per stats query — kept alive so the channel stays open.
    pub stats_tx: std::sync::mpsc::Sender<(usize, crate::engine::container::instance::ContainerStats)>,
    /// Tracks when the last stats query was dispatched so we don't spam.
    pub last_stats_poll: std::time::Instant,
}

impl App {
    pub fn new(
        catalogue: &'static CommandCatalogue,
        engines: Engines,
        session_manager: Arc<RwLock<SessionManager>>,
        initial_tab: Tab,
        runtime_handle: tokio::runtime::Handle,
        session: Arc<RwLock<Session>>,
    ) -> Self {
        let (stats_tx, stats_rx) = std::sync::mpsc::channel();
        Self {
            tabs: vec![initial_tab],
            active_tab: 0,
            active_dialog: None,
            focus: Focus::CommandBox,
            catalogue,
            engines,
            session_manager,
            command_input: TextEdit::new(false),
            suggestion_row: Vec::new(),
            input_error: None,
            status_bar: StatusBar::default(),
            should_quit: false,
            needs_redraw: true,
            command_dialog_active: false,
            runtime_handle,
            session,
            stats_rx: Some(stats_rx),
            stats_tx,
            last_stats_poll: std::time::Instant::now() - std::time::Duration::from_secs(10),
        }
    }

    pub fn active_tab(&self) -> &Tab {
        &self.tabs[self.active_tab]
    }

    pub fn active_tab_mut(&mut self) -> &mut Tab {
        &mut self.tabs[self.active_tab]
    }

    pub fn switch_to_prev_tab(&mut self) {
        if self.active_tab > 0 {
            self.active_tab -= 1;
        } else if !self.tabs.is_empty() {
            self.active_tab = self.tabs.len() - 1;
        }
    }

    pub fn switch_to_next_tab(&mut self) {
        if self.active_tab + 1 < self.tabs.len() {
            self.active_tab += 1;
        } else {
            self.active_tab = 0;
        }
    }

    pub fn close_active_tab(&mut self) {
        if self.tabs.len() <= 1 {
            self.should_quit = true;
            return;
        }
        self.tabs.remove(self.active_tab);
        if self.active_tab >= self.tabs.len() {
            self.active_tab = self.tabs.len().saturating_sub(1);
        }
    }

    /// Spawn a parsed command as an async tokio task, wiring up all channels
    /// between the event loop and the command thread.
    pub fn spawn_command(
        &mut self,
        _command_text: &str,
        parsed: ParsedCommandBoxInput,
    ) {
        let tab = self.active_tab_mut();

        // Clear previous output so the new command starts with a fresh log.
        if let Ok(mut log) = tab.status_log.lock() {
            log.clear();
        }
        tab.scroll_offset = 0;

        // Reset the vt100 parser so the previous container's output is gone.
        let (rows, cols) = tab.vt100_parser.screen().size();
        tab.vt100_parser = vt100::Parser::new(rows, cols, 10000);
        tab.container_scroll_offset = 0;
        tab.mouse_selection = None;
        tab.last_container_summary = None;

        // Dialog channels (std::sync::mpsc — command thread blocks on recv).
        let (dialog_req_tx, dialog_req_rx) = std::sync::mpsc::channel::<DialogRequest>();
        let (dialog_resp_tx, dialog_resp_rx) = std::sync::mpsc::channel::<DialogResponse>();

        // Container I/O channels — tokio mpsc throughout so the engine PTY
        // bridge can use them from async tasks. The TUI keeps a clone of the
        // stdin sender (for user keystrokes) and the engine receives both
        // sender and receiver for the PTY bridge plus inject_prompt.
        let (stdout_tx, stdout_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let (stdin_tx, stdin_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let stdin_tx_for_engine = stdin_tx.clone();
        let (resize_tx, resize_rx) = tokio::sync::mpsc::unbounded_channel::<(u16, u16)>();

        // Command result channel.
        let (result_tx, result_rx) =
            std::sync::mpsc::channel::<Result<CommandOutcome, CommandError>>();

        // Initial PTY size: derive from the current terminal so the
        // container starts with a correctly-sized grid (otherwise TUI apps
        // inside the container, like Claude, would render against an 80x24
        // default until the first SIGWINCH).
        let initial_size = match crossterm::terminal::size() {
            Ok((cols, rows)) => crate::frontend::tui::compute_container_inner_size(cols, rows),
            Err(_) => (80u16, 24u16),
        };

        let container_io = crate::engine::container::frontend::ContainerIo {
            stdout: stdout_tx,
            stdin_tx: stdin_tx_for_engine,
            stdin_rx,
            resize: resize_rx,
            initial_size,
        };

        // Build the TUI frontend. Workflow + yolo overlays share the same
        // `Arc<Mutex<...>>` between the engine-side frontend impl and the
        // renderer.
        let frontend = TuiCommandFrontend::new(
            parsed.clone(),
            tab.status_log.clone(),
            dialog_req_tx,
            dialog_resp_rx,
            container_io,
            tab.workflow_state.clone(),
            tab.yolo_state.clone(),
            tab.pty_reset_flag.clone(),
        );

        // Store the receiving/sending ends in the tab.
        tab.container_stdout_rx = Some(stdout_rx);
        tab.container_stdin_tx = Some(stdin_tx);
        tab.container_resize_tx = Some(resize_tx);
        tab.command_result_rx = Some(result_rx);
        tab.dialog_request_rx = Some(dialog_req_rx);
        tab.dialog_response_tx = Some(dialog_resp_tx);

        let command_name = parsed.path.join(" ");
        let agent_display = agent_name_from_parsed(&parsed);

        // Pre-populate ContainerInfo so the overlay title bar can show the
        // command name and elapsed time even before the engine reports the
        // actual container's name. The engine may overwrite the container
        // name later via `report_status`.
        tab.container_info = Some(crate::frontend::tui::tabs::ContainerInfo {
            agent_display_name: agent_display.clone(),
            container_name: String::new(),
            start_time: std::time::Instant::now(),
            latest_stats: None,
            stats_history: Vec::new(),
        });

        // Show the "Interactive Mode" banner for containerized commands.
        let is_containerized = matches!(
            parsed.path.first().map(|s| s.as_str()),
            Some("chat" | "implement" | "exec")
        );
        if is_containerized {
            use crate::frontend::tui::user_message::TuiUserMessageSink;
            use crate::engine::message::UserMessageSink;
            let mut sink = TuiUserMessageSink::new(tab.status_log.clone());
            sink.info("╔══════════════════════════════════════════════════════════════╗".to_string());
            sink.info("║                                                              ║".to_string());
            sink.info("║     ╦╔╗╔╔╦╗╔═╗╦═╗╔═╗╔═╗╔╦╗╦╦  ╦╔═╗  ╔╦╗╔═╗╔╦╗╔═╗        ║".to_string());
            sink.info("║     ║║║║ ║ ║╣ ╠╦╝╠═╣║   ║ ║╚╗╔╝║╣   ║║║║ ║ ║║║╣         ║".to_string());
            sink.info("║     ╩╝╚╝ ╩ ╚═╝╩╚═╩ ╩╚═╝ ╩ ╩ ╚╝ ╚═╝  ╩ ╩╚═╝═╩╝╚═╝       ║".to_string());
            sink.info("║                                                              ║".to_string());
            sink.info(format!(
                "║  Agent '{}' is launching in INTERACTIVE mode.{}║",
                agent_display,
                " ".repeat(46usize.saturating_sub(agent_display.len() + 43))
            ));
            sink.info("║  You will need to quit the agent (Ctrl+C or exit)            ║".to_string());
            sink.info("║  when its work is complete.                                  ║".to_string());
            sink.info("║                                                              ║".to_string());
            sink.info("╚══════════════════════════════════════════════════════════════╝".to_string());
        }

        tab.execution_phase = ExecutionPhase::Running {
            command: command_name,
        };

        // Build the dispatch and spawn the command using the tab's session
        // so commands execute in the correct working directory.
        let tab_session = self.active_tab().session.clone();
        let session = Arc::new(RwLock::new(tab_session));
        let engines = self.engines.clone();
        let path_owned: Vec<String> = parsed.path.clone();

        self.runtime_handle.spawn(async move {
            let dispatch = Dispatch::new(frontend, session, engines);
            let path_refs: Vec<&str> = path_owned.iter().map(|s| s.as_str()).collect();
            let result = dispatch.run_command(&path_refs).await;
            let _ = result_tx.send(result);
        });
    }

    /// Add a new tab backed by the given session. Returns the index of the
    /// new tab.
    pub fn add_tab(&mut self, session: Session) -> usize {
        let tab = Tab::new(session);
        self.tabs.push(tab);
        self.tabs.len() - 1
    }

    /// Tick all tabs: drain container output, poll for command completion,
    /// poll for stats results, and recompute the per-tab stuck flag.
    pub fn tick_all_tabs(&mut self) {
        let active = self.active_tab;
        for (i, tab) in self.tabs.iter_mut().enumerate() {
            tab.drain_container_output();
            tab.poll_command_completion();
            tab.recompute_stuck(i == active);
        }

        // Drain any completed stats results.
        if let Some(ref rx) = self.stats_rx {
            while let Ok((tab_idx, stats)) = rx.try_recv() {
                if tab_idx < self.tabs.len() {
                    if let Some(ref mut info) = self.tabs[tab_idx].container_info {
                        info.stats_history.push((stats.cpu_percent, stats.memory_mb));
                        if info.container_name.is_empty() {
                            info.container_name = stats.name.clone();
                        }
                        info.latest_stats = Some(stats);
                    }
                }
            }
        }

        // Dispatch a new stats poll every ~3 seconds for tabs with active containers.
        if self.last_stats_poll.elapsed() >= std::time::Duration::from_secs(3) {
            self.last_stats_poll = std::time::Instant::now();
            for (i, tab) in self.tabs.iter().enumerate() {
                if !matches!(tab.execution_phase, crate::frontend::tui::tabs::ExecutionPhase::Running { .. }) {
                    continue;
                }
                if tab.container_window_state == crate::frontend::tui::tabs::ContainerWindowState::Hidden {
                    continue;
                }
                let container_name = tab.container_info.as_ref()
                    .map(|info| info.container_name.clone())
                    .unwrap_or_default();
                let runtime = self.engines.runtime.clone();
                let tx = self.stats_tx.clone();
                let tab_idx = i;
                self.runtime_handle.spawn(async move {
                    let handles = match runtime.list_running_sync() {
                        Ok(h) => h,
                        Err(_) => return,
                    };
                    // Find the right container: match by name if known,
                    // otherwise use the first amux container.
                    let target = if !container_name.is_empty() {
                        handles.iter().find(|h| h.name == container_name)
                    } else {
                        handles.first()
                    };
                    if let Some(handle) = target {
                        if let Ok(stats) = runtime.stats(handle) {
                            let _ = tx.send((tab_idx, stats));
                        }
                    }
                });
            }
        }
    }

    /// Check the active tab's dialog_request_rx and open the corresponding
    /// dialog in the App.
    pub fn poll_dialog_requests(&mut self) {
        let request = {
            let tab = &self.tabs[self.active_tab];
            tab.dialog_request_rx
                .as_ref()
                .and_then(|rx| rx.try_recv().ok())
        };

        if let Some(request) = request {
            let dialog = match request {
                DialogRequest::YesNo { title, body } => {
                    Dialog::YesNo { title, body }
                }
                DialogRequest::YesNoCancel { title, body } => {
                    Dialog::YesNoCancel { title, body }
                }
                DialogRequest::TextInput { title, prompt } => {
                    Dialog::TextInput {
                        title,
                        prompt,
                        editor: TextEdit::new(false),
                    }
                }
                DialogRequest::MultilineInput { title, prompt } => {
                    Dialog::MultilineInput {
                        title,
                        prompt,
                        editor: TextEdit::new(true),
                    }
                }
                DialogRequest::ListPicker { title, items } => {
                    Dialog::ListPicker {
                        title,
                        items,
                        selected: 0,
                    }
                }
                DialogRequest::KindSelect { title, options } => {
                    Dialog::KindSelect { title, options }
                }
                DialogRequest::WorkflowControlBoard(state) => {
                    Dialog::WorkflowControlBoard(state)
                }
                DialogRequest::WorkflowStepError(state) => {
                    Dialog::WorkflowStepError(state)
                }
                DialogRequest::WorkflowYoloCountdown(state) => {
                    Dialog::WorkflowYoloCountdown(state)
                }
                DialogRequest::AgentSetup(state) => {
                    Dialog::AgentSetup(state)
                }
                DialogRequest::MountScope(state) => {
                    Dialog::MountScope(state)
                }
                DialogRequest::AgentAuth(state) => {
                    Dialog::AgentAuth(state)
                }
                DialogRequest::QuitConfirm => Dialog::QuitConfirm,
                DialogRequest::CloseTabConfirm => Dialog::CloseTabConfirm,
                DialogRequest::WorkflowCancelConfirm => Dialog::WorkflowCancelConfirm,
                DialogRequest::ConfigShow => {
                    // ConfigShow dialog needs rows populated by the caller;
                    // open with empty state for now.
                    Dialog::ConfigShow(crate::frontend::tui::dialogs::ConfigShowState {
                        rows: Vec::new(),
                        selected: 0,
                        editing: false,
                        edit_column: 0,
                        editor: TextEdit::new(false),
                    })
                }
                DialogRequest::Loading { title } => Dialog::Loading { title },
                DialogRequest::Custom { title, body, keys } => {
                    Dialog::Custom { title, body, keys }
                }
            };
            self.active_dialog = Some(dialog);
            self.command_dialog_active = true;
        }
    }

    /// Send a dialog response through the active tab's dialog_response_tx.
    pub fn send_dialog_response(&mut self, response: DialogResponse) {
        let tab = &self.tabs[self.active_tab];
        if let Some(ref tx) = tab.dialog_response_tx {
            let _ = tx.send(response);
        }
    }

    pub fn update_suggestions(&mut self) {
        let partial = self.command_input.text.as_str();
        if partial.is_empty() {
            self.suggestion_row.clear();
            return;
        }
        let completions = self.catalogue.tui_completions(partial);
        self.suggestion_row = completions
            .into_iter()
            .map(|c| c.completion)
            .collect();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::sync::RwLock;

    use crate::command::dispatch::catalogue::CommandCatalogue;
    use crate::data::session::{Session, SessionOpenOptions, StaticGitRootResolver};
    use crate::data::session_manager::SessionManager;
    use crate::frontend::tui::tabs::Tab;

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

    fn make_engines() -> crate::command::dispatch::Engines {
        let runtime = Arc::new(crate::engine::container::ContainerRuntime::docker());
        let overlay = Arc::new(crate::engine::overlay::OverlayEngine::with_auth_resolver(
            crate::data::fs::auth_paths::AuthPathResolver::at_home(std::path::PathBuf::from("/tmp")),
        ));
        let git_engine = Arc::new(crate::engine::git::GitEngine::new());
        let agent_engine = Arc::new(crate::engine::agent::AgentEngine::new(
            overlay.clone(),
            runtime.clone(),
        ));
        let auth_engine = Arc::new(crate::engine::auth::AuthEngine::with_paths(
            crate::data::fs::auth_paths::AuthPathResolver::at_home("/tmp"),
            crate::data::fs::headless_paths::HeadlessPaths::at_root("/tmp"),
        ));
        let workflow_state_store = {
            let tmp = tempfile::tempdir().unwrap();
            Arc::new(crate::data::EngineWorkflowStateStore::at_git_root(tmp.path()))
        };
        crate::command::dispatch::Engines {
            runtime,
            git_engine,
            overlay_engine: overlay,
            auth_engine,
            agent_engine,
            workflow_state_store,
        }
    }

    fn make_app() -> App {
        let rt = Box::leak(Box::new(tokio::runtime::Runtime::new().unwrap()));
        let catalogue = CommandCatalogue::get();
        let engines = make_engines();
        let session_manager = Arc::new(RwLock::new(SessionManager::in_memory()));
        let session = make_test_session();
        let session_arc = Arc::new(RwLock::new(session.clone()));
        let tab = Tab::new(session);
        App::new(catalogue, engines, session_manager, tab, rt.handle().clone(), session_arc)
    }

    // ── update_suggestions ────────────────────────────────────────────────────

    #[test]
    fn update_suggestions_empty_input_clears_suggestions() {
        let mut app = make_app();
        app.suggestion_row = vec!["chat".to_string()];
        app.command_input.set_text("");
        app.command_input.text.clear();
        app.update_suggestions();
        assert!(app.suggestion_row.is_empty(), "empty input must clear suggestions");
    }

    #[test]
    fn update_suggestions_partial_match_populates_suggestions() {
        let mut app = make_app();
        app.command_input.set_text("cha");
        app.update_suggestions();
        assert!(
            app.suggestion_row.iter().any(|s| s == "chat"),
            "'cha' must suggest 'chat'; got: {:?}",
            app.suggestion_row
        );
    }

    #[test]
    fn update_suggestions_no_match_yields_empty() {
        let mut app = make_app();
        app.command_input.set_text("zzzzzzz");
        app.update_suggestions();
        assert!(app.suggestion_row.is_empty());
    }

    // ── tab switching ─────────────────────────────────────────────────────────

    #[test]
    fn switch_to_next_tab_wraps_around() {
        let mut app = make_app();
        app.tabs.push(Tab::new(make_test_session()));
        app.active_tab = 1;
        app.switch_to_next_tab();
        assert_eq!(app.active_tab, 0, "next tab from last must wrap to first");
    }

    #[test]
    fn switch_to_prev_tab_wraps_around() {
        let mut app = make_app();
        app.tabs.push(Tab::new(make_test_session()));
        app.active_tab = 0;
        app.switch_to_prev_tab();
        assert_eq!(app.active_tab, 1, "prev tab from first must wrap to last");
    }

    #[test]
    fn switch_to_next_advances_index() {
        let mut app = make_app();
        app.tabs.push(Tab::new(make_test_session()));
        app.active_tab = 0;
        app.switch_to_next_tab();
        assert_eq!(app.active_tab, 1);
    }

    #[test]
    fn close_active_tab_with_single_tab_sets_should_quit() {
        let mut app = make_app();
        assert_eq!(app.tabs.len(), 1);
        app.close_active_tab();
        assert!(app.should_quit);
    }

    #[test]
    fn close_active_tab_with_multiple_tabs_removes_tab() {
        let mut app = make_app();
        app.tabs.push(Tab::new(make_test_session()));
        assert_eq!(app.tabs.len(), 2);
        app.close_active_tab();
        assert_eq!(app.tabs.len(), 1);
        assert!(!app.should_quit);
    }
}
