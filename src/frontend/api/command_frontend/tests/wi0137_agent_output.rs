use super::*;
use crate::command::commands::api_server::event_bus::EventBus;
use crate::data::fs::api_paths::ApiPaths;

#[tokio::test]
async fn drain_preserves_split_unicode_and_collects_only_prompt_final_messages() {
    for command in ["exec prompt", "exec workflow"] {
        let tmp = tempfile::tempdir().unwrap();
        let writer = CommandStateWriter::new(ApiPaths::from_root(tmp.path()), "s", "c");
        let bus = EventBus::new(64);
        let mut events = bus.subscribe();
        let mut frontend =
            ApiDispatchFrontend::new(command, &[], bus.sender()).with_state_writer(writer.clone());
        let mut sink = HasAgentFrontend::container_frontend(&mut frontend);
        sink.set_agent_output_format(AgentOutputFormat::ClaudeStreamJson);
        sink.report_status(AgentStatus::Running {
            container_name: "unicode".into(),
        });
        let io = sink.take_io();
        let line = "{\"type\":\"result\",\"is_error\":false,\"result\":\"I’ll say 你好 🦀\"}\n";
        // One byte at a time includes every possible multibyte read boundary.
        for byte in line.as_bytes() {
            io.stdout.send(vec![*byte]).unwrap();
        }
        for byte in "错误 🦀\n".as_bytes() {
            io.stderr.send(vec![*byte]).unwrap();
        }
        drop(io);
        assert!(bus.wait_output_drained(Duration::from_secs(2)).await);
        let mut stdout = vec![];
        let mut stderr = vec![];
        while let Ok(event) = events.try_recv() {
            match event.payload {
                EventPayload::StdoutLine(line) => stdout.push(line),
                EventPayload::StderrLine(line) => stderr.push(line),
                _ => {}
            }
        }
        assert_eq!(stdout, ["I’ll say 你好 🦀"]);
        assert_eq!(stderr, ["错误 🦀"]);
        assert_eq!(
            writer.result_contributions().final_message.as_deref(),
            (command == "exec prompt").then_some("I’ll say 你好 🦀")
        );
        assert!(writer.result_contributions().container_started);
    }
}

#[tokio::test]
async fn closing_a_stale_sender_finalizes_the_buffered_message_before_the_snapshot() {
    let bus = EventBus::new(16);
    let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink: FinalMessageSink = {
        let captured = captured.clone();
        std::sync::Arc::new(move |message| captured.lock().unwrap().push(message))
    };
    let io = spawn_output_drains(&bus.sender(), AgentOutputFormat::CodexJson, Some(sink));
    io.stdout
        .send(
            r#"{"type":"item.completed","item":{"type":"agent_message","text":"finished 🦀"}}"#
                .as_bytes()
                .to_vec(),
        )
        .unwrap();
    assert!(!bus.wait_output_drained(Duration::from_millis(10)).await);
    bus.close_output_drains();
    assert!(bus.wait_output_drained(Duration::from_secs(2)).await);
    assert_eq!(*captured.lock().unwrap(), ["finished 🦀"]);
    assert!(io.stdout.send(b"too late".to_vec()).is_err());
}

#[tokio::test]
async fn unknown_steps_are_not_given_first_seen_indices_and_success_clears_failure() {
    let tmp = tempfile::tempdir().unwrap();
    let writer = CommandStateWriter::new(ApiPaths::from_root(tmp.path()), "s", "c");
    let bus = EventBus::new(16);
    let mut events = bus.subscribe();
    let mut frontend = ApiDispatchFrontend::new("exec workflow", &[], bus.sender())
        .with_state_writer(writer.clone());
    let step = |name: &str| WorkflowStep {
        name: name.into(),
        depends_on: vec![],
        prompt_template: "go".into(),
        agent: None,
        model: None,
        overlays: None,
        abort_on_failure: false,
    };
    let beta = step("beta");
    frontend.report_step_status(&beta, WorkflowStepStatus::Running);
    assert!(events.try_recv().is_err());
    frontend.report_workflow_state(&WorkflowState::new(
        "w".into(),
        &[step("alpha"), beta.clone()],
        "h".into(),
        None,
    ));
    frontend.report_step_launching("beta", 1);
    frontend.report_step_status(&beta, WorkflowStepStatus::Failed { exit_code: 1 });
    assert_eq!(writer.result_contributions().failed_steps.len(), 1);
    frontend.report_step_launching("beta", 2);
    frontend.report_step_status(&beta, WorkflowStepStatus::Succeeded);
    assert!(writer.result_contributions().failed_steps.is_empty());
    while let Ok(event) = events.try_recv() {
        assert_eq!(event.source.unwrap().step_index, Some(1));
    }
}

mod early_worker_path_tests {
    use super::*;
    use crate::command::commands::api_server::queue_worker::{QueueWorker, QueueWorkerDeps};
    use crate::command::commands::api_server::{ApiSessionLifecycle, TokenRegistry};
    use crate::command::dispatch::Engines;
    use crate::data::api_lifecycle_store::CommandLifecycleStore;
    use crate::data::fs::api_db::{CommandResult, SqliteSessionStore};
    use crate::data::session_manager::SessionManager;
    use std::sync::Arc;

    #[tokio::test]
    async fn command_directory_and_missing_memory_session_failures_have_structured_completion() {
        for unwritable_directory in [false, true] {
            let tmp = tempfile::tempdir().unwrap();
            let paths = ApiPaths::from_root(tmp.path());
            let store = Arc::new(SqliteSessionStore::open(tmp.path()).unwrap());
            store.insert_session("s", "/missing", "then").unwrap();
            paths.prepare_command_dir("s", "c").unwrap();
            let log = paths.command_log_path("s", "c");
            store
                .enqueue_command("c", "s", "exec prompt", "[]", &log.to_string_lossy())
                .unwrap();
            let command = store.claim_next_command("worker").unwrap().unwrap();
            if unwritable_directory {
                let directory = log.parent().unwrap();
                std::fs::remove_dir_all(directory).unwrap();
                std::fs::write(directory, "cannot create logs here").unwrap();
            }
            let engines = Engines::for_tests(&tmp.path().join("home"));
            let sessions = Arc::new(SessionManager::in_memory(
                crate::data::config::env::EnvSnapshot::empty(),
            ));
            let lifecycle = ApiSessionLifecycle::new(
                store.clone(),
                engines.clone(),
                sessions.clone(),
                paths.clone(),
            );
            let lifecycle_store = CommandLifecycleStore::open(store.clone()).unwrap();
            let worker = QueueWorker::new(
                "worker".into(),
                QueueWorkerDeps {
                    store: store.clone(),
                    engines,
                    sessions,
                    event_buses: Default::default(),
                    paths: paths.clone(),
                    frontend_factory: Arc::new(crate::frontend::api::ApiFrontendFactory),
                    lifecycle,
                    lifecycle_store,
                    cancels: Arc::new(TokenRegistry::new()),
                },
            );
            worker.execute_for_test(command).await;
            let row = store.get_command("c").unwrap().unwrap();
            let result =
                serde_json::from_str::<CommandResult>(row.result.as_deref().unwrap()).unwrap();
            assert_eq!(row.status, "error");
            assert_eq!(result.outcome, Some(CommandStatusKind::Error));
            assert_eq!(result.failure.unwrap().stage, "preflight");
            if !unwritable_directory {
                let events = paths.read_command_events("s", "c");
                assert_eq!(
                    events
                        .iter()
                        .filter(|event| matches!(event.payload, EventPayload::CommandStatus { .. }))
                        .count(),
                    1
                );
                assert!(matches!(
                    events[events.len() - 2].payload,
                    EventPayload::CommandStatus { .. }
                ));
                assert!(matches!(events.last().unwrap().payload, EventPayload::Done));
            }
        }
    }
}

/// Two fake workflow agents write interleaved stdout through their per-agent
/// proxies. The shared API frontend must stamp each drain with the source
/// lent by that proxy, including the container learned at Running.
#[tokio::test]
async fn parallel_agent_output_drains_keep_step_and_container_attribution() {
    use crate::command::commands::api_server::event_bus::EventBus;
    use crate::command::commands::exec_workflow::{AgentFrontendProxy, SharedWorkflowFrontend};
    use crate::data::execution_event::{EventPayload, EventSource, EventSourcePhase};
    use crate::engine::agent_runtime::frontend::AgentFrontend;
    use crate::frontend::api::command_frontend::ApiDispatchFrontend;

    let bus = EventBus::new(64);
    let mut events = bus.subscribe();
    let shared = SharedWorkflowFrontend::new(Box::new(ApiDispatchFrontend::new(
        "exec workflow",
        &[],
        bus.sender(),
    )));
    let mut alpha = AgentFrontendProxy::new(shared.clone(), false)
        .with_source(EventSource::step(EventSourcePhase::Main, "alpha", 0).with_step_attempt(2));
    let mut beta = AgentFrontendProxy::new(shared, false)
        .with_source(EventSource::step(EventSourcePhase::Main, "beta", 1).with_step_attempt(1));

    alpha.report_status(AgentStatus::Running {
        container_name: "container-alpha".into(),
    });
    beta.report_status(AgentStatus::Running {
        container_name: "container-beta".into(),
    });
    let alpha_io = alpha.take_io();
    let beta_io = beta.take_io();
    alpha_io.stdout.send(b"alpha one\n".to_vec()).unwrap();
    beta_io.stdout.send(b"beta one\n".to_vec()).unwrap();
    alpha_io.stdout.send(b"alpha two\n".to_vec()).unwrap();
    beta_io.stdout.send(b"beta two\n".to_vec()).unwrap();

    let mut seen = Vec::new();
    while seen.len() < 4 {
        let event = tokio::time::timeout(Duration::from_secs(2), events.recv())
            .await
            .unwrap()
            .unwrap();
        let EventPayload::StdoutLine(line) = event.payload else {
            continue;
        };
        let source = event.source.expect("agent stdout is attributed");
        let expected = if line.starts_with("alpha") {
            ("alpha", 0, "container-alpha", 2)
        } else {
            ("beta", 1, "container-beta", 1)
        };
        assert_eq!(source.step_name.as_deref(), Some(expected.0));
        assert_eq!(source.step_index, Some(expected.1));
        assert_eq!(source.container.as_deref(), Some(expected.2));
        assert_eq!(source.step_attempt, Some(expected.3));
        seen.push(line);
    }
    seen.sort();
    assert_eq!(seen, ["alpha one", "alpha two", "beta one", "beta two"]);
}

#[tokio::test]
async fn parallel_structured_formats_are_kept_on_each_container_proxy() {
    use crate::command::commands::api_server::event_bus::EventBus;
    use crate::command::commands::exec_workflow::{AgentFrontendProxy, SharedWorkflowFrontend};
    use crate::data::agent_output_format::AgentOutputFormat;
    use crate::data::execution_event::{EventPayload, EventSource, EventSourcePhase};
    use crate::engine::agent_runtime::frontend::AgentFrontend;
    use crate::frontend::api::command_frontend::ApiDispatchFrontend;
    for running_first in [true, false] {
        let bus = EventBus::new(64);
        let mut events = bus.subscribe();
        let shared = SharedWorkflowFrontend::new(Box::new(ApiDispatchFrontend::new(
            "exec workflow",
            &[],
            bus.sender(),
        )));
        let mut alpha = AgentFrontendProxy::new(shared.clone(), false)
            .with_source(EventSource::step(EventSourcePhase::Main, "alpha", 0));
        let mut beta = AgentFrontendProxy::new(shared, false).with_source(EventSource::step(
            EventSourcePhase::Main,
            "beta",
            1,
        ));
        alpha.set_agent_output_format(AgentOutputFormat::ClaudeStreamJson);
        beta.set_agent_output_format(AgentOutputFormat::CodexJson);
        if running_first {
            alpha.report_status(AgentStatus::Running {
                container_name: "a".into(),
            });
            beta.report_status(AgentStatus::Running {
                container_name: "b".into(),
            });
        }
        let a = alpha.take_io();
        let b = beta.take_io();
        a.stdout
            .send(b"{\"type\":\"result\",\"result\":\"alpha final\"}\n".to_vec())
            .unwrap();
        b.stdout.send(b"{\"type\":\"item.completed\",\"item\":{\"type\":\"command_execution\",\"command\":\"tool\",\"aggregated_output\":\"SECRET_TOOL_RESULT\",\"exit_code\":0}}\n{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"beta final\"}}\n{\"type\":\"turn.completed\"}\n".to_vec()).unwrap();
        drop(a);
        drop(b);
        assert!(bus.wait_output_drained(Duration::from_secs(2)).await);
        let mut lines = vec![];
        let mut tools = 0;
        while let Ok(event) = events.try_recv() {
            assert!(!format!("{:?}", event.payload).contains("SECRET_TOOL_RESULT"));
            match event.payload {
                EventPayload::StdoutLine(text) => {
                    lines.push((event.source.unwrap().step_name.unwrap(), text))
                }
                EventPayload::StepActivity {
                    kind: crate::data::execution_event::StepActivityKind::ToolResult,
                    ..
                } => tools += 1,
                _ => {}
            }
        }
        lines.sort();
        assert_eq!(
            lines,
            [
                ("alpha".into(), "alpha final".into()),
                ("beta".into(), "beta final".into())
            ]
        );
        assert_eq!(tools, 1);
    }
}
