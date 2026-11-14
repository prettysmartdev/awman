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
async fn multiline_dialog_arrows_follow_word_wrapping_and_resize() {
    let s = TestSandbox::for_test(TestCase::component(
        "frontend::tui::contracts::multiline_dialog_arrows_follow_word_wrapping_and_resize",
    ));
    let mut app = app(&s);
    let text = "one two three four five six seven eight nine ten";
    let mut editor = text_edit::TextEdit::new(true);
    editor.set_text(text);
    editor.move_home();
    app.active_dialog = Some(Dialog::MultilineInput {
        title: "Description".into(),
        prompt: "Enter text".into(),
        editor,
    });
    let screen = draw(&mut app, 60, 40);
    assert!(screen.contains("one two three four five six seven"));
    for key in [KeyCode::Right, KeyCode::Right, KeyCode::Down] {
        press(&mut app, key, KeyModifiers::NONE);
    }
    let Some(Dialog::MultilineInput { editor, .. }) = &app.active_dialog else {
        panic!("dialog closed")
    };
    assert_eq!(&editor.text[editor.cursor..], "ght nine ten");
    press(&mut app, KeyCode::Up, KeyModifiers::NONE);
    let Some(Dialog::MultilineInput { editor, .. }) = &app.active_dialog else {
        panic!("dialog closed")
    };
    assert_eq!(editor.cursor, 2);
    draw(&mut app, 44, 40);
    press(&mut app, KeyCode::Down, KeyModifiers::NONE);
    let Some(Dialog::MultilineInput { editor, .. }) = &app.active_dialog else {
        panic!("dialog closed")
    };
    assert!(editor.cursor > 2 && editor.cursor < 36);
    assert_eq!(editor.text, text);
    drop(app);
    s.finish();
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

#[test]
fn full_engine_new_tab_shows_capacity_choices_and_escape_cancels_ready() {
    let s = TestSandbox::for_test(TestCase::component(
        "frontend::tui::contracts::full_engine_new_tab_shows_capacity_choices_and_escape_cancels_ready",
    ));
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(capacity_dialog_scenario(&s));
    // Tab drop cancels the Git poll, but abort cannot interrupt its current
    // synchronous Git call. Join the runtime before removing its working
    // directory: Windows refuses cleanup while that process still uses it.
    drop(runtime);
    s.finish();
}

async fn capacity_dialog_scenario(s: &TestSandbox) {
    use crate::command::instance::{EngineInstance, HostedOptions, OpenTabRequest};
    use crate::data::{daemon_state::FeatureName, ids::InstanceId};
    use crate::engine::daemon::executor::SharedExecutor;
    use std::time::Duration;

    let mut app = app(s);
    let env = app.active_tab().session.env().clone();
    let repo = awman_test_support::fixtures::RepoBuilder::new(s, "capacity").init();
    let executor = SharedExecutor::new(1);
    executor.set_share(FeatureName::Engine, 1);
    let _busy = executor.try_acquire(FeatureName::Engine).unwrap();
    let instance = EngineInstance::new_hosted(
        InstanceParts {
            catalogue: CommandCatalogue::get(),
            engines: Engines::for_env(env.clone()).unwrap(),
            session_manager: Arc::new(SessionManager::in_memory(env.clone())),
            runtime: tokio::runtime::Handle::current(),
            env: Arc::new(move || env.clone()),
            presentation: Arc::new(command_frontend::TuiPresentation),
        },
        HostedOptions {
            id: InstanceId::new(),
            kind: crate::command::dispatch::catalogue::FrontendKind::ENGINE_LOCAL,
            hub: None,
            executor: Some(executor.clone()),
        },
    );
    let opened = instance
        .open_tab(OpenTabRequest {
            dir: repo.path().to_path_buf(),
            options: SessionOpenOptions::default(),
            pty_size: (80, 24),
        })
        .unwrap();
    let (tab, startup) = Tab::from_opened(opened);
    let idx = app.push_tab(tab);
    app.active_tab = idx;
    app.bind_startup(idx, startup, (80, 24));
    awman_test_support::wait_for("capacity modal", Duration::from_secs(5), || {
        app.poll_dialog_requests();
        app.active_dialog.is_some().then_some(())
    });
    let screen = draw(&mut app, 80, 24);
    for text in [
        "Engine capacity reached",
        "Engine jobs: 1 / 1",
        "[i] Increase and continue",
        "[w] Wait for a slot",
        "[c] Cancel",
        "[Esc] cancel",
    ] {
        assert!(screen.contains(text), "missing {text}: {screen}");
    }
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(
        app.active_dialog.is_some(),
        "Enter must not increase capacity"
    );
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert!(app.active_dialog.is_none());
    awman_test_support::wait_for("cancelled ready", Duration::from_secs(5), || {
        (instance.running_jobs() == 0).then_some(())
    });
    assert_eq!(
        executor.snapshot().per_feature[&FeatureName::Engine].running,
        1
    );
    instance.quit();
    drop(app);
    drop(instance);
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
