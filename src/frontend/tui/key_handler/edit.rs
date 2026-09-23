//! Keys that edit the command box or a dialog's text field, and the two
//! that submit.
//!
//! Split out of `key_handler.rs` by WI 0114 F-51. `handle_key_event`'s
//! `match` routes each family here; the outer match stays exhaustive, so a
//! new `Action` variant is a compile error there rather than a silent no-op.

use super::*;

pub(super) fn handle(app: &mut App, action: Action, ctx: FocusContext) {
    match action {
        Action::SubmitCommand => {
            if ctx == FocusContext::Dialog {
                dialog_router::handle_dialog_submit(app);
            } else if !command_box_locked(app) {
                handle_command_submit(app);
            }
        }
        Action::AutocompleteNext => {
            app.update_suggestions();
            if !app.suggestion_row.is_empty() {
                let suggestion = app.suggestion_row[0].clone();
                app.command_input.set_text(&suggestion);
            }
        }
        Action::AutocompletePrev => {
            app.update_suggestions();
            if let Some(suggestion) = app.suggestion_row.last().cloned() {
                app.command_input.set_text(&suggestion);
            }
        }
        Action::CopySelection => {
            copy_selection_to_clipboard(app);
        }
        Action::Char(c) => {
            if ctx == FocusContext::Dialog {
                dialog_router::handle_dialog_char(app, c);
            } else if command_box_locked(app) {
                // Command box is read-only while a command is executing.
            } else if c == 'q' && app.command_input.text.is_empty() {
                // `q` with an empty input opens the quit dialog (old-TUI parity).
                app.active_dialog = Some(Dialog::QuitConfirm);
            } else {
                app.command_input.insert_char(c);
                app.input_error = None;
                app.update_suggestions();
            }
        }
        Action::Backspace => {
            if ctx == FocusContext::Dialog {
                dialog_router::handle_dialog_backspace(app);
            } else if !command_box_locked(app) {
                app.command_input.backspace();
                app.input_error = None;
                app.update_suggestions();
            }
        }
        Action::Delete => {
            if ctx == FocusContext::Dialog {
                dialog_router::handle_dialog_delete(app);
            } else if !command_box_locked(app) {
                app.command_input.delete();
                app.input_error = None;
                app.update_suggestions();
            }
        }
        Action::BackspaceWord => {
            if !command_box_locked(app) {
                app.command_input.backspace_word();
                app.input_error = None;
                app.update_suggestions();
            }
        }
        Action::CursorLeft => {
            if ctx == FocusContext::Dialog {
                dialog_router::handle_dialog_cursor(app, CursorDir::Left);
            } else if !command_box_locked(app) {
                app.command_input.move_left();
            }
        }
        Action::CursorRight => {
            if ctx == FocusContext::Dialog {
                dialog_router::handle_dialog_cursor(app, CursorDir::Right);
            } else if !command_box_locked(app) {
                app.command_input.move_right();
            }
        }
        Action::CursorWordLeft => {
            if !command_box_locked(app) {
                app.command_input.move_word_left();
            }
        }
        Action::CursorWordRight => {
            if !command_box_locked(app) {
                app.command_input.move_word_right();
            }
        }
        Action::CursorHome => {
            if ctx == FocusContext::Dialog {
                dialog_router::handle_dialog_cursor(app, CursorDir::Home);
            } else if !command_box_locked(app) {
                app.command_input.move_home();
            }
        }
        Action::CursorEnd => {
            if ctx == FocusContext::Dialog {
                dialog_router::handle_dialog_cursor(app, CursorDir::End);
            } else if !command_box_locked(app) {
                app.command_input.move_end();
            }
        }
        Action::InsertNewline => {
            if !command_box_locked(app) {
                app.command_input.insert_newline();
            }
        }

        // WI 0110: Ctrl-\ leaves the container view without signalling any
        // container. A squad attach session ends outright (its local attach
        // clients are killed, the daemon's containers keep running); an
        // ordinary command's maximized container is merely minimized, so it
        // keeps streaming into its status bar and Ctrl-M brings it back. In
        // neither case does a byte reach the agent's PTY — that is the whole
        // difference from Ctrl-C.
        // Unreachable: `handle_key_event` routes only this family here.
        _ => {}
    }
}
