//! Keyboard event handling: focus-context detection, keymap action
//! dispatch, PTY passthrough, clipboard, and command submission.

use crate::command::commands::squad::commands::SquadCommand;
use crate::command::dispatch::catalogue::CommandCatalogue;
use crate::command::dispatch::FrontendAction;
use crossterm::event::{KeyCode, KeyModifiers};

use super::app::{App, Focus};
use super::dialog_router::{self, CursorDir};
use super::dialogs::{self, Dialog, DialogResponse};
use super::event_loop::resize_slots_to_terminal;
use super::keymap::{Action, FocusContext};
use super::{command_box, git_sidebar, keymap, tabs, text_edit};
use tabs::ContainerWindowState;

/// Returns true when the active tab has a command currently running.
fn command_box_locked(app: &App) -> bool {
    matches!(
        app.active_tab().execution_phase,
        tabs::ExecutionPhase::Running { .. }
    )
}

/// Determine focus context and dispatch the key event through the keymap.
mod dialog;
mod edit;
mod scroll;
mod squad;
mod tab;

/// Which pane a key is aimed at, given what is on screen.
///
/// Decided before the keymap is consulted, because the same key means
/// different things in a dialog, in a maximised container, and in the command
/// box.
pub(super) fn focus_context(app: &App) -> FocusContext {
    if app.active_dialog.is_some() {
        FocusContext::Dialog
    } else if app.active_tab().container_overlay_active()
        && matches!(
            app.active_tab().execution_phase,
            tabs::ExecutionPhase::Running { .. }
        )
    {
        // Only treat the container overlay as the focus target while a command is
        // actively running.  Once the command finishes the overlay is closed, but
        // guard here too so a race can't leave the user unable to type.
        FocusContext::ContainerMaximized
    } else if app.active_tab().is_squad && app.active_tab().container_slots.is_empty() {
        // WI 0102: the squad task list holds focus. While an attach session
        // owns the tab's slots (`container_slots` non-empty) this falls through
        // to the ordinary ContainerMaximized/ExecutionWindow handling, so
        // Ctrl-S slot cycling and PTY passthrough behave exactly as in a normal
        // workflow run.
        //
        // WI 0112: the grid holds focus *regardless* of `app.focus`. The
        // command box is permanently inactive on this tab, so there is no
        // state in which a key should reach it; `App::tick_all_tabs` also
        // normalises `focus` onto the grid, this is the belt to its braces.
        FocusContext::SquadList
    } else {
        match app.focus {
            Focus::CommandBox => FocusContext::CommandBox,
            Focus::ExecutionWindow => FocusContext::ExecutionWindow,
        }
    }
}

/// Keys a dialog claims before the generic keymap runs.
///
/// Returns `true` when the key was consumed. These cannot go through the
/// keymap because they depend on *which* dialog is open, which the keymap's
/// `FocusContext::Dialog` does not distinguish.
fn intercept_dialog_keys(app: &mut App, key: crossterm::event::KeyEvent) -> bool {
    // WorkflowControlBoard intercepts arrow keys and Ctrl+Enter before the
    // generic keymap so they map to workflow navigation rather than scroll/cursor.
    if matches!(app.active_dialog, Some(Dialog::WorkflowControlBoard(_)))
        && handle_workflow_control_board_key(app, key)
    {
        return true;
    }

    // TUI-2: Yolo countdown dialog allows tab switching — dismiss the dialog
    // (countdown continues in the tab label) and switch tabs. With only 1 tab,
    // swallow the key so the generic char handler doesn't close the dialog.
    if matches!(app.active_dialog, Some(Dialog::WorkflowYoloCountdown(_)))
        && key.modifiers.contains(KeyModifiers::CONTROL)
    {
        match key.code {
            KeyCode::Char('a') | KeyCode::Char('d') => {
                if app.tabs.len() > 1 {
                    // Clear user-activity so the departing tab stays "stuck"
                    // and doesn't send a false StepUnstuck on switch-back.
                    app.active_dialog = None;
                    if key.code == KeyCode::Char('a') {
                        app.switch_to_prev_tab();
                    } else {
                        app.switch_to_next_tab();
                    }
                }
                return true;
            }
            _ => {}
        }
    }

    // Ctrl-S also rotates the focused parallel container while the yolo
    // countdown modal is open on it, mirroring the Ctrl-A/D tab-switch
    // carve-out above (a modal shouldn't block the one navigation action
    // that lets the user check on/dismiss a sibling container). The modal is
    // dismissed here; `tick_all_tabs` re-derives it next tick from whichever
    // slot is now focused, so it reopens automatically on rotating back to a
    // slot whose countdown is still running.
    if matches!(app.active_dialog, Some(Dialog::WorkflowYoloCountdown(_)))
        && key.code == KeyCode::Char('s')
        && key.modifiers.contains(KeyModifiers::CONTROL)
        && app.active_tab().has_multiple_slots()
    {
        app.active_dialog = None;
        app.active_tab_mut().cycle_focused_slot();
        return true;
    }

    // TUI-3: In MultilineInput dialogs, bare Enter inserts a newline while
    // Ctrl+Enter submits. The generic keymap maps Enter → SubmitCommand for
    // all dialogs, so we intercept here where we can inspect the dialog type.
    // Ctrl+S is also accepted as a submit keybinding because many terminals
    // cannot distinguish Ctrl+Enter from bare Enter without the kitty
    // keyboard protocol.
    if matches!(app.active_dialog, Some(Dialog::MultilineInput { .. })) {
        if key.code == KeyCode::Enter {
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            let shift = key.modifiers.contains(KeyModifiers::SHIFT);
            if ctrl || shift {
                dialog_router::handle_dialog_submit(app);
            } else {
                if let Some(Dialog::MultilineInput { editor, .. }) = &mut app.active_dialog {
                    editor.insert_newline();
                }
            }
            return true;
        }
        if key.code == KeyCode::Char('s') && key.modifiers.contains(KeyModifiers::CONTROL) {
            dialog_router::handle_dialog_submit(app);
            return true;
        }
    }

    // WI-0096 §6: Ctrl-S cycles the focused parallel container when more than
    // one is active. With zero or one slot it falls through untouched, so a
    // single container still receives Ctrl-S (flow control) via the PTY.
    if key.code == KeyCode::Char('s')
        && key.modifiers.contains(KeyModifiers::CONTROL)
        && app.active_dialog.is_none()
        && app.active_tab().has_multiple_slots()
    {
        let tab = app.active_tab_mut();
        tab.cycle_focused_slot();
        // No manual resize here: `tick_all_tabs` keeps every slot's parser
        // and PTY in lockstep with the overlay's actual inner rect, so the
        // rotated-in slot is already correctly sized.
        return true;
    }

    // Ctrl-S inside the New Tab dialog opens the squad tab. Safe because the
    // other Ctrl-S meanings (multiline submit, slot cycling) are gated on
    // dialog types / no-dialog states that can never be the New Tab dialog,
    // so the key is genuinely unclaimed here. Do NOT add a global Ctrl-S
    // mapping for this — the binding is scoped to this one dialog.
    if key.code == KeyCode::Char('s')
        && key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(
            &app.active_dialog,
            Some(Dialog::TextInput { title, .. }) if title == dialogs::NEW_TAB_DIALOG_TITLE
        )
    {
        app.active_dialog = None;
        app.command_dialog_active = false;
        app.open_or_focus_squad_tab();
        return true;
    }

    false
}

pub(super) fn handle_key_event(app: &mut App, key: crossterm::event::KeyEvent) {
    let ctx = focus_context(app);
    if intercept_dialog_keys(app, key) {
        return;
    }

    let overview_hscroll_active = app
        .active_tab()
        .last_overview_hlayout
        .is_some_and(|layout| layout.overflows());
    let action = keymap::map_key(key, ctx, overview_hscroll_active);

    match action {
        Action::OpenNewTabDialog
        | Action::PreviousTab
        | Action::NextTab
        | Action::CloseTabOrQuit
        | Action::CycleContainerWindow
        | Action::ToggleWorkflowOverview
        | Action::ToggleGitSidebar
        | Action::FocusExecutionWindow
        | Action::FocusCommandBox
        | Action::ToggleStatusLog
        | Action::DetachContainers => tab::handle(app, action),

        Action::ScrollUp
        | Action::ScrollDown
        | Action::ScrollPageUp
        | Action::ScrollPageDown
        | Action::ScrollToTop
        | Action::ScrollToBottom => scroll::handle(app, action, ctx),

        Action::SubmitCommand
        | Action::AutocompleteNext
        | Action::AutocompletePrev
        | Action::CopySelection
        | Action::Char(_)
        | Action::Backspace
        | Action::Delete
        | Action::BackspaceWord
        | Action::CursorLeft
        | Action::CursorRight
        | Action::CursorWordLeft
        | Action::CursorWordRight
        | Action::CursorHome
        | Action::CursorEnd
        | Action::InsertNewline => edit::handle(app, action, ctx),

        Action::SquadShowDetail
        | Action::SquadShowHistory
        | Action::SquadAttach
        | Action::SquadNew
        | Action::SquadEdit
        | Action::SquadPause
        | Action::SquadResume
        | Action::SquadTrigger
        | Action::SquadCancel
        | Action::SquadDelete
        | Action::SquadMoveLeft
        | Action::SquadMoveRight => squad::handle(app, action),

        Action::WorkflowControl
        | Action::OpenConfigShow
        | Action::DismissDialog
        | Action::NewMapEntry => dialog::handle(app, action),

        Action::ForwardToPty(key_event) => {
            forward_key_to_pty(app, key_event);
        }

        Action::ScrollWorkflowOverviewLeft => app
            .active_tab_mut()
            .scroll_workflow_overview_horizontal(false),
        Action::ScrollWorkflowOverviewRight => app
            .active_tab_mut()
            .scroll_workflow_overview_horizontal(true),

        Action::None => {
            // When the execution window is focused and the command is finished,
            // any unhandled key press returns focus to the command box.
            if ctx == FocusContext::ExecutionWindow {
                let done_or_error = matches!(
                    app.active_tab().execution_phase,
                    tabs::ExecutionPhase::Done { .. } | tabs::ExecutionPhase::Error { .. }
                );
                if done_or_error {
                    app.focus = Focus::CommandBox;
                }
            }
        }
    }
}

/// Extract the selected text from a snapshot. Range is inclusive on both ends;
/// trailing whitespace per line is stripped; rows are joined with `\n`.
pub(super) fn extract_selection_text(sel: &tabs::TextSelection) -> String {
    let (sr, sc, er, ec) = if sel.start_row < sel.end_row
        || (sel.start_row == sel.end_row && sel.start_col <= sel.end_col)
    {
        (
            sel.start_row as usize,
            sel.start_col as usize,
            sel.end_row as usize,
            sel.end_col as usize,
        )
    } else {
        (
            sel.end_row as usize,
            sel.end_col as usize,
            sel.start_row as usize,
            sel.start_col as usize,
        )
    };
    let mut result = String::new();
    for row in sr..=er {
        if row >= sel.snapshot.len() {
            break;
        }
        let row_data = &sel.snapshot[row];
        let col_start = if row == sr { sc } else { 0 };
        let col_end = if row == er {
            (ec + 1).min(row_data.len())
        } else {
            row_data.len()
        };
        let mut line = String::new();
        for col in col_start..col_end {
            if col < row_data.len() {
                line.push_str(&row_data[col]);
            }
        }
        result.push_str(line.trim_end());
        if row < er {
            result.push('\n');
        }
    }
    result
}

// ─── PTY forwarding ──────────────────────────────────────────────────────────

fn forward_key_to_pty(app: &mut App, key: crossterm::event::KeyEvent) {
    if let Some(bytes) = key_to_bytes(&key) {
        // Keystrokes (incl. Ctrl-C) go only to the focused slot's PTY.
        if let Some(slot) = app.active_tab_mut().focused_slot_mut() {
            if let Some(tx) = slot.container_stdin_tx.as_ref() {
                let _ = tx.send(bytes);
            }
        }
    }
}

fn key_to_bytes(key: &crossterm::event::KeyEvent) -> Option<Vec<u8>> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Char(c) => {
            if ctrl {
                let n = (c as u8).to_ascii_lowercase();
                if n.is_ascii_lowercase() {
                    return Some(vec![n - b'a' + 1]);
                }
            }
            let mut buf = [0u8; 4];
            Some(c.encode_utf8(&mut buf).as_bytes().to_vec())
        }
        KeyCode::Enter => Some(b"\r".to_vec()),
        KeyCode::Backspace => Some(b"\x7f".to_vec()),
        KeyCode::Tab => Some(b"\t".to_vec()),
        KeyCode::Esc => Some(b"\x1b".to_vec()),
        KeyCode::Up => Some(b"\x1b[A".to_vec()),
        KeyCode::Down => Some(b"\x1b[B".to_vec()),
        KeyCode::Right => Some(b"\x1b[C".to_vec()),
        KeyCode::Left => Some(b"\x1b[D".to_vec()),
        KeyCode::Home => Some(b"\x1b[H".to_vec()),
        KeyCode::End => Some(b"\x1b[F".to_vec()),
        KeyCode::PageUp => Some(b"\x1b[5~".to_vec()),
        KeyCode::PageDown => Some(b"\x1b[6~".to_vec()),
        KeyCode::Delete => Some(b"\x1b[3~".to_vec()),
        KeyCode::F(n) => Some(format!("\x1b[{}~", n).into_bytes()),
        _ => None,
    }
}

// ─── Clipboard ───────────────────────────────────────────────────────────────

/// Put `text` on the system clipboard.
///
/// Under test isolation the "clipboard" is a process-local buffer instead:
/// the real one is the developer's, and a copy test would otherwise replace
/// whatever they had on it (with a squad key, at that).
fn set_clipboard_text(text: &str) -> Result<(), String> {
    if crate::data::config::env::test_isolation_active() {
        *isolated_clipboard()
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(text.to_string());
        return Ok(());
    }
    arboard::Clipboard::new()
        .and_then(|mut cb| cb.set_text(text))
        .map_err(|e| e.to_string())
}

/// The clipboard [`set_clipboard_text`] writes under test isolation.
fn isolated_clipboard() -> &'static std::sync::Mutex<Option<String>> {
    static CLIPBOARD: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
    &CLIPBOARD
}

fn copy_selection_to_clipboard(app: &mut App) {
    let tab = app.active_tab();
    let text = match tab.mouse_selection.as_ref() {
        Some(sel) if !sel.snapshot.is_empty() => extract_selection_text(sel),
        _ => return,
    };
    if text.is_empty() {
        return;
    }
    match set_clipboard_text(&text) {
        Ok(()) => {
            // Drop the selection after a successful copy so the copy hint
            // disappears and a subsequent Ctrl+Y doesn't re-yank.
            app.active_tab_mut().mouse_selection = None;
        }
        Err(e) => {
            app.active_tab_mut()
                .shared
                .status_log
                .lock()
                .map(|mut log| {
                    log.push(crate::frontend::tui::user_message::StatusLogEntry {
                        level: crate::data::message::MessageLevel::Error,
                        text: format!("clipboard unavailable: {e}"),
                    })
                })
                .ok();
        }
    }
}

/// Copy `text` to the clipboard for a dialog's `[c]`/`[z]` copy action (WI
/// 0111) and report the outcome via `status_log`. Unlike a mouse-selection
/// copy, a dialog has no selection state to clear on success, so this needs
/// its own success feedback: `label` (e.g. "squad key") names what was
/// copied.
pub(super) fn copy_dialog_text_to_clipboard(app: &mut App, label: &str, text: &str) {
    let (level, message) = match set_clipboard_text(text) {
        Ok(()) => (
            crate::data::message::MessageLevel::Info,
            format!("{label} copied to clipboard"),
        ),
        Err(e) => (
            crate::data::message::MessageLevel::Error,
            format!("clipboard unavailable: {e}"),
        ),
    };
    app.active_tab_mut()
        .shared
        .status_log
        .lock()
        .map(|mut log| {
            log.push(crate::frontend::tui::user_message::StatusLogEntry {
                level,
                text: message,
            })
        })
        .ok();
}

// ─── Command submission ──────────────────────────────────────────────────────

/// Handle command submission from the command box.
fn handle_command_submit(app: &mut App) {
    let text = app.command_input.text.clone();
    if text.trim().is_empty() {
        return;
    }

    match command_box::parse_input(&text) {
        Ok(parsed) => {
            app.input_error = None;
            app.command_input.set_text("");
            app.suggestion_row.clear();
            app.spawn_command(parsed);
        }
        Err(err) => {
            app.input_error = Some(command_box::format_parse_error(&err));
        }
    }
}

// ─── squad list helpers (WI 0102) ─────────────────────────────────────────────

/// Dispatch an `squad <subcommand> <name>` action (pause/resume) for the
/// selected task through the ordinary Layer-2 path. No-op when the list
/// is empty.
/// Dispatch `squad <subcommand> <name>` through the ordinary Layer-2 path.
/// `pub(super)` so the detail modal's action tooltip (`dialog_router.rs`) can
/// reuse it against the task the modal is showing, rather than the list's
/// current selection.
/// Raise `action` against task `name` from a key.
///
/// Whether the user is asked first, and in what words, is
/// `SquadCommand::confirm_prompt`'s answer, not this module's: a prompt means
/// the confirmation modal opens over whatever is showing (the detail modal,
/// when pressed from there), and no prompt means the action goes straight
/// out. Before WI 0114 F-55 the TUI made both calls — three actions routed
/// through a confirm helper and `resume` around it.
pub(super) fn squad_task_action(app: &mut App, action: FrontendAction, name: String) {
    match SquadCommand::confirm_prompt(action, &name) {
        Some(prompt) => {
            app.active_dialog = Some(Dialog::SquadActionConfirm {
                action,
                name,
                prompt,
            })
        }
        None => squad_dispatch(app, action, &name),
    }
}

/// Dispatch `action` against task `name` through the ordinary Layer-2 path.
///
/// The subcommand and the positional argument's name both come out of the
/// catalogue (`CommandCatalogue::action_input`); this function knows neither.
/// `pub(super)` so the confirmation's `y` in `dialog_router.rs` can send the
/// action it has just had approved.
pub(super) fn squad_dispatch(app: &mut App, action: FrontendAction, name: &str) {
    let input = CommandCatalogue::get().action_input(action, Some(name));
    app.spawn_command(input);
}

/// Open the run-history modal for `name`. `from_detail` is what Esc later
/// consults: `true` reopens the detail modal the user came from, `false`
/// closes back to the card grid. `pub(super)` so `dialog_router.rs` can open
/// it from the detail modal's `h` key.
///
/// The runs come from the tab snapshot the poller publishes, which holds the
/// history of the *selected* task — the same source the detail modal used
/// before the history moved out of it.
pub(super) fn open_squad_history(app: &mut App, name: &str, from_detail: bool) {
    let runs = app
        .active_tab()
        .squad
        .as_ref()
        .and_then(|state| state.snapshot.lock().ok().map(|snap| snap.runs.clone()))
        .unwrap_or_default();
    app.active_dialog = Some(Dialog::SquadTaskHistory(dialogs::SquadHistoryState {
        name: name.to_string(),
        runs,
        scroll: 0,
        from_detail,
    }));
}

/// Reopen the detail modal for `name` from the active squad tab's snapshot —
/// what Esc does in a history modal that was opened from the detail modal.
/// Returns `false` when the task is no longer in the snapshot (removed while
/// the history was up), leaving the caller to just close the modal.
pub(super) fn reopen_squad_detail(app: &mut App, name: &str) -> bool {
    let task = app.active_tab().squad.as_ref().and_then(|state| {
        state
            .snapshot
            .lock()
            .ok()
            .and_then(|snap| snap.tasks.iter().find(|t| t.name == name).cloned())
    });
    match task {
        Some(task) => {
            app.active_dialog = Some(Dialog::SquadTaskDetail(dialogs::SquadDetailState {
                name: name.to_string(),
                task,
            }));
            true
        }
        None => false,
    }
}

/// Dispatch `squad edit <name> --interview` through the ordinary Layer-2 path
/// (WI 0110). `pub(super)` so the detail modal can edit the task it is showing.
pub(super) fn squad_edit_by_name(app: &mut App, name: &str) {
    let parsed = app
        .catalogue
        .action_input(FrontendAction::EditSquadTask, Some(name));
    app.spawn_command(parsed);
}

// ─── WorkflowControlBoard special handler ────────────────────────────────────

/// Handle arrow keys, Ctrl+Enter, and `[d]` for the WorkflowControlBoard dialog.
///
/// Returns `true` if the key was consumed; `false` to let it fall through to
/// the generic dialog handler (for char keys like 'a', Esc, etc.).
///
/// Each arrow is gated on the engine's matching `can_*` flag: the board already
/// renders an unavailable action greyed out with its reason, and sending the
/// action anyway just makes the engine re-present the same board — a keystroke
/// that looks broken. An unavailable arrow is swallowed instead, leaving the
/// board up (WI-0115 §1).
fn handle_workflow_control_board_key(app: &mut App, key: crossterm::event::KeyEvent) -> bool {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

    let Some(Dialog::WorkflowControlBoard(state)) = &app.active_dialog else {
        return false;
    };
    let (can_finish, can_launch_next, can_restart, can_go_back, can_continue) = (
        state.can_finish,
        state.can_launch_next,
        state.can_restart,
        state.can_go_back,
        state.can_continue_current,
    );

    let response = match key.code {
        KeyCode::Right if can_launch_next => DialogResponse::Char('>'),
        KeyCode::Down if can_continue => DialogResponse::Char('v'),
        KeyCode::Up if can_restart => DialogResponse::Char('^'),
        KeyCode::Left if can_go_back => DialogResponse::Char('<'),
        // An arrow for an action this board does not offer: consume it so it
        // cannot fall through to the generic handler, and leave the board up.
        KeyCode::Right | KeyCode::Down | KeyCode::Up | KeyCode::Left => return true,
        // Many terminals cannot distinguish Ctrl+Enter from bare Enter
        // without the kitty keyboard protocol, so accept plain Enter too.
        KeyCode::Enter if can_finish => DialogResponse::Char('f'),
        KeyCode::Enter if ctrl => return false,
        KeyCode::Char('c') if ctrl => DialogResponse::Char('a'),
        _ => return false,
    };
    app.send_dialog_response(response);
    app.active_dialog = None;
    app.command_dialog_active = false;
    true
}

/// Raise a new-tab failure as a modal, and leave its text in the status bar
/// behind it. A status-bar line alone is easy to miss, which made a failed
/// open (e.g. a malformed `config.json`) look like Ctrl-T silently did nothing.
fn report_new_tab_failure(app: &mut App, message: String) {
    app.status_bar.text = message.clone();
    app.active_dialog = Some(Dialog::Notice {
        title: "could not open new tab".to_string(),
        body: message,
        copy_key: None,
        copy_zshrc_snippet: None,
    });
    app.command_dialog_active = false;
}

/// Handle path selection from the new-tab dialog.
pub(super) fn handle_new_tab_path(app: &mut App, path: &str) {
    let path = path.trim();
    if path.is_empty() {
        return;
    }
    let raw = std::path::PathBuf::from(path);
    let dir = if raw.is_absolute() {
        raw
    } else {
        app.active_tab().session.working_dir().join(raw)
    };
    if !dir.is_dir() {
        report_new_tab_failure(app, format!("Not a directory: {path}"));
        return;
    }

    let idx = match app.add_tab(dir, crate::data::session::SessionOpenOptions::default()) {
        Ok(idx) => idx,
        Err(error) => {
            report_new_tab_failure(app, format!("Failed to open session: {error}"));
            return;
        }
    };
    app.active_tab = idx;
    let startup = app.catalogue.startup_command(&app.tabs[idx].session);
    app.spawn_command(startup);
}
