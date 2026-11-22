//! Component evidence for terminal replacement and intentional socket drops.
use awman_test_support::{
    fakes::FakeWorkServer, wait_for_async, DeadlineBudget, TestCase, TestSandbox,
};
use awman_wsp::{
    conformance,
    mock::{MockFault, MockWorkServer},
    terminal::{LeaseTerminalFrame, TerminalFrame},
    Client, RegisterRequest,
};
use futures_util::{SinkExt, StreamExt};
use std::{sync::Arc, time::Duration};

#[tokio::test]
async fn reconnect_probe_accepts_replacement_and_rejects_a_server_retaining_the_old_socket() {
    let sandbox =
        TestSandbox::for_test(TestCase::component("terminal_reconnect::replacement_probe"));
    let server = FakeWorkServer::start(&sandbox).await;
    conformance::run_case(
        server.base_url(),
        server.server().as_ref(),
        "terminal_reconnect",
    )
    .await
    .unwrap();
    server.shutdown().await;
    let faulty = FakeWorkServer::start_with(
        &sandbox,
        Arc::new(MockWorkServer::with_fault(
            MockFault::IgnoreTerminalReplacement,
        )),
    )
    .await;
    // The probe names the step the server failed, not just any error.
    let error = conformance::run_case(
        faulty.base_url(),
        faulty.server().as_ref(),
        "terminal_reconnect",
    )
    .await
    .unwrap_err();
    assert!(
        matches!(
            error,
            awman_wsp::Error::ConformanceCheck(step) if step.contains("superseded socket was not closed")
        ),
        "{error}"
    );
    faulty.shutdown().await;
    sandbox.finish();
}

#[tokio::test]
async fn dropped_terminal_accepts_same_epoch_replacement_and_refreshes_access_tokens() {
    let sandbox =
        TestSandbox::for_test(TestCase::component("terminal_reconnect::intentional_drop"));
    let server = FakeWorkServer::start(&sandbox).await;
    let link = server.server().link();
    server
        .server()
        .approve_link(&link.link_id, Default::default());
    let linker = Client::new(server.base_url(), String::new()).unwrap();
    let token = linker.poll_link(&link.link_id).await.unwrap().unwrap();
    sandbox.redact(&token.refresh_token);
    let client = Client::new(server.base_url(), token.refresh_token).unwrap();
    client
        .register(&RegisterRequest {
            name: "reconnect".into(),
            labels: Default::default(),
            capacity: 1,
            features: vec![awman_wsp::features::TERMINAL.into()],
        })
        .await
        .unwrap();
    server
        .server()
        .enqueue(conformance::work_item("drop"), chrono::Utc::now())
        .unwrap();
    server.server().set_terminal("drop", true, false);
    let lease = client.claim(Some("drop"), 0).await.unwrap().unwrap();
    let client = client.with_lease_epoch(&lease.id, lease.epoch);
    let mut first = client.open_terminal(&lease.id).await.unwrap();
    wait_for_async("first socket", Duration::from_secs(1), || async {
        (server.server().terminal_connection_frames(&lease.id).len() == 1).then_some(())
    })
    .await;
    server.server().drop_terminal(&lease.id).unwrap();
    let budget = DeadlineBudget::new(Duration::from_secs(2));
    assert!(tokio::time::timeout(budget.remaining(), first.next())
        .await
        .unwrap()
        .is_none());
    server.server().invalidate_access_tokens();
    let mut replacement = client.open_terminal(&lease.id).await.unwrap();
    let snapshot = LeaseTerminalFrame {
        terminal_id: "term_11111111111111111111111111111111".into(),
        frame: TerminalFrame::Snapshot {
            has_output: false,
            cols: 80,
            rows: 24,
            screen: String::new(),
        },
    };
    replacement.send(snapshot.clone()).await.unwrap();
    wait_for_async("replacement snapshot", Duration::from_secs(1), || async {
        let sockets = server.server().terminal_connection_frames(&lease.id);
        (sockets.len() == 2 && sockets[1] == [snapshot.clone()]).then_some(())
    })
    .await;
    let stale = client.with_lease_epoch(&lease.id, lease.epoch + 1);
    assert!(stale
        .open_terminal(&lease.id)
        .await
        .unwrap_err()
        .is_lease_lost());
    drop(replacement);
    server.shutdown().await;
    sandbox.finish();
}
