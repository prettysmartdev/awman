//! Squad-tab attach presentation. The shared daemon viewer and its local
//! session lifecycle live in Layer 2; this module restores the card grid.

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
    let Some(state) = tab.squad.as_mut() else {
        return false;
    };
    if state.attach_view.is_none()
        && state.attached_task().is_none()
        && tab.container_slots.is_empty()
    {
        return false;
    }
    let task = state
        .end_attach()
        .or_else(|| state.attach_view.as_ref().map(|view| view.task.clone()))
        .or_else(|| state.selected_name())
        .unwrap_or_else(|| "the task".into());
    state.attach_view = None;
    tab.command_result_rx = None;
    tab.dialog_request_rx = None;
    tab.dialog_response_tx = None;
    tab.end_attach_session();
    if app.command_dialog_active {
        app.active_dialog = None;
    }
    app.command_dialog_active = false;
    app.focus = Focus::ExecutionWindow;
    let message =
        format!("Detached from {task}. Its containers are still running — press 'a' to reattach.");
    if let Ok(mut status) = app.active_tab().shared.attach_status.lock() {
        *status = Some(crate::data::message::UserMessage {
            level: crate::data::message::MessageLevel::Info,
            text: message.clone(),
        });
    }
    app.status_bar.text = message;
    app.needs_redraw = true;
    true
}

/// A remote instance can disappear along with its Squad daemon. Preserve the
/// local viewer/grid so Ctrl-\\ still performs a local detach. Other tabs and
/// intentional/protocol detach reasons retain the ordinary fatal-engine UI.
pub(super) fn show_daemon_loss(
    app: &mut App,
    loss: &crate::command::instance::backend::BackendLoss,
) -> bool {
    use crate::command::instance::backend::BackendLoss;
    use crate::data::frontend_protocol::DetachReasonWire;
    if !app.active_tab().is_squad
        || !matches!(
            loss,
            BackendLoss::Transport(_)
                | BackendLoss::Detached(
                    DetachReasonWire::DaemonShutdown | DetachReasonWire::InstanceEnded
                )
        )
    {
        return false;
    }
    let tab = app.active_tab_mut();
    let state = tab.squad.as_mut().expect("Squad tab state");
    state
        .daemon_reachable
        .store(false, std::sync::atomic::Ordering::Relaxed);
    let text = if state.attach_view.is_some() {
        crate::command::commands::attach::AttachViewState::DaemonUnreachable.message()
    } else {
        "squad daemon not reachable — task list frozen".into()
    };
    if let Ok(mut snapshot) = state.snapshot.lock() {
        snapshot.error = Some("squad daemon not reachable".into());
        snapshot.loaded = true;
    }
    if let Ok(mut status) = tab.shared.attach_status.lock() {
        *status = Some(crate::data::message::UserMessage {
            level: crate::data::message::MessageLevel::Warning,
            text: text.clone(),
        });
    }
    app.status_bar.text = text;
    app.needs_redraw = true;
    true
}
