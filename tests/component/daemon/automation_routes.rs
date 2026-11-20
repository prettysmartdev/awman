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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restart_publishes_committed_automation_once_and_never_requeues_input() {
    use awman::command::automation::{
        controller::AutomationHost, launch::HeadlessRun, results::HeadlessObservation,
    };
    use awman::command::commands::api_server::event_bus::EventBus;
    use awman::data::automation::{AutomationState, StatusSource, StatusSupport};
    use awman::data::automation_records::{AutomationSnapshot, ExecutionProfileBinding};
    use awman::data::automation_store::AutomationStore;
    use awman::data::execution_event::{EventPayload, ExecutionEvent};
    use awman::data::fs::api_command_log::{EventLog, OrderedCommandLog};
    use awman::data::fs::api_db::CommandResult;
    use awman::data::ids::ExecutionId;
    use awman::data::settings_profile::{NativeSessionId, SettingsProfileName};
    struct FlushFault {
        log: OrderedCommandLog,
        terminal: bool,
    }
    impl EventLog for FlushFault {
        fn append(&mut self, event: &ExecutionEvent) -> std::io::Result<()> {
            self.terminal |= matches!(event.payload, EventPayload::AgentTurnCompleted { .. });
            self.log.append(event)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            if self.terminal {
                Err(std::io::Error::other("completion flush failed"))
            } else {
                self.log.flush()
            }
        }
    }
    let sandbox=support::sandbox("daemon::automation_routes::restart_publishes_committed_automation_once_and_never_requeues_input");
    let server = Api::with_config(&sandbox, worker_config()).await;
    let api = server.api.worker.as_ref().unwrap();
    let db = api.store.clone();
    let paths = api.paths.clone();
    // Stop the fixture before reserving the interrupted execution: an orderly
    // StopMode::Now shutdown cancels running commands, unlike process loss.
    // The production controller then leaves its committed state in this same
    // store for the next daemon lifetime to recover.
    server.shutdown().await;
    let session = WorkerSessionId::new();
    let command = CommandId::new();
    db.insert_session(
        session.as_str(),
        sandbox.work().to_str().unwrap(),
        &chrono::Utc::now().to_rfc3339(),
    )
    .unwrap();
    db.insert_running_lease_command(
        command.as_str(),
        session.as_str(),
        "exec prompt",
        "[]",
        false,
    )
    .unwrap();
    let store = AutomationStore::open(db.clone()).unwrap();
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../../web/test/lib/fixtures/terminal-frames.json"
    ))
    .unwrap();
    let mut snapshot: AutomationSnapshot =
        serde_json::from_value(fixture["automation_examples"]["snapshot"].clone()).unwrap();
    snapshot.execution_id = ExecutionId::new();
    snapshot.state = AutomationState::Starting;
    snapshot.revision = 0;
    snapshot.active_turn_id = None;
    snapshot.native_session_id = None;
    snapshot.status_support = StatusSupport::None;
    snapshot.status_source = StatusSource::None;
    let binding = ExecutionProfileBinding {
        repository_key: "123456789abcdef0".into(),
        root_sha256: "123456789abcdef0".repeat(4),
        provider: snapshot.provider.clone(),
        name: SettingsProfileName::parse("fixture").unwrap(),
    };
    let bus = EventBus::new(16);
    bus.set_log_writer(Ok(Box::new(FlushFault {
        log: OrderedCommandLog::open(&paths, session.as_str(), command.as_str()).unwrap(),
        terminal: false,
    })));
    let mut run = HeadlessRun::reserve(
        AutomationHost {
            store: store.clone(),
            command_id: command.clone(),
            attempt: 1,
            owner_key: "uid:test:restart".into(),
            events: Some(bus.sender().lifecycle_publisher()),
            finished: None,
        },
        snapshot,
        binding,
        "fixture".into(),
        None,
        &Default::default(),
        "deliver once",
    )
    .unwrap();
    run.launching().unwrap();
    run.running().unwrap();
    let finished = run.finish_observed(
        &HeadlessObservation {
            identity: Some(NativeSessionId::parse("session-1").unwrap()),
            outcome: Some(Ok(Some("committed answer".into()))),
            protocol_error: None,
        },
        Ok(0),
        false,
    );
    assert_eq!(finished.failure.unwrap().code, "storage_error");
    assert!(!finished.automation.completion_verified);
    assert!(store
        .current_execution(&command)
        .unwrap()
        .unwrap()
        .pending_events
        .is_some());
    drop(bus);
    drop(store);
    // This fixture seeds only command/execution state, not the session's
    // launch authority and environment. Keep restart focused on publication
    // recovery rather than trying to restore that incomplete session.
    db.close_session_force(session.as_str(), &chrono::Utc::now().to_rfc3339())
        .unwrap();
    // A lost final command CAS must stop startup before generic stale-command
    // recovery can requeue it. Its already repaired lifecycle batch survives.
    db.with_conn(|c| {
        c.execute_batch("CREATE TRIGGER fault BEFORE UPDATE OF status ON commands WHEN NEW.status='done' BEGIN SELECT RAISE(IGNORE); END;")?;
        Ok(())
    }).unwrap();
    let restart = awman::command::daemon_runtime::DaemonRuntime::bootstrap(
        awman::command::daemon_runtime::DaemonBootstrap {
            env: support::env_snapshot(&sandbox),
            set: Default::default(),
            squad_run_frontends: awman::frontend::squad::unattended::UnattendedFrontends::shared(),
            push_frontend_factory: awman::frontend::api::ApiFrontendFactory,
            terminal_hub: std::sync::Arc::new(awman::engine::daemon::core::TerminalHub::new(
                Default::default(),
            )),
            engine_presentation: std::sync::Arc::new(
                awman::command::instance::frontend::PlainPresentation,
            ),
        },
    )
    .await;
    let error = match restart {
        Err(error) => error,
        Ok(_) => panic!("startup must reject a lost completion CAS"),
    };
    assert!(matches!(error, CommandError::Data(_)), "{error}");
    assert!(error.to_string().contains("compare-and-swap"), "{error}");
    assert_eq!(
        db.get_command(command.as_str()).unwrap().unwrap().status,
        awman::data::command_status::CommandStatus::Running
    );
    db.with_conn(|c| {
        c.execute_batch("DROP TRIGGER fault")?;
        Ok(())
    })
    .unwrap();
    for _ in 0..2 {
        let restarted = Api::restart_with_config(&sandbox, worker_config()).await;
        let api = restarted.api.worker.as_ref().unwrap();
        let row = api.store.get_command(command.as_str()).unwrap().unwrap();
        assert_eq!(row.status, awman::data::command_status::CommandStatus::Done);
        assert_eq!(row.attempt, 1);
        let result: CommandResult = serde_json::from_str(row.result.as_ref().unwrap()).unwrap();
        assert!(result.automation.unwrap().completion_verified);
        assert_eq!(result.final_message.as_deref(), Some("committed answer"));
        let turns = worker::commands_turns(api, &command, &Default::default())
            .unwrap()
            .unwrap();
        assert_eq!(turns.items.len(), 1);
        assert!(turns.items[0].result.as_ref().unwrap().completion_verified);
        let events = paths.read_command_events(session.as_str(), command.as_str());
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e.payload, EventPayload::AgentTurnCompleted { .. }))
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e.payload, EventPayload::Done))
                .count(),
            1
        );
        assert!(events.windows(2).all(|w| w[0].cursor() < w[1].cursor()));
        restarted.shutdown().await;
    }
    sandbox.finish();
}
