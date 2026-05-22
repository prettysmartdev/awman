/// Integration tests for work item 0054: Ctrl-C behavior unchanged.
///
/// Work item 0054 adds Ctrl-M (container window toggle) and Ctrl-, (config show)
/// but must not alter existing Ctrl-C handling:
/// - From CommandBox focus: single tab → QuitConfirm; multiple tabs → CloseTabConfirm.
/// - From ExecutionWindow focus (Running, no container window, no status-watch cancel):
///   forward the Ctrl-C byte (0x03) to the PTY.
use awman::tui::input::{handle_key, Action};
use awman::tui::state::{App, ContainerWindowState, Dialog, ExecutionPhase, Focus};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

fn new_app() -> App {
    App::new(std::path::PathBuf::new())
}

#[test]
fn ctrl_c_from_command_box_single_tab_opens_quit_confirm() {
    let mut app = new_app();
    app.active_tab_mut().focus = Focus::CommandBox;
    assert_eq!(app.tabs.len(), 1);

    let key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
    let action = handle_key(&mut app, key);

    assert!(matches!(action, Action::None));
    assert_eq!(app.active_tab().dialog, Dialog::QuitConfirm);
}

#[test]
fn ctrl_c_from_command_box_multiple_tabs_opens_close_tab_confirm() {
    let mut app = new_app();
    app.create_tab(std::path::PathBuf::new());
    assert_eq!(app.tabs.len(), 2);
    app.active_tab_mut().focus = Focus::CommandBox;

    let key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
    let action = handle_key(&mut app, key);

    assert!(matches!(action, Action::None));
    assert_eq!(app.active_tab().dialog, Dialog::CloseTabConfirm);
}

#[test]
fn ctrl_c_from_execution_window_running_forwards_to_pty() {
    let mut app = new_app();
    app.active_tab_mut().phase = ExecutionPhase::Running { command: "implement 0001".into() };
    app.active_tab_mut().focus = Focus::ExecutionWindow;
    // container_window defaults to Hidden — no PTY window to intercept.
    assert_eq!(app.active_tab().container_window, ContainerWindowState::Hidden);
    // status_watch_cancel_tx is None (default) → Ctrl-C is forwarded to the PTY.

    let key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
    let action = handle_key(&mut app, key);

    assert!(
        matches!(action, Action::ForwardToPty(ref b) if b == &[3u8]),
        "Ctrl-C from execution window must forward byte 0x03 to the PTY",
    );
    assert_eq!(app.active_tab().dialog, Dialog::None);
}
