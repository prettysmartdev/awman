//! Tests for `dialog_router` behavior: dismissal, char/submit handling, and
//! cursor/scroll editing across the various `Dialog` variants.

use super::*;

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

// ─── FatalError dialog (invalid runtime config) ───────────────────────────

fn fatal_error_dialog() -> Dialog {
    Dialog::FatalError {
        title: "Invalid Runtime Configuration".into(),
        body: "invalid runtime 'blarg'".into(),
    }
}

#[test]
fn fatal_error_dialog_enter_quits() {
    let mut app = make_app();
    app.active_dialog = Some(fatal_error_dialog());
    press_key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(app.should_quit, "Enter on FatalError must quit the TUI");
    assert!(app.active_dialog.is_none());
}

#[test]
fn fatal_error_dialog_esc_quits() {
    let mut app = make_app();
    app.active_dialog = Some(fatal_error_dialog());
    press_key(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert!(app.should_quit, "Esc on FatalError must quit the TUI");
}

#[test]
fn fatal_error_dialog_ctrl_c_quits() {
    let mut app = make_app();
    app.active_dialog = Some(fatal_error_dialog());
    press_key(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
    assert!(app.should_quit, "Ctrl-C on FatalError must quit the TUI");
}

#[test]
fn fatal_error_dialog_ignores_regular_chars() {
    let mut app = make_app();
    app.active_dialog = Some(fatal_error_dialog());
    press_char(&mut app, 'x');
    assert!(!app.should_quit);
    assert!(
        matches!(app.active_dialog, Some(Dialog::FatalError { .. })),
        "regular chars must not dismiss the fatal dialog"
    );
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
