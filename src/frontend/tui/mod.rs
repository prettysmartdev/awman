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

use tokio::sync::RwLock;

use crate::command::dispatch::catalogue::CommandCatalogue;
use crate::command::dispatch::parsed_input::ParsedCommandBoxInput;
use crate::data::session_manager::SessionManager;
use crate::frontend::cli::RuntimeContext;

pub mod app;
pub mod command_box;
pub mod command_frontend;
pub mod container_view;
mod dialog_router;
pub mod dialogs;
mod event_loop;
pub mod git_sidebar;
pub mod hints;
pub mod keymap;
mod key_handler;
mod mouse;
mod mouse_handler;
pub mod per_command;
pub mod pty;
mod region_scroll;
pub mod render;
pub mod tabs;
pub mod text_edit;
pub mod user_message;
pub mod workflow_view;

#[cfg(test)]
mod tests;

use app::App;
use dialogs::Dialog;
use tabs::Tab;

/// Entry point invoked by `main.rs` for bare (no-subcommand) launches.
///
/// `fatal_runtime_error` carries the invalid-runtime config message when the
/// global config names a runtime awman doesn't recognize. In that case the
/// TUI presents only a fatal modal (Enter quits) — no startup command runs.
pub async fn run(
    _matches: clap::ArgMatches,
    ctx: RuntimeContext,
    fatal_runtime_error: Option<String>,
) -> ExitCode {
    let catalogue = CommandCatalogue::get();
    let session_manager = Arc::new(RwLock::new(SessionManager::in_memory()));

    let session = ctx.session.read().await.clone();
    let initial_tab = Tab::new(session);
    let runtime_handle = tokio::runtime::Handle::current();

    let mut app = App::new(
        catalogue,
        ctx.engines,
        session_manager,
        initial_tab,
        runtime_handle,
    );

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
    // for non-git directories.
    let is_git = app.active_tab().session.git_root().join(".git").exists();
    if is_git {
        app.spawn_command(
            "ready",
            ParsedCommandBoxInput {
                path: vec!["ready".into()],
                flags: Default::default(),
                arguments: Default::default(),
            },
        );
    } else {
        let mut flags = std::collections::BTreeMap::new();
        flags.insert(
            "watch".to_string(),
            crate::command::dispatch::parsed_input::FlagValue::Bool(true),
        );
        app.spawn_command(
            "status --watch",
            ParsedCommandBoxInput {
                path: vec!["status".into()],
                flags,
                arguments: Default::default(),
            },
        );
    }

    match event_loop::run_event_loop(&mut app) {
        Ok(()) => ExitCode::from(0),
        Err(e) => {
            eprintln!("awman: TUI error: {e}");
            ExitCode::from(1)
        }
    }
}
