//! Keys for the work-queue panel and the lease close prompt (WI 0143).
//!
//! The TUI only presents queue data and returns a typed selection: a pick
//! calls the Layer 2 controller (through [`QueueSource`]), which claims the
//! item and hands back the command line to run. Nothing here claims, releases
//! or decides eligibility itself.

use super::*;
use crate::command::dispatch::parsed_input;
use crate::data::session::SessionOpenOptions;
use crate::frontend::tui::queue::{
    tab_label, LeaseCloseChoice, LeaseCloseState, PickError, PickRun, QueueBadge, QueuePanelState,
    TabLease,
};
use crossterm::event::KeyEvent;

/// `Ctrl-Q`: open the panel (or say why it cannot open).
pub(super) fn open_panel(app: &mut App) {
    let Some(source) = app.queue.clone() else {
        app.status_bar.text =
            "The work queue is not available: this TUI's worker is turned off.".to_string();
        return;
    };
    source.refresh();
    let snapshot = source.snapshot();
    source.mark_seen();
    app.queue_badge = QueueBadge::default();
    app.active_dialog = Some(Dialog::QueuePanel(QueuePanelState::from_snapshot(snapshot)));
    app.command_dialog_active = false;
}

/// Every key while the panel is open. Returns after handling it.
pub(super) fn handle_panel_key(app: &mut App, key: KeyEvent) {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let Some(Dialog::QueuePanel(state)) = &mut app.active_dialog else {
        return;
    };
    state.message = None;
    match key.code {
        KeyCode::Char('q') | KeyCode::Char('c') if ctrl => app.active_dialog = None,
        KeyCode::Esc => {
            if state.showing_body {
                state.showing_body = false;
            } else {
                app.active_dialog = None;
            }
        }
        KeyCode::Up => state.move_by(-1),
        KeyCode::Down => state.move_by(1),
        KeyCode::PageUp => state.move_by(-10),
        KeyCode::PageDown => state.move_by(10),
        KeyCode::Enter => {
            if state.selected_entry().is_some() {
                state.showing_body = true;
            }
        }
        KeyCode::Char('r') if !ctrl => {
            if let Some(source) = app.queue.clone() {
                source.refresh();
                state.apply(source.snapshot());
            }
        }
        KeyCode::Char('p') if !ctrl => pick_selected(app),
        _ => {}
    }
}

fn pick_selected(app: &mut App) {
    let Some(source) = app.queue.clone() else {
        return;
    };
    let Some(Dialog::QueuePanel(state)) = &app.active_dialog else {
        return;
    };
    let Some(entry) = state.selected_entry() else {
        return;
    };
    let (server, id) = (entry.server.clone(), entry.id.clone());
    match source.pick(&server, &id) {
        Ok(run) => {
            app.active_dialog = None;
            open_pick_tab(app, source.as_ref(), run);
        }
        Err(error) => {
            // The list is stale (or the pick failed): refresh it and say why.
            let message = match error {
                PickError::Ineligible(why) => format!("Item no longer eligible: {why}"),
                PickError::Taken => "Item was claimed by another worker.".to_string(),
                PickError::Failed(why) => format!("Pick failed: {why}"),
            };
            source.refresh();
            if let Some(Dialog::QueuePanel(state)) = &mut app.active_dialog {
                state.apply(source.snapshot());
                state.showing_body = false;
                state.message = Some(message);
            }
        }
    }
}

/// Open a tab for a claimed item and run its command in it, exactly as the
/// command box would run `exec workflow --dynamic`.
fn open_pick_tab(
    app: &mut App,
    source: &dyn crate::frontend::tui::queue::QueueSource,
    run: PickRun,
) {
    let release = |app: &mut App, run: &PickRun, why: String| {
        let _ = source.finish_lease(&run.lease_id, LeaseCloseChoice::Release);
        app.status_bar.text = why;
    };
    let parsed = match parsed_input::parse(&run.command_line, app.catalogue) {
        Ok(parsed) => parsed,
        Err(error) => return release(app, &run, error.to_string()),
    };
    let idx = match app.add_tab(run.cwd.clone(), SessionOpenOptions::default()) {
        Ok(idx) => idx,
        Err(error) => return release(app, &run, error.to_string()),
    };
    app.active_tab = idx;
    app.focus = Focus::CommandBox;
    let tab = app.active_tab_mut();
    tab.label_override = Some(tab_label(&run.title, &run.server));
    tab.lease = Some(TabLease {
        lease_id: run.lease_id.clone(),
        server: run.server,
        title: run.title,
    });
    app.spawn_command_with_lease(parsed, Some(run.lease_id));
}

/// Closing the active tab: when it runs a picked item, ask what to do with
/// the lease. Returns true when the prompt was opened.
pub(super) fn prompt_lease_close(app: &mut App) -> bool {
    let tab = app.active_tab();
    let running = matches!(tab.execution_phase, tabs::ExecutionPhase::Running { .. });
    let Some(lease) = tab.lease.clone().filter(|_| running && app.queue.is_some()) else {
        return false;
    };
    app.active_dialog = Some(Dialog::LeaseCloseConfirm(LeaseCloseState { lease }));
    true
}

/// Keys of the lease close prompt.
pub(super) fn handle_close_key(app: &mut App, key: KeyEvent) {
    let choice = match key.code {
        KeyCode::Char('r') | KeyCode::Char('R') => LeaseCloseChoice::Release,
        KeyCode::Char('c') | KeyCode::Char('C')
            if !key.modifiers.contains(KeyModifiers::CONTROL) =>
        {
            LeaseCloseChoice::Cancel
        }
        KeyCode::Esc | KeyCode::Char('k') => LeaseCloseChoice::KeepRunning,
        _ => return,
    };
    let Some(Dialog::LeaseCloseConfirm(state)) = app.active_dialog.take() else {
        return;
    };
    if choice == LeaseCloseChoice::KeepRunning {
        return;
    }
    if let Some(source) = app.queue.clone() {
        if let Err(error) = source.finish_lease(&state.lease.lease_id, choice) {
            // The tab stays: closing it now would orphan the lease.
            app.status_bar.text = format!("Could not update the work item: {error}");
            return;
        }
    }
    app.close_active_tab();
}
