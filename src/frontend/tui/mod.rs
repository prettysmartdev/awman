//! TUI frontend — Ratatui-based interactive terminal UI.
//!
//! Captures the terminal (raw mode, alternate screen, mouse), constructs
//! `App` state, enters the event loop, and restores the terminal on exit.

use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

static TUI_ACTIVE: AtomicBool = AtomicBool::new(false);

pub fn is_tui_active() -> bool {
    TUI_ACTIVE.load(Ordering::Relaxed)
}

use crate::command::commands::view::{ViewArgs, ViewEntry};
use crate::command::dispatch::catalogue::CommandCatalogue;
use crate::command::dispatch::RuntimeContext;
use crate::command::engine_handoff::{
    self, BackendSelection, DaemonLink, EngineHandoff, HandoffError, HandoffPrompter,
    LocalDaemonLink,
};
use crate::command::error::CommandError;
use crate::command::instance::backend::InstanceBackend;
use crate::command::instance::remote::{RemoteInstanceBackend, RemoteOpen};
use crate::command::instance::{InitialTabOpened, InitialTabSpec, StartupRun};
use crate::command::integrations::IntegrationRegistry;
use crate::data::config::env::EnvSnapshot;
use crate::data::config::tui::TuiBackend;
use crate::data::frontend_protocol::{
    FrontendKindWire, FrontendRole, Hello, TermSize, ViewScope, FRONTEND_SUBPROTOCOL,
};
use crate::data::session::Session;
use crate::data::session_manager::SessionManager;

pub mod acp_view;
pub mod app;
pub mod command_box;
pub mod command_frontend;
pub mod container_view;
mod dialog_router;
pub mod dialogs;
mod event_loop;
pub mod git_sidebar;
pub mod hints;
mod input_reader;
mod key_handler;
pub mod keymap;
mod mouse;
mod mouse_handler;
pub mod program_status_emitter;
pub mod pty;
mod region_scroll;
// WI 0142: window-management requests, claims and engine loss over the backend.
pub mod queue;
pub mod remote_tab;
pub mod render;
pub mod squad_attach;
pub mod squad_indicator;
pub mod squad_poll;
pub mod tabs;
pub mod text_edit;
pub mod user_message;
pub mod workflow_view;

#[cfg(test)]
pub(crate) mod tests;

use app::App;
use dialogs::Dialog;
use tabs::Tab;

/// What the TUI opens with. The normal tab is built from `ctx.session`; squad
/// opens the singleton squad tab and no directory-bound tab at all; `View` is
/// the hidden `awman view` secondary frontend over an existing daemon-hosted
/// instance.
pub enum InitialTab {
    Normal,
    Squad,
    View(ViewArgs),
}

/// Entry point invoked by `main.rs` for bare (no-subcommand) launches, for
/// bare `awman squad` in a TTY, and (through [`ViewEntryPoint`]) for the
/// hidden `awman view`.
///
/// `fatal_runtime_error` carries the invalid-runtime config message when the
/// global config names a runtime awman doesn't recognize. In that case the
/// TUI presents only a fatal modal (Enter quits) on the builtin engine — no
/// startup command runs and no daemon is contacted.
///
/// Otherwise the Layer 2 [`EngineHandoff`] chooses the engine (WI 0142 §1):
/// builtin → the in-process `LocalInstanceBackend` exactly as before;
/// daemon → a [`RemoteInstanceBackend`] attached to the daemon's `engine`
/// feature. Nothing else in the TUI depends on which one it got. A fallback
/// to builtin shows its one-line notice in the status bar; a stale daemon
/// with running work is confirmed in the TUI's `YesNo` dialog first.
///
/// `initial_tab` selects the opening tab: `Normal` uses `ctx.session`,
/// including the `ready` / `agents status --watch` startup auto-spawn;
/// `Squad` opens the singleton squad tab (§2.2) with no auto-spawn.
///
/// `startup_messages` are printed to stderr before the TUI takes the terminal.
///
/// `notices` are startup runtime notices (an in-place builtin state upgrade);
/// they are shown in the status area rather than printed, since the TUI owns
/// the terminal.
pub async fn run(
    _matches: clap::ArgMatches,
    ctx: RuntimeContext,
    fatal_runtime_error: Option<crate::command::startup::RuntimeStartupFailure>,
    initial_tab: InitialTab,
    startup_messages: Vec<String>,
    notices: Vec<String>,
) -> ExitCode {
    ExitCode::from(
        run_tui(
            ctx,
            fatal_runtime_error,
            initial_tab,
            startup_messages,
            notices,
        )
        .await,
    )
}

/// [`run`] returning the raw exit code (the `view` entry point reports it).
async fn run_tui(
    ctx: RuntimeContext,
    fatal_runtime_error: Option<crate::command::startup::RuntimeStartupFailure>,
    initial_tab: InitialTab,
    startup_messages: Vec<String>,
    mut notices: Vec<String>,
) -> u8 {
    for message in &startup_messages {
        eprintln!("{message}");
    }
    let catalogue = CommandCatalogue::get();
    let session = ctx.session.read().await.clone();
    let runtime_handle = tokio::runtime::Handle::current();

    // Pick the engine. A fatal startup error is shown on the builtin engine
    // without contacting a daemon.
    let initial_tab = if fatal_runtime_error.is_some() {
        InitialTab::Normal
    } else {
        initial_tab
    };
    let chosen = match (&initial_tab, fatal_runtime_error.is_some()) {
        (_, true) => Chosen::Builtin,
        (InitialTab::View(args), false) => match connect_view(&session, args).await {
            Ok(backend) => Chosen::Remote(backend),
            Err(message) => {
                eprintln!("awman: {message}");
                return 1;
            }
        },
        (InitialTab::Normal | InitialTab::Squad, false) => {
            match select_backend(&session, catalogue).await {
                Ok(chosen) => chosen,
                Err(error) => {
                    eprintln!("awman: {}", error.message);
                    return error.exit_code;
                }
            }
        }
    };
    let backend: Arc<dyn InstanceBackend> = match chosen {
        Chosen::Builtin => {
            let session_manager = Arc::new(SessionManager::in_memory(session.env().clone()));
            App::local_backend(catalogue, ctx.engines, session_manager, &runtime_handle)
        }
        Chosen::Fallback(notice) => {
            notices.push(notice);
            let session_manager = Arc::new(SessionManager::in_memory(session.env().clone()));
            App::local_backend(catalogue, ctx.engines, session_manager, &runtime_handle)
        }
        Chosen::Remote(backend) => backend,
    };

    // The selected engine instance owns the worker and all picked leases.
    let queue_session = session.clone();

    // Open the first tab through the instance. The instance decides and
    // starts the startup command (`ready` / `agents status --watch`) — only
    // for a directory-bound tab, never for the squad tab, and never when
    // only a fatal error is shown; this binds what it started.
    let spec = match initial_tab {
        InitialTab::Normal | InitialTab::View(_) => InitialTabSpec::Session {
            session,
            fatal_startup_error: fatal_runtime_error.is_some(),
            pty_size: App::new_tab_pty_size(),
        },
        InitialTab::Squad => InitialTabSpec::Squad { fallback: session },
    };
    let (mut app, startup) = match backend.open_initial(spec) {
        InitialTabOpened::Normal { tab } => {
            let (tab, startup) = Tab::from_opened(tab);
            (App::with_backend(backend, tab, runtime_handle), startup)
        }
        InitialTabOpened::Squad { session, gateway } => {
            let build = App::assemble_squad_tab(session, gateway, &runtime_handle);
            (
                App::with_backend(backend, build.tab, runtime_handle),
                StartupRun::NotStarted,
            )
        }
        InitialTabOpened::SquadFailed { tab, error } => {
            let (tab, startup) = Tab::from_opened(tab);
            let mut app = App::with_backend(backend, tab, runtime_handle);
            app.status_bar.text = error.to_string();
            (app, startup)
        }
    };

    app.show_runtime_notices(notices);
    match app.backend.queue_source(&queue_session) {
        Ok(source) => app.queue = source,
        Err(message) => app.status_bar.text = message,
    }
    app.bind_startup(0, startup, App::new_tab_pty_size());

    if let Some(failure) = fatal_runtime_error {
        app.active_dialog = Some(Dialog::FatalError {
            title: "Invalid Runtime Configuration".to_string(),
            body: failure.error.to_string(),
        });
        let result = event_loop::run_event_loop(&mut app);
        // Every exit ends the instance through the same policy; the fatal
        // path keeps its own exit code.
        let _ = app.backend.quit();
        return match result {
            Ok(()) => failure.error.exit_code(),
            Err(e) => {
                eprintln!("awman: TUI error: {e}");
                1
            }
        };
    }

    // WI 0112: the bottom-row squad indicator probes the daemon for the
    // life of the event loop, on every tab, whether or not a squad tab ever
    // opens. Started here — never in `App::new` — so unit-test apps stay
    // free of filesystem side effects.
    let indicator_cancel = tokio_util::sync::CancellationToken::new();
    let indicator_handle = app.backend.start_squad_indicator(indicator_cancel.clone());

    let result = event_loop::run_event_loop(&mut app);
    indicator_cancel.cancel();
    if let Some(handle) = indicator_handle {
        handle.abort();
    }

    // Quit, last-tab close and a TUI error all end the instance here, which
    // cancels every running command (a panic unwinds through the instance's
    // drop, which applies the same policy). Ctrl-\ minimizes a container and
    // never reaches this point; the squad daemon is not stopped. A TUI that
    // lost its engine exits with the fatal code instead.
    let plan = app.backend.quit();
    match result {
        Ok(()) => match app.remote_view.fatal_exit_code() {
            Some(code) => code,
            None => u8::try_from(plan.exit_code).unwrap_or(1),
        },
        Err(e) => {
            eprintln!("awman: TUI error: {e}");
            1
        }
    }
}

/// The engine `tui::run` presents.
enum Chosen {
    Builtin,
    /// The hand-off fell back to builtin; the notice goes to the status bar.
    Fallback(String),
    Remote(Arc<dyn InstanceBackend>),
}

/// The start-up hand-off for a primary TUI: resolve, then attach to the
/// daemon's engine when it chose the daemon.
async fn select_backend(
    session: &Session,
    catalogue: &'static CommandCatalogue,
) -> Result<Chosen, HandoffError> {
    let config = session.effective_config();
    let env = session.env().clone();
    let handoff = EngineHandoff::new(
        env.clone(),
        config.clone(),
        IntegrationRegistry::compiled_in(),
        Arc::new(TuiHandoffPrompter),
    );
    let result = handoff.resolve().await?;
    let BackendSelection::Daemon { endpoint, .. } = &result.selection else {
        return Ok(match result.notice {
            Some(notice) => Chosen::Fallback(notice),
            None => Chosen::Builtin,
        });
    };
    let hello = hello(
        session,
        FrontendRole::Primary,
        result.annotations.clone(),
        None,
    );
    let open = RemoteOpen::primary(endpoint, hello, process_env(), catalogue);
    match RemoteInstanceBackend::connect(open).await {
        Ok(backend) => Ok(Chosen::Remote(backend)),
        // The hand-off's own fallback rule: `daemon` is strict, `auto`
        // falls back to builtin with the one-line notice.
        Err(error) if config.tui_backend() == TuiBackend::Daemon => Err(HandoffError {
            message: engine_handoff::fallback_notice(&error.to_string()),
            exit_code: 1,
        }),
        Err(error) => Ok(Chosen::Fallback(engine_handoff::fallback_notice(
            &error.to_string(),
        ))),
    }
}

/// `awman view`: a secondary frontend on the running daemon's instance,
/// announcing its own detected annotations plus `--annotation` values.
async fn connect_view(
    session: &Session,
    args: &ViewArgs,
) -> Result<Arc<dyn InstanceBackend>, String> {
    let config = session.effective_config();
    let env = session.env().clone();
    let mut annotations = IntegrationRegistry::compiled_in()
        .detect_all(&env, &config)
        .map_err(|error| error.to_string())?
        .annotations;
    annotations
        .merge_disjoint(args.annotations.clone(), "--annotation", "integrations")
        .map_err(|error| error.to_string())?;
    let endpoint = LocalDaemonLink::new(env, config)
        .endpoint()
        .map_err(|error| format!("no awman daemon is reachable: {error}"))?;
    let scope = match &args.tab {
        Some(tab) => ViewScope::Tab(tab.clone()),
        None => ViewScope::All,
    };
    let hello = hello(session, FrontendRole::Secondary, annotations, Some(scope));
    let open = RemoteOpen::secondary(
        &endpoint,
        args.instance.clone(),
        hello,
        process_env(),
        CommandCatalogue::get(),
    );
    let backend = RemoteInstanceBackend::connect(open)
        .await
        .map_err(|error| error.to_string())?;
    Ok(backend)
}

/// This frontend's `Hello`. `env` is the whole process environment: the
/// instance spawns host processes with it (it is never logged or stored).
fn hello(
    session: &Session,
    role: FrontendRole,
    annotations: crate::data::annotations::Annotations,
    view: Option<ViewScope>,
) -> Hello {
    use std::io::IsTerminal;
    let (cols, rows) = App::new_tab_pty_size();
    let env = process_env();
    Hello {
        protocol: FRONTEND_SUBPROTOCOL.to_string(),
        build: env!("CARGO_PKG_VERSION").to_string(),
        kind: FrontendKindWire::Tui,
        role,
        annotations,
        cwd: session.working_dir().to_path_buf(),
        argv: env
            .argv()
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect(),
        env: env.to_hello_env(),
        term_size: TermSize { cols, rows },
        stdin_is_tty: std::io::stdin().is_terminal(),
        stdout_is_tty: std::io::stdout().is_terminal(),
        view,
        resume: None,
        launch_context: None,
    }
}

/// The frontend's full environment and invocation (Layer 0 capture).
fn process_env() -> EnvSnapshot {
    crate::data::config::env::Env::from_process()
}

/// The hand-off asks before restarting a stale daemon that has running work:
/// the TUI's `YesNo` dialog, before the TUI starts.
struct TuiHandoffPrompter;

#[async_trait::async_trait]
impl HandoffPrompter for TuiHandoffPrompter {
    async fn confirm_daemon_restart(&self, running_jobs: usize) -> bool {
        let body = engine_handoff::restart_advice(running_jobs);
        tokio::task::spawn_blocking(move || {
            event_loop::ask_yes_no_before_start("Restart the awman daemon?", &body)
        })
        .await
        .unwrap_or(false)
    }
}

/// The TUI's implementation of the `view` entry point: [`run`] with
/// [`InitialTab::View`]. Called from `main.rs` through `ViewCommand::run`.
pub struct ViewEntryPoint {
    launch: std::sync::Mutex<Option<ViewLaunch>>,
}

/// What `main.rs` hands the view: the start-up context and its messages.
struct ViewLaunch {
    ctx: RuntimeContext,
    startup_messages: Vec<String>,
    notices: Vec<String>,
}

impl ViewEntryPoint {
    pub fn new(ctx: RuntimeContext, startup_messages: Vec<String>, notices: Vec<String>) -> Self {
        Self {
            launch: std::sync::Mutex::new(Some(ViewLaunch {
                ctx,
                startup_messages,
                notices,
            })),
        }
    }
}

impl ViewEntry for ViewEntryPoint {
    fn open_view(&self, args: ViewArgs) -> Result<i32, CommandError> {
        let taken = self.launch.lock().ok().and_then(|mut launch| launch.take());
        let Some(ViewLaunch {
            ctx,
            startup_messages,
            notices,
        }) = taken
        else {
            return Err(CommandError::Other("the view was already opened".into()));
        };
        let handle = tokio::runtime::Handle::current();
        let code = tokio::task::block_in_place(|| {
            handle.block_on(run_tui(
                ctx,
                None,
                InitialTab::View(args),
                startup_messages,
                notices,
            ))
        });
        Ok(i32::from(code))
    }
}
