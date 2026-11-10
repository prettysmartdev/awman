//! awman instances (WI 0142 Part A §2): everything one TUI process holds today
//! minus rendering and input. `EngineInstance` owns the `SessionManager`, the
//! ordered tabs with their command threads and dialog channels, the producer
//! side of the container lifecycle feeds, the git-poll source, the squad
//! indicator, the squad gateway and the active-tab hint. Builtin (`local`) and
//! daemon (`remote`, `host`) frontends use the same Layer 2 contracts; every
//! command is built through `Dispatch` against the Layer 0 `Session` the
//! instance's `SessionManager` owns.
//!
//! Submodules (contract: `/awman/context/workflow/0142-design.md` §4.7–§4.8):
//! - `feeds`: the command-owned half of the former `TabSharedState`
//!   (`TabCommandFeeds`, `ContainerSlotEvent`, `ContainerLifecycle`, …)
//! - `frontend`: `InstanceCommandFrontend`, the per-command frontend impls
//!   that produce those feeds (moved from the TUI, D2)
//! - `backend`: the `InstanceBackend` trait a presenting frontend holds
//! - `local`: `LocalInstanceBackend`, the builtin in-process engine
//! - `remote`: `RemoteInstanceBackend`, the daemon-backed client
//! - `host`: `EngineInstanceHost`, the daemon-side instance host
//! - `relay`: `TabRelay`, feeds → `EngineFrame`s (the cut line)
//!
//! Ownership boundary with the presenter (TUI `App`/`Tab`): the instance never
//! holds a view object. A presenting tab keeps the consumer ends of its feeds
//! (`TabFeeds`, `TabCommandFeeds`, the slot I/O) plus its own view state; the
//! instance keeps the session, the tab's identity and order, and the command
//! thread it spawned.

pub mod admission;
pub mod backend;
pub mod feeds;
pub mod frontend;
pub mod host;
pub mod lifecycle;
pub mod local;
pub mod output;
pub mod per_command;
mod queue;
pub mod relay;
pub mod remote;
#[cfg(test)]
pub(crate) mod test_support;
#[cfg(any(test, feature = "test-support"))]
pub mod trace;

use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};

use chrono::{DateTime, Utc};
use tokio_util::sync::CancellationToken;

use crate::command::commands::squad::gateway::TaskGateway;
use crate::command::commands::squad::indicator::{
    EnvSource, SharedSquadHealth, SquadIndicatorPoller,
};
use crate::command::commands::squad::supervisor::{
    DaemonResolver, SquadStartError, SquadStartupResult,
};
use crate::command::dispatch::catalogue::{CommandCatalogue, FrontendKind};
use crate::command::dispatch::parsed_input::ParsedCommandBoxInput;
use crate::command::dispatch::{CommandOutcome, Dispatch, Engines, LaunchDisplay};
use crate::command::error::CommandError;
use crate::command::git_summary::GitSummaryPoller;
use crate::command::stats_sampler::ContainerStatsSampler;
use crate::data::annotations::Annotations;
use crate::data::dialog::{DialogRequest, DialogResponse};
use crate::data::ids::{InstanceId, TabId};
use crate::data::session::{Session, SessionId, SessionOpenOptions};
use crate::data::session_manager::SessionManager;

use feeds::{ContainerSlotIo, SharedPtySize, SharedSquadAttach, TabCommandFeeds, TabFeeds};
use frontend::{DialogChannels, InstanceCommandFrontend, InstancePresentation};

/// What builds an [`EngineInstance`]. The builtin TUI supplies its process
/// `Engines`, a session manager seeded with the startup session's environment,
/// the runtime it runs on, a re-capturing [`EnvSource`] and its own
/// presentation hooks.
pub struct InstanceParts {
    pub catalogue: &'static CommandCatalogue,
    pub engines: Engines,
    pub session_manager: Arc<SessionManager>,
    pub runtime: tokio::runtime::Handle,
    /// The environment squad operations resolve against (daemon paths, keys).
    pub env: EnvSource,
    pub presentation: Arc<dyn InstancePresentation>,
}

/// What an engine-hosted instance adds to [`InstanceParts`] (WI 0142 §2):
/// the id the host reserved, the frontend kind its commands run as
/// ([`FrontendKind::ENGINE_LOCAL`]), the hub host-process PTYs register
/// with, and the shared executor its commands take permits from.
pub struct HostedOptions {
    pub id: InstanceId,
    pub kind: FrontendKind,
    pub hub: Option<Arc<crate::engine::terminal_hub::TerminalHub>>,
    pub executor: Option<Arc<crate::engine::daemon::executor::SharedExecutor>>,
}

/// How the first tab of an instance opens — exactly `tui::run()`'s choices.
pub enum InitialTabSpec {
    /// A directory-bound tab on an already-opened session. The instance
    /// starts the tab's startup command (`ready` / `agents status --watch`)
    /// itself, unless `fatal_startup_error` says the presenter opens only to
    /// show a fatal startup error. `pty_size` is the presenter's agent-window
    /// size for that first launch (D8).
    Session {
        session: Session,
        fatal_startup_error: bool,
        pty_size: (u16, u16),
    },
    /// The singleton squad tab (`awman squad`). `fallback` is the cwd session
    /// a normal tab degrades to when squad cannot open. Neither the squad tab
    /// nor the fallback runs a startup command.
    Squad { fallback: Session },
}

/// A request to open a directory tab (Ctrl-T in the TUI).
pub struct OpenTabRequest {
    pub dir: PathBuf,
    pub options: SessionOpenOptions,
    /// The presenter's agent-window PTY size `(cols, rows)` for the tab.
    pub pty_size: (u16, u16),
}

/// What became of a tab's startup command. The instance decides whether a
/// tab runs one and which; the presenter only binds the result.
pub enum StartupRun {
    /// No startup command for this tab (squad, fallback, fatal start).
    NotStarted,
    /// The startup command is running; bind its feeds to the tab.
    Started(SubmittedCommand),
    /// The startup command was refused (runtime admission); show the error.
    Refused(CommandError),
}

/// A directory tab the instance opened: its session, the git-summary source
/// the presenter's tab polls, the tab's command feeds (shared with any
/// command the instance runs in it) and its startup command.
pub struct OpenedTab {
    pub session: Session,
    pub git_summary: GitSummaryPoller,
    pub feeds: TabCommandFeeds,
    pub startup: StartupRun,
}

/// The result of [`EngineInstance::open_initial`].
pub enum InitialTabOpened {
    /// A normal tab, with its startup command already submitted.
    Normal { tab: OpenedTab },
    /// The squad tab's registered session and gateway; the presenter builds
    /// its view (card grid and poller) over the gateway.
    Squad {
        session: Session,
        gateway: Arc<dyn TaskGateway>,
    },
    /// Squad could not open: a normal tab on the fallback session, with no
    /// startup command, and the error to surface.
    SquadFailed {
        tab: OpenedTab,
        error: SquadStartError,
    },
}

/// One command submission for a tab: everything `App::spawn_command` used to
/// assemble around `Dispatch`.
pub struct SubmitRequest {
    pub lease_id: Option<String>,
    pub session_id: SessionId,
    pub parsed: ParsedCommandBoxInput,
    /// The tab's fresh feeds (reset by the presenter for this command).
    pub feeds: TabCommandFeeds,
    /// The agent window's PTY size `(cols, rows)`; also recorded as the
    /// tab's current size for later launches.
    pub pty_size: (u16, u16),
    /// The squad tab's attach context, for commands run in the squad tab.
    pub squad_attach: Option<SharedSquadAttach>,
}

/// What a submission hands back to the presenting tab.
pub struct SubmittedCommand {
    /// Layer 2's answer to "what does this run look like" (agent name,
    /// unattended indicator).
    pub display: LaunchDisplay,
    /// Whether the runtime is sandbox-class (labels the overlay).
    pub sandboxed: bool,
    /// Consumer ends of the command result and dialog channels.
    pub feeds: TabFeeds,
    /// The first container slot's I/O (the N==1 slot).
    pub slot_io: ContainerSlotIo,
}

/// Outcome of closing a tab.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CloseTabOutcome {
    Closed,
    /// The last tab closed: today's policy ends the instance (as
    /// [`EngineInstance::quit`] does) and the presenter quits.
    ClosedLastTab,
}

/// What the presenter does when the instance quits (today's TUI exit policy:
/// running commands are cancelled, containers are not detached, exit code 0).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuitPlan {
    pub detach_containers: bool,
    pub exit_code: i32,
    pub cancel_running: bool,
}

/// One spawned command thread and the token that cancels it. The instance
/// keeps every run until its thread finishes — a replaced or closed tab's run
/// included — so job counts and shutdown always cover it.
struct CommandRun {
    cancel: CancellationToken,
    task: tokio::task::JoinHandle<()>,
}

impl CommandRun {
    fn is_running(&self) -> bool {
        !self.task.is_finished()
    }
}

/// The instance's record of one tab: identity, order, the presenter's PTY
/// size and the tab's current command.
struct InstanceTab {
    tab_id: TabId,
    session_id: SessionId,
    pty_size: SharedPtySize,
    run: Option<CommandRun>,
}

/// Everything tab open/close, command admission and shutdown synchronise on.
/// One lock, so a close or quit can never interleave with a submission
/// between its admission check and the installation of its run.
#[derive(Default)]
struct Lifecycle {
    /// Set by quit, by closing the last tab and by drop; nothing new is
    /// admitted afterwards.
    ended: bool,
    tabs: Vec<InstanceTab>,
    /// Cancelled runs whose threads have not finished yet.
    retired: Vec<CommandRun>,
}

impl Lifecycle {
    fn tab_mut(&mut self, session_id: &SessionId) -> Option<&mut InstanceTab> {
        self.tabs
            .iter_mut()
            .find(|tab| &tab.session_id == session_id)
    }

    /// Register a tab for `session_id` (idempotent). Refused once ended.
    fn register(&mut self, session_id: SessionId, pty_size: (u16, u16)) -> bool {
        if self.ended {
            return false;
        }
        if self.tab_mut(&session_id).is_none() {
            self.tabs.push(InstanceTab {
                tab_id: TabId::new(),
                session_id,
                pty_size: SharedPtySize::new(pty_size),
                run: None,
            });
        }
        true
    }

    /// Cancel a run and keep it until its thread finishes.
    fn retire(&mut self, run: CommandRun) {
        run.cancel.cancel();
        if run.is_running() {
            self.retired.push(run);
        }
    }

    fn running_jobs(&mut self) -> usize {
        self.retired.retain(CommandRun::is_running);
        let current = self
            .tabs
            .iter()
            .filter(|tab| tab.run.as_ref().is_some_and(CommandRun::is_running))
            .count();
        current + self.retired.len()
    }

    /// End the instance: refuse new work and cancel every run. Idempotent.
    fn end(&mut self) {
        self.ended = true;
        let runs: Vec<CommandRun> = self.tabs.iter_mut().filter_map(|t| t.run.take()).collect();
        for run in runs {
            self.retire(run);
        }
        for run in &self.retired {
            run.cancel.cancel();
        }
    }
}

fn instance_ended() -> CommandError {
    CommandError::Other("this awman instance has ended".to_string())
}

fn no_tab(session_id: &SessionId) -> CommandError {
    CommandError::Other(format!(
        "no open tab for session {session_id} in this instance"
    ))
}

/// One awman instance (WI 0142 §2).
pub struct EngineInstance {
    queue: Mutex<Option<crate::command::worker::queue_source::SharedQueueSource>>,
    input_available: std::sync::atomic::AtomicBool,
    wake: Mutex<Arc<tokio::sync::Notify>>,
    #[cfg(any(test, feature = "test-support"))]
    trace: Arc<trace::OutcomeTrace>,
    id: InstanceId,
    created_at: DateTime<Utc>,
    catalogue: &'static CommandCatalogue,
    engines: Engines,
    session_manager: Arc<SessionManager>,
    runtime: tokio::runtime::Handle,
    env: EnvSource,
    presentation: Arc<dyn InstancePresentation>,
    squad_gateway: Mutex<Option<Arc<dyn TaskGateway>>>,
    squad_health: SharedSquadHealth,
    lifecycle: Mutex<Lifecycle>,
    active_hint: Mutex<Option<SessionId>>,
    annotations: Mutex<Annotations>,
    /// The frontend kind commands run as: `Tui` for the builtin engine,
    /// `ENGINE_LOCAL` when the daemon hosts the instance.
    kind: FrontendKind,
    hub: Option<Arc<crate::engine::terminal_hub::TerminalHub>>,
    executor: Option<Arc<crate::engine::daemon::executor::SharedExecutor>>,
}

impl Drop for EngineInstance {
    /// Dropping the last handle ends the instance like a quit: no command it
    /// started outlives it uncancelled, whether or not a presenter's views
    /// (which cancel their own feeds) still exist.
    fn drop(&mut self) {
        match self.lifecycle.get_mut() {
            Ok(lifecycle) => lifecycle.end(),
            Err(poisoned) => poisoned.into_inner().end(),
        }
    }
}

impl EngineInstance {
    pub fn new(parts: InstanceParts) -> Arc<Self> {
        Self::build(parts, None)
    }

    /// An instance the daemon `engine` hosts for its frontends.
    pub fn new_hosted(parts: InstanceParts, hosted: HostedOptions) -> Arc<Self> {
        Self::build(parts, Some(hosted))
    }

    fn build(parts: InstanceParts, hosted: Option<HostedOptions>) -> Arc<Self> {
        let InstanceParts {
            catalogue,
            engines,
            session_manager,
            runtime,
            env,
            presentation,
        } = parts;
        let (id, kind, hub, executor) = match hosted {
            Some(h) => (h.id, h.kind, h.hub, h.executor),
            None => (InstanceId::new(), FrontendKind::Tui, None, None),
        };
        Arc::new(Self {
            queue: Default::default(),
            input_available: std::sync::atomic::AtomicBool::new(kind.can_ask_a_human()),
            wake: Mutex::new(Arc::new(tokio::sync::Notify::new())),
            #[cfg(any(test, feature = "test-support"))]
            trace: Arc::new(trace::OutcomeTrace::default()),
            id,
            kind,
            hub,
            executor,
            created_at: Utc::now(),
            catalogue,
            engines,
            session_manager,
            runtime,
            env,
            presentation,
            squad_gateway: Mutex::new(None),
            squad_health: Arc::new(Mutex::new(crate::engine::squad::SquadHealth::Unknown)),
            lifecycle: Mutex::new(Lifecycle::default()),
            active_hint: Mutex::new(None),
            annotations: Mutex::new(Annotations::default()),
        })
    }

    pub fn id(&self) -> &InstanceId {
        &self.id
    }
    pub fn set_wake(&self, wake: Arc<tokio::sync::Notify>) {
        *self.wake.lock().unwrap() = wake;
    }
    pub fn set_input_available(&self, available: bool) {
        self.input_available
            .store(available, std::sync::atomic::Ordering::Relaxed);
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn outcome_trace(&self) -> Arc<trace::OutcomeTrace> {
        self.trace.clone()
    }

    pub fn created_at(&self) -> DateTime<Utc> {
        self.created_at
    }

    pub fn catalogue(&self) -> &'static CommandCatalogue {
        self.catalogue
    }

    pub fn annotations(&self) -> Annotations {
        self.annotations
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }

    /// The lifecycle lock. A panic while it was held must not turn into a
    /// silently skipped cancellation, so poisoning is ignored.
    fn lifecycle(&self) -> MutexGuard<'_, Lifecycle> {
        self.lifecycle
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Whether the instance has ended (quit, last tab closed).
    pub fn has_ended(&self) -> bool {
        self.lifecycle().ended
    }

    // ── Tabs ─────────────────────────────────────────────────────────────

    /// Open the first tab exactly as `tui::run()` does, starting its
    /// startup command when it has one.
    pub fn open_initial(&self, spec: InitialTabSpec) -> InitialTabOpened {
        match spec {
            InitialTabSpec::Session {
                session,
                fatal_startup_error,
                pty_size,
            } => {
                self.session_manager
                    .create(session.clone())
                    .expect("startup session id must be unique");
                self.lifecycle().register(session.id(), pty_size);
                #[cfg(any(test, feature = "test-support"))]
                self.trace.record("session_created", session.working_dir());
                // Only a tab opened to show a fatal startup error runs no
                // startup command.
                InitialTabOpened::Normal {
                    tab: self.opened(session, !fatal_startup_error, pty_size),
                }
            }
            InitialTabSpec::Squad { fallback } => match self.open_squad_blocking() {
                Ok((session, gateway)) => InitialTabOpened::Squad { session, gateway },
                Err(error) => {
                    // `main.rs` calls `ensure_running` before routing here, so
                    // this should not happen; degrade to a normal tab on the
                    // cwd session and surface the specific error.
                    self.adopt_session(&fallback);
                    InitialTabOpened::SquadFailed {
                        tab: self.opened(fallback, false, SharedPtySize::DEFAULT),
                        error,
                    }
                }
            },
        }
    }

    /// Build the presenter's handle on a registered tab and, when
    /// `start_startup`, start its startup command: `ready` for a git repo,
    /// `agents status --watch` otherwise (the catalogue's startup policy).
    fn opened(&self, session: Session, start_startup: bool, pty_size: (u16, u16)) -> OpenedTab {
        let feeds = TabCommandFeeds::new();
        let startup = if !start_startup {
            StartupRun::NotStarted
        } else {
            match self.submit(SubmitRequest {
                lease_id: None,
                session_id: session.id(),
                parsed: self.startup_command(&session),
                feeds: feeds.clone(),
                pty_size,
                squad_attach: None,
            }) {
                Ok(submitted) => StartupRun::Started(submitted),
                Err(error) => StartupRun::Refused(error),
            }
        };
        OpenedTab {
            session,
            git_summary: GitSummaryPoller::new(self.engines.git_engine.clone()),
            feeds,
            startup,
        }
    }

    /// Open and register a directory session for a new tab and start its
    /// startup command (`ready` / `agents status --watch`).
    pub fn open_tab(&self, request: OpenTabRequest) -> Result<OpenedTab, CommandError> {
        self.open_tab_with_startup(request, true)
    }

    pub fn open_tab_with_startup(
        &self,
        request: OpenTabRequest,
        start_startup: bool,
    ) -> Result<OpenedTab, CommandError> {
        let OpenTabRequest {
            dir,
            options,
            pty_size,
        } = request;
        if self.has_ended() {
            return Err(instance_ended());
        }
        let session_id = self.session_manager.open_or_create(dir, options)?;
        let session = self
            .session_manager
            .get(&session_id)
            .expect("newly-created session must be registered")
            .try_read()
            .expect("newly-created session must not be locked")
            .clone();
        if !self.lifecycle().register(session_id, pty_size) {
            return Err(instance_ended());
        }
        #[cfg(any(test, feature = "test-support"))]
        self.trace.record("session_created", session.working_dir());
        Ok(self.opened(session, start_startup, pty_size))
    }

    /// Make sure the manager owns `session` and the instance lists its tab.
    /// Idempotent; covers tabs a presenter built itself (test fixtures) and
    /// the squad tab. Does nothing once the instance has ended.
    pub fn adopt_session(&self, session: &Session) {
        let id = session.id();
        if self.has_ended() {
            return;
        }
        if self.session_manager.get(&id).is_none() {
            #[cfg(any(test, feature = "test-support"))]
            self.trace.record("session_created", session.working_dir());
            self.session_manager
                .create(session.clone())
                .expect("tab session id must be unique");
        }
        self.lifecycle().register(id, SharedPtySize::DEFAULT);
    }

    /// The session's current state, if no command holds its write lock.
    pub fn session_snapshot(&self, session_id: &SessionId) -> Option<Session> {
        let managed = self.session_manager.get(session_id)?;
        let snapshot = managed.try_read().ok().map(|guard| guard.clone());
        snapshot
    }

    /// Close a tab: cancel its running command and drop it from the order.
    /// The session stays with the manager, as before the split, but no
    /// command is admitted for it again. Closing the last tab ends the
    /// instance.
    pub fn close_tab(&self, session_id: &SessionId) -> CloseTabOutcome {
        let mut lifecycle = self.lifecycle();
        if let Some(idx) = lifecycle
            .tabs
            .iter()
            .position(|tab| &tab.session_id == session_id)
        {
            let tab = lifecycle.tabs.remove(idx);
            if let Some(run) = tab.run {
                lifecycle.retire(run);
            }
        }
        if lifecycle.tabs.is_empty() {
            lifecycle.end();
            CloseTabOutcome::ClosedLastTab
        } else {
            CloseTabOutcome::Closed
        }
    }

    /// The tab ids in order.
    pub fn tab_ids(&self) -> Vec<TabId> {
        self.lifecycle()
            .tabs
            .iter()
            .map(|tab| tab.tab_id.clone())
            .collect()
    }

    /// The tab id of the tab showing `session_id`.
    pub fn tab_id_for(&self, session_id: &SessionId) -> Option<TabId> {
        self.lifecycle()
            .tab_mut(session_id)
            .map(|tab| tab.tab_id.clone())
    }

    /// The session a tab shows.
    pub fn session_for_tab(&self, tab_id: &TabId) -> Option<SessionId> {
        self.lifecycle()
            .tabs
            .iter()
            .find(|tab| &tab.tab_id == tab_id)
            .map(|tab| tab.session_id)
    }

    /// The open tabs in order, with their sessions.
    pub fn tab_entries(&self) -> Vec<(TabId, SessionId)> {
        self.lifecycle()
            .tabs
            .iter()
            .map(|tab| (tab.tab_id.clone(), tab.session_id))
            .collect()
    }

    /// The frontend kind this instance's commands run as.
    pub fn kind(&self) -> FrontendKind {
        self.kind
    }

    /// Record the agent-window PTY size the tab's presenter now shows. Every
    /// later launch in the tab starts at it (D8).
    pub fn resize_tab(&self, session_id: &SessionId, pty_size: (u16, u16)) {
        if let Some(tab) = self.lifecycle().tab_mut(session_id) {
            tab.pty_size.set(pty_size);
        }
    }

    /// The tab's current agent-window PTY size, if the tab is open.
    pub fn tab_pty_size(&self, session_id: &SessionId) -> Option<(u16, u16)> {
        self.lifecycle()
            .tab_mut(session_id)
            .map(|tab| tab.pty_size.get())
    }

    /// Record which tab the (primary) frontend shows. A hint only: every
    /// frontend keeps its own focus.
    pub fn set_active_tab_hint(&self, session_id: &SessionId) {
        if let Ok(mut hint) = self.active_hint.lock() {
            *hint = Some(*session_id);
        }
    }

    pub fn active_tab_hint(&self) -> Option<SessionId> {
        self.active_hint.lock().ok().and_then(|hint| *hint)
    }

    /// Commands whose thread is still running, including replaced and
    /// closed-tab runs that are still winding down after cancellation.
    pub fn running_jobs(&self) -> usize {
        self.lifecycle().running_jobs()
    }

    /// Today's TUI exit policy: end the instance (nothing new is admitted)
    /// and cancel every running command. Idempotent; the squad daemon and
    /// its containers are not touched.
    pub fn quit(&self) -> QuitPlan {
        self.lifecycle().end();
        self.queue.lock().unwrap().take();
        QuitPlan {
            detach_containers: false,
            exit_code: 0,
            cancel_running: true,
        }
    }

    // ── Commands ─────────────────────────────────────────────────────────

    /// The startup command for a directory tab (`ready` for a git repo,
    /// `agents status --watch` otherwise).
    pub fn startup_command(&self, session: &Session) -> ParsedCommandBoxInput {
        self.catalogue.startup_command(session)
    }

    /// The catalogue's runtime-tier admission, synchronously, so a refusal
    /// can be shown before the presenter resets the tab for a new command.
    pub fn admit(&self, path: &[String]) -> Result<(), CommandError> {
        let path_refs: Vec<&str> = path.iter().map(String::as_str).collect();
        Dispatch::<InstanceCommandFrontend>::validate_runtime_admission(&self.engines, &path_refs)
    }

    /// Build and spawn one command for an open tab: today's
    /// `App::spawn_command` minus the view work. Creates the dialog,
    /// container I/O and result channels, builds the command frontend and
    /// `Dispatch` against the manager-owned session, records the start on the
    /// session, and spawns the command thread on the instance runtime.
    ///
    /// A tab runs one command at a time: a new submission replaces the
    /// running one, which is cancelled and kept until its thread finishes.
    /// A closed or never-opened tab, or an ended instance, admits nothing.
    pub fn submit(&self, request: SubmitRequest) -> Result<SubmittedCommand, CommandError> {
        let SubmitRequest {
            lease_id,
            session_id,
            mut parsed,
            feeds,
            pty_size,
            squad_attach,
        } = request;
        let lease_execution = lease_id.is_some();
        let picked = if let Some(id) = lease_id {
            let source =
                self.queue.lock().unwrap().clone().ok_or_else(|| {
                    CommandError::Other("this instance has no worker queue".into())
                })?;
            let run = source.prepared_run(&id).ok_or_else(|| {
                CommandError::Other("this instance does not own the picked lease".into())
            })?;
            parsed =
                crate::command::dispatch::parsed_input::parse(&run.command_line, self.catalogue)?;
            Some((source, run))
        } else {
            None
        };
        self.admit(&parsed.path)?;
        #[cfg(any(test, feature = "test-support"))]
        self.trace.record("command", serde_json::json!({"path":parsed.path,"args":parsed.args.to_argv(self.catalogue.lookup(&parsed.path.iter().map(String::as_str).collect::<Vec<_>>()).expect("parsed command"))}));

        // Early refusal, before anything is built. The binding check is the
        // one under the lifecycle lock below.
        let (tab_pty_size, tab_id) = {
            let mut lifecycle = self.lifecycle();
            if lifecycle.ended {
                return Err(instance_ended());
            }
            let tab = lifecycle
                .tab_mut(&session_id)
                .ok_or_else(|| no_tab(&session_id))?;
            tab.pty_size.set(pty_size);
            (tab.pty_size.clone(), tab.tab_id.clone())
        };
        let host_terminals = self.hub.as_ref().map(|hub| frontend::HostTerminals {
            hub: Arc::clone(hub),
            owner: crate::data::terminal::TerminalOwnerInfo {
                owner: crate::data::terminal::TerminalOwner::Tab(tab_id),
                feature: crate::data::daemon_state::FeatureName::Engine,
                source: None,
                attempt: 1,
                allow_remote_input: false,
            },
        });
        let session = self
            .session_manager
            .get(&session_id)
            .ok_or_else(|| no_tab(&session_id))?;

        // Dialog channels (std::sync::mpsc — command thread blocks on recv).
        let (dialog_req_tx, dialog_req_rx) = std::sync::mpsc::channel::<DialogRequest>();
        let (dialog_resp_tx, dialog_resp_rx) = std::sync::mpsc::channel::<DialogResponse>();

        // Container I/O channels — tokio mpsc throughout so the engine PTY
        // bridge can use them from async tasks. The presenter keeps a clone
        // of the stdin sender (for user keystrokes) and the engine receives
        // both sender and receiver for the PTY bridge plus inject_prompt.
        let (stdout_tx, stdout_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let (stdin_tx, stdin_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let stdin_tx_for_engine = stdin_tx.clone();
        let (resize_tx, resize_rx) = tokio::sync::mpsc::unbounded_channel::<(u16, u16)>();

        // Command result channel.
        let (result_tx, result_rx) =
            std::sync::mpsc::channel::<Result<CommandOutcome, CommandError>>();

        let container_io = crate::engine::agent_runtime::frontend::AgentIo {
            stdout: stdout_tx.clone(),
            stderr: stdout_tx,
            stdin_tx: stdin_tx_for_engine,
            stdin_rx,
            resize: Some(resize_rx),
            initial_size: Some(pty_size),
        };

        let command_cancel = feeds.command_cancel.clone();
        #[cfg(any(test, feature = "test-support"))]
        feeds.container.set_trace(self.trace.clone());
        let command_bytes = feeds.command_bytes.clone();
        let wake = self.wake.lock().unwrap().clone();
        let frontend = InstanceCommandFrontend::new(
            parsed.clone(),
            DialogChannels::new(dialog_req_tx, dialog_resp_rx),
            container_io,
            feeds,
        )
        .with_kind(self.kind)
        .with_input_available(
            self.input_available
                .load(std::sync::atomic::Ordering::Relaxed),
        )
        .with_wake(wake.clone())
        .with_host_terminals(host_terminals)
        .with_presentation(Arc::clone(&self.presentation))
        .with_pty_size(tab_pty_size)
        .with_squad_attach(squad_attach);

        #[cfg(any(test, feature = "test-support"))]
        let frontend = frontend.with_trace(self.trace.clone());

        let command_name = parsed.path.join(" ");
        let path_owned: Vec<String> = parsed.path.clone();
        let session_for_state = Arc::clone(&session);
        let mut dispatch = Dispatch::new(frontend, session, self.engines.clone())
            .with_cancellation(command_cancel.clone());
        if let Some((source, run)) = picked {
            dispatch = dispatch.with_picked_lease(source, run);
        }
        // WI 0102: in-tab `squad` actions reach the daemon through the same
        // gateway the CLI subcommands use. It is offered for every command;
        // the catalogue's `gateway_need` decides which ones receive it
        // (WI 0114 F-15).
        if let Some(gateway) = self.squad_gateway() {
            dispatch = dispatch.with_task_gateway(gateway);
        }
        // What the tab shows while the command runs — the overlay's agent
        // name, the unattended indicator — is a Layer 2 answer.
        let display = {
            let path_refs: Vec<&str> = path_owned.iter().map(String::as_str).collect();
            dispatch.launch_display(&path_refs)
        };

        // Admission and installation happen under the lifecycle lock that
        // close and quit take, so the tab cannot close (and the instance
        // cannot end) between the check and the run being owned here.
        let mut lifecycle = self.lifecycle();
        if lifecycle.ended {
            return Err(instance_ended());
        }
        let previous = match lifecycle.tab_mut(&session_id) {
            Some(tab) => tab.run.take(),
            None => return Err(no_tab(&session_id)),
        };
        // Replace: the previous command is cancelled before the new one runs.
        if let Some(previous) = previous {
            lifecycle.retire(previous);
        }

        // Record the start on the session: the tab's `execution_phase` is
        // derived from `SessionState` each tick (Q3, WI 0114 F-22).
        // `Dispatch::run_command` records the same start on the command
        // thread; doing it here too, synchronously and before the spawn,
        // means the tab never renders an idle frame in between.
        // `begin_command` is idempotent for the same command.
        if let Ok(mut guard) = session_for_state.try_write() {
            guard.state_mut().begin_command(command_name, Vec::new());
        }

        // A picked lease already holds the worker admission share. Taking an
        // additional engine permit would deadlock a host with capacity one.
        let executor = if lease_execution || parsed.path == ["queue", "pick"] {
            None
        } else {
            self.executor.clone()
        };
        let command_runtime = self.runtime.clone();
        #[cfg(any(test, feature = "test-support"))]
        let trace = self.trace.clone();
        let task = self.runtime.spawn(async move {
            // Hosted commands share the daemon executor under the engine's
            // `maxConcurrentJobs` (WI 0142 §1, executor share).
            let _permit = match executor {
                Some(executor) => Some(
                    executor
                        .acquire(crate::data::daemon_state::FeatureName::Engine)
                        .await,
                ),
                None => None,
            };
            // Frontend trait methods ask synchronous questions. Run the
            // command on a dedicated worker: parking a Tokio worker here can
            // also strand the host pump in that worker's non-stealable LIFO
            // slot, leaving the dialog that would wake the command unsent.
            let handle = command_runtime.clone();
            let (finished_tx, finished_rx) = tokio::sync::oneshot::channel();
            // A command can synchronously enter the dynamic-workflow launch
            // stack. Give it the CLI main thread's normal stack budget;
            // Tokio's default 2 MiB blocking workers are too small in debug
            // builds. The executor permit above still bounds concurrency.
            let worker = std::thread::Builder::new()
                .name("awman-command".into())
                .stack_size(8 * 1024 * 1024)
                .spawn(move || {
                    let result = handle.block_on(Box::pin(async move {
                        let path_refs: Vec<&str> = path_owned.iter().map(String::as_str).collect();
                        // Command futures can include a complete dynamic
                        // workflow. Keep their state on the heap rather than
                        // copying it through the blocking worker's stack.
                        let result = Box::pin(dispatch.run_command(&path_refs)).await;
                        command_bytes.wait_drained().await;
                        result
                    }));
                    let _ = finished_tx.send(result);
                });
            let result = match worker {
                Ok(_) => finished_rx.await.unwrap_or_else(|_| {
                    Err(CommandError::Other(
                        "command task ended unexpectedly".into(),
                    ))
                }),
                Err(error) => Err(CommandError::Other(format!(
                    "could not start command worker: {error}"
                ))),
            };
            #[cfg(any(test, feature = "test-support"))]
            trace.record("result", relay::command_result_wire(&result));
            let _ = result_tx.send(result);
            wake.notify_one();
        });
        if let Some(tab) = lifecycle.tab_mut(&session_id) {
            tab.run = Some(CommandRun {
                cancel: command_cancel,
                task,
            });
        }
        drop(lifecycle);

        Ok(SubmittedCommand {
            display,
            sandboxed: self.engines.is_sandboxed(),
            feeds: TabFeeds {
                command_result_rx: result_rx,
                dialog_request_rx: dialog_req_rx,
                dialog_response_tx: dialog_resp_tx,
            },
            slot_io: ContainerSlotIo {
                stdout_rx,
                stdin_tx,
                resize_tx,
            },
        })
    }

    /// Notices the container runtime queued since the last call.
    pub fn take_runtime_notices(&self) -> Vec<String> {
        self.engines.take_runtime_notices()
    }

    /// The container stats sampler for this instance's runtime.
    pub fn stats_sampler(&self) -> ContainerStatsSampler {
        ContainerStatsSampler::new(self.engines.runtime.clone(), self.runtime.clone())
    }

    // ── Squad ────────────────────────────────────────────────────────────

    /// The squad daemon health the indicator poller publishes.
    pub fn squad_health(&self) -> SharedSquadHealth {
        Arc::clone(&self.squad_health)
    }

    /// Start the squad indicator poller for the life of the instance.
    pub fn start_squad_indicator(&self, cancel: CancellationToken) -> tokio::task::JoinHandle<()> {
        let _guard = self.runtime.enter();
        SquadIndicatorPoller::new(self.squad_health(), Arc::clone(&self.env)).start(cancel)
    }

    /// Whether this instance's runtime can back squad at all.
    pub fn squad_admission(&self) -> Result<(), SquadStartError> {
        DaemonResolver::admit_runtime(&self.engines)
    }

    /// Whether opening squad would change the persistent global config.
    pub fn squad_enabled_in_config(&self) -> Result<bool, CommandError> {
        DaemonResolver::from_env(&(self.env)())?.squad_enabled_in_config()
    }

    /// Whether a squad daemon is already running for this squad root.
    pub fn squad_daemon_is_running(&self) -> Result<bool, CommandError> {
        DaemonResolver::from_env(&(self.env)())?.daemon_is_running()
    }

    /// Ensure a squad daemon is running and obtain a gateway, on the
    /// instance runtime; the result arrives on the returned channel.
    pub fn start_squad_gateway(&self) -> std::sync::mpsc::Receiver<SquadStartupResult> {
        let (tx, rx) = std::sync::mpsc::channel();
        let engines = self.engines.clone();
        let env = (self.env)();
        self.runtime.spawn(async move {
            let _ = tx.send(DaemonResolver::open_from_env(&env, &engines).await);
        });
        rx
    }

    /// Open the squad tab's synthetic session, rooted at the squad storage
    /// root, and register its tab — the one squad-open operation both the
    /// startup path and an in-app open use, so the squad tab is in the
    /// instance's order as soon as it exists.
    pub fn open_squad_session(&self) -> Result<Session, SquadStartError> {
        if self.has_ended() {
            return Err(SquadStartError::Other(
                "this awman instance has ended".to_string(),
            ));
        }
        let session = Session::open_squad_root(&(self.env)()).map_err(|error| {
            SquadStartError::Other(format!("failed to open squad tab: {error}"))
        })?;
        self.adopt_session(&session);
        Ok(session)
    }

    /// Install the squad gateway Dispatch offers every command.
    pub fn set_squad_gateway(&self, gateway: Arc<dyn TaskGateway>) {
        if let Ok(mut slot) = self.squad_gateway.lock() {
            *slot = Some(gateway);
        }
    }

    pub fn squad_gateway(&self) -> Option<Arc<dyn TaskGateway>> {
        self.squad_gateway.lock().ok().and_then(|slot| slot.clone())
    }

    /// The blocking squad open used before any event loop exists (bare
    /// `awman squad`): admission, then the gateway, then the session.
    fn open_squad_blocking(&self) -> Result<(Session, Arc<dyn TaskGateway>), SquadStartError> {
        // A sandbox-class runtime cannot back squad: report the runtime error,
        // never a generic connection failure.
        self.squad_admission()?;
        // Drive the async start on the runtime and block on its channel: the
        // caller may be on a tokio worker where `Handle::block_on` panics.
        let startup = match self.start_squad_gateway().recv() {
            Ok(result) => result,
            Err(_) => Err(SquadStartError::Other(
                "squad daemon startup was interrupted".to_string(),
            )),
        }?;
        let session = self.open_squad_session()?;
        self.set_squad_gateway(Arc::clone(&startup.gateway));
        Ok((session, startup.gateway))
    }
}

#[cfg(test)]
mod tests;
