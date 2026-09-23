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

use crate::command::dispatch::catalogue::CommandCatalogue;
use crate::command::dispatch::RuntimeContext;
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
mod key_handler;
pub mod keymap;
mod mouse;
mod mouse_handler;
pub mod per_command;
pub mod pty;
mod region_scroll;
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

use app::{App, SquadTabStart};
use dialogs::Dialog;
use tabs::Tab;

/// What the TUI opens with. The normal tab is built from `ctx.session`; squad
/// opens the singleton squad tab and no directory-bound tab at all.
pub enum InitialTab {
    Normal,
    Squad,
}

/// Entry point invoked by `main.rs` for bare (no-subcommand) launches and for
/// bare `awman squad` in a TTY.
///
/// `fatal_runtime_error` carries the invalid-runtime config message when the
/// global config names a runtime awman doesn't recognize. In that case the
/// TUI presents only a fatal modal (Enter quits) — no startup command runs.
///
/// `initial_tab` selects the opening tab: `Normal` uses `ctx.session`,
/// behaviour, including the `ready` / `status --watch` startup auto-spawn;
/// `Squad` opens the singleton squad tab (§2.2) with no auto-spawn.
pub async fn run(
    _matches: clap::ArgMatches,
    ctx: RuntimeContext,
    fatal_runtime_error: Option<String>,
    initial_tab: InitialTab,
) -> ExitCode {
    let catalogue = CommandCatalogue::get();
    let session_manager = Arc::new(SessionManager::in_memory());
    let runtime_handle = tokio::runtime::Handle::current();

    // Build the App and decide whether the normal startup auto-spawn runs — it
    // does only for a directory-bound tab, never for the squad tab.
    let (mut app, run_startup_spawn) = match initial_tab {
        InitialTab::Normal => {
            let session = ctx.session.read().await.clone();
            session_manager
                .create(session.clone())
                .expect("startup session id must be unique");
            let tab = Tab::new_with_git_engine(session, ctx.engines.git_engine.clone());
            let app = App::new(catalogue, ctx.engines, session_manager, tab, runtime_handle);
            (app, true)
        }
        InitialTab::Squad => match App::build_squad_tab(&ctx.engines, &runtime_handle) {
            Ok(SquadTabStart::Ready(build)) => {
                let key_setup = build.key_setup;
                let mut app = App::new(
                    catalogue,
                    ctx.engines,
                    session_manager,
                    build.tab,
                    runtime_handle,
                );
                app.squad_gateway = Some(build.gateway);
                // First run: the bearer key was minted a moment ago and lives
                // only in memory. Show it before the event loop starts.
                if let Some(key_setup) = key_setup {
                    app.active_dialog = Some(crate::frontend::tui::per_command::key_setup_dialog(
                        &key_setup,
                    ));
                }
                (app, false)
            }
            // The daemon is up, but this process holds no key for it. Open on
            // the working directory rather than on a squad tab that would only
            // ever render a 401, and put the one recovery in front of the user;
            // accepting it builds the squad tab through the ordinary path.
            Ok(SquadTabStart::KeyMissing) => {
                let session = ctx.session.read().await.clone();
                let tab = Tab::new_with_git_engine(session, ctx.engines.git_engine.clone());
                let mut app =
                    App::new(catalogue, ctx.engines, session_manager, tab, runtime_handle);
                app.active_dialog = Some(Dialog::SquadKeyMissing);
                (app, false)
            }
            Err(error) => {
                // `main.rs` calls `ensure_running` before routing here, so this
                // should not happen; degrade to a normal tab on the cwd session
                // and surface the specific error rather than failing to open.
                let session = ctx.session.read().await.clone();
                let tab = Tab::new_with_git_engine(session, ctx.engines.git_engine.clone());
                let mut app =
                    App::new(catalogue, ctx.engines, session_manager, tab, runtime_handle);
                app.status_bar.text = error.to_string();
                (app, false)
            }
        },
    };

    if let Some(message) = fatal_runtime_error {
        app.active_dialog = Some(Dialog::FatalError {
            title: "Invalid Runtime Configuration".to_string(),
            body: format!(
                "{message}\n\nUpdate the 'runtime' value in $HOME/.awman/config.json \
                 and restart awman."
            ),
        });
        return match event_loop::run_event_loop(&mut app) {
            Ok(()) => ExitCode::from(2),
            Err(e) => {
                eprintln!("awman: TUI error: {e}");
                ExitCode::from(1)
            }
        };
    }

    // Auto-spawn startup command: `ready` for git repos, `status --watch`
    // for non-git directories. Skipped entirely for the squad tab.
    if run_startup_spawn {
        let startup = app.catalogue.startup_command(&app.active_tab().session);
        app.spawn_command(startup);
    }

    // WI 0112: the bottom-row squad indicator probes the daemon for the
    // life of the event loop, on every tab, whether or not a squad tab ever
    // opens. Started here — never in `App::new` — so unit-test apps stay
    // free of filesystem side effects.
    let indicator_cancel = tokio_util::sync::CancellationToken::new();
    let indicator_handle = {
        let _guard = app.runtime_handle.enter();
        squad_indicator::SquadIndicatorPoller::new(app.squad_indicator.clone())
            .start(indicator_cancel.clone())
    };

    let result = event_loop::run_event_loop(&mut app);
    indicator_cancel.cancel();
    indicator_handle.abort();

    match result {
        Ok(()) => ExitCode::from(0),
        Err(e) => {
            eprintln!("awman: TUI error: {e}");
            ExitCode::from(1)
        }
    }
}
