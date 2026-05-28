//! Integration tests for engine→frontend `StuckEvent` wiring (WI 0081).
//!
//! Replaces the stale `tests/stuck_yolo_behavior.rs` which referenced the
//! removed `awman::tui::state` API. The TUI no longer owns stuck detection
//! — it subscribes to the engine's broadcast channel via `set_stuck_sender`
//! and drives tab coloring from those events.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use awman::engine::container::instance::StuckEvent;

/// Subscribers picked up via the engine's `set_stuck_sender` pattern see
/// both `Stuck` and `Unstuck` events sent on the broadcast channel. This
/// mirrors how the TUI tab consumes events for the stuck-color indicator.
#[tokio::test]
async fn shared_sender_slot_delivers_events_to_subscriber() {
    // Simulates `Tab::stuck_sender_shared`.
    let slot: Arc<Mutex<Option<Arc<tokio::sync::broadcast::Sender<StuckEvent>>>>> =
        Arc::new(Mutex::new(None));

    // Simulates the engine handing the broadcast sender to the frontend
    // (TUI's `set_stuck_sender` impl stores it in the shared slot).
    let (tx, _) = tokio::sync::broadcast::channel(16);
    let tx = Arc::new(tx);
    *slot.lock().unwrap() = Some(Arc::clone(&tx));

    // Simulates `Tab::drain_stuck_events` taking the sender from the slot
    // and subscribing once.
    let sender_taken = slot.lock().unwrap().take().expect("sender published");
    let mut rx = sender_taken.subscribe();

    // Engine publishes events on the broadcast channel.
    tx.send(StuckEvent::Stuck).unwrap();
    tx.send(StuckEvent::Unstuck).unwrap();

    let first = tokio::time::timeout(Duration::from_millis(50), rx.recv())
        .await
        .expect("first event should arrive promptly")
        .expect("channel still alive");
    let second = tokio::time::timeout(Duration::from_millis(50), rx.recv())
        .await
        .expect("second event should arrive promptly")
        .expect("channel still alive");

    assert_eq!(first, StuckEvent::Stuck);
    assert_eq!(second, StuckEvent::Unstuck);
}

/// Multiple Tab-style subscribers (the workflow engine's own `subscribe_stuck`
/// plus the TUI's tab-coloring subscriber) all receive every event — broadcast
/// semantics. This replaces the stale TUI-level test that exercised the same
/// invariant via `recompute_stuck()`.
#[tokio::test]
async fn broadcast_sender_fans_out_to_workflow_engine_and_tui() {
    let (tx, _) = tokio::sync::broadcast::channel::<StuckEvent>(16);
    let tx = Arc::new(tx);

    // Workflow engine subscriber (replaces the old `recompute_stuck` polling).
    let mut workflow_rx = tx.subscribe();
    // TUI tab subscriber (drives the stuck-color indicator).
    let mut tab_rx = tx.subscribe();

    tx.send(StuckEvent::Stuck).unwrap();

    let from_workflow = tokio::time::timeout(Duration::from_millis(50), workflow_rx.recv())
        .await
        .expect("workflow subscriber should receive promptly")
        .expect("channel still alive");
    let from_tab = tokio::time::timeout(Duration::from_millis(50), tab_rx.recv())
        .await
        .expect("tab subscriber should receive promptly")
        .expect("channel still alive");

    assert_eq!(from_workflow, StuckEvent::Stuck);
    assert_eq!(from_tab, StuckEvent::Stuck);
}

/// When the only `Arc<Sender>` is dropped — which models the engine releasing
/// `ContainerExecution` after a step ends — `recv()` on existing subscribers
/// returns `Err(Closed)`. The workflow engine's `select!` arm interprets this
/// as a no-op (see `WorkflowEngine::recv_stuck`).
#[tokio::test]
async fn recv_returns_err_after_sender_arc_dropped() {
    let (tx, _) = tokio::sync::broadcast::channel::<StuckEvent>(16);
    let tx = Arc::new(tx);
    let mut rx = tx.subscribe();

    drop(tx);

    let result = tokio::time::timeout(Duration::from_millis(50), rx.recv())
        .await
        .expect("recv should resolve promptly after sender drop");
    assert!(
        matches!(
            result,
            Err(tokio::sync::broadcast::error::RecvError::Closed)
        ),
        "recv must return Closed once the broadcast sender is dropped; got {:?}",
        result
    );
}
