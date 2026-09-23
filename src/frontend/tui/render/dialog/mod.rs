//! Modal dialog rendering: dispatch over the `Dialog` enum plus the
//! ConfigShow table widget and its cursor-windowing helper.

use super::*;

/// Render the currently active dialog.
pub(super) fn render_dialog(dialog: &dialogs::Dialog, area: Rect, frame: &mut Frame) {
    match dialog {
        dialogs::Dialog::QuitConfirm => {
            dialogs::render_quit_confirm(area, frame);
        }
        dialogs::Dialog::CloseTabConfirm => {
            dialogs::render_close_tab_confirm(area, frame);
        }
        dialogs::Dialog::WorkflowCancelConfirm => {
            dialogs::render_workflow_cancel_confirm(area, frame);
        }
        dialogs::Dialog::YesNo { title, body } => {
            dialogs::render_yes_no(title, body, area, frame);
        }
        dialogs::Dialog::YesNoCancel { title, body } => {
            // Same dynamic sizing as render_yes_no, plus an explicit Cancel.
            let max_w = area.width.saturating_sub(6).max(40);
            let max_body_w = body
                .lines()
                .map(unicode_width::UnicodeWidthStr::width)
                .max()
                .unwrap_or(0) as u16;
            let title_w = unicode_width::UnicodeWidthStr::width(title.as_str()) as u16 + 4;
            let width = max_body_w.saturating_add(6).max(50).max(title_w).min(max_w);
            let inner_w = width.saturating_sub(4) as usize;
            let wrapped_lines: usize = body
                .lines()
                .map(|line| {
                    let w = unicode_width::UnicodeWidthStr::width(line);
                    if inner_w == 0 || w == 0 {
                        1
                    } else {
                        w.div_ceil(inner_w)
                    }
                })
                .sum();
            let body_h = wrapped_lines as u16;
            // `body_h + 6`: 4 rows of frame + a blank separator + the hint
            // row. See `dialogs::render_yes_no` — a smaller height clips the
            // key hints off the bottom of the dialog.
            let height = (body_h + 6).min(area.height.saturating_sub(2)).max(8);
            let dialog_area = dialogs::centered_fixed(width, height, area);
            let inner = dialogs::render_dialog_frame(title, Color::Yellow, dialog_area, frame);
            let text = format!("{body}\n\n  [y] Yes   [n] No   [Esc] Cancel");
            frame.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }), inner);
        }
        dialogs::Dialog::TextInput {
            title,
            prompt,
            editor,
        } => input::render_text_input(title, prompt, editor, area, frame),
        dialogs::Dialog::MultilineInput {
            title,
            prompt,
            editor,
        } => input::render_multiline_input(title, prompt, editor, area, frame),
        dialogs::Dialog::ListPicker {
            title,
            items,
            selected,
        } => input::render_list_picker(title, items, *selected, area, frame),
        dialogs::Dialog::KindSelect { title, options } => {
            input::render_kind_select(title, options, area, frame)
        }
        dialogs::Dialog::WorkflowControlBoard(state) => {
            workflow::render_control_board(state, area, frame)
        }
        dialogs::Dialog::WorkflowYoloCountdown(state) => {
            workflow::render_yolo_countdown(state, area, frame)
        }
        dialogs::Dialog::AgentSetup(state) => misc::render_agent_setup(state, area, frame),
        dialogs::Dialog::MountScope(state) => misc::render_mount_scope(state, area, frame),
        dialogs::Dialog::AgentAuth(state) => misc::render_agent_auth(state, area, frame),
        dialogs::Dialog::ConfigShow(state) => {
            render_config_show(state, area, frame);
        }
        dialogs::Dialog::SquadTaskDetail(state) => {
            render_squad_detail(state, area, frame);
        }
        dialogs::Dialog::SquadTaskHistory(state) => {
            render_squad_history(state, area, frame);
        }
        dialogs::Dialog::SquadStartConfirm => squad::render_start_confirm(area, frame),
        dialogs::Dialog::SquadKeyMissing => squad::render_key_missing(area, frame),
        dialogs::Dialog::SquadRemoveConfirm { name } => {
            squad::render_remove_confirm(name, area, frame)
        }
        dialogs::Dialog::SquadActionConfirm { prompt, .. } => {
            squad::render_action_confirm(prompt, area, frame)
        }
        dialogs::Dialog::Loading { title } => misc::render_loading(title, area, frame),
        dialogs::Dialog::WorkflowStepConfirm(state) => {
            workflow::render_step_confirm(state, area, frame)
        }
        dialogs::Dialog::Custom { title, body, keys } => {
            misc::render_custom(title, body, keys, area, frame)
        }
        dialogs::Dialog::FatalError { title, body } => {
            misc::render_fatal_error(title, body, area, frame)
        }
        dialogs::Dialog::Notice {
            title,
            body,
            copy_key,
            copy_zshrc_snippet,
        } => misc::render_notice(
            title,
            body,
            copy_key.as_ref(),
            copy_zshrc_snippet.as_ref(),
            area,
            frame,
        ),
    }
}

mod input;
mod misc;
mod squad;
mod workflow;

// `render/tests.rs` drives these two directly; the ConfigShow table is worth
// asserting on without going through the whole dialog dispatch.
#[cfg(test)]
pub(super) use input::cursor_window;
#[cfg(test)]
pub(super) use input::render_config_show as render_config_show_for_tests;
// The squad detail and history modals are called from arms that stayed in the
// match above (they were already extracted before this split).
use input::render_config_show;
use squad::{render_squad_detail, render_squad_history};
