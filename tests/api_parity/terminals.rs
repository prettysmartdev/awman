//! WI 0141 `/v1/terminals` across components: the daemon's terminal
//! operations publish `terminal_opened` / `terminal_closed` onto the owning
//! command's event bus (what SSE followers of
//! `/v1/worker/commands/{id}/events` see), and the HTTP resource lists the
//! same terminal. The transport's frame-level behaviour is unit-tested in
//! `src/frontend/api/terminals_tests.rs`.

use std::sync::Arc;
use std::time::Duration;

use awman::command::commands::api_server::event_bus::EventBus;
use awman::command::daemon_api::DaemonApi;
use awman::data::daemon_state::FeatureName;
use awman::data::execution_event::{EventPayload, EventSource, EventSourcePhase, ExecutionEvent};
use awman::data::ids::CommandId;
use awman::data::terminal::{TerminalOwner, TerminalOwnerInfo};
use awman::engine::terminal_hub::{TerminalInput, TerminalRegistration};

async fn next(rx: &mut tokio::sync::broadcast::Receiver<ExecutionEvent>) -> ExecutionEvent {
    tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("event in time")
        .unwrap()
}

#[tokio::test]
async fn terminal_lifecycle_reaches_the_command_event_stream_and_the_list() {
    let root = tempfile::tempdir().unwrap();
    let api = Arc::new(DaemonApi::for_worker_tests(super::http_consistency::state(
        root.path(),
    )));
    let cmd = CommandId::new();
    let bus = Arc::new(EventBus::new(32));
    let mut events = bus.subscribe();
    api.runtime
        .event_buses()
        .lock()
        .await
        .insert(cmd.as_str().to_owned(), bus);

    let (stdin, _stdin_rx) = awman::engine::terminal_hub::input_channel();
    let handle = api.terminals.hub().register(
        TerminalRegistration {
            owner: TerminalOwnerInfo {
                owner: TerminalOwner::Command(cmd.clone()),
                feature: FeatureName::Worker,
                source: Some(EventSource::step(EventSourcePhase::Main, "implement", 2)),
                attempt: 1,
                allow_remote_input: false,
            },
            agent: "codex".into(),
            model: None,
            cols: 120,
            rows: 40,
        },
        TerminalInput {
            stdin,
            resize: None,
        },
    );

    let opened = next(&mut events).await;
    assert_eq!(opened.payload.sse_event_type(), "terminal_opened");
    let source = opened.source.clone().expect("step source on the envelope");
    assert_eq!(source.step_name.as_deref(), Some("implement"));
    assert_eq!(source.step_index, Some(2));
    let json = serde_json::to_value(&opened.payload).unwrap();
    assert_eq!(
        json["data"],
        serde_json::json!({
            "terminal_id": handle.id().as_str(),
            "agent": "codex",
            "cols": 120,
            "rows": 40,
        })
    );

    let (addr, server) = {
        let router = awman::frontend::api::routes::build_router_for_listener(
            api.clone(),
            awman::frontend::api::routes::ListenerPolicy::Local,
        )
        .layer(axum::Extension(
            awman::frontend::api::local::OwnerAuthenticated,
        ));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        (
            addr,
            tokio::spawn(async move { axum::serve(listener, router).await.unwrap() }),
        )
    };
    let list: serde_json::Value =
        reqwest::get(format!("http://{addr}/v1/terminals?owner_id={cmd}"))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
    assert_eq!(list["items"][0]["id"], handle.id().as_str());
    assert!(list["next_cursor"].is_null());

    handle.exit(0);
    let closed = next(&mut events).await;
    assert!(matches!(
        closed.payload,
        EventPayload::TerminalClosed { ref terminal_id, exit_code: 0 } if terminal_id == handle.id()
    ));
    assert_eq!(
        closed.source.unwrap().step_name.as_deref(),
        Some("implement")
    );
    server.abort();
}

#[tokio::test]
async fn squad_run_terminal_events_are_durable_and_feature_cancel_forgets_only_its_owners() {
    let root = tempfile::tempdir().unwrap();
    let api = Arc::new(DaemonApi::for_worker_tests(super::http_consistency::state(
        root.path(),
    )));
    let now = chrono::Utc::now();
    let task: awman::data::fs::Task = serde_json::from_value(serde_json::json!({
        "id": awman::data::ids::TaskId::new(), "name":"terminal-audit", "description":"test",
        "repo_scope": root.path(), "mount_scope":"directory", "overlays":[], "interval_secs":60,
        "status":"active", "created_at": now, "updated_at": now,
        "unmet_env":[]
    }))
    .unwrap();
    let store = api.runtime.core().task_store();
    store.create(&task).unwrap();
    let run = store.start_run(&task.id, None, now).unwrap();
    let (stdin, _input) = awman::engine::terminal_hub::input_channel();
    let handle = api.terminals.hub().register(
        TerminalRegistration {
            owner: TerminalOwnerInfo {
                owner: TerminalOwner::Run(run.clone()),
                feature: FeatureName::Squad,
                source: None,
                attempt: 1,
                allow_remote_input: false,
            },
            agent: "test".into(),
            model: None,
            cols: 80,
            rows: 24,
        },
        TerminalInput {
            stdin,
            resize: None,
        },
    );
    let (stdin, _input) = awman::engine::terminal_hub::input_channel();
    let other = api.terminals.hub().register(
        TerminalRegistration {
            owner: TerminalOwnerInfo {
                owner: TerminalOwner::Command(CommandId::new()),
                feature: FeatureName::Worker,
                source: None,
                attempt: 1,
                allow_remote_input: false,
            },
            agent: "test".into(),
            model: None,
            cols: 80,
            rows: 24,
        },
        TerminalInput {
            stdin,
            resize: None,
        },
    );
    awman::command::daemon_api::core::jobs_cancel_feature(&api, FeatureName::Squad)
        .await
        .unwrap();
    assert!(api.terminals.hub().get(handle.id()).is_err());
    assert!(api.terminals.hub().get(other.id()).is_ok());
    let path = api
        .runtime
        .core()
        .data_paths()
        .root()
        .join("squad/tasks/terminal-audit/runs")
        .join(run.as_str())
        .join("events.log");
    let events = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            let events = awman::data::execution_event::parse_event_log(&text);
            if events.len() >= 2 {
                break events;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(matches!(
        events[0].payload,
        EventPayload::TerminalOpened { .. }
    ));
    assert!(matches!(
        events[1].payload,
        EventPayload::TerminalClosed { exit_code: 137, .. }
    ));
    assert_eq!((events[0].sequence, events[1].sequence), (0, 1));
}

#[tokio::test]
async fn control_events_keep_their_order_while_the_command_bus_map_is_locked() {
    use awman::data::terminal::{DetachReason, TerminalClientFrame, ViewerIdentity};
    let root = tempfile::tempdir().unwrap();
    let api = Arc::new(DaemonApi::for_worker_tests(super::http_consistency::state(
        root.path(),
    )));
    let cmd = CommandId::new();
    let bus = Arc::new(EventBus::new(128));
    let mut events = bus.subscribe();
    let mut buses = api.runtime.event_buses().lock().await;
    buses.insert(cmd.as_str().to_owned(), bus);
    let (stdin, _input) = awman::engine::terminal_hub::input_channel();
    let terminal = api.terminals.hub().register(
        TerminalRegistration {
            owner: TerminalOwnerInfo {
                owner: TerminalOwner::Command(cmd),
                feature: FeatureName::Worker,
                source: None,
                attempt: 1,
                allow_remote_input: false,
            },
            agent: "fixture".into(),
            model: None,
            cols: 80,
            rows: 24,
        },
        TerminalInput {
            stdin,
            resize: None,
        },
    );
    for _ in 0..20 {
        let mut session = api
            .terminals
            .open(terminal.id(), ViewerIdentity::local())
            .unwrap();
        session
            .handle_client_frame(TerminalClientFrame::TakeControl)
            .unwrap();
        session.detach(DetachReason::User);
    }
    drop(buses);
    assert!(matches!(
        next(&mut events).await.payload,
        EventPayload::TerminalOpened { .. }
    ));
    for _ in 0..20 {
        assert!(
            matches!(next(&mut events).await.payload, EventPayload::TerminalControl { holder, .. } if holder == "local")
        );
        assert!(
            matches!(next(&mut events).await.payload, EventPayload::TerminalControl { holder, .. } if holder.is_empty())
        );
        assert!(matches!(
            next(&mut events).await.payload,
            EventPayload::TerminalViewerDetached {
                reason: DetachReason::User,
                ..
            }
        ));
    }
}
