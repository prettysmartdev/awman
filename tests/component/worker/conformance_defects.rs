//! Seeded protocol defects prove the reusable conformance suite can fail.
use crate::support;
use awman_test_support::TestSandbox;
use awman_wsp::mock::{MockFault, MockWorkServer};
use std::{sync::Arc, time::Duration};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn client_counts_empty_event_acknowledgements_and_duplicate_posts_are_idempotent() {
    let sandbox = support::sandbox("worker::conformance_defects::client_counts_empty_event_acknowledgements_and_duplicate_posts_are_idempotent");
    let listener = sandbox.ports().tcp_listener();
    listener.set_nonblocking(true).unwrap();
    let server = Arc::new(MockWorkServer::with_fault(MockFault::EmptyEvents));
    let serving = server
        .clone()
        .serve_on(tokio::net::TcpListener::from_std(listener).unwrap())
        .await
        .unwrap();
    let link = server.link();
    server.approve_link(&link.link_id, Default::default());
    let linker = awman_wsp::Client::new(&serving.base_url, String::new()).unwrap();
    let token = linker.poll_link(&link.link_id).await.unwrap().unwrap();
    sandbox.redact(&token.refresh_token);
    let client = awman_wsp::Client::new(&serving.base_url, token.refresh_token).unwrap();
    client
        .register(&awman_wsp::RegisterRequest {
            name: "events".into(),
            labels: Default::default(),
            capacity: 1,
            features: vec![],
        })
        .await
        .unwrap();
    server
        .enqueue(
            awman_wsp::conformance::work_item("events"),
            chrono::Utc::now(),
        )
        .unwrap();
    let lease = client.claim(Some("events"), 0).await.unwrap().unwrap();
    let event = serde_json::json!({"attempt":1,"sequence":1,"type":"workflow_state","data":{}});
    assert_eq!(
        client
            .events_accepted(&lease.id, vec![event.clone()])
            .await
            .unwrap(),
        1
    );
    client.events(&lease.id, vec![event]).await.unwrap();
    assert_eq!(server.event_count(&lease.id), Some(1));
    let addr = serving.addr;
    drop(serving);
    awman_test_support::wait_for_async(
        "empty-ack server closed",
        Duration::from_secs(5),
        || async move {
            tokio::net::TcpStream::connect(addr)
                .await
                .is_err()
                .then_some(())
        },
    )
    .await;
    sandbox.finish();
}

async fn rejects(sandbox: &TestSandbox, case: &str, fault: MockFault) -> bool {
    let listener = sandbox.ports().tcp_listener();
    listener.set_nonblocking(true).unwrap();
    let server = Arc::new(MockWorkServer::with_fault(fault));
    let serving = server
        .clone()
        .serve_on(tokio::net::TcpListener::from_std(listener).unwrap())
        .await
        .unwrap();
    let failed = awman_wsp::conformance::run_case(&serving.base_url, server.as_ref(), case)
        .await
        .is_err();
    let addr = serving.addr;
    drop(serving);
    awman_test_support::wait_for_async(
        "fault server closed",
        Duration::from_secs(5),
        || async move {
            tokio::net::TcpStream::connect(addr)
                .await
                .is_err()
                .then_some(())
        },
    )
    .await;
    failed
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn probes_detect_ignored_preferences_epochs_and_duplicate_event_storage() {
    let sandbox = support::sandbox("worker::conformance_defects::probes_detect_ignored_preferences_epochs_and_duplicate_event_storage");
    for (case, fault) in [
        ("priority_age_preference", MockFault::IgnorePreferences),
        ("stale_epoch_is_fenced", MockFault::IgnoreEpoch),
        ("idempotent_events", MockFault::DuplicateEvents),
    ] {
        assert!(
            rejects(&sandbox, case, fault).await,
            "undetected defect: {fault:?}"
        );
    }
    assert!(!rejects(&sandbox, "idempotent_events", MockFault::EmptyEvents).await);
    sandbox.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wire_probe_detects_invalid_fields_on_every_protocol_response() {
    let sandbox = support::sandbox(
        "worker::conformance_defects::wire_probe_detects_invalid_fields_on_every_protocol_response",
    );
    for endpoint in [
        "link",
        "token",
        "register",
        "queue",
        "claims",
        "heartbeat",
        "events",
        "credentials",
        "result",
        "release",
        "error",
    ] {
        assert!(
            rejects(&sandbox, "wire_conventions", MockFault::Wire(endpoint)).await,
            "undetected field corruption: {endpoint}"
        );
    }
    sandbox.finish();
}
