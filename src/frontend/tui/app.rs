//! Application state — the central TUI state object.
//!
//! `App` stores UI state only. All command execution delegates to `Dispatch`
//! and the per-command frontend trait chain.

use std::sync::Arc;

use crate::command::commands::squad::supervisor::SquadGatewayResolver;
use crate::command::dispatch::catalogue::CommandCatalogue;
use crate::command::dispatch::parsed_input::ParsedCommandBoxInput;
use crate::command::dispatch::{CommandOutcome, Dispatch, Engines};
use crate::command::error::CommandError;
use crate::command::stats_sampler::{ContainerStatsSampler, StatsRequest, StatsTarget};
use crate::data::config::env::Env;
use crate::data::session::Session;
use crate::data::session_manager::SessionManager;
use crate::frontend::tui::command_frontend::{DialogChannels, TuiCommandFrontend};
use crate::frontend::tui::dialogs::{Dialog, DialogRequest, DialogResponse};
use crate::frontend::tui::tabs::{ExecutionPhase, Tab};
use crate::frontend::tui::text_edit::TextEdit;

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

/// The outcome of the pre-event-loop squad tab build (`awman squad`).
///
/// `KeyMissing` is not an error — the daemon is up and healthy — but no tab is
/// assembled for it, because a tab whose every poll is refused with 401 shows
/// nothing but that. The caller raises [`Dialog::SquadKeyMissing`] instead, and
/// accepting it builds the tab through the ordinary
/// [`App::poll_squad_startup`] path once a usable key exists.
pub enum SquadTabStart {
    Ready(Box<SquadTabBuild>),
    KeyMissing,
}

/// A live squad gateway plus what this process can authenticate to it with,
/// as Layer 2 reports it.
pub use crate::command::commands::squad::supervisor::{
    SquadKeySetup, SquadStartError, SquadStartup, SquadStartupResult,
};

/// Everything [`App::build_squad_tab`] produces. `key_setup` is `Some` only on
/// the run that minted the squad bearer key, and carries the shell snippet the
/// caller must show before it is lost — the plaintext key exists nowhere else.
pub struct SquadTabBuild {
    pub tab: Tab,
    pub gateway: Arc<dyn crate::command::commands::squad::gateway::TaskGateway>,
    pub key_setup: Option<SquadKeySetup>,
}

/// Central TUI state. Contains NO business logic — only UI state.
pub struct App {
    pub tabs: Vec<Tab>,
    pub active_tab: usize,
    pub active_dialog: Option<Dialog>,
    pub focus: Focus,
    pub catalogue: &'static CommandCatalogue,
    pub engines: Engines,
    pub session_manager: Arc<SessionManager>,
    pub command_input: TextEdit,
    pub suggestion_row: Vec<String>,
    pub input_error: Option<String>,
    pub status_bar: StatusBar,
    pub should_quit: bool,
    pub needs_redraw: bool,
    pub command_dialog_active: bool,
    pub runtime_handle: tokio::runtime::Handle,
    /// Gateway for the singleton squad tab, obtained from
    /// `SquadGatewayResolver::ensure_running()` when the tab is opened. Injected into
    /// `Dispatch` for `squad` in-tab actions so they reach the daemon. `None`
    /// until the squad tab exists.
    pub squad_gateway: Option<Arc<dyn crate::command::commands::squad::gateway::TaskGateway>>,
    /// A squad daemon start that is running on the tokio runtime, waiting for
    /// the daemon to publish its endpoint. `Some` only between the user
    /// accepting the daemon-start confirmation and
    /// [`App::poll_squad_startup`] draining the result, which is also what
    /// keeps a second `y` from spawning a second daemon.
    pub squad_startup_rx: Option<std::sync::mpsc::Receiver<SquadStartupResult>>,
    /// Samples container resource figures off the draw loop. Owns the
    /// cadence, the blocking dispatch and the one-query-per-slot rule; this
    /// module only says which slots to sample and where each sample goes
    /// (WI 0114 F-16).
    pub stats_sampler: ContainerStatsSampler,
    /// The active tab index as of the previous tick (WI 0112). Lets the tick
    /// notice a switch *away* from the squad tab and hand focus back to the
    /// command box, without every tab-switching path having to know.
    pub last_active_tab: usize,
    /// Squad daemon health for the bottom-row indicator (WI 0112). Written by
    /// the app-level `SquadIndicatorPoller` started in `tui::run`; read by
    /// the renderer. Starts `Unknown` and stays so in unit tests, which never
    /// start the poller.
    pub squad_indicator: crate::frontend::tui::squad_indicator::SharedSquadIndicator,
}

impl App {
    pub fn new(
        catalogue: &'static CommandCatalogue,
        engines: Engines,
        session_manager: Arc<SessionManager>,
        initial_tab: Tab,
        runtime_handle: tokio::runtime::Handle,
    ) -> Self {
        let stats_sampler =
            ContainerStatsSampler::new(engines.runtime.clone(), runtime_handle.clone());
        // `App` accepts a pre-built tab for rendering, but the manager owns
        // the Session object. This also covers the synthetic squad tab.
        if session_manager.get(&initial_tab.session_id).is_none() {
            session_manager
                .create(initial_tab.session.clone())
                .expect("initial tab session id must be unique");
        }
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
            squad_gateway: None,
            squad_startup_rx: None,
            stats_sampler,
            last_active_tab: 0,
            squad_indicator: Arc::new(std::sync::Mutex::new(
                crate::engine::squad::SquadHealth::Unknown,
            )),
        }
    }

    /// Whether the active tab is the squad tab showing its card grid — the
    /// state in which the command box is permanently inactive and the grid
    /// always holds focus (WI 0112). False while an attach session owns the
    /// tab's slots: the tab then behaves as an ordinary container tab.
    pub fn squad_grid_active(&self) -> bool {
        let tab = self.active_tab();
        tab.is_squad && tab.container_slots.is_empty()
    }

    /// Keep `focus` consistent with the active tab (WI 0112), once per tick:
    /// on the squad grid, focus is always the grid; leaving the squad tab for
    /// a normal tab restores the command box. A normal-to-normal switch leaves
    /// focus alone, exactly as before.
    fn normalize_focus_for_active_tab(&mut self) {
        if self.squad_grid_active() {
            self.focus = Focus::ExecutionWindow;
        } else if self.active_tab != self.last_active_tab
            && self
                .tabs
                .get(self.last_active_tab)
                .is_some_and(|tab| tab.is_squad)
        {
            self.focus = Focus::CommandBox;
        }
        self.last_active_tab = self.active_tab;
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
        // The removed index must not survive as `last_active_tab`, or a tab
        // that later lands on it could be mistaken for the one that closed.
        self.last_active_tab = self.last_active_tab.min(self.tabs.len().saturating_sub(1));
    }

    /// Focus the existing squad tab, or create the singleton one — asking
    /// first when that would also start a daemon.
    ///
    /// On failure nothing is created: the error text — which already names
    /// `awman api` (daemon conflict) or the sandbox runtime — is surfaced
    /// verbatim in the status bar, never a generic "could not connect".
    /// Idempotent: a second call focuses the tab created by the first.
    ///
    /// WI 0110: opening the tab starts a long-lived background process when no
    /// squad daemon is running, so that case raises
    /// [`Dialog::SquadStartConfirm`] and builds nothing until the user answers
    /// `y` ([`App::build_and_install_squad_tab`]). With a daemon already
    /// running there is nothing to consent to and no dialog is raised.
    pub fn open_or_focus_squad_tab(&mut self) {
        if let Some(idx) = self.tabs.iter().position(|t| t.is_squad) {
            self.active_tab = idx;
            self.needs_redraw = true;
            return;
        }
        // The runtime refusal comes first: a sandbox-class runtime cannot back
        // squad at all, so asking whether to start a daemon would be asking a
        // question whose "yes" could not be honoured.
        if let Err(error) = SquadGatewayResolver::admit_runtime(&self.engines) {
            self.status_bar.text = error.to_string();
            return;
        }
        match Self::squad_daemon_is_running() {
            Ok(true) => self.build_and_install_squad_tab(),
            Ok(false) => {
                self.active_dialog = Some(Dialog::SquadStartConfirm);
                self.needs_redraw = true;
            }
            // Whether a daemon is running could not be determined (an
            // unreadable squad root, say). Fall through to the ordinary build,
            // whose error text is the specific one worth showing.
            Err(_) => self.build_and_install_squad_tab(),
        }
    }

    /// Whether a squad daemon is already running for this squad root. Starts
    /// nothing and writes nothing — see [`SquadGatewayResolver::daemon_is_running`].
    ///
    /// [`SquadGatewayResolver::daemon_is_running`]: crate::command::commands::squad::supervisor::SquadGatewayResolver::daemon_is_running
    fn squad_daemon_is_running() -> Result<bool, crate::command::error::CommandError> {
        SquadGatewayResolver::from_env(&Env::from_process())?.daemon_is_running()
    }

    /// Start the squad tab and install it as the active tab, starting the
    /// daemon if it is not already running. The second half of
    /// [`App::open_or_focus_squad_tab`], separated so the daemon-start
    /// confirmation can call it on `y`.
    ///
    /// Starting a daemon means spawning a process and then waiting — up to ten
    /// seconds — for it to publish its endpoint. Doing that inline froze the
    /// event loop for the whole wait: the terminal could not redraw, so the
    /// confirmation the user had just answered stayed painted on screen until
    /// the wait ended, and a failure surfaced only as a status-bar line under
    /// a modal that had appeared to hang. The wait now runs on the tokio
    /// runtime, this returns immediately with a "Starting…" modal on screen,
    /// and [`App::poll_squad_startup`] installs the tab (or reports the
    /// failure in a modal of its own) when the daemon answers.
    pub fn build_and_install_squad_tab(&mut self) {
        if let Some(idx) = self.tabs.iter().position(|t| t.is_squad) {
            self.active_tab = idx;
            self.needs_redraw = true;
            return;
        }
        // A second `y` while the first start is still in flight must not spawn
        // a second daemon.
        if self.squad_startup_rx.is_some() {
            return;
        }
        // The runtime refusal is synchronous and immediate — there is nothing
        // to wait for — so it is reported without ever showing a "Starting…"
        // modal the user would watch fail.
        if let Err(error) = SquadGatewayResolver::admit_runtime(&self.engines) {
            self.status_bar.text = error.to_string();
            self.needs_redraw = true;
            return;
        }

        let (tx, rx) = std::sync::mpsc::channel();
        let engines = self.engines.clone();
        self.runtime_handle.spawn(async move {
            let _ = tx.send(Self::ensure_squad_gateway(engines).await);
        });
        self.squad_startup_rx = Some(rx);
        self.active_dialog = Some(Dialog::Loading {
            title: "Starting squad daemon".to_string(),
        });
        self.needs_redraw = true;
    }

    /// Mint a new squad bearer key and restart the daemon onto it, then open
    /// the tab. The `y` answer to [`Dialog::SquadKeyMissing`].
    ///
    /// Shares `squad_startup_rx` and [`App::poll_squad_startup`] with an
    /// ordinary start: a refresh ends in the same place a first run does — a
    /// live gateway and a a minted key to display — so there is
    /// one drain, one progress modal, and one error path rather than two.
    pub fn start_squad_key_refresh(&mut self) {
        if self.squad_startup_rx.is_some() {
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        self.runtime_handle.spawn(async move {
            let _ = tx.send(Self::refresh_squad_key().await);
        });
        self.squad_startup_rx = Some(rx);
        self.active_dialog = Some(Dialog::Loading {
            title: "Refreshing the squad key".to_string(),
        });
        self.needs_redraw = true;
    }

    /// Drain a completed daemon startup, if one has finished since the last
    /// tick. Called from [`App::tick_all_tabs`]; a no-op when no start is in
    /// flight or the daemon has not answered yet.
    ///
    /// A failure is raised as a modal rather than written to the status bar:
    /// the user explicitly asked for a daemon, so "it did not start, and here
    /// is why" is the answer to a question they asked, not an aside they may
    /// or may not notice before the next command overwrites it.
    pub(crate) fn poll_squad_startup(&mut self) {
        let Some(rx) = self.squad_startup_rx.as_ref() else {
            return;
        };
        let result = match rx.try_recv() {
            Ok(result) => result,
            Err(std::sync::mpsc::TryRecvError::Empty) => return,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => Err(SquadStartError::Other(
                "squad daemon startup was interrupted".to_string(),
            )),
        };
        self.squad_startup_rx = None;
        // Only this dialog is dismissed: an error or notice raised while the
        // start was running belongs to the user, not to us.
        if matches!(self.active_dialog, Some(Dialog::Loading { .. })) {
            self.active_dialog = None;
        }
        self.needs_redraw = true;

        match result.and_then(|startup| Self::assemble_squad_tab(startup, &self.runtime_handle)) {
            Ok(build) => {
                self.squad_gateway = Some(build.gateway);
                self.tabs.push(build.tab);
                self.active_tab = self.tabs.len() - 1;
                if let Some(key_setup) = build.key_setup {
                    self.active_dialog = Some(crate::frontend::tui::per_command::key_setup_dialog(
                        &key_setup,
                    ));
                }
            }
            // A daemon this process cannot authenticate to is not worth a tab:
            // every poll would 401 and the grid would show nothing but that.
            // Offer the one recovery there is instead, and build nothing until
            // it is taken — see `Dialog::SquadKeyMissing`.
            Err(SquadStartError::KeyMissing) => {
                self.active_dialog = Some(Dialog::SquadKeyMissing);
            }
            Err(error) => self.report_squad_start_failure(&error),
        }
    }

    /// Raise a squad start failure as a modal, and leave its text in the
    /// status bar behind it. Every variant but `KeyMissing` lands here: each
    /// already carries text that names the problem, so there is nothing to
    /// classify — see [`SquadStartError`].
    fn report_squad_start_failure(&mut self, error: &SquadStartError) {
        let message = error.to_string();
        self.status_bar.text = message.clone();
        self.active_dialog = Some(Dialog::Notice {
            title: "squad daemon did not start".to_string(),
            body: message,
            copy_key: None,
            copy_zshrc_snippet: None,
        });
    }

    /// Build the singleton squad tab: ensure the daemon is running, obtain a
    /// gateway, construct the synthetic-session tab, and start its poller.
    /// Shared by [`App::open_or_focus_squad_tab`] and `tui::run`'s
    /// `InitialTab::Squad` path. On failure returns the specific error text
    /// (naming `awman api` or the sandbox runtime), never a generic message.
    pub(crate) fn build_squad_tab(
        engines: &crate::command::dispatch::Engines,
        runtime_handle: &tokio::runtime::Handle,
    ) -> Result<SquadTabStart, SquadStartError> {
        // A sandbox-class runtime cannot back squad: report the runtime error,
        // never a generic connection failure.
        SquadGatewayResolver::admit_runtime(engines)?;

        // `ensure_squad_gateway` is async, but the caller may be on a tokio
        // worker thread where `Handle::block_on` would panic. Drive it on the
        // runtime and block on the result channel instead
        // (see /awman/context/workflow/deviations.md, D-tui-3).
        //
        // Blocking here is only tolerable because this path runs *before* the
        // event loop exists (bare `awman squad`). Once the TUI is up, the
        // daemon-start confirmation uses `build_and_install_squad_tab`, which
        // waits on the runtime instead of freezing the screen.
        let (tx, rx) = std::sync::mpsc::channel();
        let owned_engines = engines.clone();
        runtime_handle.spawn(async move {
            let _ = tx.send(Self::ensure_squad_gateway(owned_engines).await);
        });
        let startup = match rx.recv() {
            Ok(result) => result,
            Err(_) => Err(SquadStartError::Other(
                "squad daemon startup was interrupted".to_string(),
            )),
        };
        match startup {
            Ok(startup) => Self::assemble_squad_tab(startup, runtime_handle)
                .map(|build| SquadTabStart::Ready(Box::new(build))),
            Err(SquadStartError::KeyMissing) => Ok(SquadTabStart::KeyMissing),
            Err(error) => Err(error),
        }
    }

    /// Ensure a squad daemon is running and return a gateway to it, together
    /// with what this process can authenticate to it with.
    ///
    /// This is the whole waiting half of opening the squad tab, and the only
    /// half that is async. The caller decides how to wait for it: on the
    /// runtime (the TUI's daemon-start confirmation) or by blocking
    /// (`build_squad_tab`, before there is an event loop to freeze).
    pub(crate) async fn ensure_squad_gateway(
        engines: crate::command::dispatch::Engines,
    ) -> SquadStartupResult {
        SquadGatewayResolver::open_from_env(&Env::from_process(), &engines).await
    }

    /// Mint a new bearer key, restart the daemon onto it, and return the same
    /// startup an ordinary open does — so the caller drains both through one
    /// path. The key is always `Minted` here, which is the point: the recovery
    /// ends by showing the user the key they were missing.
    pub(crate) async fn refresh_squad_key() -> SquadStartupResult {
        SquadGatewayResolver::refresh_key_from_env(&Env::from_process()).await
    }

    /// Turn a live startup into the squad tab: the squad-root session, the tab
    /// itself, and its task-list poller. Pure local work — nothing here waits
    /// on the daemon, so it is safe to run on the event-loop thread.
    fn assemble_squad_tab(
        startup: SquadStartup,
        runtime_handle: &tokio::runtime::Handle,
    ) -> Result<SquadTabBuild, SquadStartError> {
        let session = Session::open_squad_root(&Env::from_process()).map_err(|error| {
            SquadStartError::Other(format!("failed to open squad tab: {error}"))
        })?;

        let gateway = startup.gateway;

        let mut tab = Tab::new_squad(session);
        {
            // Ensure a runtime context so the poller's `tokio::spawn` works even
            // when this runs off a runtime thread.
            let _guard = runtime_handle.enter();
            let (poller, cancel) = {
                let state = tab
                    .squad
                    .as_ref()
                    .expect("new_squad always installs squad state");
                (
                    crate::frontend::tui::squad_poll::SquadTaskPoller::new(gateway.clone(), state),
                    state.cancel.clone(),
                )
            };
            let handle = poller.start(cancel);
            tab.squad
                .as_mut()
                .expect("new_squad always installs squad state")
                .set_poll_handle(handle);
        }
        Ok(SquadTabBuild {
            tab,
            gateway,
            key_setup: startup.key_setup,
        })
    }

    /// Spawn a parsed command as an async tokio task, wiring up all channels
    /// between the event loop and the command thread.
    pub fn spawn_command(&mut self, parsed: ParsedCommandBoxInput) {
        let path_refs: Vec<&str> = parsed.path.iter().map(String::as_str).collect();
        if let Err(error) =
            Dispatch::<TuiCommandFrontend>::validate_runtime_admission(&self.engines, &path_refs)
        {
            self.status_bar.text = error.to_string();
            return;
        }

        // Capture the runtime class before borrowing the tab mutably: a
        // sandbox-class runtime (e.g. docker-sbx-experimental) labels the
        // overlay "(sandboxed)" rather than "(containerized)".
        let sandboxed = self.engines.sandbox_runtime.is_some();
        self.clear_active_tab_for_new_command();
        let tab = self.active_tab_mut();

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

        let initial_size = initial_pty_size(tab.git_sidebar_state);

        let container_io = crate::engine::agent_runtime::frontend::AgentIo {
            stdout: stdout_tx.clone(),
            stderr: stdout_tx,
            stdin_tx: stdin_tx_for_engine,
            stdin_rx,
            resize: Some(resize_rx),
            initial_size: Some(initial_size),
        };

        // Build the TUI frontend. Workflow + yolo overlays share the same
        // `Arc<Mutex<...>>` between the engine-side frontend impl and the
        // renderer.
        tab.shared.reset_for_new_command();
        let attach_context = tab
            .squad
            .as_ref()
            .map(|state| (state.attach_session.clone(), state.daemon_reachable.clone()));
        let frontend = TuiCommandFrontend::new(
            parsed.clone(),
            DialogChannels::new(dialog_req_tx, dialog_resp_rx),
            container_io,
            tab.shared(),
        )
        .with_squad_attach_context(
            attach_context.as_ref().map(|(session, _)| session.clone()),
            attach_context.map(|(_, reachable)| reachable),
        );

        tab.command_result_rx = Some(result_rx);
        tab.dialog_request_rx = Some(dialog_req_rx);
        tab.dialog_response_tx = Some(dialog_resp_tx);

        let command_name = parsed.path.join(" ");

        // Build the dispatch before drawing anything: what the tab shows
        // while the command runs — the overlay's agent name, the unattended
        // indicator — is a Layer 2 answer, read off `launch_display` rather
        // than recomputed here from flag names.
        let session_id = self.active_tab().session_id;
        let session = match self.session_manager.get(&session_id) {
            Some(session) => session,
            None => {
                // Test and extension callers may install a prebuilt Tab
                // directly. Normalize that legacy construction at the
                // ownership boundary before dispatching against it.
                self.session_manager
                    .create(self.active_tab().session.clone())
                    .expect("tab session id must be unique");
                self.session_manager
                    .get(&session_id)
                    .expect("registered tab session must be retrievable")
            }
        };
        let engines = self.engines.clone();
        let path_owned: Vec<String> = parsed.path.clone();
        let session_for_state = Arc::clone(&session);
        let mut dispatch = Dispatch::new(frontend, session, engines);
        // WI 0102: in-tab `squad` actions reach the daemon through the same
        // gateway the CLI subcommands use. It is offered for every command;
        // the catalogue's `gateway_need` decides which ones receive it, so
        // the TUI no longer keys the injection on the command's name
        // (WI 0114 F-15).
        if let Some(gateway) = self.squad_gateway.clone() {
            dispatch = dispatch.with_squad_gateway(gateway);
        }
        let display = {
            let path_refs: Vec<&str> = path_owned.iter().map(String::as_str).collect();
            dispatch.launch_display(&path_refs)
        };

        let tab = self.active_tab_mut();

        // Install the command's container slot (the N==1 case of the unified
        // slot model), replacing any slots from the previous command. Its
        // `ContainerInfo` lets the overlay title show the agent name and
        // elapsed time before the engine reports the actual container name;
        // the parser starts at the computed overlay size so agents never lay
        // out against an 80x24 default.
        tab.start_container(
            display.agent_display_name.clone(),
            String::new(),
            initial_size.0,
            initial_size.1,
        );
        if let Some(slot) = tab.focused_slot_mut() {
            slot.container_stdout_rx = Some(stdout_rx);
            slot.container_stdin_tx = Some(stdin_tx);
            slot.container_resize_tx = Some(resize_tx);
            if let Some(info) = slot.container_info.as_mut() {
                info.sandboxed = sandboxed;
            }
        }
        tab.suppress_container_auto_open = false;
        tab.yolo_mode = display.unattended;

        // Record the start on the session rather than on the tab: the tab's
        // `execution_phase` is derived from `SessionState` each tick (Q3,
        // WI 0114 F-22). `Dispatch::run_command` records the same start on the
        // command thread; doing it here too, synchronously, means the tab
        // never renders an idle frame in between. `begin_command` is
        // idempotent for the same command, so the second write is a no-op.
        if let Ok(mut guard) = session_for_state.try_write() {
            guard.state_mut().begin_command(command_name, Vec::new());
        }

        self.runtime_handle.spawn(async move {
            let path_refs: Vec<&str> = path_owned.iter().map(String::as_str).collect();
            let result = dispatch.run_command(&path_refs).await;
            let _ = result_tx.send(result);
        });
    }

    /// Reset the active tab's per-command view state so a new command starts
    /// with a fresh log, no workflow overview, and no stale hit-test rect.
    ///
    /// One of `spawn_command`'s labelled sections, lifted out by WI 0114 F-51.
    fn clear_active_tab_for_new_command(&mut self) {
        let tab = self.active_tab_mut();

        // Clear previous output so the new command starts with a fresh log.
        if let Ok(mut log) = tab.shared.status_log.lock() {
            log.clear();
        }
        if let Ok(mut dash) = tab.shared.status_dashboard.lock() {
            *dash = None;
        }
        tab.scroll_offset = 0;

        // Clear previous workflow state so the overview resets for the new command.
        // Also clear the overview hit-test rect so stale rects from a previous
        // workflow don't intercept mouse-scroll events meant for the container.
        if let Ok(mut guard) = tab.shared.workflow_state.lock() {
            *guard = None;
        }
        if let Ok(mut guard) = tab.shared.workflow_invocation_id.lock() {
            *guard = None;
        }
        tab.last_overview_rect = None;
        tab.last_overview_hlayout = None;
        tab.last_workflow_invocation_id = None;
        tab.workflow_overview_hscroll_offset = 0;
        tab.workflow_overview_hscroll_follow = true;
        if let Ok(mut guard) = tab.shared.yolo_state.lock() {
            *guard = None;
        }
    }

    /// Open and register a directory session, then create its tab view.
    pub fn add_tab(
        &mut self,
        dir: std::path::PathBuf,
        opts: crate::data::session::SessionOpenOptions,
    ) -> Result<usize, crate::data::error::DataError> {
        let session_id = self.session_manager.open_or_create(dir, opts)?;
        let session = self
            .session_manager
            .get(&session_id)
            .expect("newly-created session must be registered")
            .try_read()
            .expect("newly-created session must not be locked")
            .clone();
        let tab = Tab::new_with_git_engine(session, self.engines.git_engine.clone());
        self.tabs.push(tab);
        Ok(self.tabs.len() - 1)
    }

    /// Tick all tabs: drain container output, poll for command completion,
    /// poll for stats results, and recompute the per-tab stuck flag.
    pub fn tick_all_tabs(&mut self) {
        self.refresh_tabs_from_sessions();
        self.refresh_status_context();
        self.drain_stats_samples();
        self.request_stats_round();
        self.refresh_yolo_countdown_dialog();
    }

    /// Advance every tab: adopt its session's state, drain its container and
    /// dialog channels, and recompute what the renderer needs.
    fn refresh_tabs_from_sessions(&mut self) {
        // A squad daemon start runs on the runtime, so the tick is where its
        // result lands — before the tab loop, so a tab installed by this call
        // is polled on the very same tick it appears.
        self.poll_squad_startup();
        self.normalize_focus_for_active_tab();

        // Adopt each session's in-flight state before the per-tab work below,
        // so everything this tick renders or decides sees the same phase
        // (decision Q3, WI 0114 F-22). `try_read` rather than `read`: the tick
        // is synchronous, a command thread may hold the write lock, and one
        // skipped frame is invisible at a ~16ms tick.
        let session_manager = Arc::clone(&self.session_manager);
        for tab in self.tabs.iter_mut() {
            let Some(managed) = session_manager.get(&tab.session_id) else {
                continue;
            };
            let snapshot = managed.try_read().ok().map(|guard| guard.clone());
            if let Some(session) = snapshot {
                tab.refresh_from_session(&session);
            }
        }

        let active = self.active_tab;
        for (idx, tab) in self.tabs.iter_mut().enumerate() {
            // WI 0102: drive the squad tab's poll loop — it fetches only while
            // focused — and publish the selected task name for the poller
            // so it never reads UI state directly.
            if let Some(state) = tab.squad.as_ref() {
                state
                    .focused
                    .store(idx == active, std::sync::atomic::Ordering::Relaxed);
                if let Ok(mut selected) = state.poll_selected.lock() {
                    *selected = state.selected_name();
                }
            }

            // Maintain container_slots before draining output (a Launched
            // event may carry the channels the drain reads) and before
            // aggregating stuck/yolo (drain_stuck_events reads slot flags).
            tab.drain_container_slot_events();
            tab.drain_container_output();
            tab.poll_container_exit();
            tab.poll_command_completion();
            tab.drain_stuck_events();
            // Restart the git poll task if the worktree path changed.
            tab.refresh_git_poll();

            // TUI-4: keep every slot's parser and PTY in lockstep with the
            // actual rendered overlay dimensions. The overlay size varies
            // with Workflow Overview height and other dynamic chrome; the
            // initial `compute_container_inner_size` estimate may not match.
            // All slots render into the same maximized overlay when focused,
            // so they all track the same size; syncing the background slots
            // too means Ctrl-S never swaps in a stale grid.
            if tab.container_window_state
                != crate::frontend::tui::tabs::ContainerWindowState::Hidden
            {
                if let Some(inner) = tab.container_inner_area {
                    for slot in &mut tab.container_slots {
                        let (vt_rows, vt_cols) = slot.vt100_parser.screen().size();
                        if vt_cols != inner.width || vt_rows != inner.height {
                            slot.vt100_parser
                                .screen_mut()
                                .set_size(inner.height, inner.width);
                            if let Some(ref tx) = slot.container_resize_tx {
                                let _ = tx.send((inner.width, inner.height));
                            }
                        }
                    }
                }
            }

            // Pick up the container name from the engine (set via
            // `report_status(Running { container_name })`) into the focused
            // slot. Also handles sequential workflow step transitions: the
            // engine clears the shared name then sets a new one when the
            // next container reports Running. During a parallel group the
            // backbone slot is dormant and per-slot names arrive via
            // `ContainerSlotEvent::ContainerName` instead, so the shared
            // name is left alone to avoid mislabeling a group slot.
            if tab.dormant_slots.is_empty() {
                let name = tab
                    .shared
                    .container_name_shared
                    .lock()
                    .ok()
                    .and_then(|mut g| g.take());
                if let Some(name) = name {
                    if let Some(info) = tab
                        .focused_slot_mut()
                        .and_then(|s| s.container_info.as_mut())
                    {
                        info.container_name = name;
                        info.latest_stats = None;
                    }
                    // A fresh container is running — let its output
                    // auto-open the window again after a mid-workflow
                    // container exit closed it.
                    tab.suppress_container_auto_open = false;
                }
            }

            // Pick up new stdin/resize senders from sequential workflow step
            // transitions. When `recreate_container_io()` runs on the engine
            // thread, it publishes new senders via the shared slots; swap
            // them into the focused (backbone) slot so keystrokes and resize
            // events reach the new container.
            let new_stdin = tab
                .shared
                .stdin_tx_shared
                .lock()
                .ok()
                .and_then(|mut g| g.take());
            let new_resize = tab
                .shared
                .resize_tx_shared
                .lock()
                .ok()
                .and_then(|mut g| g.take());
            if new_stdin.is_some() || new_resize.is_some() {
                // These only come from the sequential path; if a parallel
                // group is active the backbone is dormant — apply them there
                // so they aren't lost (or misrouted to a group slot).
                let target = if tab.dormant_slots.is_empty() {
                    tab.focused_slot_mut()
                } else {
                    tab.dormant_slots.first_mut()
                };
                if let Some(slot) = target {
                    if let Some(new_tx) = new_stdin {
                        slot.container_stdin_tx = Some(new_tx);
                    }
                    if let Some(new_tx) = new_resize {
                        slot.container_resize_tx = Some(new_tx);
                    }
                }
            }
        }
    }

    /// Refresh the TUI context shared with the `status` command.
    ///
    /// Each tab holds a shared slot; the status watch loop reads it on every
    /// tick so it always sees current container-name and stuck state.
    fn refresh_status_context(&mut self) {
        // Refresh the TUI context shared with the status command. Each tab
        // holds a shared slot; the status watch loop reads it on every tick
        // so it always sees current container-name and stuck state.
        {
            use crate::command::commands::status::{StatusCommandTuiContext, TuiTabSnapshot};
            let snapshots: Vec<TuiTabSnapshot> = self
                .tabs
                .iter()
                .enumerate()
                .map(|(i, t)| {
                    let container_name = t
                        .focused_slot()
                        .and_then(|s| s.container_info.as_ref())
                        .map(|info| info.container_name.clone())
                        .filter(|n| !n.is_empty());
                    let command_label = match &t.execution_phase {
                        ExecutionPhase::Running { command } => command.clone(),
                        ExecutionPhase::Done { command, .. } => command.clone(),
                        ExecutionPhase::Error { command, .. } => command.clone(),
                        ExecutionPhase::Idle => String::new(),
                    };
                    TuiTabSnapshot {
                        tab_number: (i + 1) as u32,
                        container_name,
                        is_stuck: t.stuck,
                        command_label,
                    }
                })
                .collect();
            let ctx = StatusCommandTuiContext::new(snapshots);
            for tab in &self.tabs {
                if let Ok(mut g) = tab.shared.tui_context_shared.lock() {
                    *g = ctx.clone();
                }
            }
        }
    }

    /// Route each arrived stats sample to the slot it was polled for.
    ///
    /// Matched by step name; a stale sample whose slot has since exited is
    /// dropped.
    fn drain_stats_samples(&mut self) {
        // Route each arrived sample to the slot it was polled for (matched by
        // step name; a stale sample whose slot has since exited is dropped).
        for sample in self.stats_sampler.drain() {
            if sample.key >= self.tabs.len() {
                continue;
            }
            let tab = &mut self.tabs[sample.key];
            let info = tab
                .container_slots
                .iter_mut()
                .find(|s| s.step_name == sample.step_name)
                .and_then(|s| s.container_info.as_mut());
            if let Some(info) = info {
                let stats = sample.stats;
                info.stats_history
                    .push((stats.cpu_percent, stats.memory_mb));
                if info.container_name.is_empty() {
                    info.container_name = stats.name.clone();
                }
                info.latest_stats = Some(stats);
            }
        }
    }

    /// Ask the sampler for a fresh round when one is due.
    ///
    /// Which slots to sample is the only part of this the view owns; the
    /// cadence, the off-thread dispatch and the one-query-per-slot rule are
    /// the sampler's (F-16).
    fn request_stats_round(&mut self) {
        // Ask for a fresh round when the sampler says one is due. Which slots
        // to sample is the only part of this the view owns; the cadence, the
        // off-thread dispatch and the one-query-per-slot rule are the
        // sampler's (F-16).
        if self.stats_sampler.due() {
            let requests: Vec<StatsRequest> = self
                .tabs
                .iter()
                .enumerate()
                .filter(|(_, tab)| {
                    matches!(
                        tab.execution_phase,
                        crate::frontend::tui::tabs::ExecutionPhase::Running { .. }
                    )
                })
                .flat_map(|(key, tab)| {
                    stats_poll_targets(tab)
                        .into_iter()
                        .map(move |(step_name, target)| StatsRequest {
                            key,
                            step_name,
                            target,
                        })
                })
                .collect();
            self.stats_sampler.poll(requests);
        }
    }

    /// Open, update or close the yolo-countdown modal for the focused slot.
    ///
    /// The engine sets `yolo_state` through the frontend trait; this renders
    /// it as a non-modal overlay dialog. During a parallel group only the
    /// slot actually rendered maximized gets the modal — the others show
    /// their countdown in the minimized bar instead.
    fn refresh_yolo_countdown_dialog(&mut self) {
        let active = self.active_tab;
        // Engine-driven yolo countdown: the engine sets yolo_state via the
        // frontend trait; the TUI renders it as a non-modal overlay dialog.
        //
        // During a parallel group (`dormant_slots` non-empty) yolo countdowns
        // are per-slot: only the slot actually rendered maximized gets the
        // modal here — other yoloing slots show their countdown in the
        // minimized bar instead (`render_one_minimized_bar`). Rotating the
        // focus with Ctrl-S onto/off of a yoloing slot naturally opens/closes
        // the modal on the next tick since this is recomputed fresh each time.
        let active_tab = &self.tabs[active];
        let yolo_snapshot = if active_tab.dormant_slots.is_empty() {
            active_tab
                .shared
                .yolo_state
                .lock()
                .ok()
                .and_then(|g| g.clone())
        } else if active_tab.container_overlay_active() {
            active_tab
                .focused_slot()
                .and_then(|s| s.yolo_state.lock().ok().and_then(|g| g.clone()))
        } else {
            None
        };
        if let Some(state) = yolo_snapshot {
            if !self.command_dialog_active {
                self.active_dialog = Some(Dialog::WorkflowYoloCountdown(
                    crate::frontend::tui::dialogs::WorkflowYoloCountdownState {
                        step_name: state.step_name.clone(),
                        remaining_secs: state.remaining_secs,
                    },
                ));
            }
        } else if matches!(self.active_dialog, Some(Dialog::WorkflowYoloCountdown(_))) {
            self.active_dialog = None;
        }

        // WI 0102: keep the squad task-detail modal live from the active
        // tab's snapshot, matching on the task name it was opened for.
        if matches!(self.active_dialog, Some(Dialog::SquadTaskDetail(_))) {
            let snapshot = self.tabs[active]
                .squad
                .as_ref()
                .and_then(|state| state.snapshot.lock().ok().map(|g| g.clone()));
            if let (Some(Dialog::SquadTaskDetail(detail)), Some(snapshot)) =
                (&mut self.active_dialog, snapshot)
            {
                if let Some(task) = snapshot.tasks.iter().find(|c| c.name == detail.name) {
                    detail.task = task.clone();
                }
            }
        }

        // The run-history modal is kept live the same way. `snapshot.runs`
        // holds the history of the *selected* task, so it is only copied in
        // while the selection is still the task the modal was opened for —
        // otherwise the modal would quietly start showing another task's runs.
        if matches!(self.active_dialog, Some(Dialog::SquadTaskHistory(_))) {
            let polled = self.tabs[active].squad.as_ref().and_then(|state| {
                let selected = state.selected_name()?;
                let runs = state.snapshot.lock().ok().map(|g| g.runs.clone())?;
                Some((selected, runs))
            });
            if let (Some(Dialog::SquadTaskHistory(history)), Some((selected, runs))) =
                (&mut self.active_dialog, polled)
            {
                if selected == history.name {
                    history.runs = runs;
                    // A shrinking history (older runs pruned) must not leave
                    // the table scrolled past its end.
                    history.scroll = history.scroll.min(history.runs.len().saturating_sub(1));
                }
            }
        }

        // WI 0102: while an attach session owns the squad tab and the daemon is
        // unreachable, surface the frozen-overview indicator (the attached
        // container slots keep streaming from their direct runtime connections).
        if let Some(state) = self.tabs[active].squad.as_ref() {
            if state.attached_task().is_some()
                && !state
                    .daemon_reachable
                    .load(std::sync::atomic::Ordering::Relaxed)
            {
                self.status_bar.text =
                    "squad daemon not reachable — workflow overview frozen; attached containers still live"
                        .to_string();
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
                DialogRequest::YesNo { title, body } => Dialog::YesNo { title, body },
                DialogRequest::YesNoCancel { title, body } => Dialog::YesNoCancel { title, body },
                DialogRequest::TextInput {
                    title,
                    prompt,
                    default_text,
                } => {
                    let mut editor = TextEdit::new(false);
                    if let Some(text) = default_text {
                        editor.set_text(&text);
                    }
                    Dialog::TextInput {
                        title,
                        prompt,
                        editor,
                    }
                }
                DialogRequest::MultilineInput {
                    title,
                    prompt,
                    default_text,
                } => Dialog::MultilineInput {
                    title,
                    prompt,
                    editor: {
                        let mut editor = TextEdit::new(true);
                        if let Some(text) = default_text {
                            editor.set_text(&text);
                        }
                        editor
                    },
                },
                DialogRequest::ListPicker { title, items } => Dialog::ListPicker {
                    title,
                    items,
                    selected: 0,
                },
                DialogRequest::KindSelect { title, options } => {
                    Dialog::KindSelect { title, options }
                }
                DialogRequest::WorkflowControlBoard(state) => Dialog::WorkflowControlBoard(state),
                DialogRequest::WorkflowYoloCountdown(state) => Dialog::WorkflowYoloCountdown(state),
                DialogRequest::WorkflowStepConfirm(state) => Dialog::WorkflowStepConfirm(state),
                DialogRequest::AgentSetup(state) => Dialog::AgentSetup(state),
                DialogRequest::MountScope(state) => Dialog::MountScope(state),
                DialogRequest::AgentAuth(state) => Dialog::AgentAuth(state),
                DialogRequest::QuitConfirm => Dialog::QuitConfirm,
                DialogRequest::CloseTabConfirm => Dialog::CloseTabConfirm,
                DialogRequest::WorkflowCancelConfirm => Dialog::WorkflowCancelConfirm,
                DialogRequest::ConfigShow {
                    rows,
                    selected,
                    rejected,
                } => {
                    let selected = selected.min(rows.len().saturating_sub(1));
                    let mut state = crate::frontend::tui::dialogs::ConfigShowState {
                        rows,
                        selected,
                        editing: false,
                        edit_column: 0,
                        editor: TextEdit::new(false),
                        new_entry: None,
                        error: None,
                    };
                    // A rejected edit reopens in edit mode with the typed
                    // value preserved and the reason displayed, so the user
                    // corrects it instead of retyping from scratch.
                    if let Some(rej) = rejected {
                        let has_row = state.rows.iter().any(|r| r.field == rej.field);
                        let new_mapping_key = if has_row {
                            None
                        } else {
                            rej.field
                                .strip_prefix("dynamicWorkflows.agentsToModels.")
                                .map(str::to_string)
                        };
                        if has_row || new_mapping_key.is_some() {
                            state.error = Some(rej.reason);
                            state.editing = true;
                            state.edit_column = if rej.global { 0 } else { 1 };
                            state.editor.set_text(&rej.value);
                            // A rejected Ctrl+N mapping has no row yet —
                            // resume the add flow in its value phase.
                            state.new_entry = new_mapping_key.map(|key| {
                                crate::frontend::tui::dialogs::NewMapEntryPhase::Value { key }
                            });
                        }
                    }
                    Dialog::ConfigShow(state)
                }
                DialogRequest::Loading { title } => Dialog::Loading { title },
                DialogRequest::Custom { title, body, keys } => Dialog::Custom { title, body, keys },
                DialogRequest::KeySetupNotice {
                    title,
                    body,
                    copy_key,
                    copy_zshrc_snippet,
                } => Dialog::Notice {
                    title,
                    body,
                    copy_key: Some(copy_key),
                    copy_zshrc_snippet: Some(copy_zshrc_snippet),
                },
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
        self.suggestion_row = completions.into_iter().map(|c| c.completion).collect();
    }
}

/// Which of a tab's container slots to poll for stats this tick, keyed by the
/// slot's step name (empty for the single/backbone slot of a plain command)
/// so the drain can route each sample back to the slot it was polled for.
///
/// Every slot whose container name is known is polled — during a parallel
/// group that means all N containers, not just the focused one.
///
/// The "first running container" fallback covers the brief window before a
/// plain `chat`/`exec` container reports its name. It is deliberately limited
/// to a lone slot outside a parallel group: with several containers live,
/// "the first running container" is an arbitrary sibling, and because the
/// drain adopts a sample's name for a still-unnamed slot, one guess would pin
/// that slot to a sibling's stats for the rest of the run. A group slot with
/// no name yet simply waits for its `ContainerSlotEvent::ContainerName`.
pub(crate) fn stats_poll_targets(tab: &Tab) -> Vec<(String, StatsTarget)> {
    let in_parallel_group = !tab.dormant_slots.is_empty();
    let sole_slot = tab.container_slots.len() == 1 && !in_parallel_group;
    let mut targets = Vec::with_capacity(tab.container_slots.len());
    for slot in &tab.container_slots {
        let Some(info) = slot.container_info.as_ref() else {
            continue;
        };
        if !info.container_name.is_empty() {
            targets.push((
                slot.step_name.clone(),
                StatsTarget::Named(info.container_name.clone()),
            ));
        } else if sole_slot {
            targets.push((slot.step_name.clone(), StatsTarget::FirstRunning));
        }
    }
    targets
}

/// The PTY grid a container should start with, derived from the terminal.
///
/// Without this a TUI agent inside the container lays out against an 80x24
/// default until the first SIGWINCH. Falls back to 80x24 when the terminal
/// size cannot be read at all. One of `spawn_command`'s labelled sections,
/// lifted out by WI 0114 F-51.
fn initial_pty_size(
    git_sidebar_state: crate::frontend::tui::git_sidebar::GitSidebarState,
) -> (u16, u16) {
    match crossterm::terminal::size() {
        Ok((cols, rows)) => {
            let sidebar = crate::frontend::tui::git_sidebar::sidebar_width(cols, git_sidebar_state);
            crate::frontend::tui::event_loop::compute_container_inner_size(
                cols.saturating_sub(sidebar),
                rows,
            )
        }
        Err(_) => (80u16, 24u16),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::dispatch::catalogue::CommandCatalogue;
    use crate::command::dispatch::Engines;
    use crate::data::session::Session;
    use crate::data::session_manager::SessionManager;
    use crate::frontend::tui::tabs::Tab;
    use std::sync::Arc;

    /// The fixture directory is removed as soon as the session is open: these
    /// tests assert on an `App`'s state machine, never on the workdir's
    /// contents. Unchanged from before WI 0114 F-52, which replaced only the
    /// `Session::open` boilerplate.
    fn make_test_session() -> Session {
        let tmp = tempfile::tempdir().unwrap();
        Session::for_tests(tmp.path())
    }

    /// Shared across every test in this module for the same reason as
    /// `tui::tests::test_runtime_handle`: leaking a fresh multi-threaded
    /// `Runtime` per call exhausts the OS thread budget once enough tests
    /// have run in the same process.
    fn test_runtime_handle() -> tokio::runtime::Handle {
        static RUNTIME: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
        RUNTIME
            .get_or_init(|| tokio::runtime::Runtime::new().unwrap())
            .handle()
            .clone()
    }

    fn make_app() -> App {
        let catalogue = CommandCatalogue::get();
        let engines = Engines::for_tests(std::path::Path::new("/tmp"));
        let session_manager = Arc::new(SessionManager::in_memory());
        let session = make_test_session();
        let tab = Tab::new(session);
        App::new(
            catalogue,
            engines,
            session_manager,
            tab,
            test_runtime_handle(),
        )
    }

    // ── update_suggestions ────────────────────────────────────────────────────

    #[test]
    fn update_suggestions_empty_input_clears_suggestions() {
        let mut app = make_app();
        app.suggestion_row = vec!["chat".to_string()];
        app.command_input.set_text("");
        app.command_input.text.clear();
        app.update_suggestions();
        assert!(
            app.suggestion_row.is_empty(),
            "empty input must clear suggestions"
        );
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

    // ── stats drain pipeline ─────────────────────────────────────────────

    #[test]
    fn stats_drain_populates_latest_stats() {
        let mut app = make_app();
        let tab = app.active_tab_mut();
        tab.execution_phase = crate::frontend::tui::tabs::ExecutionPhase::Running {
            command: "chat".into(),
        };
        // The command's single slot, as spawn_command installs it.
        tab.start_container("Claude".into(), "awman-test-1234".into(), 80, 24);
        assert!(tab
            .focused_slot()
            .and_then(|s| s.container_info.as_ref())
            .unwrap()
            .latest_stats
            .is_none());

        // Simulate a stats result arriving on the channel (empty step name =
        // the single/backbone slot).
        let stats = crate::engine::agent_runtime::execution::AgentStats {
            name: "awman-test-1234".into(),
            cpu_percent: 42.5,
            memory_mb: 256.0,
        };
        app.stats_sampler
            .sender()
            .send(crate::command::stats_sampler::StatsSample {
                key: 0,
                step_name: String::new(),
                stats,
            })
            .unwrap();

        // tick_all_tabs drains the channel.
        app.tick_all_tabs();

        let tab = app.active_tab();
        let info = tab
            .focused_slot()
            .and_then(|s| s.container_info.as_ref())
            .unwrap();
        assert!(
            info.latest_stats.is_some(),
            "latest_stats must be populated after drain"
        );
        let s = info.latest_stats.as_ref().unwrap();
        assert_eq!(s.cpu_percent, 42.5);
        assert_eq!(s.memory_mb, 256.0);
        assert_eq!(s.name, "awman-test-1234");
        assert_eq!(info.stats_history.len(), 1);
    }

    #[test]
    fn stats_drain_routes_parallel_samples_to_the_right_slot() {
        use crate::frontend::tui::tabs::ContainerSlot;

        let mut app = make_app();
        let tab = app.active_tab_mut();
        tab.execution_phase = crate::frontend::tui::tabs::ExecutionPhase::Running {
            command: "exec workflow".into(),
        };
        tab.container_slots
            .push(ContainerSlot::new("build".into(), "claude".into(), 1000));
        tab.container_slots
            .push(ContainerSlot::new("test".into(), "codex".into(), 1000));

        let stats = crate::engine::agent_runtime::execution::AgentStats {
            name: "awman-test-1".into(),
            cpu_percent: 12.5,
            memory_mb: 64.0,
        };
        app.stats_sampler
            .sender()
            .send(crate::command::stats_sampler::StatsSample {
                key: 0,
                step_name: "test".into(),
                stats,
            })
            .unwrap();
        app.tick_all_tabs();

        let tab = app.active_tab();
        let test_info = tab.container_slots[1].container_info.as_ref().unwrap();
        assert_eq!(
            test_info.latest_stats.as_ref().map(|s| s.cpu_percent),
            Some(12.5),
            "the sample must land on the slot it was polled for"
        );
        let build_info = tab.container_slots[0].container_info.as_ref().unwrap();
        assert!(
            build_info.latest_stats.is_none(),
            "the other slot must be untouched"
        );
    }

    // ── stats poll targets ───────────────────────────────────────────────

    fn named_slot(step: &str, container: &str) -> crate::frontend::tui::tabs::ContainerSlot {
        let mut slot =
            crate::frontend::tui::tabs::ContainerSlot::new(step.into(), "claude".into(), 1000);
        if let Some(info) = slot.container_info.as_mut() {
            info.container_name = container.into();
        }
        slot
    }

    #[test]
    fn stats_poll_targets_covers_every_named_slot_in_a_parallel_group() {
        let mut tab = Tab::new(make_test_session());
        // A parallel group: the sequential backbone is dormant and every
        // group slot has had its container name published by the engine.
        tab.dormant_slots
            .push(crate::frontend::tui::tabs::ContainerSlot::new(
                String::new(),
                "claude".into(),
                1000,
            ));
        tab.container_slots.push(named_slot("build", "awman-b-1"));
        tab.container_slots.push(named_slot("test", "awman-t-2"));
        tab.container_slots.push(named_slot("docs", "awman-d-3"));

        assert_eq!(
            stats_poll_targets(&tab),
            vec![
                ("build".into(), StatsTarget::Named("awman-b-1".into())),
                ("test".into(), StatsTarget::Named("awman-t-2".into())),
                ("docs".into(), StatsTarget::Named("awman-d-3".into())),
            ],
            "every container in a parallel group must be polled, not just the focused one"
        );
    }

    #[test]
    fn stats_poll_targets_never_guesses_a_container_inside_a_parallel_group() {
        let mut tab = Tab::new(make_test_session());
        tab.dormant_slots
            .push(crate::frontend::tui::tabs::ContainerSlot::new(
                String::new(),
                "claude".into(),
                1000,
            ));
        // The group is down to one slot and its name hasn't arrived yet.
        tab.container_slots
            .push(crate::frontend::tui::tabs::ContainerSlot::new(
                "build".into(),
                "claude".into(),
                1000,
            ));

        assert!(
            stats_poll_targets(&tab).is_empty(),
            "a group slot with no name yet must wait for it rather than adopting \
             an arbitrary running container's stats"
        );
    }

    #[test]
    fn stats_poll_targets_falls_back_for_a_lone_unnamed_container() {
        let mut tab = Tab::new(make_test_session());
        tab.start_container("Claude".into(), String::new(), 80, 24);

        assert_eq!(
            stats_poll_targets(&tab),
            vec![(String::new(), StatsTarget::FirstRunning)],
            "a plain command's slot still gets stats before its name arrives"
        );
    }

    #[test]
    fn tick_all_tabs_shows_yolo_modal_for_focused_slot_in_parallel_group() {
        use crate::frontend::tui::tabs::{ContainerSlot, ContainerWindowState, YoloState};

        let mut app = make_app();
        let tab = app.active_tab_mut();
        // A parallel group is active: the backbone is stashed in
        // `dormant_slots` and the group's own slots are in `container_slots`.
        tab.dormant_slots
            .push(ContainerSlot::new(String::new(), "claude".into(), 1000));
        tab.container_slots
            .push(ContainerSlot::new("build".into(), "claude".into(), 1000));
        tab.container_slots
            .push(ContainerSlot::new("test".into(), "codex".into(), 1000));
        tab.focused_slot_idx = 1; // "test" is the maximized slot
        tab.container_window_state = ContainerWindowState::Maximized;
        *tab.container_slots[1].yolo_state.lock().unwrap() = Some(YoloState {
            step_name: "test".into(),
            remaining_secs: 12,
        });

        app.tick_all_tabs();

        match &app.active_dialog {
            Some(Dialog::WorkflowYoloCountdown(state)) => {
                assert_eq!(state.step_name, "test");
                assert_eq!(state.remaining_secs, 12);
            }
            _ => panic!("expected WorkflowYoloCountdown for the focused slot"),
        }
    }

    #[test]
    fn tick_all_tabs_hides_yolo_modal_when_yoloing_slot_is_not_focused() {
        use crate::frontend::tui::tabs::{ContainerSlot, ContainerWindowState, YoloState};

        let mut app = make_app();
        let tab = app.active_tab_mut();
        tab.dormant_slots
            .push(ContainerSlot::new(String::new(), "claude".into(), 1000));
        tab.container_slots
            .push(ContainerSlot::new("build".into(), "claude".into(), 1000));
        tab.container_slots
            .push(ContainerSlot::new("test".into(), "codex".into(), 1000));
        tab.focused_slot_idx = 0; // "build" is maximized; "test" is a bar
        tab.container_window_state = ContainerWindowState::Maximized;
        *tab.container_slots[1].yolo_state.lock().unwrap() = Some(YoloState {
            step_name: "test".into(),
            remaining_secs: 12,
        });

        app.tick_all_tabs();

        assert!(
            app.active_dialog.is_none(),
            "the countdown for a non-focused slot belongs in its minimized bar, not a modal"
        );
    }

    #[test]
    fn tick_all_tabs_hides_yolo_modal_when_group_is_minimized() {
        use crate::frontend::tui::tabs::{ContainerSlot, ContainerWindowState, YoloState};

        let mut app = make_app();
        let tab = app.active_tab_mut();
        tab.dormant_slots
            .push(ContainerSlot::new(String::new(), "claude".into(), 1000));
        tab.container_slots
            .push(ContainerSlot::new("test".into(), "codex".into(), 1000));
        tab.focused_slot_idx = 0;
        // Nothing is rendered maximized while Minimized — every slot is a bar.
        tab.container_window_state = ContainerWindowState::Minimized;
        *tab.container_slots[0].yolo_state.lock().unwrap() = Some(YoloState {
            step_name: "test".into(),
            remaining_secs: 12,
        });

        app.tick_all_tabs();

        assert!(
            app.active_dialog.is_none(),
            "no slot is maximized, so no modal should show"
        );
    }

    #[test]
    fn container_name_picked_up_from_shared_state() {
        let mut app = make_app();
        let tab = app.active_tab_mut();
        tab.execution_phase = crate::frontend::tui::tabs::ExecutionPhase::Running {
            command: "chat".into(),
        };
        tab.start_container("Claude".into(), String::new(), 80, 24);

        // Simulate the engine reporting the container name.
        if let Ok(mut guard) = tab.shared.container_name_shared.lock() {
            *guard = Some("awman-new-container".into());
        }

        app.tick_all_tabs();

        let info = app
            .active_tab()
            .focused_slot()
            .and_then(|s| s.container_info.as_ref())
            .unwrap();
        assert_eq!(info.container_name, "awman-new-container");
    }

    #[test]
    fn new_container_name_overwrites_old_and_clears_stats() {
        let mut app = make_app();
        let tab = app.active_tab_mut();
        tab.execution_phase = crate::frontend::tui::tabs::ExecutionPhase::Running {
            command: "exec workflow".into(),
        };
        tab.start_container("Claude".into(), "awman-old-container".into(), 80, 24);
        if let Some(info) = tab
            .focused_slot_mut()
            .and_then(|s| s.container_info.as_mut())
        {
            info.latest_stats = Some(crate::engine::agent_runtime::execution::AgentStats {
                name: "awman-old-container".into(),
                cpu_percent: 10.0,
                memory_mb: 100.0,
            });
            info.stats_history = vec![(10.0, 100.0)];
        }

        // Simulate a workflow step transition reporting a new container name.
        if let Ok(mut guard) = tab.shared.container_name_shared.lock() {
            *guard = Some("awman-step2-container".into());
        }

        app.tick_all_tabs();

        let info = app
            .active_tab()
            .focused_slot()
            .and_then(|s| s.container_info.as_ref())
            .unwrap();
        assert_eq!(info.container_name, "awman-step2-container");
        assert!(
            info.latest_stats.is_none(),
            "latest_stats must be cleared when a new container name arrives"
        );
    }

    #[test]
    fn stats_title_shows_values_when_stats_present() {
        use crate::engine::agent_runtime::execution::AgentStats;
        use crate::frontend::tui::tabs::ContainerInfo;

        let info = ContainerInfo {
            agent_display_name: "Claude".into(),
            container_name: "awman-test".into(),
            start_time: std::time::Instant::now(),
            latest_stats: Some(AgentStats {
                name: "awman-test".into(),
                cpu_percent: 42.5,
                memory_mb: 256.0,
            }),
            stats_history: Vec::new(),
            sandboxed: false,
        };

        let title = crate::frontend::tui::container_view::build_stats_title_from_info(&info);
        assert!(title.contains("42.5%"), "title must contain CPU: {title}");
        assert!(
            title.contains("256MiB"),
            "title must contain memory: {title}"
        );
        assert!(
            title.contains("awman-test"),
            "title must contain name: {title}"
        );
    }

    // ── ConfigShow dialog request: rejected-edit reopen ──────────────────

    fn config_show_row(field: &str) -> crate::frontend::tui::dialogs::ConfigShowRow {
        crate::frontend::tui::dialogs::ConfigShowRow {
            field: field.to_string(),
            global: String::new(),
            repo: String::new(),
            effective: String::new(),
            read_only: false,
            global_writable: false,
            repo_writable: true,
            value_hint: None,
            shape: crate::command::commands::config::ConfigFieldShape::Scalar,
        }
    }

    fn deliver_config_show(
        app: &mut App,
        rows: Vec<crate::frontend::tui::dialogs::ConfigShowRow>,
        rejected: Option<crate::frontend::tui::dialogs::ConfigShowRejectedEdit>,
    ) {
        let (tx, rx) = std::sync::mpsc::channel();
        app.tabs[0].dialog_request_rx = Some(rx);
        tx.send(DialogRequest::ConfigShow {
            rows,
            selected: 0,
            rejected,
        })
        .unwrap();
        app.poll_dialog_requests();
    }

    #[test]
    fn config_show_request_without_rejection_opens_in_browse_mode() {
        let mut app = make_app();
        deliver_config_show(&mut app, vec![config_show_row("agent")], None);

        let Some(Dialog::ConfigShow(state)) = &app.active_dialog else {
            panic!("ConfigShow request must open the dialog");
        };
        assert!(!state.editing);
        assert_eq!(state.error, None);
    }

    #[test]
    fn config_show_request_with_rejection_reopens_edit_with_input_preserved() {
        let mut app = make_app();
        deliver_config_show(
            &mut app,
            vec![config_show_row("dynamicWorkflows.defaultLeader")],
            Some(crate::frontend::tui::dialogs::ConfigShowRejectedEdit {
                field: "dynamicWorkflows.defaultLeader".into(),
                value: "claude".into(),
                global: false,
                reason: "expected agent::model".into(),
            }),
        );

        let Some(Dialog::ConfigShow(state)) = &app.active_dialog else {
            panic!("ConfigShow request must open the dialog");
        };
        assert!(
            state.editing,
            "a rejected edit must reopen in edit mode so the input is not lost"
        );
        assert_eq!(state.edit_column, 1, "repo-scoped edit stays on Repo");
        assert_eq!(
            state.editor.text, "claude",
            "the rejected input must be preserved for correction"
        );
        assert_eq!(state.error.as_deref(), Some("expected agent::model"));
    }

    #[test]
    fn config_show_request_with_rejected_new_mapping_resumes_value_phase() {
        // A rejected Ctrl+N mapping has no table row yet; the dialog must
        // resume the add flow in its value phase for the same key.
        let mut app = make_app();
        deliver_config_show(
            &mut app,
            vec![config_show_row("agent")],
            Some(crate::frontend::tui::dialogs::ConfigShowRejectedEdit {
                field: "dynamicWorkflows.agentsToModels.maki".into(),
                value: "model-a, model-b".into(),
                global: false,
                reason: "could not write config".into(),
            }),
        );

        let Some(Dialog::ConfigShow(state)) = &app.active_dialog else {
            panic!("ConfigShow request must open the dialog");
        };
        assert_eq!(
            state.new_entry,
            Some(crate::frontend::tui::dialogs::NewMapEntryPhase::Value { key: "maki".into() }),
            "the add-mapping flow must resume in the value phase"
        );
        assert!(state.editing);
        assert_eq!(state.editor.text, "model-a, model-b");
        assert_eq!(state.error.as_deref(), Some("could not write config"));
    }
}
