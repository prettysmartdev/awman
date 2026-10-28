//! WI 0149 presentation and input contracts.
use super::*;
use crate::command::commands::workflow_prerequisites::*;
use crate::data::workflow_prerequisites::*;
use crate::frontend::tui::dialogs::{DialogRequest, ReviewState, SecretBuffer};

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
#[test]
fn wi0149_prerequisites_modal_renders_summary_gap_and_actions() {
    let mut app = make_app();
    let report = report();
    app.active_dialog = Some(Dialog::Review(ReviewState::new(
        report.summary_line(),
        report.render_lines().join("\n"),
        vec![('c', "Continue".into()), ('a', "Abort the workflow".into())],
        vec![],
    )));
    let text = render(&mut app);
    assert!(text.contains(&report.summary_line()), "{text}");
    press_key(&mut app, KeyCode::Down, KeyModifiers::NONE);
    assert!(render(&mut app).contains("No token"));
    assert!(text.contains("Continue"));
    assert!(text.contains("Abort the workflow"));
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
        DialogRequest::Custom {
            title,
            body,
            keys: choices,
        } => {
            assert_eq!(title, expected.title);
            assert_eq!(body, expected.body);
            assert_eq!(
                choices,
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
        let err = frontend.show_workflow_prerequisites(&report()).unwrap_err();
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
        let DialogRequest::Custom { title, body, keys } = requests
            .recv_timeout(std::time::Duration::from_secs(3))
            .unwrap()
        else {
            panic!("expected keyed prompt");
        };
        assert_eq!(title, expected.title);
        assert_eq!(body, expected.body);
        assert_eq!(
            keys,
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
    let prompt = crate::command::prompts::prerequisite_report(&report());
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
