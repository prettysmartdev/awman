//! Application state — the central TUI state object.
//!
//! `App` stores UI state only. Command execution and instance state (sessions,
//! command threads, the squad gateway) go through the Layer 2 `InstanceBackend`,
//! which builds every command through `Dispatch`
//! and the per-command frontend trait chain (WI 0142 §5).

use std::sync::Arc;

use crate::command::dispatch::catalogue::CommandCatalogue;
use crate::command::dispatch::parsed_input::ParsedCommandBoxInput;
use crate::command::dispatch::Engines;
use crate::command::instance::backend::InstanceBackend;
use crate::command::instance::local::LocalInstanceBackend;
use crate::command::instance::{
    CloseTabOutcome, InstanceParts, OpenTabRequest, StartupRun, SubmitRequest, SubmittedCommand,
};
use crate::command::stats_sampler::{ContainerStatsSampler, StatsRequest, StatsTarget};
use crate::data::config::env::Env;
use crate::data::session::{Session, SessionId};
use crate::data::session_manager::SessionManager;
use crate::frontend::tui::command_frontend::TuiPresentation;
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
    /// A runtime notice (an in-place builtin state upgrade). While set it
    /// replaces the hints in the status bar; the next key press clears it.
    pub notice: Option<String>,
}

/// A live squad gateway, as Layer 2 reports it.
pub use crate::command::commands::squad::supervisor::{
    SquadStartError, SquadStartup, SquadStartupResult,
};

/// Everything [`App::assemble_squad_tab`] produces.
pub struct SquadTabBuild {
    pub tab: Tab,
    pub gateway: Arc<dyn crate::command::commands::squad::gateway::TaskGateway>,
}

/// Hand the terminal to a host command, or take it back (see
/// `DialogRequest::TerminalRelease`).
#[derive(Debug)]
pub enum TerminalOp {
    /// `terminal_id` names the hub terminal a daemon-hosted host process
    /// runs in; the event loop bridges the real terminal to it.
    Release {
        title: String,
        terminal_id: Option<crate::data::ids::TerminalId>,
    },
    Restore,
}

/// Central TUI state. Contains NO business logic — only UI state. Instance
/// state (sessions, command threads, the squad gateway) lives behind the
/// Layer 2 [`InstanceBackend`] facade (WI 0142 §5).
pub struct App {
    pub tabs: Vec<Tab>,
    pub active_tab: usize,
    pub active_dialog: Option<Dialog>,
    /// One queued request per tab, retained until that tab is focused. This
    /// also lets the host terminal show a background tab waiting on input.
    pub(super) pending_dialog_requests: std::collections::HashMap<SessionId, DialogRequest>,
    pub focus: Focus,
    pub catalogue: &'static CommandCatalogue,
    /// The instance this TUI presents (builtin: in-process).
    pub backend: Arc<dyn InstanceBackend>,
    pub command_input: TextEdit,
    pub suggestion_row: Vec<String>,
    pub input_error: Option<String>,
    pub status_bar: StatusBar,
    pub should_quit: bool,
    pub needs_redraw: bool,
    pub command_dialog_active: bool,
    pub command_dialog_deadline: Option<std::time::Instant>,
    /// A terminal hand-over the command thread is waiting on (WI 0149); the
    /// event loop performs it and answers.
    pub terminal_op: Option<TerminalOp>,
    pub runtime_handle: tokio::runtime::Handle,
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
    /// the instance's `SquadIndicatorPoller` started in `tui::run`; read by
    /// the renderer. Starts `Unknown` and stays so in unit tests, which never
    /// start the poller.
    pub squad_indicator: crate::frontend::tui::squad_indicator::SharedSquadIndicator,
    /// Window-management requests, claims and engine loss (WI 0142); the
    /// renderer asks it how claimed windows draw.
    pub remote_view: crate::frontend::tui::remote_tab::RemoteTabView,
    /// The Layer 2 worker controller's queue view; `None` when the TUI's
    /// worker is off (WI 0143).
    pub queue: Option<crate::frontend::tui::queue::SharedQueueSource>,
    /// Status-bar badge for new / for-you work items.
    pub queue_badge: crate::frontend::tui::queue::QueueBadge,
    /// Last snapshot generation the badge and open panel reflect.
    pub(super) queue_generation: u64,
}

impl App {
    pub fn new(
        catalogue: &'static CommandCatalogue,
        engines: Engines,
        session_manager: Arc<SessionManager>,
        initial_tab: Tab,
        runtime_handle: tokio::runtime::Handle,
    ) -> Self {
        let backend = Self::local_backend(catalogue, engines, session_manager, &runtime_handle);
        Self::with_backend(backend, initial_tab, runtime_handle)
    }

    /// The builtin engine: an in-process instance over this process's
    /// `Engines`, with the TUI's presentation hooks. Squad operations re-read
    /// the process environment each time, as the TUI always did.
    pub fn local_backend(
        catalogue: &'static CommandCatalogue,
        engines: Engines,
        session_manager: Arc<SessionManager>,
        runtime_handle: &tokio::runtime::Handle,
    ) -> Arc<dyn InstanceBackend> {
        LocalInstanceBackend::open(InstanceParts {
            catalogue,
            engines,
            session_manager,
            runtime: runtime_handle.clone(),
            env: Arc::new(Env::from_process),
            presentation: Arc::new(TuiPresentation),
        })
    }

    /// Present `backend`'s instance, starting with `initial_tab`.
    pub fn with_backend(
        backend: Arc<dyn InstanceBackend>,
        initial_tab: Tab,
        runtime_handle: tokio::runtime::Handle,
    ) -> Self {
        let stats_sampler = backend.stats_sampler();
        // `App` accepts a pre-built tab for rendering, but the instance owns
        // the Session object. This also covers the synthetic squad tab.
        backend.adopt_session(&initial_tab.session);
        let squad_indicator = backend.squad_health();
        Self {
            tabs: vec![initial_tab],
            active_tab: 0,
            active_dialog: None,
            pending_dialog_requests: std::collections::HashMap::new(),
            focus: Focus::CommandBox,
            catalogue: backend.catalogue(),
            backend,
            command_input: TextEdit::new(false),
            suggestion_row: Vec::new(),
            input_error: None,
            status_bar: StatusBar::default(),
            should_quit: false,
            needs_redraw: true,
            command_dialog_active: false,
            command_dialog_deadline: None,
            terminal_op: None,
            runtime_handle,
            squad_startup_rx: None,
            stats_sampler,
            last_active_tab: 0,
            squad_indicator,
            remote_view: crate::frontend::tui::remote_tab::RemoteTabView::new(),
            queue: None,
            queue_badge: Default::default(),
            queue_generation: u64::MAX,
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
        if let Some(tab) = self.tabs.get(self.active_tab) {
            self.backend.focus_hint(&tab.session_id);
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
        // The instance cancels the command it runs in this tab (a dashboard
        // watch or attach session included, which has nowhere left to draw),
        // drops the tab from its order and, for the last tab, ends itself.
        // The view list below is ours.
        let outcome = self.backend.close_tab(&self.active_tab().session_id);
        // An ended instance admits nothing more, so its last-tab policy
        // decides the quit as much as our own view list does.
        if outcome == CloseTabOutcome::ClosedLastTab || self.tabs.len() <= 1 {
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
    /// first when that would change config or start a daemon.
    ///
    /// On failure nothing is created: the error text — which already names
    /// a daemon conflict or unsupported runtime — is surfaced
    /// verbatim in the status bar, never a generic "could not connect".
    /// Idempotent: a second call focuses the tab created by the first.
    ///
    /// Opening the tab enables squad in global config when necessary and
    /// starts the daemon when necessary. The confirmation describes both.
    pub fn open_or_focus_squad_tab(&mut self) {
        if let Some(idx) = self.tabs.iter().position(|t| t.is_squad) {
            self.active_tab = idx;
            self.needs_redraw = true;
            return;
        }
        // The runtime refusal comes first: a sandbox-class runtime cannot back
        // squad at all, so asking whether to start a daemon would be asking a
        // question whose "yes" could not be honoured.
        if let Err(error) = self.backend.squad_admission() {
            self.status_bar.text = error.to_string();
            return;
        }
        let enable_config = match self.backend.squad_enabled_in_config() {
            Ok(enabled) => !enabled,
            Err(error) => {
                self.status_bar.text = format!("could not read squad config: {error}");
                return;
            }
        };
        let start_daemon = match self.backend.squad_daemon_is_running() {
            Ok(running) => !running,
            // When the state is uncertain, startup may still need to spawn a
            // daemon. Keep that possibility in the confirmation.
            Err(_) => true,
        };
        if enable_config || start_daemon {
            self.active_dialog = Some(Dialog::SquadStartConfirm {
                enable_config,
                start_daemon,
            });
            self.needs_redraw = true;
        } else {
            self.build_and_install_squad_tab();
        }
    }

    /// Start the squad tab and install it as the active tab, enabling squad
    /// in config and starting the daemon when needed. The second half of
    /// [`App::open_or_focus_squad_tab`], separated so the daemon-start
    /// confirmation can call it on `y`.
    ///
    /// Starting a daemon means spawning a process and then waiting for it to
    /// publish its endpoint. Doing that inline froze the
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
        if let Err(error) = self.backend.squad_admission() {
            self.status_bar.text = error.to_string();
            self.needs_redraw = true;
            return;
        }

        self.squad_startup_rx = Some(self.backend.start_squad_gateway());
        self.active_dialog = Some(Dialog::Loading {
            title: "Opening squad".to_string(),
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

        // `open_squad_session` registers the squad tab with the instance, as
        // the startup squad path does.
        let built = result.and_then(|startup| {
            let session = self.backend.open_squad_session()?;
            Ok(Self::assemble_squad_tab(
                session,
                startup.gateway,
                &self.runtime_handle,
            ))
        });
        match built {
            Ok(build) => {
                self.backend.set_squad_gateway(build.gateway);
                self.active_tab = self.push_tab(build.tab);
            }
            Err(error) => self.report_squad_start_failure(&error),
        }
    }

    /// Raise a squad start failure as a modal, and leave its text in the
    /// status bar behind it. Every variant lands here: each
    /// already carries text that names the problem, so there is nothing to
    /// classify — see [`SquadStartError`].
    fn report_squad_start_failure(&mut self, error: &SquadStartError) {
        let message = error.to_string();
        self.status_bar.text = message.clone();
        self.active_dialog = Some(Dialog::Notice {
            title: "could not open squad".to_string(),
            body: message,
            copy_key: None,
            copy_zshrc_snippet: None,
        });
    }

    /// Turn a squad session and gateway into the squad tab: the tab itself
    /// and its task-list poller (a Layer 3 view over the `TaskGateway`). Pure
    /// local work — nothing here waits on the daemon, so it is safe to run on
    /// the event-loop thread. Shared by [`App::poll_squad_startup`] and
    /// `tui::run`'s `InitialTab::Squad` path.
    pub(crate) fn assemble_squad_tab(
        session: Session,
        gateway: Arc<dyn crate::command::commands::squad::gateway::TaskGateway>,
        runtime_handle: &tokio::runtime::Handle,
    ) -> SquadTabBuild {
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
        SquadTabBuild { tab, gateway }
    }

    /// Submit a parsed command into the active tab. The instance builds and
    /// spawns it (channels, command frontend, `Dispatch`, command thread —
    /// `EngineInstance::submit`) and cancels whatever the tab was running;
    /// this resets the tab's view for the new command and binds the returned
    /// feeds to it.
    pub fn spawn_command(&mut self, parsed: ParsedCommandBoxInput) {
        self.spawn_command_with_lease(parsed, None);
    }

    pub(crate) fn spawn_command_with_lease(
        &mut self,
        parsed: ParsedCommandBoxInput,
        lease_id: Option<String>,
    ) {
        if let Err(error) = self.backend.admit(&parsed.path) {
            self.status_bar.text = error.to_string();
            return;
        }

        self.clear_active_tab_for_new_command();
        let tab = self.active_tab();
        let initial_size = initial_pty_size(terminal_size(), tab.git_sidebar_state);
        let squad_attach =
            tab.squad
                .as_ref()
                .map(|state| crate::command::instance::feeds::SharedSquadAttach {
                    session: state.attach_session.clone(),
                    daemon_reachable: state.daemon_reachable.clone(),
                });
        let request = SubmitRequest {
            lease_id,
            session_id: tab.session_id,
            parsed,
            feeds: tab.shared(),
            pty_size: initial_size,
            squad_attach,
        };
        let submitted = match self.backend.submit(request) {
            Ok(submitted) => submitted,
            Err(error) => {
                self.status_bar.text = error.to_string();
                return;
            }
        };
        let idx = self.active_tab;
        self.bind_submitted(idx, submitted, initial_size);
    }

    /// Bind a tab's startup run (the instance chose and started it) to the
    /// tab view at `idx`. `pty_size` is the size the tab was opened with.
    pub fn bind_startup(&mut self, idx: usize, startup: StartupRun, pty_size: (u16, u16)) {
        match startup {
            StartupRun::NotStarted => {}
            StartupRun::Started(submitted) => self.bind_submitted(idx, submitted, pty_size),
            StartupRun::Refused(error) => self.status_bar.text = error.to_string(),
        }
    }

    /// Bind a submitted command's feeds to the tab view at `idx`.
    fn bind_submitted(&mut self, idx: usize, submitted: SubmittedCommand, size: (u16, u16)) {
        let Some(tab) = self.tabs.get_mut(idx) else {
            return;
        };
        tab.command_result_rx = Some(submitted.feeds.command_result_rx);
        tab.dialog_request_rx = Some(submitted.feeds.dialog_request_rx);
        tab.dialog_response_tx = Some(submitted.feeds.dialog_response_tx);
        tab.reported_pty_size = Some(size);

        // Install the command's container slot (the N==1 case of the unified
        // slot model), replacing any slots from the previous command. Its
        // `ContainerInfo` lets the overlay title show the agent name and
        // elapsed time before the engine reports the actual container name;
        // the parser starts at the computed overlay size so agents never lay
        // out against an 80x24 default.
        let display = submitted.display;
        tab.start_container(
            display.agent_display_name.clone(),
            String::new(),
            size.0,
            size.1,
        );
        if let Some(slot) = tab.focused_slot_mut() {
            slot.bind_io(submitted.slot_io);
            if let Some(info) = slot.container_info.as_mut() {
                info.sandboxed = submitted.sandboxed;
            }
        }
        tab.suppress_container_auto_open = false;
        tab.yolo_mode = display.unattended;
    }

    /// Reset the active tab's per-command view state so a new command starts
    /// with a fresh log, no workflow overview, and no stale hit-test rect.
    ///
    /// One of `spawn_command`'s labelled sections, lifted out by WI 0114 F-51.
    fn clear_active_tab_for_new_command(&mut self) {
        let tab = self.active_tab_mut();

        tab.shared.reset_for_new_command();
        tab.scroll_offset = 0;

        // Lifecycle handles are replaced before the next command is built.
        tab.last_overview_rect = None;
        tab.last_overview_hlayout = None;
        tab.last_workflow_invocation_id = None;
        tab.workflow_overview_hscroll_offset = 0;
        tab.workflow_overview_hscroll_follow = true;
        tab.engine_tx = None;
    }

    /// The agent-window PTY size a new directory tab (git sidebar closed)
    /// opens with on the current terminal.
    pub fn new_tab_pty_size() -> (u16, u16) {
        initial_pty_size(
            terminal_size(),
            crate::frontend::tui::git_sidebar::GitSidebarState::Closed,
        )
    }

    /// Open a directory tab through the instance — which registers it and
    /// starts its startup command — then create its view and bind that run.
    pub fn add_tab(
        &mut self,
        dir: std::path::PathBuf,
        opts: crate::data::session::SessionOpenOptions,
    ) -> Result<usize, crate::command::error::CommandError> {
        let pty_size = Self::new_tab_pty_size();
        let opened = self.backend.open_tab(OpenTabRequest {
            dir,
            options: opts,
            pty_size,
        })?;
        let (tab, startup) = Tab::from_opened(opened);
        self.tabs.push(tab);
        let idx = self.tabs.len() - 1;
        self.bind_startup(idx, startup, pty_size);
        Ok(idx)
    }

    /// Add a tab view the presenter built itself (the squad tab, test
    /// fixtures), registering its session with the instance first.
    pub fn push_tab(&mut self, tab: Tab) -> usize {
        self.backend.adopt_session(&tab.session);
        self.tabs.push(tab);
        self.tabs.len() - 1
    }

    /// Report each tab's current agent-window PTY size to the instance when
    /// it changed (terminal resize, git sidebar toggle). D8: the frontend
    /// chooses the size, and the instance starts every later launch in the
    /// tab — a workflow step, a parallel peer, an attach — at it.
    fn report_pty_sizes(&mut self) {
        let terminal = terminal_size();
        for tab in &mut self.tabs {
            let size = initial_pty_size(terminal, tab.git_sidebar_state);
            if tab.reported_pty_size != Some(size) {
                self.backend.resize_tab(&tab.session_id, size);
                tab.reported_pty_size = Some(size);
            }
        }
    }

    /// Tick all tabs: drain container output, poll for command completion,
    /// poll for stats results, and recompute the per-tab stuck flag.
    pub fn tick_all_tabs(&mut self) {
        if self.command_dialog_active
            && self
                .command_dialog_deadline
                .is_some_and(|deadline| std::time::Instant::now() >= deadline)
        {
            super::dialog_router::dismiss_dialog(self);
            self.command_dialog_deadline = None;
        }
        self.refresh_tabs_from_sessions();
        self.refresh_agents_context();
        self.drain_stats_samples();
        self.request_stats_round();
        self.refresh_yolo_countdown_dialog();
        self.report_pty_sizes();
        // Last, so the window-management transitions of this tick are
        // reported and engine events land before the next frame.
        self.tick_queue();
        crate::frontend::tui::remote_tab::RemoteTabView::sync(self);
    }

    /// Mirror the worker controller's queue into the badge (and the open
    /// panel) whenever its generation changes (WI 0143).
    fn tick_queue(&mut self) {
        let Some(source) = self.queue.clone() else {
            return;
        };
        let generation = source.generation();
        if generation == self.queue_generation {
            return;
        }
        self.queue_generation = generation;
        let snapshot = source.snapshot();
        if let Some(Dialog::QueuePanel(state)) = &mut self.active_dialog {
            state.apply(snapshot);
            source.mark_seen();
            self.queue_badge = Default::default();
        } else {
            self.queue_badge = crate::frontend::tui::queue::QueueBadge {
                for_you: snapshot.for_you,
                new: snapshot.new_since_seen,
            };
        }
        self.needs_redraw = true;
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
        for tab in self.tabs.iter_mut() {
            if let Some(session) = self.backend.session_snapshot(&tab.session_id) {
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

            // Lifecycle events carry identity and I/O together, in engine order.
            tab.drain_container_slot_events();
            tab.drain_container_output();
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
        }
    }

    /// Refresh the TUI context shared with `agents status`.
    ///
    /// Each tab holds a shared slot; the `agents status --watch` loop reads it
    /// on every tick so it always sees current container-name and stuck state.
    fn refresh_agents_context(&mut self) {
        {
            use crate::command::commands::agents::{AgentsTuiContext, TuiTabSnapshot};
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
                        program_status: t.program_status(),
                        command_label,
                    }
                })
                .collect();
            let ctx = AgentsTuiContext::new(snapshots);
            for tab in &self.tabs {
                if let Ok(mut g) = tab.shared.tui_context_shared.lock() {
                    *g = ctx.clone();
                }
            }
        }
    }

    /// Route each arrived stats sample to the slot it was polled for.
    ///
    /// Matched by launch identity, including across retries and tab-index shifts.
    fn drain_stats_samples(&mut self) {
        // The sampler echoes our opaque launch identity in `step_name`. A
        // tab index alone can be reused after a close, and step names repeat.
        for sample in self.stats_sampler.drain() {
            if sample.key >= self.tabs.len() {
                continue;
            }
            let tab = &mut self.tabs[sample.key];
            let info = tab
                .container_slots
                .iter_mut()
                .find(|s| s.launch_id.to_string() == sample.step_name)
                .and_then(|s| s.container_info.as_mut());
            if let Some(info) = info {
                info.record_stats(sample.stats);
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
            if state.remaining_secs == 0 && self.command_dialog_active {
                super::dialog_router::dismiss_dialog(self);
            }
            if !self.command_dialog_active {
                self.active_dialog = Some(Dialog::WorkflowYoloCountdown(
                    crate::frontend::tui::dialogs::WorkflowYoloCountdownState {
                        step_name: state.step_name.clone(),
                        remaining_secs: state.remaining_secs,
                        kind: state.kind,
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
                    // An open run detail follows its run by id, so a run that
                    // finishes while it is up shows its final status. A run
                    // pruned from the history keeps its last copy.
                    if let Some(detail) = history.run_detail.as_mut() {
                        if let Some(run) = runs.iter().find(|r| r.id == detail.run.id) {
                            detail.run = run.clone();
                        }
                    }
                    history.runs = runs;
                    // A shrinking history (older runs pruned) must not leave
                    // the selection past its end.
                    history.selected = history.selected.min(history.runs.len().saturating_sub(1));
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

    /// Show runtime notices in the status bar (the same one-line text the
    /// CLI prints on stderr).
    pub fn show_runtime_notices(&mut self, notices: Vec<String>) {
        if !notices.is_empty() {
            self.status_bar.notice = Some(notices.join("; "));
        }
    }

    /// Pick up notices from a runtime that opened after startup (a deferred
    /// builtin backend whose first open was retried by a later command).
    pub fn poll_runtime_notices(&mut self) {
        let notices = self.backend.take_runtime_notices();
        self.show_runtime_notices(notices);
    }

    /// Check the active tab's dialog_request_rx and open the corresponding
    /// dialog in the App.
    pub fn poll_dialog_requests(&mut self) {
        self.pending_dialog_requests
            .retain(|id, _| self.tabs.iter().any(|tab| tab.session_id == *id));
        for tab in &self.tabs {
            if let std::collections::hash_map::Entry::Vacant(entry) =
                self.pending_dialog_requests.entry(tab.session_id)
            {
                if let Some(request) = tab
                    .dialog_request_rx
                    .as_ref()
                    .and_then(|rx| rx.try_recv().ok())
                {
                    entry.insert(request);
                }
            }
        }
        self.dismiss_stale_report_boards();
        let request = self
            .pending_dialog_requests
            .remove(&self.tabs[self.active_tab].session_id);

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
                DialogRequest::SecretInput { title, prompt } => Dialog::SecretInput {
                    title,
                    prompt,
                    buffer: crate::frontend::tui::dialogs::SecretBuffer::default(),
                },
                // The event loop owns the terminal: park the operation for it
                // and let it answer once the screen has changed.
                DialogRequest::TerminalRelease { title, terminal_id } => {
                    self.terminal_op = Some(TerminalOp::Release { title, terminal_id });
                    return;
                }
                DialogRequest::TerminalRestore => {
                    self.terminal_op = Some(TerminalOp::Restore);
                    return;
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
                        use crate::command::commands::config::ConfigRowKind;
                        let existing = state.rows.iter().position(|r| r.field == rej.field);
                        let new_member = state.rows.iter().enumerate().find_map(|(index, row)| {
                            let member = rej.field.strip_prefix(row.shape.child_prefix()?)?;
                            if member.is_empty() {
                                return None;
                            }
                            let phase = match row.shape {
                                ConfigRowKind::MapHeader { .. } => {
                                    crate::frontend::tui::dialogs::NewMapEntryPhase::Value {
                                        key: member.to_string(),
                                    }
                                }
                                ConfigRowKind::ListHeader { .. } => {
                                    crate::frontend::tui::dialogs::NewMapEntryPhase::ListEntry
                                }
                                _ => return None,
                            };
                            Some((index, phase))
                        });
                        if let Some(index) =
                            existing.or_else(|| new_member.as_ref().map(|(i, _)| *i))
                        {
                            state.selected = index;
                            state.error = Some(rej.reason);
                            state.editing = true;
                            state.edit_column = if rej.global { 0 } else { 1 };
                            state.editor.set_text(&rej.value);
                            // A rejected Ctrl+N member has no row yet.
                            if existing.is_none() {
                                state.new_entry = new_member.map(|(_, phase)| phase);
                            }
                        }
                    }
                    Dialog::ConfigShow(state)
                }
                DialogRequest::Loading { title } => Dialog::Loading { title },
                DialogRequest::Custom { title, body, keys } => Dialog::Custom { title, body, keys },
                DialogRequest::Review(state) => Dialog::Review(state.into()),
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
            self.command_dialog_deadline = self
                .active_tab()
                .shared
                .yolo_state
                .lock()
                .ok()
                .and_then(|state| {
                    state.as_ref().map(|s| {
                        std::time::Instant::now() + std::time::Duration::from_secs(s.remaining_secs)
                    })
                });
        }
    }

    /// WI 0141: close a workflow board the agent's own report opened once
    /// the command says the agent resumed before the user touched it (see
    /// `TuiCommandFrontend::ask_report_dialog`). A visible board is closed;
    /// one still queued for a background tab is dropped. Either way the
    /// command gets `Dismissed`, and the step keeps running.
    fn dismiss_stale_report_boards(&mut self) {
        fn is_board(request: &DialogRequest) -> bool {
            matches!(
                request,
                DialogRequest::WorkflowControlBoard(_) | DialogRequest::WorkflowStepConfirm(_)
            )
        }
        for index in 0..self.tabs.len() {
            if !self.tabs[index].shared.signals.take_report_dialog_dismiss() {
                continue;
            }
            let session = self.tabs[index].session_id;
            let visible = index == self.active_tab
                && matches!(
                    self.active_dialog,
                    Some(Dialog::WorkflowControlBoard(_) | Dialog::WorkflowStepConfirm(_))
                );
            let queued = self
                .pending_dialog_requests
                .get(&session)
                .is_some_and(is_board);
            if visible {
                self.active_dialog = None;
            } else if queued {
                self.pending_dialog_requests.remove(&session);
            } else {
                continue;
            }
            if let Some(tx) = &self.tabs[index].dialog_response_tx {
                let _ = tx.send(DialogResponse::Dismissed);
            }
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
/// slot's launch identity, so a sample cannot cross a retry or tab close.
///
/// Every slot whose container name is known is polled — during a parallel
/// group that means all N containers, not just the focused one.
///
/// The "first running container" fallback covers the brief window before a
/// plain `chat`/`exec` container reports its name. It is deliberately limited
/// to a lone slot outside a parallel group: with several containers live,
/// "the first running container" is an arbitrary sibling. Samples update only
/// stats; lifecycle events own the container identity. A group slot with
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
                slot.launch_id.to_string(),
                StatsTarget::Named(info.container_name.clone()),
            ));
        } else if sole_slot {
            targets.push((slot.launch_id.to_string(), StatsTarget::FirstRunning));
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
    terminal: Option<(u16, u16)>,
    git_sidebar_state: crate::frontend::tui::git_sidebar::GitSidebarState,
) -> (u16, u16) {
    match terminal {
        Some((cols, rows)) => {
            let sidebar = crate::frontend::tui::git_sidebar::sidebar_width(cols, git_sidebar_state);
            crate::frontend::tui::event_loop::compute_container_inner_size(
                cols.saturating_sub(sidebar),
                rows,
            )
        }
        None => (80u16, 24u16),
    }
}

/// This process's terminal size, the input to every PTY-size decision.
fn terminal_size() -> Option<(u16, u16)> {
    crossterm::terminal::size().ok()
}
