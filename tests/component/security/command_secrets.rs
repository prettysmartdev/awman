use awman::command::commands::api_server::event_bus::EventBus;
use awman::data::execution_event::{CommandStatusKind, EventPayload, StepActivityKind};
use awman::data::fs::api_command_log::CommandLogWriter;
use awman::data::fs::api_db::{CommandFailure, CommandResult};
use awman::data::fs::ApiPaths;
use awman::data::ids::{CommandId, WorkerSessionId};
use awman::data::secret::SecretString;

#[tokio::test]
async fn agent_events_are_redacted_in_both_logs_while_output_files_remain_untouched() {
    let s = crate::support::sandbox("security::command_secrets::agent_events_are_redacted_in_both_logs_while_output_files_remain_untouched");
    let secret = "test-secret-output";
    s.redact(secret);
    let paths = ApiPaths::from_root(s.work().join("api"));
    let sid = WorkerSessionId::new();
    let cid = CommandId::new();
    paths
        .prepare_command_dir(sid.as_str(), cid.as_str())
        .unwrap();
    let output = paths.command_output_dir(sid.as_str(), cid.as_str());
    std::fs::create_dir_all(&output).unwrap();
    std::fs::write(output.join("agent.txt"), secret).unwrap();
    let bus = EventBus::new(32);
    let mut receiver = bus.subscribe_log();
    bus.redaction().add(SecretString::new(secret));
    for event in [
        EventPayload::StdoutLine(format!("{secret} {secret}")),
        EventPayload::StderrLine(secret.into()),
        EventPayload::StatusMessage {
            phase: "main".into(),
            message: secret.into(),
        },
        EventPayload::StepActivity {
            kind: StepActivityKind::Message,
            summary: secret.into(),
        },
    ] {
        bus.sender().emit(event);
    }
    bus.sender().finish(EventPayload::CommandStatus {
        status: CommandStatusKind::Error,
        exit_code: Some(1),
        error: Some(secret.into()),
        result: Some(Box::new(CommandResult {
            final_message: Some(secret.into()),
            failure: Some(CommandFailure {
                stage: "step".into(),
                code: "step_failed".into(),
                message: secret.into(),
                step: None,
            }),
            ..Default::default()
        })),
    });
    let mut writer = CommandLogWriter::open_append(&paths, sid.as_str(), cid.as_str())
        .await
        .unwrap();
    while let Some(event) = receiver.recv().await {
        writer.write_event(&event).await;
        if matches!(event.payload, EventPayload::Done) {
            break;
        }
    }
    writer.flush().await;
    for path in [
        paths.command_events_log_path(sid.as_str(), cid.as_str()),
        paths.command_log_path(sid.as_str(), cid.as_str()),
    ] {
        let text = std::fs::read_to_string(path).unwrap();
        assert!(!text.contains(secret));
        assert!(text.contains("***"));
    }
    assert_eq!(
        std::fs::read_to_string(output.join("agent.txt")).unwrap(),
        secret
    );
}

#[cfg(unix)]
#[tokio::test]
async fn session_socket_output_and_workflow_api_snapshot_are_redacted_without_command_secrets() {
    use awman::command::dispatch::ephemeral_secrets::{EphemeralSecrets, SessionGitCredential};
    use awman::data::git_transport::{GitAuthKind, GitAuthSpec};
    use awman::data::{
        fs::api_command_log::CommandStateWriter,
        workflow_state::{StepState, WorkflowState},
    };
    use awman::engine::git::credential_socket::CacheSocketListener;
    use awman::engine::git::lease_transport::CredentialScope;
    use std::sync::Arc;
    let s = crate::support::sandbox("security::command_secrets::session_socket_output_and_workflow_api_snapshot_are_redacted_without_command_secrets");
    s.require::<awman_test_support::capability::LocalGit>();
    let secret = "test-secret-session-output";
    s.redact(secret);
    let paths = ApiPaths::from_root(s.work().join("api"));
    let sid = WorkerSessionId::new();
    let cid = CommandId::new();
    paths
        .prepare_command_dir(sid.as_str(), cid.as_str())
        .unwrap();
    let scope = CredentialScope::for_http_url("https://git.example/team/repo.git").unwrap();
    let spec = Arc::new(
        GitAuthSpec::new(
            GitAuthKind::Basic,
            Some("alice".into()),
            SecretString::new(secret),
        )
        .unwrap(),
    );
    let listener =
        CacheSocketListener::start(&s.root().join("r"), spec.clone(), scope.clone()).unwrap();
    let memory = EphemeralSecrets::default();
    memory.insert_session_git_auth(sid.as_str(), SessionGitCredential { spec, scope });
    let bus = EventBus::new(32);
    memory.register_session_redaction(sid.as_str(), &bus.redaction());
    let mut events = bus.subscribe_log();
    let output = s
        .command("git")
        .args(["credential-cache", "--socket"])
        .arg(listener.socket())
        .arg("get")
        .stdin(awman_test_support::Stdin::Bytes(
            b"protocol=https\nhost=git.example\npath=team/repo.git\n\n".to_vec(),
        ))
        .run();
    output.assert_success();
    assert!(output.stdout_text().contains(secret));
    bus.sender()
        .emit(EventPayload::StdoutLine(output.stdout_text()));
    let mut state = WorkflowState::new(secret.into(), &[], "hash".into(), None);
    state.step_states.insert(
        format!("deploy-{secret}"),
        StepState::Failed {
            exit_code: 1,
            error_message: Some(secret.into()),
        },
    );
    let writer = CommandStateWriter::new(paths.clone(), sid.as_str(), cid.as_str());
    awman::command::worker::events::persist_workflow_snapshot(&writer, &bus.sender(), &state);
    let raw = paths
        .read_command_workflow_state_raw(sid.as_str(), cid.as_str())
        .unwrap()
        .unwrap();
    assert!(!raw.contains(secret));
    assert!(raw.contains("***"));
    bus.sender().finish(EventPayload::CommandStatus {
        status: CommandStatusKind::Error,
        exit_code: Some(1),
        error: Some(secret.into()),
        result: Some(Box::new(CommandResult {
            final_message: Some(secret.into()),
            data: Some(serde_json::json!({secret:secret})),
            ..Default::default()
        })),
    });
    let mut log = CommandLogWriter::open_append(&paths, sid.as_str(), cid.as_str())
        .await
        .unwrap();
    while let Some(event) = events.recv().await {
        assert!(!serde_json::to_string(&event).unwrap().contains(secret));
        log.write_event(&event).await;
        if matches!(event.payload, EventPayload::Done) {
            break;
        }
    }
    log.flush().await;
    for path in [
        paths.command_events_log_path(sid.as_str(), cid.as_str()),
        paths.command_log_path(sid.as_str(), cid.as_str()),
    ] {
        assert!(!std::fs::read_to_string(path).unwrap().contains(secret));
    }
}
