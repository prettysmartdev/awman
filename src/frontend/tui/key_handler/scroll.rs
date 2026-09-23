//! Keys that scroll the focused pane.
//!
//! Split out of `key_handler.rs` by WI 0114 F-51. `handle_key_event`'s
//! `match` routes each family here; the outer match stays exhaustive, so a
//! new `Action` variant is a compile error there rather than a silent no-op.

use super::*;

pub(super) fn handle(app: &mut App, action: Action, ctx: FocusContext) {
    match action {
        Action::ScrollUp => {
            if ctx == FocusContext::Dialog {
                dialog_router::handle_dialog_scroll(app, -1);
            } else if ctx == FocusContext::SquadList {
                if let Some(state) = app.active_tab_mut().squad.as_mut() {
                    state.move_selection(-1);
                }
            } else {
                let tab = app.active_tab_mut();
                tab.scroll_offset = tab.scroll_offset.saturating_add(1);
            }
        }
        Action::ScrollDown => {
            if ctx == FocusContext::Dialog {
                dialog_router::handle_dialog_scroll(app, 1);
            } else if ctx == FocusContext::SquadList {
                if let Some(state) = app.active_tab_mut().squad.as_mut() {
                    state.move_selection(1);
                }
            } else {
                let tab = app.active_tab_mut();
                tab.scroll_offset = tab.scroll_offset.saturating_sub(1);
            }
        }
        Action::ScrollPageUp => {
            if ctx == FocusContext::Dialog {
                dialog_router::handle_dialog_scroll(app, -10);
            } else {
                let tab = app.active_tab_mut();
                tab.scroll_offset = tab.scroll_offset.saturating_add(20);
            }
        }
        Action::ScrollPageDown => {
            if ctx == FocusContext::Dialog {
                dialog_router::handle_dialog_scroll(app, 10);
            } else {
                let tab = app.active_tab_mut();
                tab.scroll_offset = tab.scroll_offset.saturating_sub(20);
            }
        }
        Action::ScrollToTop => {
            let tab = app.active_tab_mut();
            tab.scroll_offset = usize::MAX / 2;
        }
        Action::ScrollToBottom => {
            let tab = app.active_tab_mut();
            tab.scroll_offset = 0;
        }
        // Unreachable: `handle_key_event` routes only this family here.
        _ => {}
    }
}
