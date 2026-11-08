//! Terminal setup/teardown, the main render→input event loop, panic
//! recovery, and terminal-resize handling.

use std::io::{self, IsTerminal};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{Event, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::prelude::*;

use super::app::{App, TerminalOp};
use super::dialogs::{Dialog, DialogRequest, DialogResponse};
use super::input_reader::InputReader;
use super::program_status_emitter::{
    clear_terminal_status, emission_enabled, ProgramStatusEmitter,
};
use super::tabs::{self, ContainerWindowState};
use super::{git_sidebar, key_handler, mouse_handler, render, TUI_ACTIVE};
use crate::command::program_status_projection::{
    AwmanState, EmitInput, EmitSlot, EmitTab, ProgramStatusProjection,
};

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

/// Take the terminal back after a host command had it: raw mode, the
/// alternate screen and the keyboard protocol, then a full repaint.
fn reenter_terminal(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    keyboard_enhanced: bool,
) -> io::Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
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
    terminal.clear()
}

/// Where TUI panics are recorded. `None` when the home directory cannot be
/// resolved.
pub(super) fn panic_log() -> Option<crate::data::fs::PanicLog> {
    crate::data::fs::PanicLog::from_env(&crate::data::config::env::Env::from_process())
}

/// Append a panic report (message, location, thread, backtrace) to the Layer 0
/// [`PanicLog`]. Best-effort — a panic hook must never itself panic or block
/// on errors.
///
/// The hook formats; Layer 0 resolves the path and writes (F-44).
fn log_panic_to_file(info: &std::panic::PanicHookInfo<'_>) {
    let Some(log) = panic_log() else {
        return;
    };
    let thread = std::thread::current();
    let entry = format!(
        "──── panic at {} ────\nthread: {}\n{}\nbacktrace:\n{}\n",
        chrono::Utc::now().to_rfc3339(),
        thread.name().unwrap_or("<unnamed>"),
        info,
        std::backtrace::Backtrace::force_capture(),
    );
    log.append(&entry);
}

fn install_panic_hook(keyboard_enhanced: bool, status_active: Arc<AtomicBool>) {
    let original_hook = std::panic::take_hook();
    let main_thread = std::thread::current().id();
    std::panic::set_hook(Box::new(move |info| {
        log_panic_to_file(info);
        if std::thread::current().id() == main_thread {
            TUI_ACTIVE.store(false, Ordering::Relaxed);
            let _ = clear_terminal_status(
                status_active.swap(false, Ordering::Relaxed),
                &mut io::stdout(),
            );
            restore_terminal(keyboard_enhanced);
            original_hook(info);
        }
    }));
}

/// Set up the terminal, run the main loop, and restore on exit.
pub(super) fn run_event_loop(app: &mut App) -> io::Result<()> {
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
    //
    // Every panic is also appended to `$HOME/.awman/panic.log` with a full
    // backtrace. For panics on a worker thread (e.g. a spawned command task)
    // the TUI keeps running and would immediately repaint over anything the
    // default hook printed, so the log file is the only readable record —
    // the terminal is left alone and the default hook is skipped.
    let status_active = Arc::new(AtomicBool::new(false));
    install_panic_hook(keyboard_enhanced, status_active.clone());

    TUI_ACTIVE.store(true, Ordering::Relaxed);

    let config = app
        .tabs
        .first()
        .is_some_and(|tab| tab.session.effective_config().tui_program_status_emit());
    let term = crate::data::config::env::Env::from_process()
        .get("TERM")
        .map(str::to_owned);
    let enabled = emission_enabled(config, stdout.is_terminal(), term.as_deref())
        && InputReader::supports_probe();
    let mut emitter = ProgramStatusEmitter::new(enabled);
    // Keep every fallible startup operation inside the teardown boundary.
    let result = (|| {
        let mut input = InputReader::new(enabled)?;
        emitter = ProgramStatusEmitter::new(input.probe_enabled());
        if emitter.start_probe(&mut stdout).is_err() {
            emitter.disable();
        }
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;
        main_loop(
            &mut terminal,
            app,
            keyboard_enhanced,
            &mut emitter,
            &mut input,
            &status_active,
        )
    })();

    TUI_ACTIVE.store(false, Ordering::Relaxed);
    let clear_result = emitter.clear(&mut io::stdout());
    status_active.store(false, Ordering::Relaxed);
    restore_terminal(keyboard_enhanced);
    let _ = std::panic::take_hook();

    result.and(clear_result)
}

/// The main event loop: render → tick → poll → handle input → repeat.
fn main_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
    keyboard_enhanced: bool,
    emitter: &mut ProgramStatusEmitter,
    input: &mut InputReader,
    status_active: &AtomicBool,
) -> io::Result<()> {
    // True while a host command owns the terminal (WI 0149): nothing is drawn
    // and no input is read until the matching restore.
    let mut released = false;
    loop {
        if app.should_quit {
            break;
        }

        app.poll_dialog_requests();
        if let Some(op) = app.terminal_op.take() {
            match op {
                TerminalOp::Release { title } => {
                    if emitter.clear(&mut io::stdout()).is_err() {
                        emitter.disable();
                    }
                    status_active.store(false, Ordering::Relaxed);
                    restore_terminal(keyboard_enhanced);
                    TUI_ACTIVE.store(false, Ordering::Relaxed);
                    println!("awman: {title}");
                    released = true;
                    app.send_dialog_response(DialogResponse::Yes);
                }
                TerminalOp::Restore => {
                    reenter_terminal(terminal, keyboard_enhanced)?;
                    TUI_ACTIVE.store(true, Ordering::Relaxed);
                    released = false;
                    app.send_dialog_response(DialogResponse::Dismissed);
                }
            }
            continue;
        }
        if released {
            std::thread::sleep(Duration::from_millis(50));
            continue;
        }

        terminal.draw(|frame| {
            render::render_frame(app, frame);
        })?;

        app.tick_all_tabs();
        app.poll_runtime_notices();

        status_active.store(emitter.active(), Ordering::Relaxed);
        let reports = ProgramStatusProjection.reports(&emit_input(app));
        if emitter
            .tick(&reports, Instant::now(), &mut io::stdout())
            .is_err()
        {
            emitter.disable();
        }

        if let Some(event) = input.read(emitter, Duration::from_millis(50))? {
            // Set the panic-path latch before handing input to application
            // code, which may panic after the feature reply was consumed.
            status_active.store(emitter.active(), Ordering::Relaxed);
            match event {
                Event::Key(key_event) => {
                    if key_event.kind != KeyEventKind::Press {
                        continue;
                    }
                    key_handler::handle_key_event(app, key_event);
                }
                Event::Mouse(mouse) => {
                    mouse_handler::handle_mouse_event(app, mouse);
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

/// Snapshot presentation state; Layer 2 decides precedence and aggregation.
fn emit_input(app: &App) -> EmitInput {
    EmitInput {
        tabs: app
            .tabs
            .iter()
            .enumerate()
            .map(|(index, tab)| {
                let command = match tab.execution_phase {
                    tabs::ExecutionPhase::Idle => None,
                    tabs::ExecutionPhase::Running { .. } => Some(AwmanState::CommandRunning),
                    tabs::ExecutionPhase::Done { exit_code: 0, .. } => {
                        Some(AwmanState::CommandDone)
                    }
                    tabs::ExecutionPhase::Done { .. } | tabs::ExecutionPhase::Error { .. } => {
                        Some(AwmanState::CommandFailed)
                    }
                };
                let queued = match app.pending_dialog_requests.get(&tab.session_id) {
                    Some(DialogRequest::AgentAuth(_)) => Some(AwmanState::AuthDialog),
                    Some(
                        DialogRequest::Loading { .. }
                        | DialogRequest::WorkflowYoloCountdown(_)
                        | DialogRequest::TerminalRelease { .. }
                        | DialogRequest::TerminalRestore
                        | DialogRequest::KeySetupNotice { .. },
                    )
                    | None => None,
                    Some(_) => Some(AwmanState::DialogWaiting),
                };
                let dialog = if index == app.active_tab {
                    match app.active_dialog.as_ref() {
                        Some(Dialog::AgentAuth(_)) => Some(AwmanState::AuthDialog),
                        Some(Dialog::Loading { .. } | Dialog::WorkflowYoloCountdown(_)) | None => {
                            None
                        }
                        Some(Dialog::FatalError { .. }) => Some(AwmanState::CommandFailed),
                        Some(_) => Some(AwmanState::DialogWaiting),
                    }
                } else {
                    queued
                };
                let mut slots: Vec<_> = tab
                    .container_slots
                    .iter()
                    .enumerate()
                    .map(|(slot, agent)| EmitSlot {
                        slot,
                        status: agent.program_status.clone(),
                        awman: if slot == tab.focused_slot_idx {
                            dialog.or(command)
                        } else {
                            command
                        },
                    })
                    .collect();
                if slots.is_empty() {
                    slots.push(EmitSlot {
                        slot: 0,
                        status: tab.program_status(),
                        awman: dialog.or(command),
                    });
                }
                EmitTab {
                    index: index + 1,
                    label: tab.project_name(u16::MAX),
                    slots,
                }
            })
            .collect(),
    }
}

// ─── Resize ──────────────────────────────────────────────────────────────────

fn handle_resize(app: &mut App, cols: u16, rows: u16) {
    for tab in &mut app.tabs {
        tab.mouse_selection = None;
        if tab.container_window_state != ContainerWindowState::Hidden {
            let sidebar = git_sidebar::sidebar_width(cols, tab.git_sidebar_state);
            let left_cols = cols.saturating_sub(sidebar);
            let (inner_cols, inner_rows) = compute_container_inner_size(left_cols, rows);
            resize_slots(tab, inner_cols, inner_rows);
        }
    }
}

/// Resize every slot's parser and PTY to the estimated overlay inner size
/// for the current terminal dimensions. Broadcast to all slots (not just the
/// focused one) so each container tracks the real size even while minimized;
/// the per-tick sync in `tick_all_tabs` corrects any residual mismatch
/// against the actually-rendered overlay rect. Forwarding the size to the
/// PTY master triggers the SIGWINCH that reflows TUI apps inside the
/// container.
pub(super) fn resize_slots_to_terminal(tab: &mut tabs::Tab) {
    if let Ok((cols, rows)) = crossterm::terminal::size() {
        let sidebar = git_sidebar::sidebar_width(cols, tab.git_sidebar_state);
        let left_cols = cols.saturating_sub(sidebar);
        let (inner_cols, inner_rows) = compute_container_inner_size(left_cols, rows);
        resize_slots(tab, inner_cols, inner_rows);
    }
}

fn resize_slots(tab: &mut tabs::Tab, inner_cols: u16, inner_rows: u16) {
    for slot in &mut tab.container_slots {
        slot.vt100_parser
            .screen_mut()
            .set_size(inner_rows, inner_cols);
        if let Some(ref tx) = slot.container_resize_tx {
            let _ = tx.send((inner_cols, inner_rows));
        }
    }
}

/// Compute the vt100 grid size that fits inside the container overlay,
/// accounting for the 95% sizing within the execution window area and the
/// 2-cell border subtraction. The container window lives between the tab
/// bar (3 rows) and the bottom chrome (5 rows: status bar + command box +
/// suggestion row), plus any Workflow Overview or extra bar below.
///
/// `extra_bottom` accounts for the Workflow Overview height and the
/// minimized/summary bar (3 rows each when present). Callers that don't
/// know the exact extra height can pass 0 for a best-effort estimate.
pub(super) fn compute_container_inner_size(term_cols: u16, term_rows: u16) -> (u16, u16) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::program_status::{BlockedKind, EffectiveStatus, ProgramState, ReportState};
    use crate::frontend::osc7501::CLEAR_ALL;
    use crate::frontend::tui::dialogs::AgentAuthState;
    use crate::frontend::tui::tabs::{ContainerSlot, ExecutionPhase, Tab};
    use crate::frontend::tui::tests::make_app;

    #[test]
    fn snapshots_agents_and_awman_command_and_dialog_states() {
        let mut app = make_app();
        app.tabs[0].execution_phase = ExecutionPhase::Running {
            command: "chat".into(),
        };
        let mut agent = ContainerSlot::new(String::new(), "claude".into(), 10);
        agent.program_status = Some(EffectiveStatus {
            state: ProgramState::Working,
            kind: None,
            progress: Some(20),
            msg: Some("building".into()),
            app: None,
            id: None,
        });
        app.tabs[0].container_slots.push(agent);
        let input = emit_input(&app);
        assert_eq!(input.tabs[0].label, app.tabs[0].project_name(u16::MAX));
        assert_eq!(
            input.tabs[0].slots[0]
                .status
                .as_ref()
                .unwrap()
                .msg
                .as_deref(),
            Some("building")
        );
        assert_eq!(
            input.tabs[0].slots[0].awman,
            Some(AwmanState::CommandRunning)
        );
        app.active_dialog = Some(Dialog::YesNo {
            title: "Review".into(),
            body: "Proceed?".into(),
        });
        let reports = ProgramStatusProjection.reports(&emit_input(&app));
        assert_eq!(reports[0].state, ReportState::State(ProgramState::Blocked));
        assert_eq!(reports[0].kind, Some(BlockedKind::Question));
        app.active_dialog = Some(Dialog::AgentAuth(AgentAuthState {
            agent_name: "claude".into(),
            env_vars: vec![],
        }));
        assert_eq!(
            ProgramStatusProjection.reports(&emit_input(&app))[0].kind,
            Some(BlockedKind::Auth)
        );
        app.active_dialog = None;
        app.tabs[0].container_slots.clear();
        for (phase, state) in [
            (
                ExecutionPhase::Done {
                    command: "chat".into(),
                    exit_code: 0,
                },
                ProgramState::Done,
            ),
            (
                ExecutionPhase::Done {
                    command: "chat".into(),
                    exit_code: 3,
                },
                ProgramState::Error,
            ),
            (
                ExecutionPhase::Error {
                    command: "chat".into(),
                    message: "failed".into(),
                },
                ProgramState::Error,
            ),
        ] {
            app.tabs[0].execution_phase = phase;
            assert_eq!(
                ProgramStatusProjection.reports(&emit_input(&app))[0].state,
                ReportState::State(state)
            );
        }
    }

    #[test]
    fn background_dialogs_are_reported_and_still_delivered_on_focus() {
        let mut app = make_app();
        let session = app.tabs[0].session.clone();
        let mut background = Tab::new(session);
        // Tab::new mirrors the session id, so give the background its own
        // session identity (normal tabs are created through SessionManager).
        background.session_id = crate::data::session::SessionId::new();
        let (tx, rx) = std::sync::mpsc::channel();
        background.dialog_request_rx = Some(rx);
        app.tabs.push(background);
        tx.send(DialogRequest::AgentAuth(AgentAuthState {
            agent_name: "claude".into(),
            env_vars: vec![],
        }))
        .unwrap();
        app.poll_dialog_requests();
        assert!(app.active_dialog.is_none());
        let reports = ProgramStatusProjection.reports(&emit_input(&app));
        assert_eq!(reports[0].kind, Some(BlockedKind::Auth));
        assert_eq!(reports[1].id.as_deref(), Some("tab2/0"));
        app.active_tab = 1;
        app.poll_dialog_requests();
        assert!(matches!(app.active_dialog, Some(Dialog::AgentAuth(_))));
        assert!(app.pending_dialog_requests.is_empty());
    }

    #[test]
    fn panic_hook_writes_clear_before_restoring_terminal() {
        // Global hooks are isolated in a subprocess; this exercises the real
        // installed TUI hook without changing another test's panic handling.
        const CHILD: &str = "AWMAN_TEST_TUI_STATUS_PANIC_CHILD";
        if std::env::var_os(CHILD).is_some() {
            let active = Arc::new(AtomicBool::new(true));
            install_panic_hook(false, active.clone());
            assert!(std::panic::catch_unwind(|| panic!("emitter panic-path test")).is_err());
            assert!(!active.load(Ordering::Relaxed));
            return;
        }
        let home = tempfile::tempdir().unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "frontend::tui::event_loop::tests::panic_hook_writes_clear_before_restoring_terminal", "--nocapture"])
            .env(CHILD, "1")
            .env("HOME", home.path())
            .env("AWMAN_HOME", home.path())
            .output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let clear = output
            .stdout
            .windows(CLEAR_ALL.len())
            .position(|w| w == CLEAR_ALL)
            .expect("panic hook emitted clear");
        let leave = output
            .stdout
            .windows(8)
            .position(|w| w == b"\x1b[?1049l")
            .expect("restored alternate screen");
        assert!(clear < leave);
    }
}
