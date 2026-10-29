//! WI 0149 presentation and input contracts.
use super::*;
use crate::command::commands::workflow_prerequisites::*;
use crate::data::workflow_prerequisites::*;
use crate::frontend::tui::dialogs::{DialogRequest, ReviewBody, ReviewState, SecretBuffer};

fn report() -> WorkflowPrerequisiteReport {
    WorkflowPrerequisiteReport {
        phase: PrerequisitePhase::Start,
        origin: PrerequisiteOrigin::Static,
        checks: vec![PrerequisiteCheck {
            prerequisite: WorkflowPrerequisite::GitHubToken {
                host: "github.com".into(),
            },
            status: PrerequisiteStatus::Missing,
            explanation: "No token".into(),
            needed_by: vec![],
            remediations: vec![],
        }],
        advisories: vec![],
        notes: vec![],
    }
}
fn questions() -> PrerequisiteReportNext {
    PrerequisiteReportNext::Questions {
        unresolved: crate::data::config::workflows::UnresolvedMode::Warn,
    }
}
fn render(app: &mut App) -> String {
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 35)).unwrap();
    terminal
        .draw(|f| crate::frontend::tui::render::render_frame(app, f))
        .unwrap();
    terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|c| c.symbol())
        .collect()
}
/// Render at `w`×`h` and return the screen as rows.
fn screen(app: &mut App, w: u16, h: u16) -> Vec<String> {
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
    terminal
        .draw(|f| crate::frontend::tui::render::render_frame(app, f))
        .unwrap();
    let buffer = terminal.backend().buffer();
    (0..h)
        .map(|y| (0..w).map(|x| buffer[(x, y)].symbol()).collect())
        .collect()
}

/// A report with long text in every column, as a real dynamic run has.
fn busy_report() -> WorkflowPrerequisiteReport {
    let check = |prerequisite, status, explanation: &str| PrerequisiteCheck {
        prerequisite,
        status,
        explanation: explanation.into(),
        needed_by: vec![],
        remediations: vec![],
    };
    WorkflowPrerequisiteReport {
        phase: PrerequisitePhase::Start,
        origin: PrerequisiteOrigin::AssumedAll,
        checks: vec![
            check(
                WorkflowPrerequisite::GitIdentity,
                PrerequisiteStatus::Satisfied,
                "Ada Lovelace <ada@example.com> from the host's global git config",
            ),
            check(
                WorkflowPrerequisite::PushAuthSsh {
                    remote: "origin".into(),
                    host: "github.com".into(),
                },
                PrerequisiteStatus::NeedsConsent,
                "push_branch pushes to github.com:example/project.git over SSH; three keys \
                 were found in ~/.ssh and none is allowed yet",
            ),
            check(
                WorkflowPrerequisite::GitHubToken {
                    host: "github.com".into(),
                },
                PrerequisiteStatus::Missing,
                "No token in GH_TOKEN or GITHUB_TOKEN, and gh is not logged in to github.com",
            ),
        ],
        advisories: vec![],
        notes: vec!["repo config mounts ~/.ssh into every step".into()],
    }
}

/// Ask the real frontend to show `report` and put its request on screen.
fn open_report(
    app: &mut App,
    report: WorkflowPrerequisiteReport,
    next: PrerequisiteReportNext,
) -> std::sync::mpsc::Receiver<DialogResponse> {
    let (mut frontend, requests, _responses) =
        test_command_frontend(&["exec", "workflow"], Default::default());
    std::thread::spawn(move || {
        let _ = frontend.show_workflow_prerequisites(&report, next);
    });
    let DialogRequest::Review(state) = requests
        .recv_timeout(std::time::Duration::from_secs(3))
        .unwrap()
    else {
        panic!("expected the report review");
    };
    setup_command_dialog(app, Dialog::Review(state))
}

#[test]
fn prerequisites_modal_lays_out_one_entry_per_check_without_column_wrapping() {
    let mut app = make_app();
    let _rx = open_report(&mut app, busy_report(), questions());
    let rows = screen(&mut app, 120, 50);
    let text = rows.join("\n");
    // The summary is the title; needing attention comes first, each check's
    // name and status share its first row, and the detail is under it.
    assert!(
        text.contains("2 of 3 workflow prerequisites need your attention"),
        "{text}"
    );
    let row_of = |needle: &str| {
        rows.iter()
            .position(|r| r.contains(needle))
            .unwrap_or_else(|| panic!("{needle:?} not on screen:\n{text}"))
    };
    let token = row_of("✗ GitHub token (github.com)");
    assert!(rows[token].contains("missing │"), "{}", rows[token]);
    assert!(row_of("No token in GH_TOKEN") == token + 1, "{text}");
    assert!(row_of("! Push over SSH (github.com)") > token);
    assert!(row_of("✓ Git identity") > row_of("! Push over SSH"));
    assert!(!text.contains("needed by: any teardown step"), "{text}");
    assert!(
        text.contains("• repo config mounts ~/.ssh into every step"),
        "{text}"
    );
    // What continuing does is pinned beside the choices.
    let next = row_of("one at a time");
    assert!(text.contains("does not stop the run"), "{text}");
    assert!(text.contains("dynamic workflow"), "{text}");
    assert!(row_of("[c] Continue to the questions") > next);
    assert!(text.contains("[a] Abort the run"));
    assert!(text.contains("[Esc] Continue to the questions"), "{text}");
}

#[test]
fn prerequisites_modal_keeps_its_choices_on_a_small_terminal_and_scrolls() {
    let mut app = make_app();
    let rx = open_report(&mut app, busy_report(), questions());
    let text = screen(&mut app, 70, 24).join("\n");
    assert!(text.contains("[c] Continue to the questions"), "{text}");
    assert!(text.contains("[Esc] Continue"), "{text}");
    assert!(text.contains("▼ more below"), "{text}");
    for _ in 0..40 {
        press_key(&mut app, KeyCode::Down, KeyModifiers::NONE);
    }
    let text = screen(&mut app, 70, 24).join("\n");
    assert!(text.contains("end of review"), "{text}");
    press_char(&mut app, 'a');
    assert!(matches!(rx.try_recv().unwrap(), DialogResponse::Char('a')));
}

#[test]
fn check_only_report_offers_close_not_continue() {
    let mut app = make_app();
    let _rx = open_report(&mut app, busy_report(), PrerequisiteReportNext::CheckOnly);
    let text = screen(&mut app, 120, 50).join("\n");
    assert!(text.contains("[c] Close"), "{text}");
    assert!(text.contains("no workflow ran"), "{text}");
    assert!(!text.contains("Continue"), "{text}");
    assert!(!text.contains("Abort"), "{text}");
}

#[test]
fn config_migrate_preview_scrolls_and_keeps_its_choices_visible() {
    let mut app = make_app();
    // A long, nested preview; the field names are not the point.
    let before = serde_json::json!({
        "oldKey": {
            "list": (0..30).map(|i| format!("entry {i}")).collect::<Vec<_>>(),
            "map": {"a": "1", "b": "2"}
        }
    });
    let preview = format!(
        "/repo/.awman/config.json\n\nMove oldKey\nBefore:\n{}\nAfter:\n{}",
        serde_json::to_string_pretty(&before).unwrap(),
        serde_json::to_string_pretty(&before).unwrap()
    );
    let prompt = crate::command::prompts::config_migrate_confirm(&preview);
    let (mut frontend, requests, responses) =
        test_command_frontend(&["config", "migrate"], Default::default());
    let worker = std::thread::spawn(move || {
        use crate::command::commands::config::ConfigCommandFrontend;
        frontend.confirm_config_migration(&prompt).unwrap()
    });
    let DialogRequest::Review(state) = requests
        .recv_timeout(std::time::Duration::from_secs(3))
        .unwrap()
    else {
        panic!("expected a scrolling review");
    };
    let _rx = setup_command_dialog(&mut app, Dialog::Review(state));
    let rows = screen(&mut app, 100, 30);
    let text = rows.join("\n");
    // The dialog fits the terminal, the warning is read first and the
    // choices are on screen however long the preview is.
    assert!(text.contains("Teammates on awman versions"), "{text}");
    assert!(text.contains("[y] Move it"), "{text}");
    assert!(text.contains("[n] Cancel"), "{text}");
    assert!(text.contains("[Esc] Cancel"), "{text}");
    assert!(text.contains("▼ more below"), "{text}");
    responses.send(DialogResponse::Dismissed).unwrap();
    assert_eq!(
        worker.join().unwrap(),
        crate::command::commands::config::MigrateChoice::Cancel
    );
}
#[test]
fn wi0149_secret_input_masks_unicode_and_debug_and_submits_secret() {
    let mut app = make_app();
    let mut buffer = SecretBuffer::default();
    for ch in "ghp_秘密_TOKEN".chars() {
        buffer.insert_char(ch);
    }
    assert!(!format!("{buffer:?}").contains("TOKEN"));
    let rx = setup_command_dialog(
        &mut app,
        Dialog::SecretInput {
            title: "Paste a GitHub token".into(),
            prompt: "Input hidden".into(),
            buffer,
        },
    );
    let text = render(&mut app);
    assert!(text.contains(&"•".repeat("ghp_秘密_TOKEN".chars().count())));
    assert!(!text.contains("TOKEN"));
    assert!(!text.contains("秘密"));
    press_key(&mut app, KeyCode::Backspace, KeyModifiers::NONE);
    press_char(&mut app, 'X');
    press_key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    match rx.try_recv().unwrap() {
        DialogResponse::Secret(secret) => assert_eq!(secret.expose(), "ghp_秘密_TOKEX"),
        other => panic!("{other:?}"),
    }
    assert!(app.active_dialog.is_none());
}
#[test]
fn wi0149_secret_input_escape_discards_value() {
    for (code, modifiers) in [
        (KeyCode::Esc, KeyModifiers::NONE),
        (KeyCode::Char('c'), KeyModifiers::CONTROL),
    ] {
        let mut app = make_app();
        let rx = setup_command_dialog(
            &mut app,
            Dialog::SecretInput {
                title: "Token".into(),
                prompt: "Hidden".into(),
                buffer: SecretBuffer::default(),
            },
        );
        press_char(&mut app, 'x');
        press_key(&mut app, code, modifiers);
        assert!(matches!(rx.try_recv().unwrap(), DialogResponse::Dismissed));
        assert!(app.active_dialog.is_none());
    }
}
#[test]
fn wi0149_tui_prerequisite_prompt_preserves_command_labels_and_default() {
    let check = report().checks.remove(0);
    let prompt = crate::command::prompts::prerequisite_token_consent(&check, "GH_TOKEN");
    let (mut frontend, requests, responses) =
        test_command_frontend(&["exec", "workflow"], Default::default());
    let expected = prompt.clone();
    let worker =
        std::thread::spawn(move || frontend.ask_prerequisite_token_consent(&prompt).unwrap());
    match requests
        .recv_timeout(std::time::Duration::from_secs(3))
        .unwrap()
    {
        DialogRequest::Review(state) => {
            assert_eq!(state.title, expected.title);
            assert!(matches!(&state.body, ReviewBody::Prose(body) if *body == expected.body));
            assert_eq!(state.dismiss_label.as_deref(), Some("No"));
            assert_eq!(
                state.keys,
                expected
                    .choices
                    .iter()
                    .map(|c| (c.key, c.label.clone()))
                    .collect::<Vec<_>>()
            );
        }
        other => panic!("unexpected dialog: {other:?}"),
    }
    responses.send(DialogResponse::Dismissed).unwrap();
    assert_eq!(worker.join().unwrap(), expected.default_on_dismiss.unwrap());
}
#[test]
fn wi0149_tui_report_abort_returns_130_and_secret_request_is_typed() {
    let (mut frontend, requests, responses) =
        test_command_frontend(&["exec", "workflow"], Default::default());
    let worker = std::thread::spawn(move || {
        let err = frontend
            .show_workflow_prerequisites(&report(), questions())
            .unwrap_err();
        assert_eq!(err.exit_code(), 130);
        frontend
            .ask_prerequisite_secret(&crate::command::prompts::prerequisite_token_paste())
            .unwrap()
    });
    assert!(matches!(
        requests
            .recv_timeout(std::time::Duration::from_secs(3))
            .unwrap(),
        DialogRequest::Review { .. }
    ));
    responses.send(DialogResponse::Char('a')).unwrap();
    assert!(matches!(
        requests
            .recv_timeout(std::time::Duration::from_secs(3))
            .unwrap(),
        DialogRequest::SecretInput { .. }
    ));
    responses.send(DialogResponse::Dismissed).unwrap();
    assert!(worker.join().unwrap().is_none());
}

#[test]
fn wi0149_tui_terminal_release_restore_occurs_even_when_host_command_fails() {
    use crate::engine::auth::host_secret_command::HostSecretCommandError;
    let (mut frontend, requests, responses) =
        test_command_frontend(&["exec", "workflow"], Default::default());
    let worker = std::thread::spawn(move || {
        frontend.with_terminal_released("Host login", &mut || {
            Err(HostSecretCommandError::EmptyOutput {
                label: "fake".into(),
            })
        })
    });
    assert!(
        matches!(requests.recv_timeout(std::time::Duration::from_secs(3)).unwrap(), DialogRequest::TerminalRelease { title } if title == "Host login")
    );
    responses.send(DialogResponse::Yes).unwrap();
    assert!(matches!(
        requests
            .recv_timeout(std::time::Duration::from_secs(3))
            .unwrap(),
        DialogRequest::TerminalRestore
    ));
    responses.send(DialogResponse::Dismissed).unwrap();
    assert!(worker.join().unwrap().is_err());
}

#[test]
fn wi0149_secret_input_ctrl_c_discards_value() {
    let mut app = make_app();
    let rx = setup_command_dialog(
        &mut app,
        Dialog::SecretInput {
            title: "Token".into(),
            prompt: "Hidden".into(),
            buffer: SecretBuffer::default(),
        },
    );
    press_char(&mut app, 'x');
    press_key(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
    assert!(matches!(rx.try_recv().unwrap(), DialogResponse::Dismissed));
    assert!(app.active_dialog.is_none());
}

#[test]
fn wi0149_every_tui_prerequisite_choice_uses_command_labels_keys_and_defaults() {
    use crate::data::prompt::Prompt;
    use crate::frontend::tui::command_frontend::TuiCommandFrontend;
    fn verify<D: Clone + std::fmt::Debug + PartialEq + Send + 'static>(
        prompt: Prompt<D>,
        ask: impl FnOnce(
                &mut TuiCommandFrontend,
                &Prompt<D>,
            ) -> Result<D, crate::command::error::CommandError>
            + Send
            + 'static,
    ) {
        let (mut frontend, requests, responses) =
            test_command_frontend(&["exec", "workflow"], Default::default());
        let expected = prompt.clone();
        let worker = std::thread::spawn(move || ask(&mut frontend, &prompt).unwrap());
        let DialogRequest::Review(state) = requests
            .recv_timeout(std::time::Duration::from_secs(3))
            .unwrap()
        else {
            panic!("expected keyed prompt");
        };
        assert_eq!(state.title, expected.title);
        assert!(matches!(&state.body, ReviewBody::Prose(body) if *body == expected.body));
        assert!(state.gated_keys.is_empty());
        assert_eq!(
            state.keys,
            expected
                .choices
                .iter()
                .map(|c| (c.key, c.label.clone()))
                .collect::<Vec<_>>()
        );
        responses.send(DialogResponse::Dismissed).unwrap();
        assert_eq!(worker.join().unwrap(), expected.default_on_dismiss.unwrap());
    }
    let check = report().checks.remove(0);
    verify(
        crate::command::prompts::prerequisite_identity(&check),
        |f, p| f.ask_prerequisite_identity(p),
    );
    verify(
        crate::command::prompts::prerequisite_identity_persist("git config --local"),
        |f, p| f.ask_prerequisite_identity_persist(p),
    );
    verify(
        crate::command::prompts::prerequisite_ssh(&check, "github.com:org/repo"),
        |f, p| f.ask_prerequisite_ssh(p),
    );
    verify(
        crate::command::prompts::prerequisite_token_consent(&check, "GH_TOKEN"),
        |f, p| f.ask_prerequisite_token_consent(p),
    );
    verify(
        crate::command::prompts::prerequisite_token_source(&check),
        |f, p| f.ask_prerequisite_token_source(p),
    );
    verify(
        crate::command::prompts::prerequisite_missing_tool(&check),
        |f, p| f.ask_prerequisite_tool(p),
    );
    verify(crate::command::prompts::prerequisite_gap(&check), |f, p| {
        f.ask_prerequisite_gap(p)
    });
}

#[test]
fn terminal_release_is_restored_during_unwind() {
    let (mut frontend, requests, responses) =
        test_command_frontend(&["exec", "workflow"], Default::default());
    let worker = std::thread::spawn(move || {
        let _ = frontend.with_terminal_released("Host login", &mut || panic!("fixture panic"));
    });
    assert!(matches!(
        requests
            .recv_timeout(std::time::Duration::from_secs(3))
            .unwrap(),
        DialogRequest::TerminalRelease { .. }
    ));
    responses.send(DialogResponse::Yes).unwrap();
    assert!(matches!(
        requests
            .recv_timeout(std::time::Duration::from_secs(3))
            .unwrap(),
        DialogRequest::TerminalRestore
    ));
    responses.send(DialogResponse::Dismissed).unwrap();
    assert!(worker.join().is_err());
}

#[test]
fn summary_ctrl_c_is_an_interrupt_and_expired_yolo_secret_is_discarded() {
    let mut app = make_app();
    let prompt = crate::command::prompts::prerequisite_report(&report(), questions());
    let rx = setup_command_dialog(
        &mut app,
        Dialog::Review(ReviewState::new(
            prompt.title,
            prompt.body,
            prompt
                .choices
                .into_iter()
                .map(|c| (c.key, c.label))
                .collect(),
            vec![],
        )),
    );
    press_key(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
    assert!(matches!(
        rx.try_recv().unwrap(),
        DialogResponse::Interrupted
    ));
    let rx = setup_command_dialog(
        &mut app,
        Dialog::SecretInput {
            title: "Token".into(),
            prompt: "Hidden".into(),
            buffer: SecretBuffer::default(),
        },
    );
    press_char(&mut app, 'x');
    app.command_dialog_deadline = Some(std::time::Instant::now());
    app.tick_all_tabs();
    assert!(matches!(rx.try_recv().unwrap(), DialogResponse::Dismissed));
    assert!(app.active_dialog.is_none());
}
