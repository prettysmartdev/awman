//! Dialog request/response routing: dismissal, submit (Enter), and text/
//! cursor editing for the currently active dialog, including the
//! ConfigShow dialog's inline-edit and add-mapping flows.

use super::app::App;
use crate::command::commands::squad::commands::SquadConfirmDecision;
use crate::command::dispatch::FrontendAction;

use super::dialogs::{self, Dialog, DialogResponse};
use super::key_handler;

/// Dismiss the active dialog, sending Dismissed to the command thread if needed.
pub(super) fn dismiss_dialog(app: &mut App) {
    // A confirmation carries what walking away from it means
    // (`Prompt::default_on_dismiss`), so Esc is answered out of the prompt
    // rather than assumed to be "no" here (WI 0114 F-19/F-55).
    if let Some(Dialog::SquadActionConfirm {
        action,
        name,
        prompt,
    }) = &app.active_dialog
    {
        let (action, name, decision) = (*action, name.clone(), prompt.default_on_dismiss);
        app.active_dialog = None;
        apply_squad_confirm(app, action, &name, decision);
        return;
    }
    if app.command_dialog_active {
        app.send_dialog_response(DialogResponse::Dismissed);
    }
    app.active_dialog = None;
    app.command_dialog_active = false;
}

/// Handle Enter key in a dialog context.
pub(super) fn handle_dialog_submit(app: &mut App) {
    let is_command = app.command_dialog_active;

    match &app.active_dialog {
        Some(Dialog::QuitConfirm) => {}
        Some(Dialog::CloseTabConfirm) => {}
        Some(Dialog::FatalError { .. }) => {
            app.active_dialog = None;
            app.should_quit = true;
        }
        // No command thread is waiting on a Notice: Enter just closes it.
        Some(Dialog::Notice { .. }) => {
            app.active_dialog = None;
        }

        Some(Dialog::TextInput { editor, .. }) if is_command => {
            let text = editor.text.clone();
            app.send_dialog_response(DialogResponse::Text(text));
            app.active_dialog = None;
            app.command_dialog_active = false;
        }
        Some(Dialog::TextInput { editor, .. }) => {
            let path = editor.text.clone();
            app.active_dialog = None;
            key_handler::handle_new_tab_path(app, &path);
        }

        Some(Dialog::MultilineInput { editor, .. }) if is_command => {
            let text = editor.text.clone();
            app.send_dialog_response(DialogResponse::Text(text));
            app.active_dialog = None;
            app.command_dialog_active = false;
        }

        Some(Dialog::ListPicker { selected, .. }) if is_command => {
            let idx = *selected;
            app.send_dialog_response(DialogResponse::Index(idx));
            app.active_dialog = None;
            app.command_dialog_active = false;
        }

        Some(Dialog::ConfigShow(_)) if is_command => {
            config_show_submit(app);
        }

        Some(Dialog::WorkflowStepConfirm(_)) if is_command => {
            app.send_dialog_response(DialogResponse::Char('>'));
            app.active_dialog = None;
            app.command_dialog_active = false;
        }

        // A single-key Custom dialog is an acknowledgement, not a choice —
        // Enter accepts its one action. Multi-key Custom dialogs are real
        // choices and keep requiring the letter, so Enter cannot pick one of
        // several options by accident.
        Some(Dialog::Custom { keys, .. }) if is_command && keys.len() == 1 => {
            let ch = keys[0].0;
            app.send_dialog_response(DialogResponse::Char(ch));
            app.active_dialog = None;
            app.command_dialog_active = false;
        }

        _ => {}
    }
}

/// The shape of the row heading the config table's map section, if it has one.
///
/// Found by shape rather than by field-name prefix (WI 0114 F-20), so renaming
/// a config path moves nothing in this file.
fn map_header_shape(
    state: &dialogs::ConfigShowState,
) -> Option<&crate::command::commands::config::ConfigFieldShape> {
    use crate::command::commands::config::ConfigFieldShape;
    state
        .rows
        .iter()
        .map(|row| &row.shape)
        .find(|shape| matches!(shape, ConfigFieldShape::MapHeader { .. }))
}

/// The same for the table's array section.
fn array_header_shape(
    state: &dialogs::ConfigShowState,
) -> Option<&crate::command::commands::config::ConfigFieldShape> {
    use crate::command::commands::config::ConfigFieldShape;
    state
        .rows
        .iter()
        .map(|row| &row.shape)
        .find(|shape| matches!(shape, ConfigFieldShape::ArrayHeader { .. }))
}

/// Handle Enter in the ConfigShow dialog: advance the add-mapping flow, save
/// the active inline edit, or begin editing the selected row.
fn config_show_submit(app: &mut App) {
    use crate::command::commands::config::{ConfigEditRequest, ConfigFieldShape};
    use dialogs::NewMapEntryPhase;

    let mut toast: Option<String> = None;
    let mut response: Option<ConfigEditRequest> = None;
    let mut begin_edit = false;

    if let Some(Dialog::ConfigShow(state)) = &mut app.active_dialog {
        match state.new_entry.clone() {
            // Phase 1 of Ctrl+N: confirm the agent name.
            Some(NewMapEntryPhase::Key) => {
                let key = state.editor.text.trim().to_string();
                // The agent-name rule is `AgentName`'s, in Layer 0. This used
                // to be a hand-copied `is_valid_map_key` (F-20), which could
                // — and would — drift from the rule the config writer applies.
                if key.is_empty() {
                    toast = Some("Type an agent name, or press Esc to cancel".to_string());
                } else if let Err(error) = crate::data::session::AgentName::new(key.clone()) {
                    toast = Some(error.to_string());
                } else {
                    let field = map_header_shape(state)
                        .and_then(|header| {
                            ConfigEditRequest::new_member(header, &key, String::new())
                        })
                        .map(|request| request.field)
                        .unwrap_or_default();
                    if let Some(idx) = state.rows.iter().position(|r| r.field == field) {
                        // Already mapped: jump to the existing row and edit it
                        // instead of silently overwriting.
                        let current = state.rows[idx].repo.clone();
                        state.selected = idx;
                        state.new_entry = None;
                        state.editing = true;
                        state.edit_column = 1;
                        state.editor = crate::frontend::tui::text_edit::TextEdit::new(false);
                        state.editor.set_text(&current);
                        toast = Some(format!("'{key}' is already mapped — editing its models"));
                    } else {
                        state.new_entry = Some(NewMapEntryPhase::Value { key });
                        state.editor = crate::frontend::tui::text_edit::TextEdit::new(false);
                    }
                }
            }
            // Phase 2 of Ctrl+N: save the model list for the new key.
            Some(NewMapEntryPhase::Value { key }) => {
                let value = state.editor.text.trim().to_string();
                if value.is_empty() {
                    toast =
                        Some("Enter at least one model name, or press Esc to cancel".to_string());
                } else {
                    response = map_header_shape(state).and_then(|header| {
                        ConfigEditRequest::new_member(header, &key, value.clone())
                    });
                }
            }
            // Ctrl+N (single-phase): append a new guidance entry. The index is
            // the current entry count, so the config layer appends it (WI-0099).
            Some(NewMapEntryPhase::GuidanceEntry) => {
                let value = state.editor.text.trim().to_string();
                if value.is_empty() {
                    toast =
                        Some("Enter a guidance instruction, or press Esc to cancel".to_string());
                } else {
                    let next_index = state
                        .rows
                        .iter()
                        .filter(|r| matches!(r.shape, ConfigFieldShape::ArrayEntry))
                        .count();
                    response = array_header_shape(state).and_then(|header| {
                        ConfigEditRequest::new_member(
                            header,
                            &next_index.to_string(),
                            value.clone(),
                        )
                    });
                }
            }
            None if state.editing => {
                // Save the edited value. The value is trimmed — stray
                // whitespace would otherwise fail validation for numbers and
                // agent::model specs.
                let row = &state.rows[state.selected];
                response = Some(ConfigEditRequest::to_field(
                    row.field.clone(),
                    state.editor.text.trim(),
                    state.edit_column == 0,
                ));
            }
            None => begin_edit = true,
        }
    }

    if begin_edit {
        config_show_begin_edit(app);
    }
    if let Some(text) = toast {
        app.status_bar.text = text;
    }
    if let Some(edit) = response {
        app.send_dialog_response(DialogResponse::ConfigEdit(edit));
        app.active_dialog = None;
        app.command_dialog_active = false;
    }
}

/// Begin inline editing of the selected ConfigShow row (Enter or `e` in
/// browse mode). Snaps the edit column to a writable scope for scope-
/// restricted fields, and refuses read-only rows with a status-bar hint.
fn config_show_begin_edit(app: &mut App) {
    let mut toast: Option<String> = None;

    if let Some(Dialog::ConfigShow(state)) = &mut app.active_dialog {
        if state.editing || state.new_entry.is_some() {
            return;
        }
        let Some(row) = state.rows.get(state.selected) else {
            return;
        };
        let (field, read_only, global_writable, repo_writable, global_val, repo_val) = (
            row.field.clone(),
            row.read_only,
            row.global_writable,
            row.repo_writable,
            row.global.clone(),
            row.repo.clone(),
        );

        if read_only {
            toast = Some(if field == "dynamicWorkflows.agentsToModels" {
                "Press Ctrl+N to add a mapping, or edit a per-agent row below".to_string()
            } else if field == "dynamicWorkflows.guidance" {
                "Press Ctrl+N to add a guidance entry, or edit a per-entry row below".to_string()
            } else {
                "This field is read-only".to_string()
            });
        } else {
            // Snap to a writable column so a repo-only field is never
            // written into the global config (and vice versa).
            let column = match (state.edit_column, global_writable, repo_writable) {
                (0, true, _) | (1, true, false) => Some(0),
                (1, _, true) | (0, false, true) => Some(1),
                _ => None,
            };
            match column {
                None => toast = Some("This field is read-only".to_string()),
                Some(column) => {
                    if column != state.edit_column {
                        toast = Some(if column == 1 {
                            format!("'{field}' is repo-only — editing the Repo value")
                        } else {
                            format!("'{field}' is global-only — editing the Global value")
                        });
                    }
                    state.edit_column = column;
                    state.error = None;
                    // Sensitive values are masked in the table; start the
                    // editor empty rather than seeding it with the mask.
                    let initial = if field == "remote.defaultAPIKey" {
                        String::new()
                    } else if column == 0 {
                        global_val
                    } else {
                        repo_val
                    };
                    state.editing = true;
                    state.editor = crate::frontend::tui::text_edit::TextEdit::new(false);
                    state.editor.set_text(&initial);
                }
            }
        }
    }

    if let Some(text) = toast {
        app.status_bar.text = text;
    }
}

pub(super) enum CursorDir {
    Left,
    Right,
    Home,
    End,
}

pub(super) fn handle_dialog_cursor(app: &mut App, dir: CursorDir) {
    match &mut app.active_dialog {
        Some(Dialog::TextInput { editor, .. }) | Some(Dialog::MultilineInput { editor, .. }) => {
            match dir {
                CursorDir::Left => editor.move_left(),
                CursorDir::Right => editor.move_right(),
                CursorDir::Home => editor.move_home(),
                CursorDir::End => editor.move_end(),
            }
        }
        Some(Dialog::ConfigShow(state)) => {
            if state.editing {
                match dir {
                    CursorDir::Left => state.editor.move_left(),
                    CursorDir::Right => state.editor.move_right(),
                    CursorDir::Home => state.editor.move_home(),
                    CursorDir::End => state.editor.move_end(),
                }
            } else {
                match dir {
                    CursorDir::Left | CursorDir::Home => state.edit_column = 0,
                    CursorDir::Right | CursorDir::End => state.edit_column = 1,
                }
            }
        }
        _ => {}
    }
}

pub(super) fn handle_dialog_backspace(app: &mut App) {
    match &mut app.active_dialog {
        Some(Dialog::TextInput { editor, .. }) | Some(Dialog::MultilineInput { editor, .. }) => {
            editor.backspace();
        }
        Some(Dialog::ConfigShow(state)) if state.editing => {
            state.editor.backspace();
        }
        _ => {}
    }
}

pub(super) fn handle_dialog_delete(app: &mut App) {
    match &mut app.active_dialog {
        Some(Dialog::TextInput { editor, .. }) | Some(Dialog::MultilineInput { editor, .. }) => {
            editor.delete();
        }
        Some(Dialog::ConfigShow(state)) if state.editing => {
            state.editor.delete();
        }
        _ => {}
    }
}

/// Handle arrow-key / page-key scrolling in list-based dialogs. `direction`
/// is a signed step count (e.g. -1 for one row up, +10 for a page down).
pub(super) fn handle_dialog_scroll(app: &mut App, direction: i32) {
    let step = direction.unsigned_abs() as usize;
    match &mut app.active_dialog {
        Some(Dialog::ListPicker {
            items, selected, ..
        }) => {
            let len = items.len();
            if len == 0 {
                return;
            }
            if direction < 0 {
                *selected = selected.saturating_sub(step);
            } else {
                *selected = (*selected + step).min(len - 1);
            }
        }
        Some(Dialog::SquadTaskHistory(state)) => {
            if direction < 0 {
                state.scroll = state.scroll.saturating_sub(step);
            } else {
                // Never scroll the last run off the top: the final row stays
                // reachable, and an over-long press cannot leave the table
                // blank.
                state.scroll = (state.scroll + step).min(state.runs.len().saturating_sub(1));
            }
        }
        Some(Dialog::ConfigShow(state)) => {
            // Row navigation is frozen mid-edit: the editor holds the value
            // of the row the edit started on.
            if state.editing || state.new_entry.is_some() {
                return;
            }
            let len = state.rows.len();
            if len == 0 {
                return;
            }
            if direction < 0 {
                state.selected = state.selected.saturating_sub(step);
            } else {
                state.selected = (state.selected + step).min(len - 1);
            }
        }
        _ => {}
    }
}

/// Handle a character key press in a dialog.
pub(super) fn handle_dialog_char(app: &mut App, c: char) {
    let is_command = app.command_dialog_active;

    match app.active_dialog.as_ref() {
        // ── Always UI-originated ─────────────────────────────────────
        Some(Dialog::QuitConfirm) => {
            // Only Ctrl-C (handled via Action::CloseTabOrQuit) or Esc
            // (handled via Action::DismissDialog) are valid here. Ignore
            // all regular char keys.
        }
        Some(Dialog::CloseTabConfirm) => {
            // Only Ctrl-C, Ctrl-T, or Esc are valid. Ignore regular chars.
        }
        Some(Dialog::WorkflowCancelConfirm) => match c {
            'y' | 'Y' => {
                // Tell the engine to abort via the dialog response channel.
                app.send_dialog_response(DialogResponse::Char('a'));
                app.active_dialog = None;
                app.command_dialog_active = false;
            }
            'n' | 'N' => {
                // Just dismiss — the engine keeps running.
                app.active_dialog = None;
            }
            _ => {}
        },
        // `y` is the only recovery there is: the previous key was never stored
        // in plaintext, so a working client means a *new* key, which means a
        // new hash and a restarted daemon. `n` opens no tab — see the variant's
        // doc comment.
        Some(Dialog::SquadKeyMissing) => match c {
            'y' | 'Y' => {
                app.active_dialog = None;
                app.start_squad_key_refresh();
            }
            'n' | 'N' => {
                app.active_dialog = None;
                app.status_bar.text =
                    "squad needs a bearer key; the squad tab was not opened.".to_string();
            }
            _ => {}
        },
        // WI 0110: `y` accepts starting a squad daemon in the background and
        // opening the tab; `n`/`Esc` opens no tab and starts nothing. The
        // dialog decides nothing itself — it only collects the consent
        // `open_or_focus_squad_tab` asked for.
        Some(Dialog::SquadStartConfirm) => match c {
            'y' | 'Y' => {
                app.active_dialog = None;
                app.build_and_install_squad_tab();
            }
            'n' | 'N' => {
                app.active_dialog = None;
                app.status_bar.text =
                    "squad daemon not started; the squad tab was not opened.".to_string();
            }
            _ => {}
        },
        // WI 0102: `y` confirms removal by dispatching `squad remove <name>`
        // through the ordinary Layer-2 path; `n`/`Esc` dismisses. This dialog
        // decides nothing itself — it only collects the confirmation.
        Some(Dialog::SquadRemoveConfirm { name }) => {
            let name = name.clone();
            match c {
                'y' | 'Y' => {
                    app.active_dialog = None;
                    // This dialog IS the confirmation; the action carries the
                    // catalogue's "assume yes" flag so Layer 2 does not ask
                    // again.
                    let parsed = app.catalogue.action_input(
                        crate::command::dispatch::FrontendAction::RemoveSquadTask,
                        Some(&name),
                    );
                    app.spawn_command(parsed);
                }
                'n' | 'N' => {
                    app.active_dialog = None;
                }
                _ => {}
            }
        }

        // Which key means what is the prompt's; the frontend only maps the
        // press (case-insensitively, as it always has) and applies the answer.
        Some(Dialog::SquadActionConfirm {
            action,
            name,
            prompt,
        }) => {
            let answer = prompt.answer_for_key(c.to_ascii_lowercase());
            let (action, name) = (*action, name.clone());
            if let Some(decision) = answer {
                app.active_dialog = None;
                apply_squad_confirm(app, action, &name, Some(decision));
            }
        }

        // ── Command-originated dialogs ───────────────────────────────
        Some(Dialog::YesNo { .. }) if is_command => match c {
            'y' => {
                app.send_dialog_response(DialogResponse::Yes);
                app.active_dialog = None;
                app.command_dialog_active = false;
            }
            'n' => {
                app.send_dialog_response(DialogResponse::No);
                app.active_dialog = None;
                app.command_dialog_active = false;
            }
            _ => {}
        },
        Some(Dialog::YesNoCancel { .. }) if is_command => match c {
            'y' => {
                app.send_dialog_response(DialogResponse::Yes);
                app.active_dialog = None;
                app.command_dialog_active = false;
            }
            'n' => {
                app.send_dialog_response(DialogResponse::No);
                app.active_dialog = None;
                app.command_dialog_active = false;
            }
            _ => {}
        },

        Some(Dialog::MountScope { .. }) => {
            app.send_dialog_response(DialogResponse::Char(c));
            app.active_dialog = None;
            app.command_dialog_active = false;
        }
        Some(Dialog::AgentSetup { .. }) => {
            app.send_dialog_response(DialogResponse::Char(c));
            app.active_dialog = None;
            app.command_dialog_active = false;
        }
        Some(Dialog::AgentAuth { .. }) => {
            app.send_dialog_response(DialogResponse::Char(c));
            app.active_dialog = None;
            app.command_dialog_active = false;
        }
        Some(Dialog::Custom { ref keys, .. }) => {
            if keys.iter().any(|(ch, _)| *ch == c) {
                app.send_dialog_response(DialogResponse::Char(c));
                app.active_dialog = None;
                app.command_dialog_active = false;
            }
        }

        Some(Dialog::WorkflowControlBoard { .. }) => {
            app.send_dialog_response(DialogResponse::Char(c));
            app.active_dialog = None;
            app.command_dialog_active = false;
        }
        Some(Dialog::WorkflowYoloCountdown { .. }) => {
            app.send_dialog_response(DialogResponse::Char(c));
            app.active_dialog = None;
            app.command_dialog_active = false;
        }

        Some(Dialog::WorkflowStepConfirm(_)) => {
            // Only Ctrl+W is handled as a char here — it escalates to the full WCB.
            // Enter and Esc are handled by SubmitCommand and DismissDialog actions.
        }

        Some(Dialog::KindSelect { options, .. }) if is_command => {
            if let Some(digit) = c.to_digit(10) {
                let idx = digit as usize;
                if idx >= 1 && idx <= options.len() {
                    app.send_dialog_response(DialogResponse::Index(idx - 1));
                    app.active_dialog = None;
                    app.command_dialog_active = false;
                }
            }
        }

        // ── Text input in dialogs ────────────────────────────────────
        Some(Dialog::TextInput { .. }) | Some(Dialog::MultilineInput { .. }) => {
            if let Some(Dialog::TextInput { editor, .. })
            | Some(Dialog::MultilineInput { editor, .. }) = &mut app.active_dialog
            {
                editor.insert_char(c);
            }
        }

        Some(Dialog::ConfigShow(state)) if state.editing => {
            if let Some(Dialog::ConfigShow(state)) = &mut app.active_dialog {
                state.editor.insert_char(c);
            }
        }
        Some(Dialog::ConfigShow(_)) => {
            // Browse mode: `e` starts editing the selected row (same as
            // Enter); other char keys are ignored.
            if c == 'e' {
                config_show_begin_edit(app);
            }
        }

        // WI 0106 Part 5: the detail modal's action tooltip — attach/pause/
        // resume/remove, scoped to the specific task the modal is showing
        // (`state.name`), never the list's current selection (which may have
        // moved since the modal was opened). Esc still dismisses.
        Some(Dialog::SquadTaskDetail(state)) => {
            let name = state.name.clone();
            match c {
                // The history replaces this modal rather than stacking over
                // it, and Esc there comes straight back here.
                'h' => {
                    key_handler::open_squad_history(app, &name, true);
                }
                'a' => {
                    app.active_dialog = None;
                    crate::frontend::tui::squad_attach::start_squad_attach(app, &name);
                }
                'p' => {
                    key_handler::squad_task_action(app, FrontendAction::PauseSquadTask, name);
                }
                'r' => {
                    app.active_dialog = None;
                    key_handler::squad_task_action(app, FrontendAction::ResumeSquadTask, name);
                }
                // Evaluate this task on the next tick regardless of its
                // schedule — the modal's counterpart to `t` on the grid, and
                // scoped to the modal's task like every other key here.
                't' => {
                    key_handler::squad_task_action(app, FrontendAction::TriggerSquadTask, name);
                }
                // Stop this task's in-progress run — the modal's counterpart
                // to `c` on the grid.
                'c' => {
                    key_handler::squad_task_action(app, FrontendAction::CancelSquadRun, name);
                }
                // WI 0110: edit the task the modal is showing, not the list's
                // current selection — the same scoping the other four keys use.
                'e' => {
                    key_handler::squad_edit_by_name(app, &name);
                    app.active_dialog = None;
                }
                'd' => {
                    app.active_dialog = Some(Dialog::SquadRemoveConfirm { name });
                }
                _ => {}
            }
        }

        // `c`/`z` copy the key or the shell snippet to the clipboard without
        // dismissing the dialog — the user may want both before pressing
        // Enter, and a copy is not itself an acknowledgment of the notice.
        Some(Dialog::Notice {
            copy_key,
            copy_zshrc_snippet,
            ..
        }) => {
            let copy = match c {
                'c' => copy_key.clone().map(|text| ("squad key", text)),
                'z' => copy_zshrc_snippet
                    .clone()
                    .map(|text| ("zshrc snippet", text)),
                _ => None,
            };
            if let Some((label, text)) = copy {
                key_handler::copy_dialog_text_to_clipboard(app, label, &text);
            }
        }

        // ── Non-interactive / fallback dialogs ─────────────────────
        // The history modal is read-only: the arrow/page keys scroll it via
        // `handle_dialog_scroll` and Esc leaves it. Per-task actions stay in
        // the detail modal, so no char key is silently overloaded here.
        Some(Dialog::SquadTaskHistory(_))
        | Some(Dialog::Loading { .. })
        | Some(Dialog::ListPicker { .. })
        | Some(Dialog::KindSelect { .. })
        | Some(Dialog::YesNo { .. })
        | Some(Dialog::YesNoCancel { .. })
        | Some(Dialog::FatalError { .. }) => {}

        None => {}
    }
}

/// Act on a squad confirmation's answer. `Dispatch` sends the action Layer 2
/// resolves to a command; `Dismiss` — and a prompt with no dismissal default —
/// does nothing.
fn apply_squad_confirm(
    app: &mut App,
    action: FrontendAction,
    name: &str,
    decision: Option<SquadConfirmDecision>,
) {
    if decision == Some(SquadConfirmDecision::Dispatch) {
        key_handler::squad_dispatch(app, action, name);
    }
}
