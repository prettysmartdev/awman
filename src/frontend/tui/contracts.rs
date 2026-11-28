//! User journeys through the real key handler and renderer on private state.
use super::*;
use crate::command::dispatch::Dispatch;
use crate::command::{
    dispatch::Engines,
    instance::{local::LocalInstanceBackend, InstanceParts},
};
use crate::data::session::SessionOpenOptions;
use crate::frontend::tui::command_frontend::TuiCommandFrontend;
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
    app_with_env(s, env)
}

fn app_with_env(s: &TestSandbox, env: EnvSnapshot) -> App {
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

#[tokio::test]
async fn ctrl_t_dialog_uses_working_directory_for_normal_tab() {
    let s = TestSandbox::for_test(TestCase::component(
        "frontend::tui::contracts::ctrl_t_dialog_uses_working_directory_for_normal_tab",
    ));
    let mut app = app(&s);
    let cwd = app
        .active_tab()
        .session
        .working_dir()
        .to_string_lossy()
        .to_string();

    press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);

    match app.active_dialog.as_ref().unwrap() {
        Dialog::TextInput { editor, .. } => assert_eq!(editor.text, cwd),
        _ => panic!("expected editable New Tab dialog"),
    }
    drop(app);
    s.finish();
}

#[tokio::test]
async fn ctrl_t_dialog_uses_captured_home_for_squad_tab() {
    let s = TestSandbox::for_test(TestCase::component(
        "frontend::tui::contracts::ctrl_t_dialog_uses_captured_home_for_squad_tab",
    ));
    let mut app = app(&s);
    let session = Session::open_squad_root(app.active_tab().session.env()).unwrap();
    assert_ne!(session.working_dir(), s.home());
    app.push_tab(Tab::new_squad(session));
    app.active_tab = 1;
    let home = s.home().display().to_string();

    press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);

    match app.active_dialog.as_ref().unwrap() {
        Dialog::TextInput { editor, .. } => assert_eq!(editor.text, home),
        _ => panic!("expected editable New Tab dialog"),
    }
    drop(app);
    s.finish();
}

#[tokio::test]
async fn ctrl_t_dialog_reports_unresolved_squad_home() {
    let s = TestSandbox::for_test(TestCase::component(
        "frontend::tui::contracts::ctrl_t_dialog_reports_unresolved_squad_home",
    ));
    let mut values: std::collections::BTreeMap<String, String> = s
        .env()
        .iter()
        .map(|(k, v)| {
            (
                k.to_string_lossy().into_owned(),
                v.to_string_lossy().into_owned(),
            )
        })
        .collect();
    values.remove(crate::data::config::env::HOME);
    let mut app = app_with_env(&s, EnvSnapshot::from_full_env(values));
    let session = app.active_tab().session.clone();
    app.push_tab(Tab::new_squad(session));
    app.active_tab = 1;

    press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);

    match app.active_dialog.as_ref().unwrap() {
        Dialog::Notice { body, .. } => {
            assert!(body.contains("Could not resolve your home directory"))
        }
        _ => panic!("expected home-resolution error notice"),
    }
    drop(app);
    s.finish();
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
async fn final_review_reopens_while_the_agent_output_stays_available() {
    use super::app::Focus;
    use crate::command::instance::feeds::ContainerSlotIo;
    use crate::data::dialog::{DialogRequest, DialogResponse};
    use crate::engine::workflow::frontend::EngineHandles;
    use crate::engine::workflow::EngineRequest;

    let s = TestSandbox::for_test(TestCase::component(
        "frontend::tui::contracts::final_review_reopens_while_the_agent_output_stays_available",
    ));
    let mut app = app(&s);
    let (requests, mut requests_rx) = tokio::sync::mpsc::unbounded_channel();
    let (dialog_tx, dialog_rx) = std::sync::mpsc::channel();
    let (answer_tx, answer_rx) = std::sync::mpsc::channel();
    let tab = app.active_tab_mut();
    tab.start_container("agent".into(), "still-running".into(), 80, 24);
    tab.execution_phase = tabs::ExecutionPhase::Running {
        command: "exec workflow".into(),
    };
    let controls = tab.shared.container.clone();
    let mut container = controls.clone();
    let (output, stdout_rx) = tokio::sync::mpsc::unbounded_channel();
    let (stdin_tx, _stdin_rx) = tokio::sync::mpsc::unbounded_channel();
    let (resize_tx, _resize_rx) = tokio::sync::mpsc::unbounded_channel();
    container.launch(
        Some("final".into()),
        ContainerSlotIo {
            stdout_rx: stdout_rx.into(),
            stdin_tx,
            resize_tx,
        },
    );
    // Workflow controls are command-scoped even when attached from an
    // earlier container's lifecycle handle.
    controls.attach_engine(EngineHandles::requests(requests));
    tab.drain_container_slot_events();
    tab.dialog_request_rx = Some(dialog_rx);
    tab.dialog_response_tx = Some(answer_tx);
    output.send(b"Final agent output".to_vec()).unwrap();
    tab.drain_container_output();
    let board = serde_json::from_value(serde_json::json!({
        "step_name": "final", "focused_step_name": "final",
        "can_launch_next": false, "can_continue_current": false,
        "can_restart": true, "can_go_back": true, "can_finish": true,
        "continue_unavailable_reason": null, "cancel_to_previous_unavailable_reason": null,
        "finish_workflow_unavailable_reason": null, "restart_unavailable_reason": null,
        "can_dismiss": true, "launch_next_label": null,
        "parallel_peer_count": 0, "parallel_peers_running": 0,
        "failure_lines": [], "retry_failed_step": null, "in_parallel_group": false
    }))
    .unwrap();
    let request = DialogRequest::WorkflowControlBoard(board);
    dialog_tx.send(request.clone()).unwrap();
    app.poll_dialog_requests();
    for window in [
        tabs::ContainerWindowState::Maximized,
        tabs::ContainerWindowState::Minimized,
    ] {
        for focus in [Focus::CommandBox, Focus::ExecutionWindow] {
            app.active_tab_mut().container_window_state = window;
            app.focus = focus;
            press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
            assert!(app.active_dialog.is_none());
            assert_eq!(answer_rx.try_recv().unwrap(), DialogResponse::Dismissed);
            assert_eq!(
                app.active_tab()
                    .focused_slot()
                    .unwrap()
                    .vt100_parser
                    .screen()
                    .contents(),
                "Final agent output"
            );
            press(&mut app, KeyCode::Char('w'), KeyModifiers::CONTROL);
            assert!(matches!(
                requests_rx.try_recv().unwrap(),
                EngineRequest::OpenControlBoard { .. }
            ));
            dialog_tx.send(request.clone()).unwrap();
            app.poll_dialog_requests();
            assert!(matches!(
                app.active_dialog,
                Some(Dialog::WorkflowControlBoard(_))
            ));
            assert!(draw(&mut app, 100, 40).contains("Finish workflow"));
        }
    }
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(answer_rx.try_recv().unwrap(), DialogResponse::Char('f'));
    assert!(!app.active_tab().shared.command_cancel.is_cancelled());
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

/// WI 0157: a TUI command naming the retired built-in `gemini` shows the
/// migration note and never starts a container. The same refusal on the CLI,
/// daemon and API paths is proven in `tests/component/{parity,command}`.
#[test]
fn retired_gemini_selection_shows_the_migration_note_without_a_container() {
    let s = TestSandbox::for_test(TestCase::component(
        "frontend::tui::contracts::retired_gemini_selection_shows_the_migration_note_without_a_container",
    ));
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(retired_gemini_selection_journey(&s));
    // Tab cancellation cannot interrupt an in-flight synchronous Git poll.
    // Join it before removing the working directory, including on Windows.
    drop(runtime);
    s.finish();
}

async fn retired_gemini_selection_journey(s: &TestSandbox) {
    let mut app = app(s);
    for ch in "exec prompt --agent gemini hello".chars() {
        press(&mut app, KeyCode::Char(ch), KeyModifiers::NONE);
    }
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    let note = "'gemini' is no longer a built-in agent";
    // The command runs on the runtime's worker threads; the tick adopts
    // its messages into the tab.
    awman_test_support::wait_for(
        "the TUI to show the retired-gemini migration note",
        std::time::Duration::from_secs(15),
        || {
            app.tick_all_tabs();
            let text = app.active_tab().output_lines.join("\n");
            (text.contains(note) || draw(&mut app, 200, 60).contains(note)).then_some(())
        },
    );
    assert!(
        app.active_tab().focused_slot().is_none(),
        "a container slot was started for a refused agent"
    );
    drop(app);
}

fn squad_app(s: &TestSandbox) -> App {
    let mut app = app(s);
    let session = Session::open_squad_root(app.active_tab().session.env()).unwrap();
    app.push_tab(Tab::new_squad(session));
    app.active_tab = 1;
    app.focus = app::Focus::ExecutionWindow;
    app
}

#[tokio::test]
async fn squad_detach_returns_grid_and_discards_late_events_and_completion() {
    use crate::command::instance::feeds::ContainerSlotEvent;
    use crate::frontend::tui::tabs::ExecutionPhase;
    let s = TestSandbox::for_test(TestCase::component("frontend::tui::contracts::squad_detach_returns_grid_and_discards_late_events_and_completion"));
    let mut app = squad_app(&s);
    let tab_id = app.active_tab().session_id;
    let state = app.active_tab_mut().squad.as_mut().unwrap();
    state.selected = 3;
    let poller = state.cancel.clone();
    let session = state.attach_session.clone();
    let cancel = app.active_tab().shared.command_cancel.clone();
    let generation = session.begin_command("squad attach task", cancel.clone());
    app.active_tab_mut().squad.as_mut().unwrap().attach_view =
        Some(tabs::squad_state::SquadAttachView {
            generation,
            task: "task".into(),
        });
    let events = app.active_tab().shared.container.clone();
    let (result_tx, result_rx) = std::sync::mpsc::channel();
    {
        let tab = app.active_tab_mut();
        tab.command_result_rx = Some(result_rx);
        tab.execution_phase = ExecutionPhase::Running {
            command: "squad attach task".into(),
        };
        tab.start_container("agent".into(), "daemon-owned".into(), 80, 24);
        tab.container_window_state = tabs::ContainerWindowState::Maximized;
    }
    events.push(ContainerSlotEvent::Launched {
        step_name: "queued".into(),
        agent: "agent".into(),
        model: None,
        io: None,
    });
    press(&mut app, KeyCode::Char('\\'), KeyModifiers::CONTROL);
    assert!(cancel.is_cancelled());
    assert_eq!(app.active_tab().session_id, tab_id);
    assert!(app.squad_grid_active());
    assert!(draw(&mut app, 120, 40).contains(" squad "));
    assert_eq!(app.active_tab().squad.as_ref().unwrap().selected, 3);
    assert!(!poller.is_cancelled());
    assert!(app.active_tab().command_result_rx.is_none());
    assert!(app.active_tab().container_slots.is_empty());
    events.push(ContainerSlotEvent::Launched {
        step_name: "late".into(),
        agent: "agent".into(),
        model: None,
        io: None,
    });
    assert!(result_tx
        .send(Ok(crate::command::dispatch::CommandOutcome::Empty))
        .is_err());
    app.active_tab_mut().drain_container_slot_events();
    app.active_tab_mut().poll_command_completion();
    assert!(app.squad_grid_active());
    let next_cancel = tokio_util::sync::CancellationToken::new();
    let next_generation = session.begin_command("reattach", next_cancel.clone());
    session.end_if_current(generation);
    assert!(
        !next_cancel.is_cancelled(),
        "old completion must not end the new viewer"
    );
    session.end_if_current(next_generation);
    assert!(next_cancel.is_cancelled());
    assert!(
        !squad_attach::detach_squad_attach(&mut app),
        "double detach is harmless"
    );
    drop(app);
    s.finish();
}

#[tokio::test]
async fn squad_waiting_and_timeout_are_visible_and_ctrl_c_drops_dialogs() {
    use crate::command::commands::attach::AttachCommandFrontend;
    use crate::data::message::{MessageLevel, UserMessage};
    let s = TestSandbox::for_test(TestCase::component(
        "frontend::tui::contracts::squad_waiting_and_timeout_are_visible_and_ctrl_c_drops_dialogs",
    ));
    let mut app = squad_app(&s);
    // Exercise the production instance producer, including synchronous submit
    // registration, before any daemon round trip has had a chance to finish.
    let tab = app.active_tab_mut();
    let generation = tab
        .squad
        .as_ref()
        .unwrap()
        .attach_session
        .begin_command("task", tab.shared.command_cancel.clone());
    tab.squad.as_mut().unwrap().attach_view = Some(tabs::squad_state::SquadAttachView {
        generation,
        task: "task".into(),
    });
    let parsed =
        Dispatch::<TuiCommandFrontend>::parse_command_box_input("squad attach task").unwrap();
    let (request_tx, request_rx) = std::sync::mpsc::channel();
    let (response_tx, response_rx) = std::sync::mpsc::channel();
    let (stdout_tx, _) = tokio::sync::mpsc::unbounded_channel();
    let (stdin_tx, stdin_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut producer = crate::command::instance::frontend::InstanceCommandFrontend::new(
        parsed,
        crate::command::instance::frontend::DialogChannels::new(request_tx, response_rx),
        crate::engine::agent_runtime::frontend::AgentIo {
            stdout: stdout_tx.clone(),
            stderr: stdout_tx,
            stdin_tx,
            stdin_rx,
            resize: None,
            initial_size: None,
        },
        tab.shared(),
    )
    .with_squad_attach(Some(crate::command::instance::feeds::SharedSquadAttach {
        session: tab.squad.as_ref().unwrap().attach_session.clone(),
        daemon_reachable: tab.squad.as_ref().unwrap().daemon_reachable.clone(),
        generation: Some(generation),
    }));
    let cancel = producer.begin_resolve("task");
    producer.attach_status(UserMessage {
        level: MessageLevel::Info,
        text: crate::command::commands::squad::attach::WAITING_FOR_TERMINAL.into(),
    });
    assert!(draw(&mut app, 120, 40).contains("Waiting for agent terminal to start…"));
    app.active_tab_mut().execution_phase = tabs::ExecutionPhase::Error {
        command: "squad attach task".into(),
        message: "daemon unavailable".into(),
    };
    let mut terminal = Terminal::new(TestBackend::new(140, 40)).unwrap();
    terminal
        .draw(|frame| render::render_frame(&mut app, frame))
        .unwrap();
    let status_line: String = (0..140)
        .map(|x| terminal.backend().buffer()[(x, 35)].symbol())
        .collect();
    assert!(status_line.contains("daemon unavailable"));
    // An active viewer now keeps the execution log on screen, including its
    // earlier startup message. The current status must show the failure.
    assert!(!status_line.contains("Waiting for agent terminal to start…"));
    app.active_tab_mut().execution_phase = tabs::ExecutionPhase::Idle;
    producer.terminal_ready();
    assert!(app
        .active_tab()
        .shared
        .attach_status
        .lock()
        .unwrap()
        .is_none());
    producer.attach_status(UserMessage {
        level: MessageLevel::Warning,
        text: crate::command::commands::squad::attach::TaskAttachEnd::StartupTimedOut
            .message(Default::default()),
    });
    assert!(draw(&mut app, 140, 40).contains("The agent terminal did not start within 10 seconds"));
    let (agent_input, mut received_input) = tokio::sync::mpsc::unbounded_channel();
    app.active_tab_mut()
        .start_container("agent".into(), "daemon-owned".into(), 80, 24);
    app.active_tab_mut()
        .focused_slot_mut()
        .unwrap()
        .container_stdin_tx = Some(agent_input);
    app.active_tab_mut().container_window_state = tabs::ContainerWindowState::Maximized;
    app.active_tab_mut().dialog_request_rx = Some(request_rx);
    app.active_tab_mut().dialog_response_tx = Some(response_tx);
    app.active_dialog = Some(Dialog::Notice {
        title: "command dialog".into(),
        body: "pending".into(),
        copy_key: None,
        copy_zshrc_snippet: None,
    });
    app.command_dialog_active = true;
    press(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
    assert!(cancel.is_cancelled());
    assert!(
        received_input.try_recv().is_err(),
        "Ctrl-C must not reach the daemon-owned agent"
    );
    assert!(app.active_dialog.is_none() && !app.command_dialog_active);
    assert!(
        app.active_tab().dialog_request_rx.is_none()
            && app.active_tab().dialog_response_tx.is_none()
    );
    assert!(app.squad_grid_active());
    assert!(draw(&mut app, 120, 40).contains(" squad "));
    producer.end_attach();
    app.active_tab_mut().drain_container_slot_events();
    assert!(app.squad_grid_active());
    drop(producer);
    drop(app);
    s.finish();
}

#[tokio::test]
async fn squad_new_tab_cancel_opens_nothing_and_relative_confirm_uses_home() {
    let s = TestSandbox::for_test(TestCase::component("frontend::tui::contracts::squad_new_tab_cancel_opens_nothing_and_relative_confirm_uses_home"));
    let mut app = squad_app(&s);
    std::fs::create_dir_all(s.home().join("projects/chosen")).unwrap();
    let count = app.tabs.len();
    press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert_eq!(app.tabs.len(), count);
    press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
    let Some(Dialog::TextInput { editor, .. }) = app.active_dialog.as_mut() else {
        panic!("New Tab dialog missing")
    };
    editor.set_text("projects/chosen");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(app.tabs.len(), count + 1);
    assert_eq!(
        app.active_tab().session.working_dir(),
        s.home().join("projects/chosen")
    );
    drop(app);
    s.finish();
}

mod squad_follow;
