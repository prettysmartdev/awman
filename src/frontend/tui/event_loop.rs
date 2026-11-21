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
    // `Some` while a host command owns the terminal (WI 0149): nothing is
    // drawn and no input is read until the matching restore.
    let mut released: Option<Released> = None;
    loop {
        if app.should_quit {
            break;
        }

        app.poll_dialog_requests();
        if let Some(op) = app.terminal_op.take() {
            let mut control = CrosstermControl {
                terminal,
                keyboard_enhanced,
                emitter,
                status_active,
            };
            perform_terminal_op(app, op, &mut control, &mut released)?;
            continue;
        }
        if released.is_some() {
            // An engine that is gone never sends the restore: take the
            // terminal back so its fatal dialog can be shown.
            if app.backend.health().is_err() {
                let mut control = CrosstermControl {
                    terminal,
                    keyboard_enhanced,
                    emitter,
                    status_active,
                };
                perform_terminal_op(app, TerminalOp::Restore, &mut control, &mut released)?;
                continue;
            }
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

// ─── The start-up hand-off prompt (WI 0142) ──────────────────────────────────

/// Ask a yes/no question in the TUI's own `YesNo` dialog before the TUI
/// starts (the hand-off runs before any tab exists): take the terminal, draw
/// the dialog alone, read `y` or `n` (Esc and Ctrl-C decline), and give the terminal
/// back. Any terminal error answers no.
pub(super) fn ask_yes_no_before_start(title: &str, body: &str) -> bool {
    let dialog = Dialog::YesNo {
        title: title.to_string(),
        body: body.to_string(),
    };
    let ask = || -> io::Result<bool> {
        let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
        loop {
            terminal.draw(|frame| render::render_dialog_only(&dialog, frame))?;
            if let Event::Key(key) = crossterm::event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                if let Some(answer) = yes_no_answer(key) {
                    return Ok(answer);
                }
            }
        }
    };
    if enable_raw_mode().is_err() {
        return false;
    }
    let answer = execute!(io::stdout(), EnterAlternateScreen)
        .and_then(|_| ask())
        .unwrap_or(false);
    restore_terminal(false);
    answer
}

/// Today's `YesNo` keys: `y` → yes, `n` → no; Esc and Ctrl-C decline too
/// (the question stops running work, so nothing answers yes by accident).
fn yes_no_answer(key: crossterm::event::KeyEvent) -> Option<bool> {
    use crossterm::event::{KeyCode, KeyModifiers};
    match key.code {
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => Some(false),
        KeyCode::Char('y') => Some(true),
        KeyCode::Char('n') | KeyCode::Esc => Some(false),
        _ => None,
    }
}

// ─── Host commands on the real terminal (WI 0149, WI 0142) ───────────────────

/// What the event loop does to the real terminal around a host command. The
/// TUI's implementation is [`CrosstermControl`]; tests record the calls.
pub(super) trait TerminalControl {
    /// Hand the terminal over: clear the program status, leave raw mode, the
    /// keyboard protocol and the alternate screen, and print the title.
    fn release(&mut self, title: &str);
    /// Take the terminal back after a release (and full repaint).
    fn reenter(&mut self) -> io::Result<()>;
    /// Raw keystrokes in and bytes out for a hub-terminal bridge. The real
    /// terminal goes back into raw mode (no alternate screen) so the remote
    /// PTY, not the local line discipline, echoes and edits — a secret typed
    /// at a host prompt is never echoed locally.
    fn bridge_ends(&mut self) -> io::Result<(Box<dyn BridgeInput>, Box<dyn io::Write + Send>)>;
    /// Leave the bridge's raw mode again (before [`Self::reenter`]).
    fn end_bridge(&mut self);
}

/// The terminal is released to a host command.
pub(super) struct Released {
    /// Set when the host command runs in an engine PTY (a hub terminal).
    bridge: Option<HostTerminalBridge>,
}

/// Perform one terminal hand-over and answer the command thread, in today's
/// order: release, (bridge), answer `Yes` … restore, answer `Dismissed`.
pub(super) fn perform_terminal_op(
    app: &mut App,
    op: TerminalOp,
    control: &mut dyn TerminalControl,
    released: &mut Option<Released>,
) -> io::Result<()> {
    match op {
        TerminalOp::Release { title, terminal_id } => {
            control.release(&title);
            // A daemon-hosted command runs its host process in an engine PTY
            // registered on the hub: show it on this terminal.
            let bridge = terminal_id.and_then(|terminal_id| {
                let ends = app
                    .backend
                    .open_host_terminal(&terminal_id)
                    .map_err(|error| error.to_string())
                    .and_then(|io| {
                        control
                            .bridge_ends()
                            .map(|(input, output)| (io, input, output))
                            .map_err(|error| error.to_string())
                    });
                match ends {
                    Ok((io, input, output)) => Some(HostTerminalBridge::start(io, input, output)),
                    Err(error) => {
                        println!("awman: cannot show the host terminal: {error}");
                        None
                    }
                }
            });
            *released = Some(Released { bridge });
            app.send_dialog_response(DialogResponse::Yes);
        }
        TerminalOp::Restore => {
            if let Some(Released {
                bridge: Some(bridge),
            }) = released.take()
            {
                bridge.stop();
                control.end_bridge();
            }
            control.reenter()?;
            app.send_dialog_response(DialogResponse::Dismissed);
        }
    }
    Ok(())
}

/// The real terminal under crossterm.
struct CrosstermControl<'a> {
    terminal: &'a mut Terminal<CrosstermBackend<io::Stdout>>,
    keyboard_enhanced: bool,
    emitter: &'a mut ProgramStatusEmitter,
    status_active: &'a AtomicBool,
}

impl TerminalControl for CrosstermControl<'_> {
    fn release(&mut self, title: &str) {
        if self.emitter.clear(&mut io::stdout()).is_err() {
            self.emitter.disable();
        }
        self.status_active.store(false, Ordering::Relaxed);
        restore_terminal(self.keyboard_enhanced);
        TUI_ACTIVE.store(false, Ordering::Relaxed);
        println!("awman: {title}");
    }

    fn reenter(&mut self) -> io::Result<()> {
        reenter_terminal(self.terminal, self.keyboard_enhanced)?;
        TUI_ACTIVE.store(true, Ordering::Relaxed);
        Ok(())
    }

    fn bridge_ends(&mut self) -> io::Result<(Box<dyn BridgeInput>, Box<dyn io::Write + Send>)> {
        let input = StdinInput::new()?;
        enable_raw_mode()?;
        Ok((Box::new(input), Box::new(io::stdout())))
    }

    fn end_bridge(&mut self) {
        let _ = disable_raw_mode();
    }
}

/// Keystrokes for a bridge, read with a timeout so the bridge can stop.
pub(super) trait BridgeInput: Send {
    /// `Ok(0)` when nothing arrived within `timeout`.
    fn read_timeout(&mut self, buf: &mut [u8], timeout: Duration) -> io::Result<usize>;
    /// The real terminal's size now (forwarded to the hub when it changes).
    fn size(&self) -> Option<(u16, u16)>;
}

/// Raw bytes from the process's stdin (the TUI's terminal).
struct StdinInput;

impl StdinInput {
    #[cfg(unix)]
    fn new() -> io::Result<Self> {
        Ok(Self)
    }

    #[cfg(not(unix))]
    fn new() -> io::Result<Self> {
        // Engine PTYs exist only on Unix daemons; a Windows TUI is never
        // handed a hub terminal for a host process.
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "host terminals are not supported on this platform",
        ))
    }
}

impl BridgeInput for StdinInput {
    #[cfg(unix)]
    fn read_timeout(&mut self, buf: &mut [u8], timeout: Duration) -> io::Result<usize> {
        use nix::poll::{poll, PollFd, PollFlags};
        use std::os::fd::AsFd;
        let stdin = io::stdin();
        let millis = u16::try_from(timeout.as_millis()).unwrap_or(u16::MAX);
        let mut fds = [PollFd::new(stdin.as_fd(), PollFlags::POLLIN)];
        if poll(&mut fds, millis).map_err(io::Error::from)? == 0 {
            return Ok(0);
        }
        // Read the descriptor directly: std's buffered `Stdin` would keep
        // bytes the TUI's own reader needs after the restore.
        match nix::unistd::read(stdin.as_fd(), buf).map_err(io::Error::from)? {
            0 => Err(io::ErrorKind::UnexpectedEof.into()),
            n => Ok(n),
        }
    }

    #[cfg(not(unix))]
    fn read_timeout(&mut self, _buf: &mut [u8], _timeout: Duration) -> io::Result<usize> {
        Err(io::ErrorKind::Unsupported.into())
    }

    fn size(&self) -> Option<(u16, u16)> {
        crossterm::terminal::size().ok()
    }
}

/// The real terminal bridged to a hub terminal: keystrokes → its input,
/// its screen and output → the terminal, the terminal's size → its resize
/// (the engine and the hub). Stopped on `TerminalRestore`.
pub(super) struct HostTerminalBridge {
    stop: Arc<AtomicBool>,
    /// `false` once stopped: output that arrives later is discarded instead
    /// of painting over the re-entered TUI.
    output_open: Arc<std::sync::Mutex<bool>>,
    input: Option<std::thread::JoinHandle<()>>,
}

impl HostTerminalBridge {
    const POLL: Duration = Duration::from_millis(50);

    pub(super) fn start(
        io: crate::command::instance::feeds::ContainerSlotIo,
        mut input: Box<dyn BridgeInput>,
        mut output: Box<dyn io::Write + Send>,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let output_open = Arc::new(std::sync::Mutex::new(true));
        let crate::command::instance::feeds::ContainerSlotIo {
            mut stdout_rx,
            stdin_tx,
            resize_tx,
        } = io;

        let open = Arc::clone(&output_open);
        let _ = std::thread::Builder::new()
            .name("awman-host-terminal-out".into())
            .spawn(move || {
                while let Some(bytes) = stdout_rx.blocking_recv() {
                    let Ok(open) = open.lock() else { return };
                    if !*open {
                        return;
                    }
                    if output
                        .write_all(&bytes)
                        .and_then(|_| output.flush())
                        .is_err()
                    {
                        return;
                    }
                }
            });

        let stopping = Arc::clone(&stop);
        let input = std::thread::Builder::new()
            .name("awman-host-terminal-in".into())
            .spawn(move || {
                let mut size = None;
                let mut buf = [0u8; 8192];
                while !stopping.load(Ordering::Relaxed) {
                    let now = input.size();
                    if now.is_some() && now != size {
                        size = now;
                        if let Some(size) = now {
                            let _ = resize_tx.send(size);
                        }
                    }
                    match input.read_timeout(&mut buf, Self::POLL) {
                        Ok(0) => {}
                        Ok(n) => {
                            if stdin_tx.send(buf[..n].to_vec()).is_err() {
                                break;
                            }
                        }
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                        Err(_) => break,
                    }
                }
                // Dropping the input and resize ends ends the subscription.
                drop(stdin_tx);
                drop(resize_tx);
            })
            .ok();
        Self {
            stop,
            output_open,
            input,
        }
    }

    /// Stop forwarding. Returns once no more bytes can reach the terminal.
    pub(super) fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Ok(mut open) = self.output_open.lock() {
            *open = false;
        }
        if let Some(input) = self.input.take() {
            let _ = input.join();
        }
    }
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
