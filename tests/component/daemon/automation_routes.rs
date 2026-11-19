//! WI 0070 owner-local automation routes and API chat admission through the
//! production router and typed Layer 2 operations. No provider is launched.
use super::support::Api;
use crate::support;
use awman::command::commands::api_server::admission::{AdmissionError, CommandSubmission};
use awman::command::daemon_api::{keys, worker};
use awman::command::error::CommandError;
use awman::data::config::daemon::Interaction;
use awman::data::daemon_state::{CommandListener, CommandRequestContext};
use awman::data::fs::api_labels::Labels;
use awman::data::ids::{CommandId, WorkerSessionId};
use serde_json::json;

fn worker_config() -> serde_json::Value {
    json!({"daemon":{"features":{"worker":{"enabled":true,"push":{"enabled":true}}},
        "network":{"listeners":{"component":{"features":["engine","worker"]}}}}})
}

fn submission(args: &[&str], interaction: Interaction) -> CommandSubmission {
    CommandSubmission {
        secrets: None,
        allow_remote_input: false,
        interaction: Some(interaction),
        subcommand: "chat".into(),
        args: args.iter().map(|s| (*s).to_owned()).collect(),
        labels: Labels::empty(),
        work_item: None,
    }
}

fn context(listener: CommandListener, owner: bool) -> CommandRequestContext {
    CommandRequestContext {
        profile_owner: owner,
        listener_name: None,
        listener,
        trace_parent: None,
    }
}

fn code(error: &AdmissionError) -> String {
    match error {
        AdmissionError::CommandValidation(e) | AdmissionError::CommandAdmission(e) => {
            format!("{}:{}", e.http_status(), e.code())
        }
        other => format!("{other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn owner_only_routes_answer_403_on_the_network_and_typed_operations_serve_the_owner() {
    let sandbox = support::sandbox(
        "daemon::automation_routes::owner_only_routes_answer_403_on_the_network_and_typed_operations_serve_the_owner",
    );
    let server = Api::with_config(&sandbox, worker_config()).await;
    let key = keys::keys_create(&server.api, "worker").unwrap();
    sandbox.redact(&key.key);
    let session = WorkerSessionId::new();
    let command = CommandId::new();
    // A known owner-only operation is 403 on the network, not a disguised 404.
    for path in [
        format!("/v1/worker/commands/{}/turns", command.as_str()),
        format!(
            "/v1/worker/sessions/{}/agent-capabilities",
            session.as_str()
        ),
    ] {
        let (status, body) = server.get(&path, Some(&key.key)).await;
        assert_eq!(status, 403, "{path}: {body}");
        assert_eq!(body["code"], "forbidden", "{body}");
    }
    // The typed operations HTTP delegates to.
    let api = server.api.worker.as_ref().unwrap();
    api.store
        .insert_session(
            session.as_str(),
            "/fixture",
            &chrono::Utc::now().to_rfc3339(),
        )
        .unwrap();
    api.store
        .insert_running_lease_command(command.as_str(), session.as_str(), "chat", "[]", false)
        .unwrap();
    let page = worker::commands_turns(api, &command, &Default::default())
        .unwrap()
        .unwrap();
    assert!(page.items.is_empty());
    assert_eq!(page.next_cursor, None);
    assert!(
        worker::commands_turns(api, &CommandId::new(), &Default::default())
            .unwrap()
            .is_none()
    );
    let bad = awman::data::wire::ListQuery {
        limit: None,
        cursor: Some("not-a-cursor".into()),
    };
    assert!(worker::commands_turns(api, &command, &bad).is_err());
    // An unknown session has no capability list (404 over HTTP).
    assert!(
        worker::sessions_agent_capabilities(api, &WorkerSessionId::new())
            .await
            .is_none()
    );
    // Status advertises the working automation features.
    let (_, status) = server.get("/v1/status", Some(&key.key)).await;
    let caps = status["capabilities"].to_string();
    for name in [
        "agent_capabilities",
        "agent_automation",
        "terminal_turns",
        "automation_results",
        "agent_turn_records",
        "agent_state",
        "agent_identity",
        "agent_turn_started",
        "agent_turn_completed",
    ] {
        assert!(caps.contains(&format!("\"{name}\"")), "{name}: {caps}");
    }
    server.shutdown().await;
    sandbox.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_chat_is_an_owner_local_interactive_automation_terminal_only() {
    let sandbox = support::sandbox(
        "daemon::automation_routes::api_chat_is_an_owner_local_interactive_automation_terminal_only",
    );
    let server = Api::with_config(&sandbox, worker_config()).await;
    let api = server.api.worker.as_ref().unwrap();
    let session = WorkerSessionId::new();
    api.store
        .insert_session(
            session.as_str(),
            "/fixture",
            &chrono::Utc::now().to_rfc3339(),
        )
        .unwrap();
    let full = [
        "--agent",
        "codex",
        "--settings-profile",
        "review",
        "--automation",
    ];
    // Network (even the owner principal over a network listener): 403.
    let error = worker::commands_create(
        api,
        context(CommandListener::Network, false),
        &session,
        submission(&full, Interaction::Interactive),
        None,
    )
    .await
    .err()
    .expect("refused");
    assert_eq!(code(&error), "403:forbidden");
    let local = || context(CommandListener::Local, true);
    // Missing --automation or headless: 400 invalid_request.
    let error = worker::commands_create(
        api,
        local(),
        &session,
        submission(
            &["--agent", "codex", "--settings-profile", "review"],
            Interaction::Interactive,
        ),
        None,
    )
    .await
    .err()
    .expect("refused");
    assert_eq!(code(&error), "400:invalid_request");
    let error = worker::commands_create(
        api,
        local(),
        &session,
        submission(&full, Interaction::Headless),
        None,
    )
    .await
    .err()
    .expect("refused");
    assert_eq!(code(&error), "400:invalid_request");
    // --automation without --settings-profile is refused too.
    let error = worker::commands_create(
        api,
        local(),
        &session,
        submission(
            &["--agent", "codex", "--automation"],
            Interaction::Interactive,
        ),
        None,
    )
    .await
    .err()
    .expect("refused");
    assert_eq!(code(&error), "400:invalid_request");
    // Other commands keep their availability.
    assert!(!matches!(
        worker::commands_create(
            api,
            local(),
            &session,
            CommandSubmission {
                subcommand: "status".into(),
                ..submission(&[], Interaction::Headless)
            },
            None,
        )
        .await,
        Err(AdmissionError::CommandValidation(
            CommandError::InvalidRequest(_)
        ))
    ));
    server.shutdown().await;
    sandbox.finish();
}
