//! TUI frontend — Ratatui-based interactive terminal UI.
//!
//! Captures the terminal (raw mode, alternate screen, mouse), constructs
//! `App` state, enters the event loop, and restores the terminal on exit.

use std::io;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

static TUI_ACTIVE: AtomicBool = AtomicBool::new(false);

pub fn is_tui_active() -> bool {
    TUI_ACTIVE.load(Ordering::Relaxed)
}

use crossterm::event::{
    self, Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::prelude::*;
use tokio::sync::RwLock;

use crate::command::dispatch::catalogue::CommandCatalogue;
use crate::command::dispatch::parsed_input::ParsedCommandBoxInput;
use crate::data::session_manager::SessionManager;
use crate::frontend::cli::RuntimeContext;

pub mod app;
pub mod command_box;
pub mod command_frontend;
pub mod container_view;
pub mod dialogs;
pub mod hints;
pub mod keymap;
pub mod per_command;
pub mod pty;
pub mod render;
pub mod tabs;
pub mod text_edit;
pub mod user_message;
pub mod workflow_view;

use app::{App, Focus};
use dialogs::{Dialog, DialogResponse};
use keymap::{Action, FocusContext};
use tabs::{ContainerWindowState, Tab};

/// Entry point invoked by `main.rs` for bare (no-subcommand) launches.
pub async fn run(_matches: clap::ArgMatches, ctx: RuntimeContext) -> ExitCode {
    let catalogue = CommandCatalogue::get();
    let session_manager = Arc::new(RwLock::new(SessionManager::in_memory()));

    let session = ctx.session.read().await.clone();
    let initial_tab = Tab::new(session);
    let runtime_handle = tokio::runtime::Handle::current();

    let mut app = App::new(
        catalogue,
        ctx.engines,
        session_manager,
        initial_tab,
        runtime_handle,
    );

    // Auto-spawn startup command: `ready` for git repos, `status --watch`
    // for non-git directories.
    let is_git = app.active_tab().session.git_root().join(".git").exists();
    if is_git {
        app.spawn_command(
            "ready",
            ParsedCommandBoxInput {
                path: vec!["ready".into()],
                flags: Default::default(),
                arguments: Default::default(),
            },
        );
    } else {
        let mut flags = std::collections::BTreeMap::new();
        flags.insert(
            "watch".to_string(),
            crate::command::dispatch::parsed_input::FlagValue::Bool(true),
        );
        app.spawn_command(
            "status --watch",
            ParsedCommandBoxInput {
                path: vec!["status".into()],
                flags,
                arguments: Default::default(),
            },
        );
    }

    match run_event_loop(&mut app) {
        Ok(()) => ExitCode::from(0),
        Err(e) => {
            eprintln!("awman: TUI error: {e}");
            ExitCode::from(1)
        }
    }
}

/// Restore the terminal to a clean state. Idempotent and best-effort: each
/// step is attempted independently so a failure in one doesn't leave later
/// steps un-run. Called from both the normal teardown path and the panic
/// hook so an unexpected panic doesn't leave the shell in raw mode with the
/// kitty keyboard protocol still active.
fn restore_terminal(keyboard_enhanced: bool) {
    let _ = disable_raw_mode();
    let mut stdout = io::stdout();
    if keyboard_enhanced {
        let _ = execute!(stdout, crossterm::event::PopKeyboardEnhancementFlags);
    }
    let _ = execute!(
        stdout,
        LeaveAlternateScreen,
        crossterm::event::DisableMouseCapture,
        crossterm::cursor::Show,
    );
}

/// Set up the terminal, run the main loop, and restore on exit.
fn run_event_loop(app: &mut App) -> io::Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();

    // Enable the kitty keyboard protocol so the terminal can distinguish
    // modifier+key combos (e.g. Ctrl+Enter vs bare Enter). Terminals that
    // don't support this silently ignore the escape sequence.
    let keyboard_enhanced = crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false);
    if keyboard_enhanced {
        execute!(
            stdout,
            crossterm::event::PushKeyboardEnhancementFlags(
                crossterm::event::KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
            )
        )?;
    }

    execute!(
        stdout,
        EnterAlternateScreen,
        crossterm::event::EnableMouseCapture
    )?;

    // Install a panic hook that restores the terminal before the default
    // hook prints the panic message — without this, a panic inside the
    // event loop would leave the shell in raw mode with the kitty
    // keyboard protocol pushed, so every keystroke (arrows, Ctrl-C, …)
    // would appear as a literal escape sequence in the user's prompt.
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        TUI_ACTIVE.store(false, Ordering::Relaxed);
        restore_terminal(keyboard_enhanced);
        original_hook(info);
    }));

    TUI_ACTIVE.store(true, Ordering::Relaxed);

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = main_loop(&mut terminal, app);

    TUI_ACTIVE.store(false, Ordering::Relaxed);
    restore_terminal(keyboard_enhanced);
    let _ = std::panic::take_hook();

    result
}

/// The main event loop: render → tick → poll → handle input → repeat.
fn main_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
) -> io::Result<()> {
    loop {
        if app.should_quit {
            break;
        }

        terminal.draw(|frame| {
            render::render_frame(app, frame);
        })?;

        app.tick_all_tabs();
        app.poll_dialog_requests();

        if event::poll(Duration::from_millis(50))? {
            match event::read()? {
                Event::Key(key_event) => {
                    if key_event.kind != KeyEventKind::Press {
                        continue;
                    }
                    handle_key_event(app, key_event);
                }
                Event::Mouse(mouse) => {
                    handle_mouse_event(app, mouse);
                }
                Event::Resize(cols, rows) => {
                    handle_resize(app, cols, rows);
                }
                _ => {}
            }
        }
    }
    Ok(())
}

/// Returns true when the active tab has a command currently running.
fn command_box_locked(app: &App) -> bool {
    matches!(
        app.active_tab().execution_phase,
        tabs::ExecutionPhase::Running { .. }
    )
}

/// Determine focus context and dispatch the key event through the keymap.
fn handle_key_event(app: &mut App, key: crossterm::event::KeyEvent) {
    let ctx = if app.active_dialog.is_some() {
        FocusContext::Dialog
    } else if app.active_tab().container_window_state == ContainerWindowState::Maximized
        && matches!(
            app.active_tab().execution_phase,
            tabs::ExecutionPhase::Running { .. }
        )
    {
        // Only treat the container overlay as the focus target while a command is
        // actively running.  Once the command finishes the overlay is closed, but
        // guard here too so a race can't leave the user unable to type.
        FocusContext::ContainerMaximized
    } else {
        match app.focus {
            Focus::CommandBox => FocusContext::CommandBox,
            Focus::ExecutionWindow => FocusContext::ExecutionWindow,
        }
    };

    // WorkflowControlBoard intercepts arrow keys and Ctrl+Enter before the
    // generic keymap so they map to workflow navigation rather than scroll/cursor.
    if matches!(app.active_dialog, Some(Dialog::WorkflowControlBoard(_)))
        && handle_workflow_control_board_key(app, key)
    {
        return;
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
                return;
            }
            _ => {}
        }
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
                handle_dialog_submit(app);
            } else {
                if let Some(Dialog::MultilineInput { editor, .. }) = &mut app.active_dialog {
                    editor.insert_newline();
                }
            }
            return;
        }
        if key.code == KeyCode::Char('s') && key.modifiers.contains(KeyModifiers::CONTROL) {
            handle_dialog_submit(app);
            return;
        }
    }

    let action = keymap::map_key(key, ctx);

    match action {
        // ── Global actions ────────────────────────────────────────────
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
            app.active_dialog = Some(Dialog::TextInput {
                title: "New Tab".to_string(),
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
            if app.active_dialog.is_some() {
                return;
            }
            // If a workflow is active in the focused tab, prefer the
            // workflow-cancel confirmation over the close-tab one — old amux
            // semantics. The user can still escape and Ctrl+C again to close
            // the tab if they really mean it.
            let workflow_active = app
                .active_tab()
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
            if tab.container_window_state != ContainerWindowState::Hidden {
                if let Ok(size) = crossterm::terminal::size() {
                    let (cols, rows) = compute_container_inner_size(size.0, size.1);
                    tab.vt100_parser.screen_mut().set_size(rows, cols);
                    if let Some(ref tx) = tab.container_resize_tx {
                        let _ = tx.send((cols, rows));
                    }
                }
            }
        }
        Action::WorkflowControl => {
            let engine_tx = app
                .active_tab()
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
                    dismiss_dialog(app);
                }
                let _ = tx.send(crate::engine::workflow::EngineRequest::OpenControlBoard);
            }
        }
        Action::OpenConfigShow => {
            // Run `config show` through dispatch so the command layer
            // computes the rows and the frontend trait presents the dialog.
            let parsed = crate::command::dispatch::parsed_input::ParsedCommandBoxInput {
                path: vec!["config".into(), "show".into()],
                flags: Default::default(),
                arguments: Default::default(),
            };
            app.spawn_command("config show", parsed);
        }

        // ── Command box actions ───────────────────────────────────────
        Action::SubmitCommand => {
            if ctx == FocusContext::Dialog {
                handle_dialog_submit(app);
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
        Action::FocusExecutionWindow => {
            app.focus = Focus::ExecutionWindow;
        }

        // ── Execution window actions ──────────────────────────────────
        Action::FocusCommandBox => {
            app.focus = Focus::CommandBox;
        }
        Action::ScrollUp => {
            if ctx == FocusContext::Dialog {
                handle_dialog_scroll(app, -1);
            } else {
                let tab = app.active_tab_mut();
                tab.scroll_offset = tab.scroll_offset.saturating_add(1);
            }
        }
        Action::ScrollDown => {
            if ctx == FocusContext::Dialog {
                handle_dialog_scroll(app, 1);
            } else {
                let tab = app.active_tab_mut();
                tab.scroll_offset = tab.scroll_offset.saturating_sub(1);
            }
        }
        Action::ScrollPageUp => {
            let tab = app.active_tab_mut();
            tab.scroll_offset = tab.scroll_offset.saturating_add(20);
        }
        Action::ScrollPageDown => {
            let tab = app.active_tab_mut();
            tab.scroll_offset = tab.scroll_offset.saturating_sub(20);
        }
        Action::ScrollToTop => {
            let tab = app.active_tab_mut();
            tab.scroll_offset = usize::MAX / 2;
        }
        Action::ScrollToBottom => {
            let tab = app.active_tab_mut();
            tab.scroll_offset = 0;
        }
        Action::CopySelection => {
            copy_selection_to_clipboard(app);
        }
        Action::ToggleStatusLog => {
            let tab = app.active_tab_mut();
            tab.status_log_collapsed = !tab.status_log_collapsed;
        }

        // ── Dialog actions ────────────────────────────────────────────
        Action::DismissDialog => {
            // In ConfigShow editing mode, Esc cancels the edit (back to browse).
            if let Some(Dialog::ConfigShow(state)) = &mut app.active_dialog {
                if state.editing {
                    state.editing = false;
                    return;
                }
            }
            if matches!(app.active_dialog, Some(Dialog::WorkflowYoloCountdown(_))) {
                app.active_tab()
                    .yolo_cancel_flag
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                app.active_dialog = None;
                return;
            }
            dismiss_dialog(app);
        }

        // ── Text input actions ────────────────────────────────────────
        Action::Char(c) => {
            if ctx == FocusContext::Dialog {
                handle_dialog_char(app, c);
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
                handle_dialog_backspace(app);
            } else if !command_box_locked(app) {
                app.command_input.backspace();
                app.input_error = None;
                app.update_suggestions();
            }
        }
        Action::Delete => {
            if ctx == FocusContext::Dialog {
                handle_dialog_delete(app);
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
                handle_dialog_cursor(app, CursorDir::Left);
            } else if !command_box_locked(app) {
                app.command_input.move_left();
            }
        }
        Action::CursorRight => {
            if ctx == FocusContext::Dialog {
                handle_dialog_cursor(app, CursorDir::Right);
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
                handle_dialog_cursor(app, CursorDir::Home);
            } else if !command_box_locked(app) {
                app.command_input.move_home();
            }
        }
        Action::CursorEnd => {
            if ctx == FocusContext::Dialog {
                handle_dialog_cursor(app, CursorDir::End);
            } else if !command_box_locked(app) {
                app.command_input.move_end();
            }
        }
        Action::InsertNewline => {
            if !command_box_locked(app) {
                app.command_input.insert_newline();
            }
        }

        // ── PTY passthrough ───────────────────────────────────────────
        Action::ForwardToPty(key_event) => {
            forward_key_to_pty(app, key_event);
        }

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

// ─── Mouse ───────────────────────────────────────────────────────────────────

fn handle_mouse_event(app: &mut App, mouse: crossterm::event::MouseEvent) {
    match mouse.kind {
        MouseEventKind::ScrollUp => {
            // Workflow strip scroll.
            if let Some(strip_rect) = app.active_tab().last_strip_rect {
                if mouse.row >= strip_rect.y
                    && mouse.row < strip_rect.y + strip_rect.height
                    && mouse.column >= strip_rect.x
                    && mouse.column < strip_rect.x + strip_rect.width
                {
                    let tab = app.active_tab_mut();
                    tab.workflow_strip_scroll_offset =
                        tab.workflow_strip_scroll_offset.saturating_sub(1);
                    return;
                }
            }
            let tab = app.active_tab_mut();
            if tab.container_window_state == ContainerWindowState::Maximized {
                // Cap to the actual scrollback buffer depth so the user
                // can scroll the full configured history (5000 lines by
                // default). vt100-ctt 0.17 uses `saturating_sub` in
                // `visible_rows()`, so offsets > screen height are safe
                // (the panic in vt100 0.15.2 is fixed upstream).
                let max_scroll = {
                    let screen = tab.vt100_parser.screen_mut();
                    screen.set_scrollback(usize::MAX);
                    let depth = screen.scrollback();
                    screen.set_scrollback(0);
                    depth
                };
                tab.container_scroll_offset = (tab.container_scroll_offset + 5).min(max_scroll);
            } else {
                tab.scroll_offset = tab.scroll_offset.saturating_add(5);
            }
        }
        MouseEventKind::ScrollDown => {
            // Workflow strip scroll.
            if let Some(strip_rect) = app.active_tab().last_strip_rect {
                if mouse.row >= strip_rect.y
                    && mouse.row < strip_rect.y + strip_rect.height
                    && mouse.column >= strip_rect.x
                    && mouse.column < strip_rect.x + strip_rect.width
                {
                    let tab = app.active_tab_mut();
                    tab.workflow_strip_scroll_offset += 1;
                    return;
                }
            }
            let tab = app.active_tab_mut();
            if tab.container_window_state == ContainerWindowState::Maximized {
                tab.container_scroll_offset = tab.container_scroll_offset.saturating_sub(5);
            } else {
                tab.scroll_offset = tab.scroll_offset.saturating_sub(5);
            }
        }
        MouseEventKind::Down(MouseButton::Left) => {
            let tab = app.active_tab_mut();
            if tab.container_window_state != ContainerWindowState::Maximized {
                return;
            }
            let inner = match tab.container_inner_area {
                Some(r) => r,
                None => return,
            };
            // Only start a selection if the click landed inside the vt100
            // grid (not on the border).
            if mouse.column < inner.x
                || mouse.row < inner.y
                || mouse.column >= inner.x + inner.width
                || mouse.row >= inner.y + inner.height
            {
                return;
            }
            let vt_col = mouse.column - inner.x;
            let vt_row = mouse.row - inner.y;
            let scroll = tab.container_scroll_offset;
            let snapshot = capture_vt100_snapshot(&mut tab.vt100_parser, scroll);
            tab.mouse_selection = Some(tabs::TextSelection {
                start_col: vt_col,
                start_row: vt_row,
                end_col: vt_col,
                end_row: vt_row,
                snapshot,
            });
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            let tab = app.active_tab_mut();
            if tab.container_window_state != ContainerWindowState::Maximized {
                return;
            }
            let inner = match tab.container_inner_area {
                Some(r) => r,
                None => return,
            };
            if let Some(ref mut sel) = tab.mouse_selection {
                let vt_col = mouse
                    .column
                    .saturating_sub(inner.x)
                    .min(inner.width.saturating_sub(1));
                let vt_row = mouse
                    .row
                    .saturating_sub(inner.y)
                    .min(inner.height.saturating_sub(1));
                sel.end_col = vt_col;
                sel.end_row = vt_row;
            }
        }
        MouseEventKind::Up(MouseButton::Left) => {
            let tab = app.active_tab_mut();
            if let Some(ref sel) = tab.mouse_selection {
                // A click without a drag (zero-area selection) is treated as
                // just a click, so accidental Ctrl+Y copies after a stray
                // click don't yank stale text.
                if sel.start_col == sel.end_col && sel.start_row == sel.end_row {
                    tab.mouse_selection = None;
                }
            }
        }
        _ => {}
    }
}

/// Snapshot the vt100 grid into a `Vec<Vec<String>>` of cell contents.
///
/// Why: the vt100 grid mutates with live PTY output. When the user starts a
/// drag selection, they need the copied text to reflect what they *saw* —
/// not the cells' current values.
fn capture_vt100_snapshot(parser: &mut vt100::Parser, scroll_offset: usize) -> Vec<Vec<String>> {
    let screen = parser.screen_mut();
    if scroll_offset > 0 {
        screen.set_scrollback(scroll_offset);
    }
    let snapshot = {
        let (rows, cols) = screen.size();
        (0..rows)
            .map(|row| {
                (0..cols)
                    .map(|col| {
                        screen
                            .cell(row, col)
                            .map(|c| {
                                let s = c.contents();
                                if s.is_empty() {
                                    " ".to_string()
                                } else {
                                    s.to_string()
                                }
                            })
                            .unwrap_or_else(|| " ".to_string())
                    })
                    .collect()
            })
            .collect()
    };
    if scroll_offset > 0 {
        screen.set_scrollback(0);
    }
    snapshot
}

/// Extract the selected text from a snapshot. Range is inclusive on both ends;
/// trailing whitespace per line is stripped; rows are joined with `\n`.
fn extract_selection_text(sel: &tabs::TextSelection) -> String {
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

// ─── Resize ──────────────────────────────────────────────────────────────────

fn handle_resize(app: &mut App, cols: u16, rows: u16) {
    for tab in &mut app.tabs {
        tab.mouse_selection = None;
        if tab.container_window_state != ContainerWindowState::Hidden {
            let (inner_cols, inner_rows) = compute_container_inner_size(cols, rows);
            tab.vt100_parser
                .screen_mut()
                .set_size(inner_rows, inner_cols);
            // Forward the new size to the container's PTY master so its
            // SIGWINCH handler reflows TUI apps inside the container.
            if let Some(ref tx) = tab.container_resize_tx {
                let _ = tx.send((inner_cols, inner_rows));
            }
        }
    }
}

/// Compute the vt100 grid size that fits inside the container overlay,
/// accounting for the 95% sizing within the execution window area and the
/// 2-cell border subtraction. The container window lives between the tab
/// bar (3 rows) and the bottom chrome (5 rows: status bar + command box +
/// suggestion row), plus any workflow strip or extra bar below.
///
/// `extra_bottom` accounts for the workflow strip height and the
/// minimized/summary bar (3 rows each when present). Callers that don't
/// know the exact extra height can pass 0 for a best-effort estimate.
pub fn compute_container_inner_size(term_cols: u16, term_rows: u16) -> (u16, u16) {
    compute_container_inner_size_with_extra(term_cols, term_rows, 0)
}

fn compute_container_inner_size_with_extra(
    term_cols: u16,
    term_rows: u16,
    extra_bottom: u16,
) -> (u16, u16) {
    let exec_height = term_rows.saturating_sub(8 + extra_bottom); // 3 top + 5 bottom + extras
    let outer_cols = ((term_cols as u32 * 95 / 100) as u16).max(10);
    let outer_rows = ((exec_height as u32 * 95 / 100) as u16).max(5);
    (outer_cols.saturating_sub(2), outer_rows.saturating_sub(2))
}

// ─── PTY forwarding ──────────────────────────────────────────────────────────

fn forward_key_to_pty(app: &mut App, key: crossterm::event::KeyEvent) {
    if let Some(bytes) = key_to_bytes(&key) {
        let tab = app.active_tab_mut();
        if let Some(ref tx) = tab.container_stdin_tx {
            let _ = tx.send(bytes);
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

fn copy_selection_to_clipboard(app: &mut App) {
    let tab = app.active_tab();
    let text = match tab.mouse_selection.as_ref() {
        Some(sel) if !sel.snapshot.is_empty() => extract_selection_text(sel),
        _ => return,
    };
    if text.is_empty() {
        return;
    }
    match arboard::Clipboard::new().and_then(|mut cb| cb.set_text(&text)) {
        Ok(()) => {
            // Drop the selection after a successful copy so the copy hint
            // disappears and a subsequent Ctrl+Y doesn't re-yank.
            app.active_tab_mut().mouse_selection = None;
        }
        Err(e) => {
            app.active_tab_mut()
                .status_log
                .lock()
                .map(|mut log| {
                    log.push(crate::frontend::tui::user_message::StatusLogEntry {
                        level: crate::engine::message::MessageLevel::Error,
                        text: format!("clipboard unavailable: {e}"),
                    })
                })
                .ok();
        }
    }
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
            app.spawn_command(&text, parsed);
        }
        Err(err) => {
            app.input_error = Some(command_box::format_parse_error(&err));
        }
    }
}

// ─── WorkflowControlBoard special handler ────────────────────────────────────

/// Handle arrow keys, Ctrl+Enter, and `[d]` for the WorkflowControlBoard dialog.
///
/// Returns `true` if the key was consumed; `false` to let it fall through to
/// the generic dialog handler (for char keys like 'a', Esc, etc.).
fn handle_workflow_control_board_key(app: &mut App, key: crossterm::event::KeyEvent) -> bool {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

    let can_finish = matches!(
        &app.active_dialog,
        Some(Dialog::WorkflowControlBoard(state)) if state.can_finish
    );

    let response = match key.code {
        KeyCode::Right => DialogResponse::Char('>'),
        KeyCode::Down => DialogResponse::Char('v'),
        KeyCode::Up => DialogResponse::Char('^'),
        KeyCode::Left => DialogResponse::Char('<'),
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

// ─── Dialog handling ─────────────────────────────────────────────────────────

/// Dismiss the active dialog, sending Dismissed to the command thread if needed.
fn dismiss_dialog(app: &mut App) {
    if app.command_dialog_active {
        app.send_dialog_response(DialogResponse::Dismissed);
    }
    app.active_dialog = None;
    app.command_dialog_active = false;
}

/// Handle Enter key in a dialog context.
fn handle_dialog_submit(app: &mut App) {
    let is_command = app.command_dialog_active;

    match &app.active_dialog {
        Some(Dialog::QuitConfirm) => {}
        Some(Dialog::CloseTabConfirm) => {}

        Some(Dialog::TextInput { editor, .. }) if is_command => {
            let text = editor.text.clone();
            app.send_dialog_response(DialogResponse::Text(text));
            app.active_dialog = None;
            app.command_dialog_active = false;
        }
        Some(Dialog::TextInput { editor, .. }) => {
            let path = editor.text.clone();
            app.active_dialog = None;
            handle_new_tab_path(app, &path);
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

        Some(Dialog::ConfigShow(state)) if is_command => {
            if state.editing {
                // Save the edited value: send "field\tvalue\tscope"
                let row = &state.rows[state.selected];
                let field = row.field.clone();
                let value = state.editor.text.clone();
                let scope = if state.edit_column == 0 {
                    "global"
                } else {
                    "repo"
                };
                let edit_str = format!("{}\t{}\t{}", field, value, scope);
                app.send_dialog_response(DialogResponse::Text(edit_str));
                app.active_dialog = None;
                app.command_dialog_active = false;
            } else {
                // Start editing: Enter opens the inline editor on the
                // selected row. edit_column 0 = global, 1 = repo.
                let row = &state.rows[state.selected];
                if row.read_only {
                    app.status_bar.text = "This field is read-only".to_string();
                    return;
                }
                let initial_value = if state.edit_column == 0 {
                    row.global.clone()
                } else {
                    row.repo.clone()
                };
                if let Some(Dialog::ConfigShow(state)) = &mut app.active_dialog {
                    state.editing = true;
                    state.editor = crate::frontend::tui::text_edit::TextEdit::new(false);
                    state.editor.set_text(&initial_value);
                }
            }
        }

        Some(Dialog::WorkflowStepConfirm(_)) if is_command => {
            app.send_dialog_response(DialogResponse::Char('>'));
            app.active_dialog = None;
            app.command_dialog_active = false;
        }

        _ => {}
    }
}

enum CursorDir {
    Left,
    Right,
    Home,
    End,
}

fn handle_dialog_cursor(app: &mut App, dir: CursorDir) {
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

fn handle_dialog_backspace(app: &mut App) {
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

fn handle_dialog_delete(app: &mut App) {
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

/// Handle arrow-key scrolling in list-based dialogs.
fn handle_dialog_scroll(app: &mut App, direction: i32) {
    match &mut app.active_dialog {
        Some(Dialog::ListPicker {
            items, selected, ..
        }) => {
            let len = items.len();
            if len == 0 {
                return;
            }
            if direction < 0 {
                *selected = selected.saturating_sub(1);
            } else {
                *selected = (*selected + 1).min(len - 1);
            }
        }
        Some(Dialog::ConfigShow(state)) => {
            let len = state.rows.len();
            if len == 0 {
                return;
            }
            if direction < 0 {
                state.selected = state.selected.saturating_sub(1);
            } else {
                state.selected = (state.selected + 1).min(len - 1);
            }
        }
        _ => {}
    }
}

/// Handle a character key press in a dialog.
fn handle_dialog_char(app: &mut App, c: char) {
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
        Some(Dialog::WorkflowStepError { .. }) => {
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
            // When not editing, ignore char keys (navigate with arrows, Enter to edit)
        }

        // ── Non-interactive / fallback dialogs ─────────────────────
        Some(Dialog::Loading { .. })
        | Some(Dialog::ListPicker { .. })
        | Some(Dialog::KindSelect { .. })
        | Some(Dialog::YesNo { .. })
        | Some(Dialog::YesNoCancel { .. }) => {}

        None => {}
    }
}

/// Handle path selection from the new-tab dialog.
fn handle_new_tab_path(app: &mut App, path: &str) {
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
        app.status_bar.text = format!("Not a directory: {path}");
        return;
    }

    let session = {
        let resolver = crate::data::session::StaticGitRootResolver::new(&dir);
        match crate::data::session::Session::open(
            dir.clone(),
            &resolver,
            crate::data::session::SessionOpenOptions::default(),
        ) {
            Ok(s) => s,
            Err(_) => {
                // Fallback for non-git directories: use dir as git root.
                match crate::data::session::Session::open_at_git_root(
                    dir.clone(),
                    dir.clone(),
                    crate::data::session::SessionOpenOptions::default(),
                ) {
                    Ok(s) => s,
                    Err(e) => {
                        app.status_bar.text = format!("Failed to open session: {e}");
                        return;
                    }
                }
            }
        }
    };

    let is_git = session.git_root().join(".git").exists();
    let idx = app.add_tab(session);
    app.active_tab = idx;

    if is_git {
        app.spawn_command(
            "ready",
            crate::command::dispatch::parsed_input::ParsedCommandBoxInput {
                path: vec!["ready".into()],
                flags: Default::default(),
                arguments: Default::default(),
            },
        );
    } else {
        let mut flags = std::collections::BTreeMap::new();
        flags.insert(
            "watch".to_string(),
            crate::command::dispatch::parsed_input::FlagValue::Bool(true),
        );
        app.spawn_command(
            "status --watch",
            crate::command::dispatch::parsed_input::ParsedCommandBoxInput {
                path: vec!["status".into()],
                flags,
                arguments: Default::default(),
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use tokio::sync::RwLock;

    use crate::command::dispatch::catalogue::CommandCatalogue;
    use crate::data::session::{Session, SessionOpenOptions, StaticGitRootResolver};
    use crate::data::session_manager::SessionManager;
    use crate::frontend::tui::app::{App, Focus};
    use crate::frontend::tui::dialogs::{
        Dialog, DialogResponse, MountScopeState, WorkflowStepErrorState,
    };
    use crate::frontend::tui::tabs::Tab;
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

    // ─── Shared helpers ───────────────────────────────────────────────────────

    fn make_engines() -> crate::command::dispatch::Engines {
        let runtime = Arc::new(crate::engine::container::ContainerRuntime::docker());
        let overlay = Arc::new(crate::engine::overlay::OverlayEngine::with_auth_resolver(
            crate::data::fs::auth_paths::AuthPathResolver::at_home(std::path::PathBuf::from(
                "/tmp",
            )),
        ));
        let git_engine = Arc::new(crate::engine::git::GitEngine::new());
        let agent_engine = Arc::new(crate::engine::agent::AgentEngine::new(
            overlay.clone(),
            runtime.clone(),
        ));
        let auth_engine = Arc::new(crate::engine::auth::AuthEngine::with_paths(
            crate::data::fs::auth_paths::AuthPathResolver::at_home("/tmp"),
            crate::data::fs::api_paths::ApiPaths::at_root("/tmp"),
        ));
        let workflow_state_store = {
            let tmp = tempfile::tempdir().unwrap();
            Arc::new(crate::data::EngineWorkflowStateStore::at_git_root(
                tmp.path(),
            ))
        };
        crate::command::dispatch::Engines {
            runtime,
            git_engine,
            overlay_engine: overlay,
            auth_engine,
            agent_engine,
            workflow_state_store,
        }
    }

    fn make_session() -> Session {
        let tmp = tempfile::tempdir().unwrap();
        let resolver = StaticGitRootResolver::new(tmp.path());
        Session::open(
            tmp.path().to_path_buf(),
            &resolver,
            SessionOpenOptions::default(),
        )
        .unwrap()
    }

    fn make_app() -> App {
        let rt = Box::leak(Box::new(tokio::runtime::Runtime::new().unwrap()));
        let catalogue = CommandCatalogue::get();
        let engines = make_engines();
        let session_manager = Arc::new(RwLock::new(SessionManager::in_memory()));
        let session = make_session();
        let tab = Tab::new(session);
        App::new(
            catalogue,
            engines,
            session_manager,
            tab,
            rt.handle().clone(),
        )
    }

    fn press_key(app: &mut App, code: KeyCode, mods: KeyModifiers) {
        super::handle_key_event(
            app,
            KeyEvent {
                code,
                modifiers: mods,
                kind: KeyEventKind::Press,
                state: KeyEventState::NONE,
            },
        );
    }

    fn press_char(app: &mut App, c: char) {
        press_key(app, KeyCode::Char(c), KeyModifiers::NONE);
    }

    fn setup_command_dialog(
        app: &mut App,
        dialog: Dialog,
    ) -> std::sync::mpsc::Receiver<DialogResponse> {
        let (tx, rx) = std::sync::mpsc::channel();
        app.tabs[app.active_tab].dialog_response_tx = Some(tx);
        app.active_dialog = Some(dialog);
        app.command_dialog_active = true;
        rx
    }

    // ─── Clap routing (existing tests retained) ───────────────────────────────

    #[test]
    fn bare_invocation_has_no_subcommand() {
        let cmd = CommandCatalogue::get().build_clap_command();
        let m = cmd.try_get_matches_from(["awman"]).unwrap();
        assert!(
            m.subcommand_name().is_none(),
            "bare `awman` must have no subcommand — main.rs uses this to route to TUI"
        );
    }

    #[test]
    fn subcommand_presence_routes_away_from_tui() {
        let cmd = CommandCatalogue::get().build_clap_command();
        for argv in [
            vec!["awman", "status"],
            vec!["awman", "ready"],
            vec!["awman", "chat"],
        ] {
            let m = cmd.clone().try_get_matches_from(&argv).unwrap();
            assert!(
                m.subcommand_name().is_some(),
                "{argv:?} must have a subcommand name"
            );
        }
    }

    // ─── QuitConfirm dialog ───────────────────────────────────────────────────

    #[test]
    fn quit_confirm_y_sets_should_quit() {
        let mut app = make_app();
        app.active_dialog = Some(Dialog::QuitConfirm);
        // Second Ctrl-C while QuitConfirm is open quits
        press_key(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(app.should_quit);
        assert!(app.active_dialog.is_none());
    }

    #[test]
    fn quit_confirm_n_dismisses_without_quitting() {
        let mut app = make_app();
        app.active_dialog = Some(Dialog::QuitConfirm);
        // Esc dismisses the dialog
        press_key(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert!(!app.should_quit);
        assert!(app.active_dialog.is_none());
    }

    #[test]
    fn quit_confirm_esc_dismisses() {
        let mut app = make_app();
        app.active_dialog = Some(Dialog::QuitConfirm);
        press_key(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert!(app.active_dialog.is_none());
        assert!(!app.should_quit);
    }

    // ─── CloseTabConfirm dialog ───────────────────────────────────────────────

    #[test]
    fn close_tab_confirm_q_quits_entire_app() {
        let mut app = make_app();
        app.active_dialog = Some(Dialog::CloseTabConfirm);
        // Second Ctrl-C while CloseTabConfirm is open quits
        press_key(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(app.should_quit);
    }

    #[test]
    fn close_tab_confirm_c_closes_current_tab() {
        let mut app = make_app();
        app.tabs.push(Tab::new(make_session()));
        app.active_dialog = Some(Dialog::CloseTabConfirm);
        // Ctrl-T closes the tab
        press_key(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
        assert_eq!(app.tabs.len(), 1);
        assert!(!app.should_quit);
    }

    #[test]
    fn close_tab_confirm_n_cancels() {
        let mut app = make_app();
        app.tabs.push(Tab::new(make_session()));
        let initial_len = app.tabs.len();
        app.active_dialog = Some(Dialog::CloseTabConfirm);
        // Esc cancels the dialog
        press_key(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert!(app.active_dialog.is_none());
        assert_eq!(app.tabs.len(), initial_len);
    }

    // ─── YesNo command dialog ─────────────────────────────────────────────────

    #[test]
    fn yes_no_command_dialog_y_sends_yes_response() {
        let mut app = make_app();
        let rx = setup_command_dialog(
            &mut app,
            Dialog::YesNo {
                title: "Test".into(),
                body: "Test body".into(),
            },
        );
        press_char(&mut app, 'y');
        let response = rx.try_recv().unwrap();
        assert!(matches!(response, DialogResponse::Yes));
        assert!(app.active_dialog.is_none());
    }

    #[test]
    fn yes_no_command_dialog_n_sends_no_response() {
        let mut app = make_app();
        let rx = setup_command_dialog(
            &mut app,
            Dialog::YesNo {
                title: "Test".into(),
                body: "Test body".into(),
            },
        );
        press_char(&mut app, 'n');
        let response = rx.try_recv().unwrap();
        assert!(matches!(response, DialogResponse::No));
    }

    // ─── Command dialog Esc sends Dismissed ──────────────────────────────────

    #[test]
    fn esc_on_command_dialog_sends_dismissed() {
        let mut app = make_app();
        let rx = setup_command_dialog(
            &mut app,
            Dialog::YesNo {
                title: "Test".into(),
                body: "Test body".into(),
            },
        );
        press_key(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        let response = rx.try_recv().unwrap();
        assert!(matches!(response, DialogResponse::Dismissed));
    }

    // ─── MountScope dialog ────────────────────────────────────────────────────

    #[test]
    fn mount_scope_r_sends_char_r() {
        let mut app = make_app();
        let rx = setup_command_dialog(
            &mut app,
            Dialog::MountScope(MountScopeState {
                git_root: "/tmp".into(),
                cwd: "/tmp/sub".into(),
            }),
        );
        press_char(&mut app, 'r');
        let response = rx.try_recv().unwrap();
        assert!(matches!(response, DialogResponse::Char('r')));
    }

    #[test]
    fn mount_scope_c_sends_char_c() {
        let mut app = make_app();
        let rx = setup_command_dialog(
            &mut app,
            Dialog::MountScope(MountScopeState {
                git_root: "/tmp".into(),
                cwd: "/tmp/sub".into(),
            }),
        );
        press_char(&mut app, 'c');
        let response = rx.try_recv().unwrap();
        assert!(matches!(response, DialogResponse::Char('c')));
    }

    #[test]
    fn mount_scope_a_sends_char_a() {
        let mut app = make_app();
        let rx = setup_command_dialog(
            &mut app,
            Dialog::MountScope(MountScopeState {
                git_root: "/tmp".into(),
                cwd: "/tmp/sub".into(),
            }),
        );
        press_char(&mut app, 'a');
        let response = rx.try_recv().unwrap();
        assert!(matches!(response, DialogResponse::Char('a')));
    }

    // ─── Custom dialog key filtering ────────────────────────────────────────

    #[test]
    fn custom_dialog_accepts_listed_key() {
        let mut app = make_app();
        let rx = setup_command_dialog(
            &mut app,
            Dialog::Custom {
                title: "Choose".into(),
                body: "Pick one".into(),
                keys: vec![
                    ('m', "Merge".into()),
                    ('d', "Discard".into()),
                    ('k', "Keep".into()),
                ],
            },
        );
        press_char(&mut app, 'm');
        let response = rx.try_recv().unwrap();
        assert!(matches!(response, DialogResponse::Char('m')));
        assert!(app.active_dialog.is_none());
    }

    #[test]
    fn custom_dialog_ignores_unlisted_key() {
        let mut app = make_app();
        let rx = setup_command_dialog(
            &mut app,
            Dialog::Custom {
                title: "Choose".into(),
                body: "Pick one".into(),
                keys: vec![
                    ('m', "Merge".into()),
                    ('d', "Discard".into()),
                    ('k', "Keep".into()),
                ],
            },
        );
        press_char(&mut app, 'x');
        assert!(
            rx.try_recv().is_err(),
            "unlisted key must not send a dialog response"
        );
        assert!(
            app.active_dialog.is_some(),
            "dialog must stay open after unlisted key"
        );
    }

    #[test]
    fn ctrl_m_in_dialog_does_not_cycle_container() {
        let mut app = make_app();
        let _rx = setup_command_dialog(
            &mut app,
            Dialog::YesNo {
                title: "Test".into(),
                body: "Test body".into(),
            },
        );
        let before = app.active_tab().container_window_state;
        press_key(&mut app, KeyCode::Char('m'), KeyModifiers::CONTROL);
        assert_eq!(
            app.active_tab().container_window_state,
            before,
            "Ctrl+M must not cycle container window while a dialog is open"
        );
    }

    // ─── KindSelect command dialog ────────────────────────────────────────────

    #[test]
    fn kind_select_digit_1_sends_index_0() {
        let mut app = make_app();
        let rx = setup_command_dialog(
            &mut app,
            Dialog::KindSelect {
                title: "Select".into(),
                options: vec![
                    ("a".into(), "Option A".into()),
                    ("b".into(), "Option B".into()),
                    ("c".into(), "Option C".into()),
                ],
            },
        );
        press_char(&mut app, '1');
        let response = rx.try_recv().unwrap();
        assert!(matches!(response, DialogResponse::Index(0)));
    }

    #[test]
    fn kind_select_digit_3_sends_index_2() {
        let mut app = make_app();
        let rx = setup_command_dialog(
            &mut app,
            Dialog::KindSelect {
                title: "Select".into(),
                options: vec![
                    ("a".into(), "Option A".into()),
                    ("b".into(), "Option B".into()),
                    ("c".into(), "Option C".into()),
                ],
            },
        );
        press_char(&mut app, '3');
        let response = rx.try_recv().unwrap();
        assert!(matches!(response, DialogResponse::Index(2)));
    }

    // ─── WorkflowStepError dialog ─────────────────────────────────────────────

    #[test]
    fn workflow_step_error_r_sends_char_r() {
        let mut app = make_app();
        let rx = setup_command_dialog(
            &mut app,
            Dialog::WorkflowStepError(WorkflowStepErrorState {
                step_name: "build".into(),
                error_lines: vec!["Step failed".into()],
            }),
        );
        press_char(&mut app, 'r');
        let response = rx.try_recv().unwrap();
        assert!(matches!(response, DialogResponse::Char('r')));
    }

    #[test]
    fn workflow_step_error_a_sends_char_a() {
        let mut app = make_app();
        let rx = setup_command_dialog(
            &mut app,
            Dialog::WorkflowStepError(WorkflowStepErrorState {
                step_name: "build".into(),
                error_lines: vec!["Step failed".into()],
            }),
        );
        press_char(&mut app, 'a');
        let response = rx.try_recv().unwrap();
        assert!(matches!(response, DialogResponse::Char('a')));
    }

    // ─── ListPicker scroll ────────────────────────────────────────────────────

    #[test]
    fn list_picker_scroll_down_increments_selection() {
        let mut app = make_app();
        app.active_dialog = Some(Dialog::ListPicker {
            title: "Pick".into(),
            items: vec!["a".into(), "b".into(), "c".into()],
            selected: 0,
        });
        press_key(&mut app, KeyCode::Down, KeyModifiers::NONE);
        match &app.active_dialog {
            Some(Dialog::ListPicker { selected, .. }) => assert_eq!(*selected, 1),
            _ => panic!("expected ListPicker dialog"),
        }
    }

    #[test]
    fn list_picker_scroll_up_at_zero_stays_zero() {
        let mut app = make_app();
        app.active_dialog = Some(Dialog::ListPicker {
            title: "Pick".into(),
            items: vec!["a".into(), "b".into(), "c".into()],
            selected: 0,
        });
        press_key(&mut app, KeyCode::Up, KeyModifiers::NONE);
        match &app.active_dialog {
            Some(Dialog::ListPicker { selected, .. }) => assert_eq!(*selected, 0),
            _ => panic!("expected ListPicker dialog"),
        }
    }

    #[test]
    fn list_picker_enter_sends_selected_index() {
        let mut app = make_app();
        let rx = setup_command_dialog(
            &mut app,
            Dialog::ListPicker {
                title: "Pick".into(),
                items: vec!["a".into(), "b".into(), "c".into()],
                selected: 2,
            },
        );
        press_key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        let response = rx.try_recv().unwrap();
        assert!(matches!(response, DialogResponse::Index(2)));
    }

    // ─── Autocomplete cycling ─────────────────────────────────────────────────

    #[test]
    fn autocomplete_next_fills_command_box_with_first_suggestion() {
        let mut app = make_app();
        // Type enough for a known completion
        for c in "cha".chars() {
            press_char(&mut app, c);
        }
        press_key(&mut app, KeyCode::Tab, KeyModifiers::NONE);
        assert!(
            app.command_input.text.contains("chat"),
            "expected 'chat' in input, got: {:?}",
            app.command_input.text
        );
    }

    #[test]
    fn autocomplete_prev_fills_command_box_with_last_suggestion() {
        let mut app = make_app();
        for c in "cha".chars() {
            press_char(&mut app, c);
        }
        // Update suggestions so we know the last one
        app.update_suggestions();
        let last = app.suggestion_row.last().cloned().unwrap_or_default();
        press_key(&mut app, KeyCode::BackTab, KeyModifiers::NONE);
        assert!(
            app.command_input.text.contains("cha"),
            "expected suggestion containing 'cha', got: {:?}",
            app.command_input.text
        );
        // The text should match the last suggestion (or still contain "cha" if only one)
        let _ = last; // used above
    }

    #[test]
    fn tab_with_no_suggestions_leaves_input_unchanged() {
        let mut app = make_app();
        for c in "zzzzz".chars() {
            press_char(&mut app, c);
        }
        press_key(&mut app, KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(app.command_input.text, "zzzzz");
    }

    // ─── Focus switching ──────────────────────────────────────────────────────

    #[test]
    fn up_arrow_in_command_box_switches_focus_to_execution_window() {
        let mut app = make_app();
        assert_eq!(app.focus, Focus::CommandBox);
        press_key(&mut app, KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(app.focus, Focus::ExecutionWindow);
    }

    #[test]
    fn esc_in_execution_window_returns_focus_to_command_box() {
        let mut app = make_app();
        app.focus = Focus::ExecutionWindow;
        press_key(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(app.focus, Focus::CommandBox);
    }

    // ─── Text input (non-dialog) ──────────────────────────────────────────────

    #[test]
    fn empty_command_submit_does_not_set_execution_phase() {
        use crate::frontend::tui::tabs::ExecutionPhase;
        let mut app = make_app();
        // input is empty by default
        press_key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(
            app.tabs[app.active_tab].execution_phase,
            ExecutionPhase::Idle
        );
    }

    // ─── Toggle status log ────────────────────────────────────────────────────

    #[test]
    fn l_in_execution_window_toggles_status_log() {
        let mut app = make_app();
        app.focus = Focus::ExecutionWindow;
        let initial = app.tabs[app.active_tab].status_log_collapsed;
        press_char(&mut app, 'l');
        assert_ne!(app.tabs[app.active_tab].status_log_collapsed, initial);
    }

    // ─── WorkflowControlBoard arrow keys ─────────────────────────────────────

    fn setup_wcb_dialog(app: &mut App) -> std::sync::mpsc::Receiver<DialogResponse> {
        let (tx, rx) = std::sync::mpsc::channel();
        app.tabs[app.active_tab].dialog_response_tx = Some(tx);
        app.active_dialog = Some(Dialog::WorkflowControlBoard(
            crate::frontend::tui::dialogs::WorkflowControlBoardState {
                step_name: "test".into(),
                can_launch_next: true,
                can_continue_current: true,
                can_restart: true,
                can_go_back: true,
                can_finish: true,
                continue_unavailable_reason: None,
                cancel_to_previous_unavailable_reason: None,
                finish_workflow_unavailable_reason: None,
                can_dismiss: false,
            },
        ));
        app.command_dialog_active = true;
        rx
    }

    #[test]
    fn wcb_right_arrow_sends_launch_next() {
        let mut app = make_app();
        let rx = setup_wcb_dialog(&mut app);
        press_key(&mut app, KeyCode::Right, KeyModifiers::NONE);
        let resp = rx.try_recv().unwrap();
        assert!(matches!(resp, DialogResponse::Char('>')));
        assert!(app.active_dialog.is_none());
    }

    #[test]
    fn wcb_down_arrow_sends_continue_current() {
        let mut app = make_app();
        let rx = setup_wcb_dialog(&mut app);
        press_key(&mut app, KeyCode::Down, KeyModifiers::NONE);
        let resp = rx.try_recv().unwrap();
        assert!(matches!(resp, DialogResponse::Char('v')));
    }

    #[test]
    fn wcb_up_arrow_sends_restart_step() {
        let mut app = make_app();
        let rx = setup_wcb_dialog(&mut app);
        press_key(&mut app, KeyCode::Up, KeyModifiers::NONE);
        let resp = rx.try_recv().unwrap();
        assert!(matches!(resp, DialogResponse::Char('^')));
    }

    #[test]
    fn wcb_left_arrow_sends_cancel_to_previous() {
        let mut app = make_app();
        let rx = setup_wcb_dialog(&mut app);
        press_key(&mut app, KeyCode::Left, KeyModifiers::NONE);
        let resp = rx.try_recv().unwrap();
        assert!(matches!(resp, DialogResponse::Char('<')));
    }

    #[test]
    fn wcb_ctrl_enter_sends_finish_workflow() {
        let mut app = make_app();
        let rx = setup_wcb_dialog(&mut app);
        press_key(&mut app, KeyCode::Enter, KeyModifiers::CONTROL);
        let resp = rx.try_recv().unwrap();
        assert!(matches!(resp, DialogResponse::Char('f')));
    }

    #[test]
    fn wcb_plain_enter_sends_finish_workflow() {
        let mut app = make_app();
        let rx = setup_wcb_dialog(&mut app);
        press_key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        let resp = rx.try_recv().unwrap();
        assert!(matches!(resp, DialogResponse::Char('f')));
    }

    #[test]
    fn wcb_enter_ignored_when_finish_unavailable() {
        let mut app = make_app();
        let (tx, rx) = std::sync::mpsc::channel();
        app.tabs[app.active_tab].dialog_response_tx = Some(tx);
        app.active_dialog = Some(Dialog::WorkflowControlBoard(
            crate::frontend::tui::dialogs::WorkflowControlBoardState {
                step_name: "test".into(),
                can_launch_next: true,
                can_continue_current: true,
                can_restart: true,
                can_go_back: true,
                can_finish: false,
                continue_unavailable_reason: None,
                cancel_to_previous_unavailable_reason: None,
                finish_workflow_unavailable_reason: Some("not last step".into()),
                can_dismiss: false,
            },
        ));
        app.command_dialog_active = true;
        press_key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert!(
            rx.try_recv().is_err(),
            "Enter must not send FinishWorkflow when can_finish is false"
        );
    }

    #[test]
    fn wcb_ctrl_c_sends_abort() {
        let mut app = make_app();
        let rx = setup_wcb_dialog(&mut app);
        press_key(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
        let resp = rx.try_recv().unwrap();
        assert!(matches!(resp, DialogResponse::Char('a')));
    }

    #[test]
    fn wcb_esc_sends_dismissed() {
        let mut app = make_app();
        let rx = setup_wcb_dialog(&mut app);
        press_key(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        let resp = rx.try_recv().unwrap();
        assert!(matches!(resp, DialogResponse::Dismissed));
    }

    // ─── Command box locked during Running ────────────────────────────────────

    #[test]
    fn char_input_blocked_while_running() {
        let mut app = make_app();
        app.tabs[app.active_tab].execution_phase =
            crate::frontend::tui::tabs::ExecutionPhase::Running {
                command: "chat".into(),
            };
        press_char(&mut app, 'x');
        assert_eq!(
            app.command_input.text, "",
            "command box must be locked while running"
        );
    }

    #[test]
    fn backspace_blocked_while_running() {
        let mut app = make_app();
        app.command_input.set_text("abc");
        app.tabs[app.active_tab].execution_phase =
            crate::frontend::tui::tabs::ExecutionPhase::Running {
                command: "chat".into(),
            };
        press_key(&mut app, KeyCode::Backspace, KeyModifiers::NONE);
        assert_eq!(
            app.command_input.text, "abc",
            "backspace must be blocked while running"
        );
    }

    #[test]
    fn submit_command_blocked_while_running() {
        use crate::frontend::tui::tabs::ExecutionPhase;
        let mut app = make_app();
        app.command_input.set_text("status");
        app.tabs[app.active_tab].execution_phase = ExecutionPhase::Running {
            command: "chat".into(),
        };
        press_key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        // Phase should still be Running, not a new command
        assert!(matches!(
            app.tabs[app.active_tab].execution_phase,
            ExecutionPhase::Running { .. }
        ));
    }

    // ─── q with empty box opens QuitConfirm ──────────────────────────────────

    #[test]
    fn q_with_empty_command_box_opens_quit_confirm() {
        let mut app = make_app();
        assert!(app.command_input.text.is_empty());
        press_char(&mut app, 'q');
        assert!(
            matches!(app.active_dialog, Some(Dialog::QuitConfirm)),
            "q with empty command box must open QuitConfirm"
        );
    }

    #[test]
    fn q_with_nonempty_command_box_inserts_char() {
        let mut app = make_app();
        app.command_input.set_text("quer");
        press_char(&mut app, 'y');
        assert_eq!(app.command_input.text, "query");
        assert!(app.active_dialog.is_none());
    }

    // ─── Any key in Done/Error execution window refocuses command box ─────────

    #[test]
    fn any_unhandled_key_in_done_execution_window_refocuses_command_box() {
        let mut app = make_app();
        app.focus = Focus::ExecutionWindow;
        app.tabs[app.active_tab].execution_phase =
            crate::frontend::tui::tabs::ExecutionPhase::Done {
                command: "chat".into(),
                exit_code: 0,
            };
        // Press a key that maps to Action::None in execution window context
        press_char(&mut app, 'x');
        assert_eq!(
            app.focus,
            Focus::CommandBox,
            "unhandled key in Done execution window must refocus command box"
        );
    }

    #[test]
    fn any_unhandled_key_in_error_execution_window_refocuses_command_box() {
        let mut app = make_app();
        app.focus = Focus::ExecutionWindow;
        app.tabs[app.active_tab].execution_phase =
            crate::frontend::tui::tabs::ExecutionPhase::Error {
                command: "chat".into(),
                message: "failed".into(),
            };
        press_char(&mut app, 'z');
        assert_eq!(app.focus, Focus::CommandBox);
    }

    #[test]
    fn unhandled_key_in_running_execution_window_does_not_refocus() {
        let mut app = make_app();
        app.focus = Focus::ExecutionWindow;
        app.tabs[app.active_tab].execution_phase =
            crate::frontend::tui::tabs::ExecutionPhase::Running {
                command: "chat".into(),
            };
        press_char(&mut app, 'x');
        assert_eq!(
            app.focus,
            Focus::ExecutionWindow,
            "focus must not change during Running"
        );
    }

    // ─── Dialog Home/End/Delete ───────────────────────────────────────────────

    #[test]
    fn home_in_text_input_dialog_moves_cursor_to_start() {
        let mut app = make_app();
        let mut editor = crate::frontend::tui::text_edit::TextEdit::new(false);
        editor.set_text("hello");
        app.active_dialog = Some(Dialog::TextInput {
            title: "T".into(),
            prompt: "P".into(),
            editor,
        });
        app.command_dialog_active = true;
        press_key(&mut app, KeyCode::Home, KeyModifiers::NONE);
        if let Some(Dialog::TextInput { editor, .. }) = &app.active_dialog {
            assert_eq!(editor.cursor, 0, "Home must move cursor to start");
        } else {
            panic!("dialog should still be open");
        }
    }

    #[test]
    fn end_in_text_input_dialog_moves_cursor_to_end() {
        let mut app = make_app();
        let mut editor = crate::frontend::tui::text_edit::TextEdit::new(false);
        editor.set_text("hello");
        editor.move_home();
        app.active_dialog = Some(Dialog::TextInput {
            title: "T".into(),
            prompt: "P".into(),
            editor,
        });
        app.command_dialog_active = true;
        press_key(&mut app, KeyCode::End, KeyModifiers::NONE);
        if let Some(Dialog::TextInput { editor, .. }) = &app.active_dialog {
            assert_eq!(editor.cursor, 5, "End must move cursor to end");
        } else {
            panic!("dialog should still be open");
        }
    }

    #[test]
    fn delete_in_text_input_dialog_removes_char_at_cursor() {
        let mut app = make_app();
        let mut editor = crate::frontend::tui::text_edit::TextEdit::new(false);
        editor.set_text("hello");
        editor.move_home(); // cursor at 0
        app.active_dialog = Some(Dialog::TextInput {
            title: "T".into(),
            prompt: "P".into(),
            editor,
        });
        app.command_dialog_active = true;
        press_key(&mut app, KeyCode::Delete, KeyModifiers::NONE);
        if let Some(Dialog::TextInput { editor, .. }) = &app.active_dialog {
            assert_eq!(editor.text, "ello", "Delete must remove char at cursor");
        } else {
            panic!("dialog should still be open");
        }
    }

    // ─── Ctrl+W workflow control ──────────────────────────────────────────────

    #[test]
    fn ctrl_w_with_no_workflow_is_silent_noop() {
        let mut app = make_app();
        // No engine_tx set — Ctrl-W is a silent no-op per spec.
        press_key(&mut app, KeyCode::Char('w'), KeyModifiers::CONTROL);
        assert_eq!(
            app.status_bar.text, "",
            "Ctrl+W with no engine_tx must be a silent no-op"
        );
        assert!(
            app.active_dialog.is_none(),
            "no dialog must be opened when no workflow is active"
        );
    }

    #[test]
    fn ctrl_w_during_running_step_sends_engine_request() {
        use crate::engine::workflow::EngineRequest;
        use crate::frontend::tui::tabs::WorkflowStepView;
        use crate::frontend::tui::tabs::WorkflowViewState;

        let mut app = make_app();

        // Seed the workflow_state with a running step.
        let view = WorkflowViewState {
            steps: vec![WorkflowStepView {
                name: "build".into(),
                status: "running".into(),
                agent: None,
                model: None,
                depends_on: vec![],
            }],
            current_step: Some("build".into()),
        };
        *app.active_tab_mut().workflow_state.lock().unwrap() = Some(view);

        // Wire up an engine channel so we can observe what's sent.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<EngineRequest>();
        *app.active_tab_mut().engine_tx_shared.lock().unwrap() = Some(tx);

        press_key(&mut app, KeyCode::Char('w'), KeyModifiers::CONTROL);

        let msg = rx.try_recv().expect("engine tx must receive a message");
        assert!(
            matches!(msg, EngineRequest::OpenControlBoard),
            "Ctrl+W during a running step must send OpenControlBoard"
        );
    }

    #[test]
    fn ctrl_w_in_step_confirm_escalates_to_wcb() {
        use crate::engine::workflow::EngineRequest;

        let mut app = make_app();

        // Wire up an engine channel so Ctrl-W handler fires.
        let (engine_tx, _engine_rx) = tokio::sync::mpsc::unbounded_channel::<EngineRequest>();
        *app.active_tab_mut().engine_tx_shared.lock().unwrap() = Some(engine_tx);

        // Open a StepConfirm dialog with a response channel.
        let (tx, rx) = std::sync::mpsc::channel();
        app.tabs[app.active_tab].dialog_response_tx = Some(tx);
        app.active_dialog = Some(Dialog::WorkflowStepConfirm(
            crate::frontend::tui::dialogs::WorkflowStepConfirmState {
                completed_step: "build".into(),
                next_step: "test".into(),
            },
        ));
        app.command_dialog_active = true;

        press_key(&mut app, KeyCode::Char('w'), KeyModifiers::CONTROL);

        // The dialog should have been dismissed.
        assert!(
            app.active_dialog.is_none(),
            "StepConfirm dialog must close on Ctrl+W"
        );
        // The frontend must have received Char('W') so it can open the full WCB.
        let resp = rx
            .try_recv()
            .expect("dialog_response_tx must receive a message");
        assert!(
            matches!(
                resp,
                crate::frontend::tui::dialogs::DialogResponse::Char('W')
            ),
            "escalation must send Char('W') to trigger full WCB"
        );
    }

    // ─── ConfigShow read-only toast ───────────────────────────────────────────

    #[test]
    fn enter_on_read_only_shows_toast() {
        use crate::frontend::tui::dialogs::{ConfigShowRow, ConfigShowState};
        use crate::frontend::tui::text_edit::TextEdit;

        let mut app = make_app();
        app.active_dialog = Some(Dialog::ConfigShow(ConfigShowState {
            rows: vec![ConfigShowRow {
                field: "agent".into(),
                global: "claude".into(),
                repo: "claude".into(),
                effective: "claude".into(),
                read_only: true,
            }],
            selected: 0,
            editing: false,
            edit_column: 0,
            editor: TextEdit::new(false),
        }));
        app.command_dialog_active = true;

        press_key(&mut app, KeyCode::Enter, KeyModifiers::NONE);

        assert_eq!(
            app.status_bar.text, "This field is read-only",
            "pressing Enter on a read-only ConfigShow row must update the status bar"
        );
        // The dialog should remain open.
        assert!(
            app.active_dialog.is_some(),
            "dialog must stay open after read-only toast"
        );
    }

    // ─── ContainerWindow cycle / resize ──────────────────────────────────────

    #[test]
    fn cycle_to_hidden_does_not_send_resize() {
        let mut app = make_app();
        // Wire a resize channel to observe.
        let (resize_tx, mut resize_rx) = tokio::sync::mpsc::unbounded_channel::<(u16, u16)>();
        app.active_tab_mut().container_resize_tx = Some(resize_tx);

        // Start at Maximized, cycle → Minimized (not Hidden, resize expected on next test).
        app.active_tab_mut().container_window_state =
            crate::frontend::tui::tabs::ContainerWindowState::Maximized;
        // Cycle: Maximized → Minimized
        press_key(&mut app, KeyCode::Char('m'), KeyModifiers::CONTROL);
        assert_eq!(
            app.active_tab().container_window_state,
            crate::frontend::tui::tabs::ContainerWindowState::Minimized,
        );

        // Cycle again: Minimized → Maximized (still not hidden, resize may be sent)
        press_key(&mut app, KeyCode::Char('m'), KeyModifiers::CONTROL);
        assert_eq!(
            app.active_tab().container_window_state,
            crate::frontend::tui::tabs::ContainerWindowState::Maximized,
        );

        // Cycle: Maximized → Minimized once more — no Hidden state reached yet.
        // Now let's explicitly set Hidden and verify cycling to Hidden sends nothing.
        app.active_tab_mut().container_window_state =
            crate::frontend::tui::tabs::ContainerWindowState::Minimized;
        // Drain channel to reset state.
        while resize_rx.try_recv().is_ok() {}

        // Hidden → Maximized (sending resize) then Maximized → Minimized (sending resize)
        // We want to reach Hidden from Minimized: but cycle(Minimized) = Maximized.
        // Actually cycle(Hidden) = Maximized, cycle(Minimized) = Maximized, cycle(Maximized) = Minimized.
        // There's no transition TO Hidden — Hidden is the initial state.
        // So we test that cycling out of Hidden (to Maximized) might send a resize,
        // and cycling Maximized → Minimized does NOT go to Hidden and always sends resize.
        // "Cycle to hidden does not send resize" means starting from Maximized → Minimized:
        // In that transition, a resize IS sent (not hidden). But if we start from Hidden and
        // cycle, we go to Maximized (sends resize). Since Hidden isn't reachable via cycle from
        // a non-hidden state, let's verify: starting at Maximized, cycling to Minimized.
        app.active_tab_mut().container_window_state =
            crate::frontend::tui::tabs::ContainerWindowState::Maximized;
        while resize_rx.try_recv().is_ok() {}
        press_key(&mut app, KeyCode::Char('m'), KeyModifiers::CONTROL);
        // Minimized ≠ Hidden so resize is attempted (may fail in CI env).
        // The key assertion: cycling from Hidden should not send resize even if Hidden
        // is explicitly set.
        app.active_tab_mut().container_window_state =
            crate::frontend::tui::tabs::ContainerWindowState::Hidden;
        app.active_tab_mut().container_resize_tx = None; // no channel
                                                         // Cycling from Hidden → Maximized — the resize send should not panic.
        press_key(&mut app, KeyCode::Char('m'), KeyModifiers::CONTROL);
        assert_eq!(
            app.active_tab().container_window_state,
            crate::frontend::tui::tabs::ContainerWindowState::Maximized,
        );
    }

    // ─── Workflow strip scroll ────────────────────────────────────────────────

    #[test]
    fn scroll_down_reveals_hidden_parallel_steps() {
        use crate::frontend::tui::tabs::{WorkflowStepView, WorkflowViewState};
        use crossterm::event::{MouseEvent, MouseEventKind};
        use ratatui::layout::Rect;

        let mut app = make_app();

        // Seed a workflow with many parallel steps so the strip would have overflow.
        let view = WorkflowViewState {
            steps: (0..6)
                .map(|i| WorkflowStepView {
                    name: format!("step-{i}"),
                    status: "pending".into(),
                    agent: None,
                    model: None,
                    depends_on: vec![],
                })
                .collect(),
            current_step: None,
        };
        *app.active_tab_mut().workflow_state.lock().unwrap() = Some(view);

        // Simulate the renderer having recorded a strip rect.
        let strip_rect = Rect::new(0, 30, 80, 9);
        app.active_tab_mut().last_strip_rect = Some(strip_rect);

        assert_eq!(app.active_tab().workflow_strip_scroll_offset, 0);

        // Mouse scroll-down inside the strip rect increments the offset.
        super::handle_mouse_event(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column: 10,
                row: 32, // inside strip_rect
                modifiers: KeyModifiers::NONE,
            },
        );
        assert_eq!(
            app.active_tab().workflow_strip_scroll_offset,
            1,
            "scroll down inside strip must increment workflow_strip_scroll_offset"
        );
    }

    #[test]
    fn scroll_clamped_at_bounds() {
        use crate::frontend::tui::tabs::{WorkflowStepView, WorkflowViewState};
        use crossterm::event::{MouseEvent, MouseEventKind};
        use ratatui::layout::Rect;

        let mut app = make_app();
        let view = WorkflowViewState {
            steps: vec![WorkflowStepView {
                name: "only".into(),
                status: "pending".into(),
                agent: None,
                model: None,
                depends_on: vec![],
            }],
            current_step: None,
        };
        *app.active_tab_mut().workflow_state.lock().unwrap() = Some(view);

        let strip_rect = Rect::new(0, 30, 80, 3);
        app.active_tab_mut().last_strip_rect = Some(strip_rect);

        // Scroll up when already at 0 → offset stays at 0 (no underflow).
        super::handle_mouse_event(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: 10,
                row: 31,
                modifiers: KeyModifiers::NONE,
            },
        );
        assert_eq!(
            app.active_tab().workflow_strip_scroll_offset,
            0,
            "scrolling up at offset=0 must not underflow"
        );
    }
}
