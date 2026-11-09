//! Squad-tab attach actions (the TUI side). The `SquadAttachFrontend`
//! producer and `SquadAttachSession` are Layer 2 since WI 0142
//! (`command::instance::{per_command, feeds}`); the session type is
//! re-exported here under its established path.

use crate::command::dispatch::Dispatch;
use crate::frontend::tui::app::{App, Focus};
use crate::frontend::tui::command_frontend::TuiCommandFrontend;

pub use crate::command::instance::feeds::SquadAttachSession;

/// Route the squad-tab attach action through the ordinary Dispatch path.
pub fn start_squad_attach(app: &mut App, task: &str) {
    let raw = format!("squad attach {task}");
    match Dispatch::<TuiCommandFrontend>::parse_command_box_input(&raw) {
        Ok(parsed) => app.spawn_command(parsed),
        Err(error) => app.status_bar.text = error.to_string(),
    }
}

/// End the local attach session while leaving every daemon-owned container
/// running.
pub fn detach_squad_attach(app: &mut App) -> bool {
    let tab = app.active_tab_mut();
    let Some(state) = tab.squad.as_ref() else {
        return false;
    };
    let Some(task) = state.end_attach() else {
        return false;
    };
    tab.command_result_rx = None;
    tab.end_attach_session();
    app.focus = Focus::ExecutionWindow;
    app.status_bar.text =
        format!("Detached from {task}. Its containers are still running — press 'a' to reattach.");
    app.needs_redraw = true;
    true
}
