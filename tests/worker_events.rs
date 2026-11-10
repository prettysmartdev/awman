//! Worker end to end (WI 0143), checklist B8: `workflow_state` snapshots
//! arrive in order on push SSE and on WSP `POST /v1/leases/{id}/events`,
//! each after it was persisted, including the initial and the final one.
//!
//! One command attempt runs on the real push API (router, event bus,
//! reliable log writer). Its persisted log is teed into the real WSP
//! `EventForwarder`, posting to the mock work server for a claimed lease.
//! Both readers must see the same ordered log, event for event.

#[path = "helpers/push_api.rs"]
mod push_api;
#[path = "helpers/smart_http.rs"]
mod smart_http;
#[path = "helpers/wsp_lease.rs"]
mod wsp_lease;

use std::sync::Arc;
use std::time::Duration;

use awman::data::execution_event::EventPayload;
use awman::engine::worker::events::{ChannelEventSource, EventForwarder};
use awman::engine::worker::{LeaseApi, LeaseRef};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use wsp_lease::{claim, item, labels, Wsp};

const COMMAND: &str = "cmd_5ebc828d04f6b77e3840ab71c121ae91";

fn snapshot(n: u64, status: &str) -> Value {
    json!({
        "workflow": "wi-0143",
        "snapshot": n,
        "steps": [{ "name": "implement", "status": status }],
    })
}

fn payload_type(event: &Value) -> String {
    event["payload"]["type"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn workflow_state_events_arrive_in_sequence_on_sse_and_wsp_including_initial_and_final() {
    let tmp = tempfile::tempdir().unwrap();
    let state = push_api::make_state(&tmp.path().join("api"));
    let base = push_api::serve(state.clone()).await;
    let wsp = Wsp::start().await;
    wsp.enqueue(item("01JEVENTS", "https://example.invalid/repo.git"));
    let worker = wsp.worker("w", 1, &[], labels(&[]), labels(&[])).await;
    let lease = claim(&worker.session, None).await.unwrap();

    push_api::enqueue(&state, COMMAND);
    let (tee, rx) = mpsc::channel(256);
    let attempt = push_api::start_attempt(&state, COMMAND, Some(tee)).await;
    let api: Arc<dyn LeaseApi> = worker.session.clone();
    let forwarder = tokio::spawn(EventForwarder::run(
        api,
        LeaseRef::of(&lease),
        Box::new(ChannelEventSource::new(rx).with_window(Duration::from_millis(20))),
        CancellationToken::new(),
    ));

    // Initial snapshot, a step's progress, an intermediate snapshot, output,
    // the final snapshot, then `done`.
    attempt.emit(EventPayload::WorkflowState(snapshot(0, "pending")));
    attempt.emit(EventPayload::StatusMessage {
        phase: "step".into(),
        message: "starting implement".into(),
    });
    attempt.emit(EventPayload::WorkflowState(snapshot(1, "running")));
    attempt.emit(EventPayload::StdoutLine("working".into()));
    // Give the forwarder a batch boundary mid-run: delivery is incremental.
    tokio::time::sleep(Duration::from_millis(100)).await;
    attempt.emit(EventPayload::WorkflowState(snapshot(2, "succeeded")));
    attempt.finish(&state, COMMAND, None).await;
    let delivered = tokio::time::timeout(Duration::from_secs(20), forwarder)
        .await
        .expect("forwarder finishes after done")
        .unwrap()
        .expect("events delivered");
    assert_eq!(delivered, 6);

    let sse: Vec<Value> = push_api::Sse::open(&base, COMMAND)
        .await
        .until_done()
        .await
        .iter()
        .map(push_api::Frame::json)
        .collect();
    let wsp_events = wsp.mock.events_for(&lease.id);

    // Same ordered log on both transports, event for event.
    assert_eq!(sse.len(), 6, "{sse:#?}");
    assert_eq!(sse, wsp_events, "push SSE and WSP carry identical events");
    let sequences: Vec<u64> = wsp_events
        .iter()
        .map(|e| e["sequence"].as_u64().unwrap())
        .collect();
    assert!(sequences.windows(2).all(|w| w[0] < w[1]), "{sequences:?}");
    let states: Vec<&Value> = wsp_events
        .iter()
        .filter(|e| payload_type(e) == "workflow_state")
        .collect();
    let snapshots: Vec<u64> = states
        .iter()
        .map(|e| e["payload"]["data"]["snapshot"].as_u64().unwrap())
        .collect();
    assert_eq!(snapshots, [0, 1, 2], "every snapshot, in order");
    assert_eq!(
        payload_type(&wsp_events[0]),
        "workflow_state",
        "initial snapshot first"
    );
    assert_eq!(
        payload_type(&wsp_events[wsp_events.len() - 2]),
        "workflow_state",
        "final snapshot right before done"
    );
    // Existing variants keep their Rust tag on the wire (`Done`); only the
    // The public projection uses the spec spelling `workflow_state`.
    assert_eq!(payload_type(wsp_events.last().unwrap()), "done");
    // Idempotent by (attempt, sequence): re-posting the log adds nothing.
    let replay: Vec<awman::data::execution_event::ExecutionEvent> = wsp_events
        .iter()
        .map(|e| serde_json::from_value(e.clone()).unwrap())
        .collect();
    let accepted = worker
        .session
        .post_events(&LeaseRef::of(&lease), &replay)
        .await
        .unwrap();
    assert_eq!(accepted, 0);
    assert_eq!(wsp.mock.event_count(&lease.id), Some(6));
}
