//! Keys that act on tabs, focus and the panes around them.
//!
//! Split out of `key_handler.rs` by WI 0114 F-51. `handle_key_event`'s
//! `match` routes each family here; the outer match stays exhaustive, so a
//! new `Action` variant is a compile error there rather than a silent no-op.

use super::*;

pub(super) fn handle(app: &mut App, action: Action) {
    match action {
        Action::OpenNewTabDialog => {
            // Ctrl-T while CloseTabConfirm is open closes just this tab.
            if matches!(app.active_dialog, Some(Dialog::CloseTabConfirm)) {
                app.active_dialog = None;
                app.close_active_tab();
                return;
            }
            let cwd = app
                .active_tab()
                .session
                .working_dir()
                .to_string_lossy()
                .to_string();
            // The squad shortcut is advertised in the dialog's key-hint row
            // (`render/dialog.rs`), next to Enter/Esc, not in the prompt.
            app.active_dialog = Some(Dialog::TextInput {
                title: dialogs::NEW_TAB_DIALOG_TITLE.to_string(),
                prompt: "Working directory:".to_string(),
                editor: {
                    let mut ed = text_edit::TextEdit::new(false);
                    ed.set_text(&cwd);
                    ed
                },
            });
            app.command_dialog_active = false;
        }
        Action::PreviousTab => app.switch_to_prev_tab(),
        Action::NextTab => app.switch_to_next_tab(),
        Action::CloseTabOrQuit => {
            // Second Ctrl-C while QuitConfirm or CloseTabConfirm is open
            // confirms the quit action immediately.
            if matches!(app.active_dialog, Some(Dialog::QuitConfirm)) {
                app.active_dialog = None;
                app.should_quit = true;
                return;
            }
            if matches!(app.active_dialog, Some(Dialog::CloseTabConfirm)) {
                app.active_dialog = None;
                app.should_quit = true;
                return;
            }
            // A fatal startup error leaves nothing to return to — Ctrl-C
            // quits outright, same as Enter/Esc.
            if matches!(app.active_dialog, Some(Dialog::FatalError { .. })) {
                app.active_dialog = None;
                app.should_quit = true;
                return;
            }
            if app.active_dialog.is_some() {
                return;
            }
            // If a workflow is active in the focused tab, prefer the
            // workflow-cancel confirmation over the close-tab one — old amux
            // semantics. The user can still escape and Ctrl+C again to close
            // the tab if they really mean it.
            let workflow_active = app
                .active_tab()
                .shared
                .workflow_state
                .lock()
                .map(|g| g.is_some())
                .unwrap_or(false);
            if workflow_active
                && matches!(
                    app.active_tab().execution_phase,
                    tabs::ExecutionPhase::Running { .. }
                )
            {
                app.active_dialog = Some(Dialog::WorkflowCancelConfirm);
            } else if app.tabs.len() > 1 {
                app.active_dialog = Some(Dialog::CloseTabConfirm);
            } else {
                app.active_dialog = Some(Dialog::QuitConfirm);
            }
        }
        Action::CycleContainerWindow => {
            let tab = app.active_tab_mut();
            tab.container_window_state = tab.container_window_state.cycle();
            // Selection coords are relative to the window the drag started
            // in; cycling swaps which window owns selections, so drop it.
            tab.mouse_selection = None;
            if tab.container_window_state != ContainerWindowState::Hidden {
                resize_slots_to_terminal(tab);
            }
        }
        Action::ToggleWorkflowOverview => {
            let tab = app.active_tab_mut();
            tab.workflow_overview_state = tab.workflow_overview_state.toggle();
            // The overview always opens at the top of the stage; a stale
            // offset from a previous maximization would hide the first steps.
            tab.workflow_overview_scroll_offset = 0;
            // Ctrl-O never touches `container_window_state` — the container
            // PTY's min/max is Ctrl-M's business alone. It does change the
            // height the PTY overlay is drawn at, so any selection anchored in
            // it no longer means anything.
            tab.mouse_selection = None;
            if tab.container_window_state != ContainerWindowState::Hidden {
                resize_slots_to_terminal(tab);
            }
        }
        Action::ToggleGitSidebar => {
            // WI 0102: the git sidebar is meaningless for the squad tab's
            // synthetic session, so Ctrl-G is a no-op while it is active.
            if app.active_tab().is_squad {
                return;
            }
            let tab = app.active_tab_mut();
            tab.git_sidebar_state = match tab.git_sidebar_state {
                git_sidebar::GitSidebarState::Open => git_sidebar::GitSidebarState::Closed,
                git_sidebar::GitSidebarState::Closed => git_sidebar::GitSidebarState::Open,
            };
            // Opening/closing the sidebar changes the width of the left chunk
            // that the container overlay occupies, so reflow the container PTY
            // to the new width. This is needed even when the container is
            // Maximized (it fills the left chunk, not the whole frame).
            if tab.container_window_state != ContainerWindowState::Hidden {
                resize_slots_to_terminal(tab);
            }
        }
        Action::FocusExecutionWindow => {
            app.focus = Focus::ExecutionWindow;
        }

        // ── Execution window actions ──────────────────────────────────
        Action::FocusCommandBox => {
            app.focus = Focus::CommandBox;
        }
        Action::ToggleStatusLog => {
            let tab = app.active_tab_mut();
            tab.status_log_collapsed = !tab.status_log_collapsed;
        }

        // ── Dialog actions ────────────────────────────────────────────
        Action::DetachContainers => {
            if crate::frontend::tui::squad_attach::detach_squad_attach(app) {
                return;
            }
            if app.active_tab().container_overlay_active() {
                app.active_tab_mut().container_window_state = tabs::ContainerWindowState::Minimized;
                app.focus = Focus::CommandBox;
                app.status_bar.text =
                    "Detached from the container. It is still running — ctrl-m to return."
                        .to_string();
                app.needs_redraw = true;
            }
        }

        // ── squad list actions (WI 0102) ───────────────────────────────
        // Each either opens a dialog or dispatches through `spawn_command`
        // into Layer 2. None calls a gateway method directly — the keys are a
        // shortcut over the same Layer-2 path the command box uses, never a
        // second path.
        // Unreachable: `handle_key_event` routes only this family here.
        _ => {}
    }
}
