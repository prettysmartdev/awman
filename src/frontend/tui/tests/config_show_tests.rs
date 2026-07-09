//! Tests for the ConfigShow dialog: inline field editing and the Ctrl+N
//! add-mapping flow, both routed through `dialog_router`.

use super::*;

// ─── ConfigShow dialog behavior ──────────────────────────────────────────

fn config_row(
    field: &str,
    global: &str,
    repo: &str,
    read_only: bool,
    global_writable: bool,
    repo_writable: bool,
) -> crate::frontend::tui::dialogs::ConfigShowRow {
    crate::frontend::tui::dialogs::ConfigShowRow {
        field: field.into(),
        global: global.into(),
        repo: repo.into(),
        effective: if repo.is_empty() { global } else { repo }.into(),
        read_only,
        global_writable,
        repo_writable,
        value_hint: None,
    }
}

fn config_show_dialog(rows: Vec<crate::frontend::tui::dialogs::ConfigShowRow>) -> Dialog {
    Dialog::ConfigShow(crate::frontend::tui::dialogs::ConfigShowState {
        rows,
        selected: 0,
        editing: false,
        edit_column: 0,
        editor: crate::frontend::tui::text_edit::TextEdit::new(false),
        new_entry: None,
        error: None,
    })
}

#[test]
fn enter_on_read_only_shows_toast() {
    let mut app = make_app();
    let _rx = setup_command_dialog(
        &mut app,
        config_show_dialog(vec![config_row(
            "auto_agent_auth_accepted",
            "true",
            "",
            true,
            false,
            false,
        )]),
    );

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

#[test]
fn enter_on_agents_to_models_summary_row_points_at_ctrl_n() {
    let mut app = make_app();
    let _rx = setup_command_dialog(
        &mut app,
        config_show_dialog(vec![config_row(
            "dynamicWorkflows.agentsToModels",
            "",
            "2 agents mapped",
            true,
            false,
            false,
        )]),
    );

    press_key(&mut app, KeyCode::Enter, KeyModifiers::NONE);

    assert_eq!(
        app.status_bar.text, "Press Ctrl+N to add a mapping, or edit a per-agent row below",
        "the map summary row must steer users to Ctrl+N / per-agent rows"
    );
    assert!(app.active_dialog.is_some(), "dialog must stay open");
}

#[test]
fn enter_on_agents_to_models_row_starts_inline_edit_in_repo_column() {
    let mut app = make_app();
    let _rx = setup_command_dialog(
        &mut app,
        config_show_dialog(vec![config_row(
            "dynamicWorkflows.agentsToModels.claude",
            "",
            "claude-opus-4-8, claude-sonnet-4-6",
            false,
            false,
            true,
        )]),
    );

    // Selection starts on the Global column; the repo-only field must
    // snap the edit to the Repo column instead of writing global config.
    press_key(&mut app, KeyCode::Enter, KeyModifiers::NONE);

    let Some(Dialog::ConfigShow(state)) = &app.active_dialog else {
        panic!("dialog must stay open in edit mode");
    };
    assert!(state.editing, "per-agent rows must be inline-editable");
    assert_eq!(state.edit_column, 1, "edit must snap to the Repo column");
    assert_eq!(
        state.editor.text, "claude-opus-4-8, claude-sonnet-4-6",
        "the editor must be seeded with the current model list"
    );
}

#[test]
fn e_key_starts_editing_like_enter() {
    let mut app = make_app();
    let _rx = setup_command_dialog(
        &mut app,
        config_show_dialog(vec![config_row("agent", "claude", "", false, true, true)]),
    );

    press_char(&mut app, 'e');

    let Some(Dialog::ConfigShow(state)) = &app.active_dialog else {
        panic!("dialog must stay open in edit mode");
    };
    assert!(state.editing, "'e' must start inline editing");
    assert_eq!(
        state.editor.text, "claude",
        "editor must be seeded with the focused column's value"
    );
}

#[test]
fn global_only_field_snaps_edit_to_global_column() {
    let mut app = make_app();
    let _rx = setup_command_dialog(
        &mut app,
        config_show_dialog(vec![config_row(
            "runtime", "docker", "", false, true, false,
        )]),
    );
    if let Some(Dialog::ConfigShow(state)) = &mut app.active_dialog {
        state.edit_column = 1; // user parked on the Repo column
    }

    press_key(&mut app, KeyCode::Enter, KeyModifiers::NONE);

    let Some(Dialog::ConfigShow(state)) = &app.active_dialog else {
        panic!("dialog must stay open in edit mode");
    };
    assert!(state.editing);
    assert_eq!(
        state.edit_column, 0,
        "global-only fields must never be edited into the repo scope"
    );
}

#[test]
fn arrow_keys_do_not_change_row_while_editing() {
    let mut app = make_app();
    let _rx = setup_command_dialog(
        &mut app,
        config_show_dialog(vec![
            config_row("agent", "claude", "", false, true, true),
            config_row("runtime", "docker", "", false, true, false),
        ]),
    );

    press_key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    press_key(&mut app, KeyCode::Down, KeyModifiers::NONE);

    let Some(Dialog::ConfigShow(state)) = &app.active_dialog else {
        panic!("dialog must stay open");
    };
    assert!(state.editing);
    assert_eq!(
        state.selected, 0,
        "row navigation must be frozen while an inline edit is active"
    );
}

#[test]
fn enter_while_editing_sends_field_value_scope_response() {
    let mut app = make_app();
    let rx = setup_command_dialog(
        &mut app,
        config_show_dialog(vec![config_row("agent", "claude", "", false, true, true)]),
    );

    press_key(&mut app, KeyCode::Enter, KeyModifiers::NONE); // start editing (global col)
    press_key(&mut app, KeyCode::Backspace, KeyModifiers::NONE); // "claud"
    press_key(&mut app, KeyCode::Enter, KeyModifiers::NONE); // save

    let resp = rx.try_recv().expect("save must send a dialog response");
    match resp {
        DialogResponse::Text(s) => assert_eq!(s, "agent\tclaud\tglobal"),
        other => panic!("expected Text response, got {other:?}"),
    }
    assert!(
        app.active_dialog.is_none(),
        "dialog closes so the command \
         loop can apply the edit and re-present the table"
    );
}

#[test]
fn enter_while_editing_trims_whitespace_before_saving() {
    let mut app = make_app();
    let rx = setup_command_dialog(
        &mut app,
        config_show_dialog(vec![config_row(
            "dynamicWorkflows.maxConcurrentSteps",
            "",
            "",
            false,
            false,
            true,
        )]),
    );

    press_key(&mut app, KeyCode::Enter, KeyModifiers::NONE); // start editing
    for c in " 3 ".chars() {
        press_char(&mut app, c);
    }
    press_key(&mut app, KeyCode::Enter, KeyModifiers::NONE); // save

    let resp = rx.try_recv().expect("save must send a dialog response");
    match resp {
        DialogResponse::Text(s) => assert_eq!(
            s, "dynamicWorkflows.maxConcurrentSteps\t3\trepo",
            "stray whitespace must be trimmed so the value validates"
        ),
        other => panic!("expected Text response, got {other:?}"),
    }
}

#[test]
fn esc_cancels_edit_and_clears_rejection_error() {
    let mut app = make_app();
    let _rx = setup_command_dialog(
        &mut app,
        config_show_dialog(vec![config_row(
            "dynamicWorkflows.defaultLeader",
            "",
            "",
            false,
            false,
            true,
        )]),
    );
    if let Some(Dialog::ConfigShow(state)) = &mut app.active_dialog {
        state.editing = true;
        state.error = Some("expected agent::model".to_string());
    }

    press_key(&mut app, KeyCode::Esc, KeyModifiers::NONE);

    let Some(Dialog::ConfigShow(state)) = &app.active_dialog else {
        panic!("Esc during an edit must cancel the edit, not close the dialog");
    };
    assert!(!state.editing);
    assert_eq!(
        state.error, None,
        "cancelling the edit must clear the stale rejection reason"
    );
}


// ─── ConfigShow Ctrl+N add-mapping flow ──────────────────────────────────

#[test]
fn ctrl_n_starts_add_mapping_flow_and_esc_cancels_it() {
    use crate::frontend::tui::dialogs::NewMapEntryPhase;

    let mut app = make_app();
    let _rx = setup_command_dialog(&mut app, config_show_dialog(vec![]));

    press_key(&mut app, KeyCode::Char('n'), KeyModifiers::CONTROL);
    {
        let Some(Dialog::ConfigShow(state)) = &app.active_dialog else {
            panic!("dialog must stay open");
        };
        assert_eq!(state.new_entry, Some(NewMapEntryPhase::Key));
        assert!(state.editing, "text input must route to the inline editor");
    }

    press_key(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    let Some(Dialog::ConfigShow(state)) = &app.active_dialog else {
        panic!("Esc during the add flow must cancel the flow, not close the dialog");
    };
    assert_eq!(state.new_entry, None);
    assert!(!state.editing);
}

#[test]
fn ctrl_n_flow_sends_repo_scoped_mapping_edit() {
    let mut app = make_app();
    let rx = setup_command_dialog(&mut app, config_show_dialog(vec![]));

    press_key(&mut app, KeyCode::Char('n'), KeyModifiers::CONTROL);
    for c in "maki".chars() {
        press_char(&mut app, c);
    }
    press_key(&mut app, KeyCode::Enter, KeyModifiers::NONE); // confirm key
    for c in "model-a, model-b".chars() {
        press_char(&mut app, c);
    }
    press_key(&mut app, KeyCode::Enter, KeyModifiers::NONE); // save mapping

    let resp = rx.try_recv().expect("saving the mapping must respond");
    match resp {
        DialogResponse::Text(s) => assert_eq!(
            s, "dynamicWorkflows.agentsToModels.maki\tmodel-a, model-b\trepo",
            "the new mapping must be written to the repo scope"
        ),
        other => panic!("expected Text response, got {other:?}"),
    }
}

#[test]
fn ctrl_n_flow_rejects_invalid_agent_name() {
    use crate::frontend::tui::dialogs::NewMapEntryPhase;

    let mut app = make_app();
    let _rx = setup_command_dialog(&mut app, config_show_dialog(vec![]));

    press_key(&mut app, KeyCode::Char('n'), KeyModifiers::CONTROL);
    for c in "bad name!".chars() {
        press_char(&mut app, c);
    }
    press_key(&mut app, KeyCode::Enter, KeyModifiers::NONE);

    let Some(Dialog::ConfigShow(state)) = &app.active_dialog else {
        panic!("dialog must stay open");
    };
    assert_eq!(
        state.new_entry,
        Some(NewMapEntryPhase::Key),
        "an invalid agent name must keep the flow in the key phase"
    );
    assert!(
        app.status_bar.text.contains("not a valid agent name"),
        "the status bar must explain the rejection: {}",
        app.status_bar.text
    );
}

#[test]
fn ctrl_n_with_existing_agent_jumps_to_that_row_for_editing() {
    let mut app = make_app();
    let _rx = setup_command_dialog(
        &mut app,
        config_show_dialog(vec![
            config_row("agent", "claude", "", false, true, true),
            config_row(
                "dynamicWorkflows.agentsToModels.claude",
                "",
                "claude-opus-4-8",
                false,
                false,
                true,
            ),
        ]),
    );

    press_key(&mut app, KeyCode::Char('n'), KeyModifiers::CONTROL);
    for c in "claude".chars() {
        press_char(&mut app, c);
    }
    press_key(&mut app, KeyCode::Enter, KeyModifiers::NONE);

    let Some(Dialog::ConfigShow(state)) = &app.active_dialog else {
        panic!("dialog must stay open");
    };
    assert_eq!(
        state.new_entry, None,
        "duplicate key must not open a new entry"
    );
    assert_eq!(state.selected, 1, "selection must jump to the existing row");
    assert!(state.editing, "the existing row must open for editing");
    assert_eq!(state.editor.text, "claude-opus-4-8");
}

