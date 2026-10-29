//! Keys that open or dismiss a dialog.
//!
//! Split out of `key_handler.rs` by WI 0114 F-51. `handle_key_event`'s
//! `match` routes each family here; the outer match stays exhaustive, so a
//! new `Action` variant is a compile error there rather than a silent no-op.

use super::*;

pub(super) fn handle(app: &mut App, action: Action) {
    match action {
        Action::WorkflowControl => {
            let engine_tx = app
                .active_tab()
                .shared
                .engine_tx_shared
                .lock()
                .ok()
                .and_then(|g| g.clone());
            if let Some(tx) = engine_tx {
                if matches!(app.active_dialog, Some(Dialog::WorkflowStepConfirm(_))) {
                    app.send_dialog_response(DialogResponse::Char('W'));
                    app.active_dialog = None;
                    app.command_dialog_active = false;
                } else if app.command_dialog_active {
                    dialog_router::dismiss_dialog(app);
                }
                let focused_step = app
                    .active_tab()
                    .focused_slot()
                    .map(|slot| slot.step_name.clone())
                    .unwrap_or_default();
                let _ = tx.send(crate::engine::workflow::EngineRequest::OpenControlBoard {
                    step_name: focused_step,
                });
            }
        }
        Action::OpenConfigShow => {
            // Run the command through dispatch so the command layer computes
            // the rows and the frontend trait presents the dialog. The
            // catalogue owns which command that is (WI 0114 F-15).
            let parsed = app.catalogue.action_input(FrontendAction::ShowConfig, None);
            app.spawn_command(parsed);
        }

        // ── Command box actions ───────────────────────────────────────
        Action::DismissDialog => {
            // A fatal startup error cannot be dismissed back into a usable
            // app — Esc quits, same as Enter.
            if matches!(app.active_dialog, Some(Dialog::FatalError { .. })) {
                app.active_dialog = None;
                app.should_quit = true;
                return;
            }
            // In ConfigShow editing / add-mapping mode, Esc cancels the edit
            // (back to browse) instead of closing the dialog.
            if let Some(Dialog::ConfigShow(state)) = &mut app.active_dialog {
                if state.editing || state.new_entry.is_some() {
                    state.editing = false;
                    state.new_entry = None;
                    state.error = None;
                    return;
                }
            }
            // Esc in the run-history modal walks back exactly one step: to the
            // detail modal when that is where `h` was pressed, and to the card
            // grid when the history was opened from the grid itself.
            // A run's detail open over the table closes first, back to the
            // table with the same row still selected.
            if let Some(Dialog::SquadTaskHistory(state)) = &mut app.active_dialog {
                if state.run_detail.take().is_some() {
                    return;
                }
            }
            if let Some(Dialog::SquadTaskHistory(state)) = &app.active_dialog {
                let (name, from_detail) = (state.name.clone(), state.from_detail);
                if !(from_detail && reopen_squad_detail(app, &name)) {
                    app.active_dialog = None;
                }
                return;
            }
            if matches!(app.active_dialog, Some(Dialog::WorkflowYoloCountdown(_))) {
                let tab = app.active_tab();
                if tab.dormant_slots.is_empty() {
                    tab.shared
                        .yolo_cancel_flag
                        .store(true, std::sync::atomic::Ordering::Relaxed);
                } else if let Some(slot) = tab.focused_slot() {
                    // Parallel group: the modal is only ever shown for the
                    // focused slot (see `tick_all_tabs`), so cancel that
                    // slot's countdown rather than the tab-level one.
                    slot.yolo_cancel_flag
                        .store(true, std::sync::atomic::Ordering::Relaxed);
                }
                app.active_dialog = None;
                return;
            }
            dialog_router::dismiss_dialog(app);
        }
        Action::NewMapEntry => {
            // Maps collect a key then a value; lists collect one value.
            // Keep the selected scope and ignore scalar rows.
            use crate::command::commands::config::ConfigRowShape;
            if let Some(Dialog::ConfigShow(state)) = &mut app.active_dialog {
                if !state.editing && state.new_entry.is_none() {
                    let phase = state
                        .rows
                        .get(state.selected)
                        .and_then(|row| match row.shape {
                            ConfigRowShape::MapHeader { .. } | ConfigRowShape::MapEntry => {
                                Some(dialogs::NewMapEntryPhase::Key)
                            }
                            ConfigRowShape::ListHeader { .. } | ConfigRowShape::ListEntry => {
                                Some(dialogs::NewMapEntryPhase::ListEntry)
                            }
                            ConfigRowShape::Scalar => None,
                        });
                    if let Some(phase) = phase {
                        state.new_entry = Some(phase);
                        state.editing = true;
                        state.error = None;
                        state.editor = crate::frontend::tui::text_edit::TextEdit::new(false);
                    }
                }
            }
        }

        // ── Text input actions ────────────────────────────────────────
        // Unreachable: `handle_key_event` routes only this family here.
        _ => {}
    }
}
