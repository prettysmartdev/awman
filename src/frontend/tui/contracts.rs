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

#[tokio::test]
async fn closed_remote_dialog_is_discarded_without_losing_the_next_dialog() {
    use crate::command::instance::backend::BackendEvent;
    use crate::data::{dialog::DialogRequest, frontend_protocol::AnsweredBy, ids::DialogId};
    let s = TestSandbox::for_test(TestCase::component("frontend::tui::contracts::closed_remote_dialog_is_discarded_without_losing_the_next_dialog"));
    let mut app = app(&s);
    let tab_id = app.backend.tab_id(&app.tabs[0].session_id).unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    app.tabs[0].dialog_request_rx = Some(rx);
    tx.send(DialogRequest::WorkflowStepConfirm(
        dialogs::WorkflowStepConfirmState {
            completed_step: "plan".into(),
            next_step: "build".into(),
        },
    ))
    .unwrap();
    remote_tab::RemoteTabView::apply(
        &mut app,
        BackendEvent::DialogClosed {
            dialog_id: Some(DialogId::new()),
            tab_id,
            answered_by: AnsweredBy::Engine,
        },
    );
    app.poll_dialog_requests();
    assert!(app.active_dialog.is_none());
    tx.send(DialogRequest::YesNo {
        title: "next".into(),
        body: "question".into(),
    })
    .unwrap();
    app.poll_dialog_requests();
    assert!(matches!(app.active_dialog, Some(Dialog::YesNo { .. })));
    drop(app);
    s.finish();
}

#[tokio::test]
async fn narrow_workflow_window_preserves_the_reported_step_status() {
    use crate::data::program_status::{EffectiveStatus, ProgramState};
    let s = TestSandbox::for_test(TestCase::component(
        "frontend::tui::contracts::narrow_workflow_window_preserves_the_reported_step_status",
    ));
    let mut app = app(&s);
    let mut slot = tabs::ContainerSlot::new("build".into(), "claude".into(), 1000);
    slot.set_program_status(Some(EffectiveStatus {
        state: ProgramState::Idle,
        kind: None,
        progress: None,
        msg: None,
        app: None,
        id: None,
    }));
    let (left, right) = container_view::slot_titles_with_status(
        &slot,
        " 🔒 Claude Code (containerized) — build ".into(),
        " very-long-container-name | ... | ... | 00:42 ".into(),
        76,
    );
    assert!(left.contains("build — idle"), "{left}");
    assert!(right.is_empty(), "statistics must yield to status");
    app.tabs[0].container_slots.push(slot);
    let mut terminal = Terminal::new(TestBackend::new(80, 30)).unwrap();
    terminal
        .draw(|frame| {
            container_view::render_container_maximized(&mut app.tabs[0], frame.area(), 0, frame)
        })
        .unwrap();
    let text: String = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect();
    assert!(text.contains("build — idle"), "{text}");

    // The state must also survive names that fill the border on their own.
    let slot = &mut app.tabs[0].container_slots[0];
    slot.step_name = "audit-actions-and-ia".into();
    slot.set_program_status(Some(EffectiveStatus {
        state: ProgramState::Working,
        kind: None,
        progress: Some(42),
        msg: None,
        app: None,
        id: None,
    }));
    for width in [48, 64, 80] {
        let (left, right) = container_view::slot_titles_with_status(
            slot,
            " 🔒 A very long agent name (containerized) — audit-actions-and-ia ".into(),
            " container-name | CPU | memory | 00:42 ".into(),
            width,
        );
        assert!(left.contains(" — ◐ 42% working"), "{left}");
        assert!(left.contains("actions-and-ia"), "{left}");
        assert!(right.is_empty());
        assert!(unicode_width::UnicodeWidthStr::width(left.as_str()) <= usize::from(width - 2));
    }
    let mut terminal = Terminal::new(TestBackend::new(64, 30)).unwrap();
    terminal
        .draw(|frame| {
            container_view::render_container_bars(&app.tabs[0], frame.area(), frame, false);
        })
        .unwrap();
    let text: String = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect();
    assert!(text.contains("actions-and-ia] — ◐ 42% working"), "{text}");

    let slot = &mut app.tabs[0].container_slots[0];
    slot.set_program_status(Some(EffectiveStatus {
        state: ProgramState::Working,
        kind: None,
        progress: None,
        msg: Some("正在检查工作流程中的所有步骤".repeat(10)),
        app: None,
        id: None,
    }));
    let (left, right) = container_view::slot_titles_with_status(
        slot,
        " audit-actions-and-ia ".into(),
        String::new(),
        64,
    );
    assert!(left.contains("working"));
    assert!(unicode_width::UnicodeWidthStr::width(left.as_str()) <= 62);
    assert!(right.is_empty());
    slot.set_program_status(None);
    let (left, _) = container_view::slot_titles_with_status(
        slot,
        " audit-actions-and-ia ".into(),
        String::new(),
        64,
    );
    assert_eq!(left, " audit-actions-and-ia ");
    drop(app);
    s.finish();
}

#[tokio::test]
async fn resumed_report_clears_builtin_dialog_ownership_and_deadline() {
    let s = TestSandbox::for_test(TestCase::component(
        "frontend::tui::contracts::resumed_report_clears_builtin_dialog_ownership_and_deadline",
    ));
    let mut app = app(&s);
    let (tx, rx) = std::sync::mpsc::channel();
    app.tabs[0].dialog_response_tx = Some(tx);
    app.active_dialog = Some(Dialog::WorkflowStepConfirm(
        dialogs::WorkflowStepConfirmState {
            completed_step: "plan".into(),
            next_step: "build".into(),
        },
    ));
    app.command_dialog_active = true;
    app.command_dialog_deadline = Some(std::time::Instant::now());
    app.tabs[0].shared.signals.request_report_dialog_dismiss();
    app.poll_dialog_requests();
    assert!(app.active_dialog.is_none());
    assert!(!app.command_dialog_active);
    assert!(app.command_dialog_deadline.is_none());
    assert_eq!(
        rx.try_recv().unwrap(),
        crate::data::dialog::DialogResponse::Dismissed
    );
}

#[tokio::test]
async fn board_activity_belongs_to_its_step_instead_of_the_focused_peer() {
    let s = TestSandbox::for_test(TestCase::component(
        "frontend::tui::contracts::board_activity_belongs_to_its_step_instead_of_the_focused_peer",
    ));
    let mut app = app(&s);
    app.active_dialog = Some(Dialog::WorkflowStepConfirm(
        dialogs::WorkflowStepConfirmState {
            completed_step: "build".into(),
            next_step: "test".into(),
        },
    ));
    press(&mut app, KeyCode::Left, KeyModifiers::NONE);
    assert_eq!(
        app.tabs[0].shared.signals.last_user_step().as_deref(),
        Some("build")
    );
    assert!(app.active_dialog.is_some());
    mouse_handler::handle_mouse_event(
        &mut app,
        crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::ScrollDown,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        },
    );
    assert_eq!(
        app.tabs[0].shared.signals.last_user_step().as_deref(),
        Some("build")
    );
}
