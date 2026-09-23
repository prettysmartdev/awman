//! Keys that act on the squad card grid.
//!
//! Split out of `key_handler.rs` by WI 0114 F-51. `handle_key_event`'s
//! `match` routes each family here; the outer match stays exhaustive, so a
//! new `Action` variant is a compile error there rather than a silent no-op.

use super::*;

pub(super) fn handle(app: &mut App, action: Action) {
    match action {
        Action::SquadShowDetail => {
            let detail = app.active_tab().squad.as_ref().and_then(|state| {
                let task = state.selected_task()?;
                Some(dialogs::SquadDetailState {
                    name: task.name.clone(),
                    task,
                })
            });
            if let Some(detail) = detail {
                app.active_dialog = Some(Dialog::SquadTaskDetail(detail));
            }
        }
        // The run history is a modal of its own, so a task with a long
        // description cannot push it off the bottom of the detail modal.
        // Opened from the grid it stands alone: Esc closes it outright rather
        // than opening a detail modal the user never asked for.
        Action::SquadShowHistory => {
            let name = app
                .active_tab()
                .squad
                .as_ref()
                .and_then(|state| state.selected_name());
            if let Some(name) = name {
                open_squad_history(app, &name, false);
            }
        }
        Action::SquadAttach => {
            let name = app
                .active_tab()
                .squad
                .as_ref()
                .and_then(|state| state.selected_name());
            if let Some(name) = name {
                crate::frontend::tui::squad_attach::start_squad_attach(app, &name);
            }
        }
        Action::SquadNew => {
            let parsed = app
                .catalogue
                .action_input(FrontendAction::NewSquadTask, None);
            app.spawn_command(parsed);
        }
        // WI 0110: `e` is `n`'s counterpart for an existing task — the same
        // Layer-2 interview, reached through the same `spawn_command` path,
        // with the task name as its argument.
        Action::SquadEdit => {
            let name = app
                .active_tab()
                .squad
                .as_ref()
                .and_then(|state| state.selected_name());
            if let Some(name) = name {
                squad_edit_by_name(app, &name);
            }
        }
        Action::SquadPause => {
            let name = app
                .active_tab()
                .squad
                .as_ref()
                .and_then(|state| state.selected_name());
            if let Some(name) = name {
                squad_task_action(app, FrontendAction::PauseSquadTask, name);
            }
        }
        Action::SquadResume => {
            let name = app
                .active_tab()
                .squad
                .as_ref()
                .and_then(|state| state.selected_name());
            if let Some(name) = name {
                squad_task_action(app, FrontendAction::ResumeSquadTask, name);
            }
        }
        Action::SquadTrigger => {
            let name = app
                .active_tab()
                .squad
                .as_ref()
                .and_then(|state| state.selected_name());
            if let Some(name) = name {
                squad_task_action(app, FrontendAction::TriggerSquadTask, name);
            }
        }
        Action::SquadCancel => {
            let name = app
                .active_tab()
                .squad
                .as_ref()
                .and_then(|state| state.selected_name());
            if let Some(name) = name {
                squad_task_action(app, FrontendAction::CancelSquadRun, name);
            }
        }
        Action::SquadDelete => {
            let name = app
                .active_tab()
                .squad
                .as_ref()
                .and_then(|state| state.selected_name());
            if let Some(name) = name {
                app.active_dialog = Some(Dialog::SquadRemoveConfirm { name });
            }
        }
        // WI 0106 Part 5: card-grid column movement. Row movement (Up/Down)
        // stays on `Action::ScrollUp`/`ScrollDown` above — only Left/Right are
        // grid-new.
        Action::SquadMoveLeft => {
            if let Some(state) = app.active_tab_mut().squad.as_mut() {
                state.move_selection_col(-1);
            }
        }
        Action::SquadMoveRight => {
            if let Some(state) = app.active_tab_mut().squad.as_mut() {
                state.move_selection_col(1);
            }
        }

        // ── PTY passthrough ───────────────────────────────────────────
        // Unreachable: `handle_key_event` routes only this family here.
        _ => {}
    }
}
