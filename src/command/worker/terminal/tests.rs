//! Real WSP transport through the foreground socket owner and owner-local pump.
use super::*;
use crate::data::{
    daemon_state::FeatureName,
    execution_event::CommandStatusKind,
    fs::api_db::CommandResult,
    ids::CommandId,
    terminal::{TerminalOwner, TerminalOwnerInfo},
};
use crate::engine::{
    terminal_hub::{
        input_channel, TerminalHandle, TerminalHub, TerminalInput, TerminalRegistration,
    },
    worker::heartbeat::{HeartbeatLoop, HeartbeatStatus},
};
use awman_test_support::{fakes::FakeWorkServer, wait_for_async, TestCase, TestSandbox};
use awman_wsp::{ClaimOutcome, HeartbeatState, Lease, LeaseResult, RegisterRequest};
use std::{collections::BTreeMap, time::Duration};

struct Foreground {
    sandbox: TestSandbox,
    server: FakeWorkServer,
    session: Arc<WspSession>,
    lease: Lease,
    terminal: TerminalHandle,
    hub: Arc<TerminalHub>,
    bridge: Arc<Bridge>,
    relay: Relay,
    control: HeartbeatControl,
    heartbeat: tokio::task::JoinHandle<crate::engine::worker::heartbeat::HeartbeatEnd>,
    done: CancellationToken,
}
impl Foreground {
    async fn start(id: &'static str) -> Self {
        let sandbox = TestSandbox::for_test(TestCase::component(id));
        let server = FakeWorkServer::start_with(
            &sandbox,
            Arc::new(
                awman_wsp::mock::MockWorkServer::new().with_lease_ttl(chrono::Duration::minutes(5)),
            ),
        )
        .await;
        let link = server.server().link();
        server.server().approve_link(&link.link_id, BTreeMap::new());
        let client = awman_wsp::Client::new(server.base_url(), String::new()).unwrap();
        let token = client.poll_link(&link.link_id).await.unwrap().unwrap();
        let session =
            Arc::new(WspSession::connect("test", server.base_url(), token.refresh_token).unwrap());
        session
            .register(&RegisterRequest {
                name: "foreground".into(),
                labels: BTreeMap::new(),
                capacity: 1,
                features: vec![awman_wsp::features::TERMINAL.into()],
            })
            .await
            .unwrap();
        server
            .server()
            .enqueue(
                awman_wsp::conformance::work_item("foreground"),
                chrono::Utc::now(),
            )
            .unwrap();
        server.server().set_terminal("foreground", true, true);
        let ClaimOutcome::Leased(lease) = session.claim(Some("foreground"), 0).await.unwrap()
        else {
            panic!("lease");
        };
        let (control, heartbeat) = HeartbeatLoop::spawn(
            session.clone(),
            LeaseRef::of(&lease),
            Duration::from_secs(60),
            HeartbeatStatus::new(HeartbeatState::Running),
            lease.expires_at,
        );
        let done = CancellationToken::new();
        let bridge = Bridge::start(
            session.clone(),
            &lease,
            control.clone(),
            done.clone(),
            Arc::new(|| true),
        );
        let hub = Arc::new(TerminalHub::new(Default::default()));
        let job = CommandId::new();
        let (stdin, _input) = input_channel();
        let terminal = hub.register(
            TerminalRegistration {
                automation: None,
                owner: TerminalOwnerInfo {
                    owner: TerminalOwner::Command(job.clone()),
                    feature: FeatureName::Worker,
                    source: None,
                    attempt: 1,
                    allow_remote_input: true,
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
        let exchange = bridge.clone();
        let relay = relay(
            hub.clone(),
            job.as_str(),
            true,
            done.child_token(),
            move |request| {
                let exchange = exchange.clone();
                async move {
                    // Exercise both wire models used across the owner-local HTTP boundary.
                    let request =
                        serde_json::from_value(serde_json::to_value(request).unwrap()).unwrap();
                    let reply = exchange.exchange(request).await?;
                    Ok(serde_json::from_value(serde_json::to_value(reply).unwrap()).unwrap())
                }
            },
        );
        let fixture = Self {
            sandbox,
            server,
            session,
            lease,
            terminal,
            hub,
            bridge,
            relay,
            control,
            heartbeat,
            done,
        };
        fixture.snapshot(0).await;
        fixture
    }
    async fn snapshot(&self, index: usize) {
        wait_for_async("foreground snapshot", Duration::from_secs(5), || async {
            self.server
                .server()
                .terminal_connection_frames(&self.lease.id)
                .get(index)
                .filter(|frames| {
                    frames
                        .iter()
                        .any(|frame| matches!(frame.frame, TerminalFrame::Snapshot { .. }))
                })
                .map(|_| ())
        })
        .await;
    }
    async fn stopped(&self) {
        wait_for_async(
            "foreground terminal stopped",
            Duration::from_secs(5),
            || async { self.bridge.status.borrow().unavailable.then_some(()) },
        )
        .await;
    }
    async fn finish(self) {
        self.terminal.exit(0);
        self.relay.flush().await;
        assert!(
            !self.control.cancelled().is_cancelled(),
            "terminal disconnect cancelled execution"
        );
        self.done.cancel();
        self.bridge.stop().await;
        self.stopped().await;
        let result = LeaseResult {
            context_changes: None,
            workflow_context: None,
            result: CommandResult {
                outcome: Some(CommandStatusKind::Done),
                exit_code: Some(0),
                ..Default::default()
            },
            outputs: None,
            outputs_mounted: false,
            duration_ms: 1,
        };
        self.session
            .result(&LeaseRef::of(&self.lease), &result)
            .await
            .unwrap();
        assert_eq!(
            self.server
                .server()
                .result_for("foreground")
                .unwrap()
                .result["outcome"],
            "done"
        );
        let attempts = self.server.server().terminal_open_attempts(&self.lease.id);
        let until = std::time::Instant::now() + Duration::from_millis(1100);
        wait_for_async(
            "no reconnect after result",
            Duration::from_secs(3),
            || async { (std::time::Instant::now() >= until).then_some(()) },
        )
        .await;
        assert_eq!(
            self.server.server().terminal_open_attempts(&self.lease.id),
            attempts
        );
        self.control.stop();
        self.heartbeat.await.unwrap();
        drop(self.relay);
        self.server.shutdown().await;
        self.sandbox.finish();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn foreground_reconnects_with_snapshot_before_output_without_cancelling() {
    let f = Foreground::start("command::worker::terminal::tests::foreground_reconnects_with_snapshot_before_output_without_cancelling").await;
    f.server.server().drop_terminal(&f.lease.id).unwrap();
    f.terminal.push_output(b"disconnected");
    f.snapshot(1).await;
    f.terminal.push_output(b"replacement");
    wait_for_async("foreground output", Duration::from_secs(5), || async {
        f.server.server().terminal_connection_frames(&f.lease.id)[1]
            .iter()
            .any(|frame| matches!(frame.frame, TerminalFrame::Output { .. }))
            .then_some(())
    })
    .await;
    let frames = f.server.server().terminal_connection_frames(&f.lease.id);
    assert!(matches!(frames[1][0].frame, TerminalFrame::Snapshot { .. }));
    f.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn foreground_reconnect_recovers_an_exit_even_after_the_terminal_is_forgotten() {
    let f = Foreground::start("command::worker::terminal::tests::foreground_reconnect_recovers_an_exit_even_after_the_terminal_is_forgotten").await;
    f.server.server().drop_terminal(&f.lease.id).unwrap();
    f.terminal.exit(7);
    assert!(f.hub.forget(f.terminal.id()));
    wait_for_async(
        "foreground recovered exit",
        Duration::from_secs(5),
        || async {
            f.server
                .server()
                .terminal_connection_frames(&f.lease.id)
                .get(1)
                .filter(|frames| {
                    frames
                        .iter()
                        .any(|frame| matches!(frame.frame, TerminalFrame::Exit { code: Some(7) }))
                })
                .map(|_| ())
        },
    )
    .await;
    f.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn foreground_terminal_404_stops_retries_and_execution_completes_done() {
    let f = Foreground::start("command::worker::terminal::tests::foreground_terminal_404_stops_retries_and_execution_completes_done").await;
    f.server.server().refuse_terminal("foreground", "not_found");
    f.server.server().drop_terminal(&f.lease.id).unwrap();
    f.stopped().await;
    assert_eq!(f.server.server().terminal_open_attempts(&f.lease.id), 2);
    f.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn foreground_terminal_409_fences_and_discards_execution() {
    let f = Foreground::start(
        "command::worker::terminal::tests::foreground_terminal_409_fences_and_discards_execution",
    )
    .await;
    f.server
        .server()
        .refuse_terminal("foreground", "lease_lost");
    f.server.server().drop_terminal(&f.lease.id).unwrap();
    f.stopped().await;
    assert!(f.control.cancelled().is_cancelled());
    assert!(!f.control.may_report());
    assert!(f.server.server().result_for("foreground").is_none());
    assert_eq!(f.server.server().terminal_open_attempts(&f.lease.id), 2);
    f.done.cancel();
    f.control.stop();
    f.heartbeat.await.unwrap();
    drop(f.relay);
    f.server.shutdown().await;
    f.sandbox.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn foreground_resends_exit_when_the_socket_drops_after_the_step_exits() {
    let f = Foreground::start("command::worker::terminal::tests::foreground_resends_exit_when_the_socket_drops_after_the_step_exits").await;
    f.terminal.exit(0);
    wait_for_async("original exit", Duration::from_secs(5), || async {
        f.server
            .server()
            .terminal_frames(&f.lease.id)
            .iter()
            .any(|frame| matches!(frame.frame, TerminalFrame::Exit { .. }))
            .then_some(())
    })
    .await;
    f.server.server().drop_terminal(&f.lease.id).unwrap();
    f.snapshot(1).await;
    wait_for_async("replacement exit", Duration::from_secs(5), || async {
        f.server.server().terminal_connection_frames(&f.lease.id)[1]
            .iter()
            .any(|frame| matches!(frame.frame, TerminalFrame::Exit { code: Some(0) }))
            .then_some(())
    })
    .await;
    f.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn foreground_disconnect_releases_input_and_rejects_stale_generation_frames() {
    let f = Foreground::start("command::worker::terminal::tests::foreground_disconnect_releases_input_and_rejects_stale_generation_frames").await;
    f.server
        .server()
        .send_terminal(
            &f.lease.id,
            LeaseTerminalFrame {
                terminal_id: f.terminal.id().to_string(),
                frame: TerminalFrame::TakeControl,
            },
        )
        .unwrap();
    wait_for_async("input holder", Duration::from_secs(5), || async {
        f.hub
            .has_input_holder(f.terminal.id())
            .unwrap()
            .then_some(())
    })
    .await;
    let generation = f.bridge.status.borrow().generation;
    f.server.server().drop_terminal(&f.lease.id).unwrap();
    wait_for_async("input grant released", Duration::from_secs(5), || async {
        (!f.hub.has_input_holder(f.terminal.id()).unwrap()).then_some(())
    })
    .await;
    f.snapshot(1).await;
    let stale = f
        .bridge
        .exchange(TerminalExchange {
            generation,
            frames: vec![LeaseTerminalFrame {
                terminal_id: f.terminal.id().to_string(),
                frame: TerminalFrame::Exit { code: Some(99) },
            }],
        })
        .await
        .unwrap();
    assert!(!stale.accepted);
    assert_ne!(stale.generation, generation);
    assert!(
        !f.server.server().terminal_connection_frames(&f.lease.id)[1]
            .iter()
            .any(|frame| matches!(frame.frame, TerminalFrame::Exit { code: Some(99) }))
    );
    f.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn foreground_done_during_disconnect_opens_no_socket_after_the_result() {
    let f = Foreground::start("command::worker::terminal::tests::foreground_done_during_disconnect_opens_no_socket_after_the_result").await;
    f.server.server().drop_terminal(&f.lease.id).unwrap();
    // Cancel only once the socket owner has seen the drop and is in backoff.
    wait_for_async(
        "socket owner disconnected",
        Duration::from_secs(5),
        || async { f.bridge.status.borrow().generation.is_none().then_some(()) },
    )
    .await;
    f.done.cancel();
    f.bridge.stop().await;
    assert_eq!(f.server.server().terminal_open_attempts(&f.lease.id), 1);
    f.finish().await;
}
