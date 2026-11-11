//! User journeys through the real key handler and renderer on private state.
use super::*;
use crate::command::{
    dispatch::Engines,
    instance::{local::LocalInstanceBackend, InstanceParts},
};
use crate::data::session::SessionOpenOptions;
use awman_test_support::{TestCase, TestSandbox};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{backend::TestBackend, Terminal};

fn app(s: &TestSandbox) -> App {
    let env = EnvSnapshot::from_full_env(
        s.env()
            .iter()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.to_string_lossy().into_owned(),
                )
            })
            .collect(),
    );
    let session = Session::open_at_directory_root(
        s.work(),
        SessionOpenOptions {
            env: Some(env.clone()),
            ..Default::default()
        },
    )
    .unwrap();
    let backend = LocalInstanceBackend::open(InstanceParts {
        catalogue: CommandCatalogue::get(),
        engines: Engines::for_env(env.clone()).unwrap(),
        session_manager: Arc::new(SessionManager::in_memory(env.clone())),
        runtime: tokio::runtime::Handle::current(),
        env: Arc::new(move || env.clone()),
        presentation: Arc::new(command_frontend::TuiPresentation),
    });
    App::with_backend(
        backend,
        Tab::new(session),
        tokio::runtime::Handle::current(),
    )
}
fn press(app: &mut App, key: KeyCode, modifiers: KeyModifiers) {
    key_handler::handle_key_event(app, KeyEvent::new(key, modifiers));
}
fn draw(app: &mut App, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| render::render_frame(app, frame))
        .unwrap();
    terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect()
}
#[tokio::test]
async fn command_edit_validation_and_resize_preserve_user_input() {
    let s = TestSandbox::for_test(TestCase::component(
        "frontend::tui::contracts::command_edit_validation_and_resize_preserve_user_input",
    ));
    let mut app = app(&s);
    for ch in "unknown-command 世界".chars() {
        press(&mut app, KeyCode::Char(ch), KeyModifiers::NONE);
    }
    assert_eq!(app.command_input.text, "unknown-command 世界");
    press(&mut app, KeyCode::Backspace, KeyModifiers::NONE);
    assert_eq!(app.command_input.text, "unknown-command 世");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(
        app.input_error.is_some(),
        "invalid input must produce a visible error"
    );
    assert_eq!(app.command_input.text, "unknown-command 世");
    assert!(!app.should_quit);
    let normal = draw(&mut app, 100, 30);
    assert!(normal.contains("unknown-command"));
    for (width, height) in [(1, 1), (20, 6), (80, 24), (140, 50)] {
        draw(&mut app, width, height);
        assert_eq!(app.command_input.text, "unknown-command 世");
    }
    drop(app);
    s.finish();
}
#[tokio::test]
async fn tab_navigation_and_dialog_dismissal_keep_sessions_distinct() {
    let s = TestSandbox::for_test(TestCase::component(
        "frontend::tui::contracts::tab_navigation_and_dialog_dismissal_keep_sessions_distinct",
    ));
    let mut app = app(&s);
    let first = app.active_tab().session.id();
    let other = s.work().join("other");
    std::fs::create_dir_all(&other).unwrap();
    let session = Session::open_at_directory_root(
        other,
        SessionOpenOptions {
            env: Some(app.active_tab().session.env().clone()),
            ..Default::default()
        },
    )
    .unwrap();
    let second = session.id();
    assert_ne!(first, second);
    app.push_tab(Tab::new(session));
    press(&mut app, KeyCode::Char('d'), KeyModifiers::CONTROL);
    assert_eq!(app.active_tab().session.id(), second);
    press(&mut app, KeyCode::Char('a'), KeyModifiers::CONTROL);
    assert_eq!(app.active_tab().session.id(), first);
    press(&mut app, KeyCode::Char('d'), KeyModifiers::CONTROL);
    assert_eq!(app.active_tab().session.id(), second);
    press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
    assert!(app.active_dialog.is_some(), "new-tab dialog opens");
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert!(app.active_dialog.is_none());
    assert_eq!(app.tabs.len(), 2);
    assert!(!app.should_quit);
    draw(&mut app, 80, 24);
    drop(app);
    s.finish();
}
