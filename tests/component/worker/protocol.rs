use crate::support;
use awman::data::execution_event::{EventPayload, ExecutionEvent};
use awman::data::fs::api_db::CommandResult;
use awman::engine::worker::events::{ChannelEventSource, EventForwarder};
use awman::engine::worker::heartbeat::{HeartbeatEnd, HeartbeatLoop, HeartbeatStatus};
use awman::engine::worker::{LeaseApi, LeaseRef, WspSession};
use awman_test_support::fakes::FakeWorkServer;
use awman_wsp::{ClaimOutcome, HeartbeatRequest, HeartbeatState, LeaseResult, RegisterRequest};
use std::{collections::BTreeMap, sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;

async fn linked(server: &FakeWorkServer) -> Arc<WspSession> {
    let link = server.server().link();
    server.server().approve_link(&link.link_id, BTreeMap::new());
    let client = awman_wsp::Client::new(server.base_url(), String::new()).unwrap();
    let token = client.poll_link(&link.link_id).await.unwrap().unwrap();
    let session =
        Arc::new(WspSession::connect("test", server.base_url(), token.refresh_token).unwrap());
    session
        .register(&RegisterRequest {
            name: "worker".into(),
            labels: BTreeMap::new(),
            capacity: 4,
            features: awman_wsp::features::ALL
                .iter()
                .map(|s| s.to_string())
                .collect(),
        })
        .await
        .unwrap();
    session
}
async fn claim(server: &FakeWorkServer, session: &WspSession, id: &str) -> awman_wsp::Lease {
    server
        .server()
        .enqueue(awman_wsp::conformance::work_item(id), chrono::Utc::now())
        .unwrap();
    let ClaimOutcome::Leased(lease) = session.claim(Some(id), 0).await.unwrap() else {
        panic!("lease missing")
    };
    lease
}
fn beat() -> HeartbeatRequest {
    HeartbeatRequest {
        state: HeartbeatState::Running,
        step: None,
        progress: None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wsp_conformance_covers_auth_affinity_events_epoch_and_wire() {
    let sandbox = support::sandbox(
        "worker::protocol::wsp_conformance_covers_auth_affinity_events_epoch_and_wire",
    );
    let server = FakeWorkServer::start(&sandbox).await;
    let report = awman_wsp::conformance::run(server.base_url(), server.server().as_ref()).await;
    assert!(report.is_success(), "{:?}", report.failed);
    assert_eq!(report.passed.len(), awman_wsp::conformance::cases().len());
    server.shutdown().await;
    sandbox.finish();
}

/// Guards: 3e8d7e6c. A stale run cannot post or prolong a reissued lease.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn regression_3e8d7e6c_reissued_lease_fences_every_old_epoch_call() {
    let sandbox = support::sandbox(
        "worker::protocol::regression_3e8d7e6c_reissued_lease_fences_every_old_epoch_call",
    );
    let server = FakeWorkServer::start(&sandbox).await;
    let session = linked(&server).await;
    let old = claim(&server, &session, "epoch").await;
    server.server().expire_lease_reusing_id(&old.id);
    let ClaimOutcome::Leased(new) = session.claim(Some("epoch"), 0).await.unwrap() else {
        panic!("reissued lease")
    };
    assert_eq!(old.id, new.id);
    assert!(new.epoch > old.epoch);
    let old = LeaseRef::of(&old);
    use awman::engine::worker::error::WorkerEngineError::LeaseLost;
    assert_eq!(
        session.heartbeat(&old, &beat()).await.unwrap_err(),
        LeaseLost
    );
    assert_eq!(session.post_events(&old, &[]).await.unwrap_err(), LeaseLost);
    assert_eq!(session.credentials(&old).await.unwrap_err(), LeaseLost);
    let result = LeaseResult {
        result: CommandResult::default(),
        outputs: None,
        outputs_mounted: false,
        duration_ms: 1,
    };
    assert_eq!(session.result(&old, &result).await.unwrap_err(), LeaseLost);
    session.result(&LeaseRef::of(&new), &result).await.unwrap();
    assert!(server.server().result_for("epoch").is_some());
    server.shutdown().await;
    sandbox.finish();
}

/// Guards: 8cf45de1. Subscribe before making the producer available; the
/// first and final snapshots must reach both the reliable log and WSP.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn regression_8cf45de1_immediate_producer_preserves_initial_and_final_events() {
    let sandbox = support::sandbox("worker::protocol::regression_8cf45de1_immediate_producer_preserves_initial_and_final_events");
    let server = FakeWorkServer::start(&sandbox).await;
    let session = linked(&server).await;
    let lease = claim(&server, &session, "events").await;
    let bus = awman::command::commands::api_server::event_bus::EventBus::new(16);
    let mut log = bus.subscribe_log();
    let mut stream = bus.subscribe();
    let ordered = bus.subscribe_ordered();
    let (tx, rx) = tokio::sync::mpsc::channel(16);
    let bridge = tokio::spawn(async move {
        let mut ordered = ordered;
        while let Some(event) = ordered.recv().await {
            let done = matches!(event.payload, EventPayload::Done);
            tx.send(event).await.unwrap();
            if done {
                break;
            }
        }
    });
    let forward = tokio::spawn(EventForwarder::run(
        session,
        LeaseRef::of(&lease),
        Box::new(ChannelEventSource::new(rx)),
        CancellationToken::new(),
    ));
    let sender = bus.sender();
    sender.emit(EventPayload::WorkflowState(
        serde_json::json!({"status":"pending"}),
    ));
    sender.emit(EventPayload::WorkflowState(
        serde_json::json!({"status":"done"}),
    ));
    sender.emit(EventPayload::Done);
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), forward)
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        3
    );
    bridge.await.unwrap();
    let received = server.server().events_for(&lease.id);
    for wire in &received {
        let persisted: ExecutionEvent = log.recv().await.unwrap();
        let streamed = stream.recv().await.unwrap();
        assert_eq!(persisted.public_value(), *wire);
        assert_eq!(streamed.public_value(), *wire);
    }
    assert_eq!(received.len(), 3);
    server.shutdown().await;
    sandbox.finish();
}

/// Guards: 3e8d7e6c. State changes wake the heartbeat independently of its tick.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn regression_3e8d7e6c_cancelled_lease_wakes_run_without_waiting_for_tick() {
    let sandbox = support::sandbox(
        "worker::protocol::regression_3e8d7e6c_cancelled_lease_wakes_run_without_waiting_for_tick",
    );
    let server = FakeWorkServer::start(&sandbox).await;
    let session = linked(&server).await;
    let lease = claim(&server, &session, "cancel").await;
    server.server().request_cancel(&lease.id, "operator");
    let (control, handle) = HeartbeatLoop::spawn(
        session,
        LeaseRef::of(&lease),
        Duration::from_secs(60),
        HeartbeatStatus::new(HeartbeatState::Running),
        lease.expires_at,
    );
    tokio::time::timeout(Duration::from_secs(5), control.cancelled().cancelled())
        .await
        .unwrap();
    assert!(control.may_report());
    control.stop();
    assert!(matches!(
        handle.await.unwrap(),
        HeartbeatEnd::Cancelled { .. }
    ));
    server.shutdown().await;
    sandbox.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expired_access_refreshes_once_and_revoked_link_latches_session_cancellation() {
    let sandbox = support::sandbox("worker::protocol::expired_access_refreshes_once_and_revoked_link_latches_session_cancellation");
    let server = FakeWorkServer::start(&sandbox).await;
    let link = server.server().link();
    server.server().approve_link(&link.link_id, BTreeMap::new());
    let linker = awman_wsp::Client::new(server.base_url(), String::new()).unwrap();
    let refresh = linker
        .poll_link(&link.link_id)
        .await
        .unwrap()
        .unwrap()
        .refresh_token;
    sandbox.redact(&refresh);
    let session = WspSession::connect("server", server.base_url(), refresh.clone()).unwrap();
    session
        .register(&RegisterRequest {
            name: "worker".into(),
            labels: BTreeMap::new(),
            capacity: 1,
            features: vec![],
        })
        .await
        .unwrap();
    server.server().invalidate_access_tokens();
    assert!(session.queue(10, None).await.unwrap().items.is_empty());
    assert!(!session.is_revoked());
    server.server().revoke(&refresh);
    assert_eq!(
        session.queue(10, None).await.unwrap_err(),
        awman::engine::worker::error::WorkerEngineError::LinkRevoked
    );
    assert!(session.revoked().is_cancelled());
    // No subsequent network operation can revive the revoked session.
    server.shutdown().await;
    assert_eq!(
        session.claim(None, 0).await.unwrap_err(),
        awman::engine::worker::error::WorkerEngineError::LinkRevoked
    );
    assert!(!format!("{session:?}").contains(&refresh));
    sandbox.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminal_relay_forwards_snapshot_then_output_and_stops_cleanly() {
    use awman::data::{
        daemon_state::FeatureName,
        ids::LeaseId,
        terminal::{TerminalOwner, TerminalOwnerInfo},
    };
    use awman::engine::terminal_hub::{
        input_channel, TerminalHub, TerminalHubConfig, TerminalInput, TerminalRegistration,
    };
    use awman::engine::worker::terminal_relay::TerminalRelay;
    use awman_wsp::terminal::TerminalFrame;
    let sandbox = support::sandbox(
        "worker::protocol::terminal_relay_forwards_snapshot_then_output_and_stops_cleanly",
    );
    let server = FakeWorkServer::start(&sandbox).await;
    let session = linked(&server).await;
    server
        .server()
        .enqueue(
            awman_wsp::conformance::work_item("terminal"),
            chrono::Utc::now(),
        )
        .unwrap();
    server.server().set_terminal("terminal", true, false);
    let ClaimOutcome::Leased(lease) = session.claim(Some("terminal"), 0).await.unwrap() else {
        panic!("lease")
    };
    let hub = Arc::new(TerminalHub::new(TerminalHubConfig::default()));
    let owner = TerminalOwner::Lease(LeaseId::new());
    let (stdin, _rx) = input_channel();
    let handle = hub.register(
        TerminalRegistration {
            owner: TerminalOwnerInfo {
                owner: owner.clone(),
                feature: FeatureName::Worker,
                source: None,
                attempt: 1,
                allow_remote_input: false,
            },
            agent: "fake-agent".into(),
            model: None,
            cols: 80,
            rows: 24,
        },
        TerminalInput {
            stdin,
            resize: None,
        },
    );
    let mut socket = session.open_terminal(&LeaseRef::of(&lease)).await.unwrap();
    let stop = CancellationToken::new();
    let relay = TerminalRelay::new(hub, owner, false);
    let stopped = stop.clone();
    let task = tokio::spawn(async move { relay.run(&mut socket, stopped).await });
    awman_test_support::wait_for_async("terminal snapshot", Duration::from_secs(5), || async {
        server
            .server()
            .terminal_frames(&lease.id)
            .iter()
            .any(|f| {
                f.terminal_id == handle.id().as_str()
                    && matches!(f.frame, TerminalFrame::Snapshot { .. })
            })
            .then_some(())
    })
    .await;
    handle.push_output(b"hello");
    awman_test_support::wait_for_async("terminal output", Duration::from_secs(5), || async {
        server
            .server()
            .terminal_frames(&lease.id)
            .iter()
            .any(|f| matches!(f.frame, TerminalFrame::Output { .. }))
            .then_some(())
    })
    .await;
    stop.cancel();
    task.await.unwrap().unwrap();
    server.shutdown().await;
    sandbox.finish();
}
