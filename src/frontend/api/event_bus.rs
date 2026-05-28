//! `EventBus` — broadcast channel for execution events within the API server.
//!
//! Created once per command/job execution. The API frontend's trait
//! implementations emit events through the sender. Subscribers (logfile writer
//! task, SSE handler connections) hold Receiver handles.
//!
//! This is a Layer 3 type — the engine layer has no knowledge of it.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use tokio::sync::broadcast;

use crate::data::execution_event::{EventPayload, ExecutionEvent};

pub struct EventBus {
    tx: broadcast::Sender<ExecutionEvent>,
    sequence: Arc<AtomicU64>,
}

impl EventBus {
    pub fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        Self {
            tx,
            sequence: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn sender(&self) -> EventBusSender {
        EventBusSender {
            tx: self.tx.clone(),
            sequence: Arc::clone(&self.sequence),
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<ExecutionEvent> {
        self.tx.subscribe()
    }
}

#[derive(Clone)]
pub struct EventBusSender {
    tx: broadcast::Sender<ExecutionEvent>,
    sequence: Arc<AtomicU64>,
}

impl EventBusSender {
    pub fn emit(&self, payload: EventPayload) {
        let seq = self.sequence.fetch_add(1, Ordering::Relaxed);
        let event = ExecutionEvent {
            timestamp: chrono::Utc::now(),
            sequence: seq,
            payload,
        };
        let _ = self.tx.send(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn emit_and_receive() {
        let bus = EventBus::new(16);
        let mut rx = bus.subscribe();
        let sender = bus.sender();

        sender.emit(EventPayload::StdoutLine("hello".into()));
        sender.emit(EventPayload::StdoutLine("world".into()));
        sender.emit(EventPayload::Done);

        let e1 = rx.recv().await.unwrap();
        assert_eq!(e1.sequence, 0);
        assert!(matches!(e1.payload, EventPayload::StdoutLine(ref s) if s == "hello"));

        let e2 = rx.recv().await.unwrap();
        assert_eq!(e2.sequence, 1);

        let e3 = rx.recv().await.unwrap();
        assert_eq!(e3.sequence, 2);
        assert!(matches!(e3.payload, EventPayload::Done));
    }

    #[tokio::test]
    async fn multiple_subscribers() {
        let bus = EventBus::new(16);
        let mut rx1 = bus.subscribe();
        let mut rx2 = bus.subscribe();
        let sender = bus.sender();

        sender.emit(EventPayload::StdoutLine("test".into()));

        let e1 = rx1.recv().await.unwrap();
        let e2 = rx2.recv().await.unwrap();
        assert_eq!(e1.sequence, e2.sequence);
    }

    #[tokio::test]
    async fn emit_10_events_all_received_with_correct_sequence_numbers() {
        let bus = EventBus::new(32);
        let mut rx = bus.subscribe();
        let sender = bus.sender();

        for i in 0..10u32 {
            sender.emit(EventPayload::StdoutLine(format!("line {i}")));
        }
        sender.emit(EventPayload::Done);

        for expected_seq in 0u64..10 {
            let ev = rx.recv().await.unwrap();
            assert_eq!(ev.sequence, expected_seq);
            assert!(matches!(ev.payload, EventPayload::StdoutLine(_)));
        }
        let done = rx.recv().await.unwrap();
        assert_eq!(done.sequence, 10);
        assert!(matches!(done.payload, EventPayload::Done));
    }

    #[tokio::test]
    async fn three_subscribers_each_receive_all_five_events() {
        let bus = EventBus::new(32);
        let mut rx1 = bus.subscribe();
        let mut rx2 = bus.subscribe();
        let mut rx3 = bus.subscribe();
        let sender = bus.sender();

        for i in 0..5u32 {
            sender.emit(EventPayload::StdoutLine(format!("ev{i}")));
        }

        for rx in [&mut rx1, &mut rx2, &mut rx3] {
            for expected_seq in 0u64..5 {
                let ev = rx.recv().await.unwrap();
                assert_eq!(ev.sequence, expected_seq);
            }
        }
    }

    #[tokio::test]
    async fn lagged_receiver_gets_lag_error_then_remaining_events() {
        use tokio::sync::broadcast::error::RecvError;

        // capacity=4: buffer holds the most recent 4 events.
        let bus = EventBus::new(4);
        let mut rx = bus.subscribe();
        let sender = bus.sender();

        // Emit 10 events without reading — fills and overwrites the 4-slot buffer.
        for i in 0..10u32 {
            sender.emit(EventPayload::StdoutLine(format!("ev{i}")));
        }

        // The first recv must report Lagged(6) — 6 messages were lost.
        match rx.recv().await {
            Err(RecvError::Lagged(n)) => assert_eq!(n, 6, "expected 6 lagged messages"),
            other => panic!("expected Lagged(6), got {other:?}"),
        }

        // After lag recovery the receiver delivers the last 4 messages.
        for i in 6u32..10 {
            let ev = rx.recv().await.unwrap();
            match &ev.payload {
                EventPayload::StdoutLine(s) => assert_eq!(s, &format!("ev{i}")),
                other => panic!("unexpected payload {other:?}"),
            }
        }
    }
}
