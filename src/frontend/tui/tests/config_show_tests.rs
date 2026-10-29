//! Tests for the ConfigShow dialog: inline field editing and the Ctrl+N
//! add-mapping flow, both routed through `dialog_router`.

use super::*;
use crate::command::commands::config::{ConfigEditRequest, ConfigRowShape};

// ─── ConfigShow dialog behavior ──────────────────────────────────────────

fn config_row(
    field: &str,
    global: &str,
    repo: &str,
    read_only: bool,
    global_writable: bool,
    repo_writable: bool,
    shape: ConfigRowShape,
) -> crate::frontend::tui::dialogs::ConfigShowRow {
    crate::frontend::tui::dialogs::ConfigShowRow {
        field: field.into(),
        global: global.into(),
        repo: repo.into(),
        effective: if repo.is_empty() { global } else { repo }.into(),
        read_only,
        global_writable,
        repo_writable,
        value_hint: match shape {
            ConfigRowShape::MapHeader { .. } => {
                Some("Press Ctrl+N to add a mapping, or edit a per-agent row below".into())
            }
            ConfigRowShape::ListHeader { .. } => {
                Some("Press Ctrl+N to add a guidance entry, or edit a per-entry row below".into())
            }
            _ => None,
        },
        shape,
    }
}

/// Synthetic rows keep frontend tests independent of registry field names.
fn map_rows(repo: serde_json::Value) -> Vec<crate::frontend::tui::dialogs::ConfigShowRow> {
    let mut rows = vec![config_row(
        "demo.map",
        "",
        "(none)",
        true,
        false,
        false,
        ConfigRowShape::MapHeader {
            child_prefix: "demo.map.",
        },
    )];
    if let Some(map) = repo["demo"]["map"].as_object() {
        for (key, models) in map {
            let value = models
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap())
                .collect::<Vec<_>>()
                .join(", ");
            rows.push(config_row(
                &format!("demo.map.{key}"),
                "",
                &value,
                false,
                true,
                true,
                ConfigRowShape::MapEntry,
            ));
        }
    }
    rows
}

/// The same for the guidance array.
fn guidance_rows(repo: serde_json::Value) -> Vec<crate::frontend::tui::dialogs::ConfigShowRow> {
    let mut rows = vec![config_row(
        "demo.list",
        "",
        "(none)",
        true,
        false,
        false,
        ConfigRowShape::ListHeader {
            child_prefix: "demo.list.",
        },
    )];
    if let Some(entries) = repo["demo"]["list"].as_array() {
        for (index, value) in entries.iter().enumerate() {
            rows.push(config_row(
                &format!("demo.list.{index}"),
                "",
                value.as_str().unwrap(),
                false,
                true,
                true,
                ConfigRowShape::ListEntry,
            ));
        }
    }
    rows
}

fn config_show_dialog(rows: Vec<crate::frontend::tui::dialogs::ConfigShowRow>) -> Dialog {
    // The production default (`app.rs`): the Global column. Repo-scoped
    // tests move to the Repo column with Right, as a user would.
    let edit_column = 0;
    Dialog::ConfigShow(crate::frontend::tui::dialogs::ConfigShowState {
        rows,
        selected: 0,
        editing: false,
        edit_column,
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
            ConfigRowShape::Scalar,
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
            "demo.map",
            "",
            "2 agents mapped",
            true,
            false,
            false,
            ConfigRowShape::MapHeader {
                child_prefix: "demo.map.",
            },
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
            "demo.map.claude",
            "",
            "claude-opus-4-8, claude-sonnet-4-6",
            false,
            false,
            true,
            ConfigRowShape::MapEntry,
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
        config_show_dialog(vec![config_row(
            "agent",
            "claude",
            "",
            false,
            true,
            true,
            ConfigRowShape::Scalar,
        )]),
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
            "runtime",
            "docker",
            "",
            false,
            true,
            false,
            ConfigRowShape::Scalar,
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
            config_row(
                "agent",
                "claude",
                "",
                false,
                true,
                true,
                ConfigRowShape::Scalar,
            ),
            config_row(
                "runtime",
                "docker",
                "",
                false,
                true,
                false,
                ConfigRowShape::Scalar,
            ),
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
        config_show_dialog(vec![config_row(
            "agent",
            "claude",
            "",
            false,
            true,
            true,
            ConfigRowShape::Scalar,
        )]),
    );

    press_key(&mut app, KeyCode::Enter, KeyModifiers::NONE); // start editing (global col)
    press_key(&mut app, KeyCode::Backspace, KeyModifiers::NONE); // "claud"
    press_key(&mut app, KeyCode::Enter, KeyModifiers::NONE); // save

    let resp = rx.try_recv().expect("save must send a dialog response");
    match resp {
        DialogResponse::ConfigEdit(edit) => {
            assert_eq!(edit, ConfigEditRequest::to_field("agent", "claud", true))
        }
        other => panic!("expected ConfigEdit response, got {other:?}"),
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
            "demo.limit",
            "",
            "",
            false,
            false,
            true,
            ConfigRowShape::Scalar,
        )]),
    );

    press_key(&mut app, KeyCode::Enter, KeyModifiers::NONE); // start editing
    for c in " 3 ".chars() {
        press_char(&mut app, c);
    }
    press_key(&mut app, KeyCode::Enter, KeyModifiers::NONE); // save

    let resp = rx.try_recv().expect("save must send a dialog response");
    match resp {
        DialogResponse::ConfigEdit(edit) => assert_eq!(
            edit,
            ConfigEditRequest::to_field("demo.limit", "3", false),
            "stray whitespace must be trimmed so the value validates"
        ),
        other => panic!("expected ConfigEdit response, got {other:?}"),
    }
}

#[test]
fn esc_cancels_edit_and_clears_rejection_error() {
    let mut app = make_app();
    let _rx = setup_command_dialog(
        &mut app,
        config_show_dialog(vec![config_row(
            "demo.leader",
            "",
            "",
            false,
            false,
            true,
            ConfigRowShape::Scalar,
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
    let _rx = setup_command_dialog(
        &mut app,
        config_show_dialog(map_rows(serde_json::json!({}))),
    );

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
    let rx = setup_command_dialog(
        &mut app,
        config_show_dialog(map_rows(serde_json::json!({}))),
    );

    press_key(&mut app, KeyCode::Right, KeyModifiers::NONE); // Repo column
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
        DialogResponse::ConfigEdit(edit) => assert_eq!(
            edit,
            ConfigEditRequest::to_field("demo.map.maki", "model-a, model-b", false),
            "the new mapping must be written to the repo scope"
        ),
        other => panic!("expected ConfigEdit response, got {other:?}"),
    }
}

#[test]
fn ctrl_n_flow_rejects_invalid_agent_name() {
    use crate::frontend::tui::dialogs::NewMapEntryPhase;

    let mut app = make_app();
    let _rx = setup_command_dialog(
        &mut app,
        config_show_dialog(map_rows(serde_json::json!({}))),
    );

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
    // The wording is `AgentName`'s, in Layer 0 — the same message the config
    // writer would produce (F-20).
    assert!(
        app.status_bar.text.contains("invalid agent name"),
        "the status bar must explain the rejection: {}",
        app.status_bar.text
    );
}

#[test]
fn ctrl_n_with_existing_agent_jumps_to_that_row_for_editing() {
    let mut app = make_app();
    let _rx = setup_command_dialog(
        &mut app,
        config_show_dialog(map_rows(serde_json::json!({
            "demo": { "map": { "claude": ["claude-opus-4-8"] } }
        }))),
    );

    press_key(&mut app, KeyCode::Right, KeyModifiers::NONE); // the mapping is repo
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

// ─── ConfigShow guidance rows (WI-0099) ──────────────────────────────────

#[test]
fn enter_on_guidance_summary_row_points_at_ctrl_n() {
    let mut app = make_app();
    let _rx = setup_command_dialog(
        &mut app,
        config_show_dialog(vec![config_row(
            "demo.list",
            "",
            "2 entries",
            true,
            false,
            false,
            ConfigRowShape::ListHeader {
                child_prefix: "demo.list.",
            },
        )]),
    );

    press_key(&mut app, KeyCode::Enter, KeyModifiers::NONE);

    assert_eq!(
        app.status_bar.text, "Press Ctrl+N to add a guidance entry, or edit a per-entry row below",
        "the guidance summary row must steer users to Ctrl+N / per-entry rows"
    );
    assert!(app.active_dialog.is_some(), "dialog must stay open");
}

#[test]
fn enter_on_guidance_row_starts_inline_edit_in_repo_column() {
    let mut app = make_app();
    let _rx = setup_command_dialog(
        &mut app,
        config_show_dialog(vec![config_row(
            "demo.list.0",
            "",
            "never spawn more than two agents in parallel",
            false,
            false,
            true,
            ConfigRowShape::ListEntry,
        )]),
    );

    press_key(&mut app, KeyCode::Enter, KeyModifiers::NONE);

    let Some(Dialog::ConfigShow(state)) = &app.active_dialog else {
        panic!("dialog must stay open in edit mode");
    };
    assert!(state.editing, "guidance rows must be inline-editable");
    assert_eq!(state.edit_column, 1, "edit must snap to the Repo column");
    assert_eq!(
        state.editor.text, "never spawn more than two agents in parallel",
        "the editor must be seeded with the current entry text"
    );
}

#[test]
fn enter_while_editing_guidance_entry_sends_response() {
    let mut app = make_app();
    let rx = setup_command_dialog(
        &mut app,
        config_show_dialog(vec![config_row(
            "demo.list.0",
            "",
            "old instruction",
            false,
            false,
            true,
            ConfigRowShape::ListEntry,
        )]),
    );

    press_key(&mut app, KeyCode::Enter, KeyModifiers::NONE); // start editing
    if let Some(Dialog::ConfigShow(state)) = &mut app.active_dialog {
        state.editor.set_text("new instruction");
    }
    press_key(&mut app, KeyCode::Enter, KeyModifiers::NONE); // save

    let resp = rx.try_recv().expect("save must send a dialog response");
    match resp {
        DialogResponse::ConfigEdit(edit) => assert_eq!(
            edit,
            ConfigEditRequest::to_field("demo.list.0", "new instruction", false),
            "editing an existing entry must send the field\\tvalue\\trepo response"
        ),
        other => panic!("expected ConfigEdit response, got {other:?}"),
    }
}

#[test]
fn enter_while_editing_guidance_entry_to_empty_value_sends_removal_response() {
    let mut app = make_app();
    let rx = setup_command_dialog(
        &mut app,
        config_show_dialog(vec![config_row(
            "demo.list.0",
            "",
            "never spawn more than two agents in parallel",
            false,
            false,
            true,
            ConfigRowShape::ListEntry,
        )]),
    );

    press_key(&mut app, KeyCode::Enter, KeyModifiers::NONE); // start editing
    if let Some(Dialog::ConfigShow(state)) = &mut app.active_dialog {
        state.editor.set_text("");
    }
    press_key(&mut app, KeyCode::Enter, KeyModifiers::NONE); // save

    let resp = rx.try_recv().expect("save must send a dialog response");
    match resp {
        DialogResponse::ConfigEdit(edit) => assert_eq!(
            edit,
            ConfigEditRequest::to_field("demo.list.0", "", false),
            "an empty value must be sent through unfiltered; the config layer coerces \
             empty guidance values to removal"
        ),
        other => panic!("expected ConfigEdit response, got {other:?}"),
    }
}

#[test]
fn ctrl_n_on_guidance_row_starts_guidance_entry_phase() {
    use crate::frontend::tui::dialogs::NewMapEntryPhase;

    let mut app = make_app();
    let _rx = setup_command_dialog(
        &mut app,
        config_show_dialog(vec![config_row(
            "demo.list",
            "",
            "(none)",
            true,
            false,
            false,
            ConfigRowShape::ListHeader {
                child_prefix: "demo.list.",
            },
        )]),
    );

    press_key(&mut app, KeyCode::Char('n'), KeyModifiers::CONTROL);

    let Some(Dialog::ConfigShow(state)) = &app.active_dialog else {
        panic!("dialog must stay open");
    };
    assert_eq!(
        state.new_entry,
        Some(NewMapEntryPhase::ListEntry),
        "Ctrl+N on a guidance row must start the single-phase guidance entry flow"
    );
    assert!(state.editing, "text input must route to the inline editor");
    assert_eq!(
        state.edit_column, 0,
        "Ctrl+N keeps the selected column; the dialog opens on Global"
    );
}

#[test]
fn ctrl_n_guidance_flow_sends_repo_scoped_append_response() {
    let mut app = make_app();
    let rx = setup_command_dialog(
        &mut app,
        config_show_dialog(guidance_rows(serde_json::json!({
            "demo": { "list": ["first entry", "second entry"] }
        }))),
    );

    press_key(&mut app, KeyCode::Right, KeyModifiers::NONE); // Repo column
    press_key(&mut app, KeyCode::Char('n'), KeyModifiers::CONTROL);
    for c in "third entry".chars() {
        press_char(&mut app, c);
    }
    press_key(&mut app, KeyCode::Enter, KeyModifiers::NONE); // save

    let resp = rx.try_recv().expect("saving the new entry must respond");
    match resp {
        DialogResponse::ConfigEdit(edit) => assert_eq!(
            edit,
            ConfigEditRequest::to_field("demo.list.2", "third entry", false),
            "the new entry must append at the current entry count"
        ),
        other => panic!("expected ConfigEdit response, got {other:?}"),
    }
}

#[test]
fn ctrl_n_guidance_flow_empty_value_shows_toast_and_stays_in_phase() {
    use crate::frontend::tui::dialogs::NewMapEntryPhase;

    let mut app = make_app();
    let _rx = setup_command_dialog(
        &mut app,
        config_show_dialog(vec![config_row(
            "demo.list",
            "",
            "(none)",
            true,
            false,
            false,
            ConfigRowShape::ListHeader {
                child_prefix: "demo.list.",
            },
        )]),
    );

    press_key(&mut app, KeyCode::Char('n'), KeyModifiers::CONTROL);
    press_key(&mut app, KeyCode::Enter, KeyModifiers::NONE); // submit with no text

    let Some(Dialog::ConfigShow(state)) = &app.active_dialog else {
        panic!("dialog must stay open");
    };
    assert_eq!(
        state.new_entry,
        Some(NewMapEntryPhase::ListEntry),
        "an empty entry must keep the flow open rather than sending a response"
    );
    assert!(
        app.status_bar.text.contains("Enter a value"),
        "the status bar must prompt for input: {}",
        app.status_bar.text
    );
}

#[test]
fn ctrl_n_on_map_row_starts_agent_key_flow() {
    use crate::frontend::tui::dialogs::NewMapEntryPhase;

    let mut app = make_app();
    let _rx = setup_command_dialog(
        &mut app,
        config_show_dialog(map_rows(serde_json::json!({}))),
    );

    press_key(&mut app, KeyCode::Char('n'), KeyModifiers::CONTROL);

    let Some(Dialog::ConfigShow(state)) = &app.active_dialog else {
        panic!("dialog must stay open");
    };
    assert_eq!(
        state.new_entry,
        Some(NewMapEntryPhase::Key),
        "Ctrl+N on a map row must start the two-phase flow"
    );
}

// ─── Ctrl+N writes to the selected column (Global by default) ─────────────

#[test]
fn ctrl_n_in_the_global_column_sends_global_map_and_list_edits() {
    let mut app = make_app();
    let rx = setup_command_dialog(
        &mut app,
        config_show_dialog(map_rows(serde_json::json!({}))),
    );
    press_key(&mut app, KeyCode::Char('n'), KeyModifiers::CONTROL);
    for c in "maki".chars() {
        press_char(&mut app, c);
    }
    press_key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    for c in "model-a".chars() {
        press_char(&mut app, c);
    }
    press_key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    match rx.try_recv().expect("saving the mapping must respond") {
        DialogResponse::ConfigEdit(edit) => assert_eq!(
            edit,
            ConfigEditRequest::to_field("demo.map.maki", "model-a", true),
            "the dialog opens on Global, so the new mapping is global"
        ),
        other => panic!("expected ConfigEdit response, got {other:?}"),
    }

    // The repo list has two entries, the global one none: a global append
    // goes at global index 0, not after the repo entries.
    let mut app = make_app();
    let rx = setup_command_dialog(
        &mut app,
        config_show_dialog(guidance_rows(serde_json::json!({
            "demo": { "list": ["first entry", "second entry"] }
        }))),
    );
    press_key(&mut app, KeyCode::Char('n'), KeyModifiers::CONTROL);
    for c in "global entry".chars() {
        press_char(&mut app, c);
    }
    press_key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    match rx.try_recv().expect("saving the entry must respond") {
        DialogResponse::ConfigEdit(edit) => assert_eq!(
            edit,
            ConfigEditRequest::to_field("demo.list.0", "global entry", true),
        ),
        other => panic!("expected ConfigEdit response, got {other:?}"),
    }
}
