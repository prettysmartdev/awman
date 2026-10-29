//! `EventBus` — broadcast channel for execution events within the API server.
//!
//! Created once per command/job execution. The API frontend's trait
//! implementations emit events through the sender. SSE connections subscribe
//! to the broadcast channel; the sole logfile writer uses a reliable queue.
//!
//! This is a Layer 3 type — the engine layer has no knowledge of it.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{broadcast, mpsc};
use tokio_util::task::TaskTracker;

use crate::data::execution_event::{EventPayload, EventSource, ExecutionEvent, FIRST_ATTEMPT};

pub struct EventBus {
    tx: broadcast::Sender<ExecutionEvent>,
    publication: Arc<Mutex<Publication>>,
    attempt: u32,
    drains: TaskTracker,
    drain_shutdown: tokio_util::sync::CancellationToken,
}

#[derive(Default)]
struct Publication {
    next_sequence: u64,
    finished: bool,
    log: Option<mpsc::UnboundedSender<ExecutionEvent>>,
}

impl EventBus {
    /// A bus for the first attempt of a command.
    pub fn new(capacity: usize) -> Self {
        Self::for_attempt(capacity, FIRST_ATTEMPT)
    }

    /// The bus for one execution of a command: every event carries `attempt`
    /// and sequence restarts at 0 for this attempt.
    pub fn for_attempt(capacity: usize, attempt: u32) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        Self {
            tx,
            publication: Arc::new(Mutex::new(Publication::default())),
            attempt,
            drains: TaskTracker::new(),
            drain_shutdown: Default::default(),
        }
    }

    pub fn attempt(&self) -> u32 {
        self.attempt
    }

    pub fn sender(&self) -> EventBusSender {
        EventBusSender {
            tx: self.tx.clone(),
            publication: Arc::clone(&self.publication),
            attempt: self.attempt,
            drains: self.drains.clone(),
            drain_shutdown: self.drain_shutdown.clone(),
            source: None,
        }
    }

    /// Wait for all stdout/stderr drains spawned by this bus, up to `timeout`.
    /// The bus belongs to one execution, so closing the tracker is permanent.
    pub async fn wait_output_drained(&self, timeout: Duration) -> bool {
        self.drains.close();
        tokio::time::timeout(timeout, self.drains.wait())
            .await
            .is_ok()
    }

    pub fn close_output_drains(&self) {
        self.drain_shutdown.cancel();
    }

    /// Register the sole persistence consumer before emitting any events.
    /// Broadcast receivers may lag; the log must retain every published id.
    pub fn subscribe_log(&self) -> mpsc::UnboundedReceiver<ExecutionEvent> {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut publication = self.publication.lock().expect("publication lock poisoned");
        assert!(publication.log.is_none() && publication.next_sequence == 0);
        publication.log = Some(tx);
        rx
    }

    pub fn subscribe(&self) -> broadcast::Receiver<ExecutionEvent> {
        self.tx.subscribe()
    }
}

#[derive(Clone)]
pub struct EventBusSender {
    tx: broadcast::Sender<ExecutionEvent>,
    publication: Arc<Mutex<Publication>>,
    attempt: u32,
    drains: TaskTracker,
    drain_shutdown: tokio_util::sync::CancellationToken,
    /// Stamped on every event this sender emits (WI 0137 §5). `None` on the
    /// bus's root sender; set by [`EventBusSender::with_source`] or
    /// [`EventBusSender::with_shared_source`].
    source: Option<SourceStamp>,
}

/// What a sender stamps: a fixed source, or one shared with whoever can
/// still learn more about it (a container that has not reported its name).
#[derive(Clone)]
enum SourceStamp {
    Fixed(EventSource),
    Shared(Arc<Mutex<EventSource>>),
}

impl SourceStamp {
    fn current(&self) -> EventSource {
        match self {
            Self::Fixed(source) => source.clone(),
            Self::Shared(cell) => cell.lock().expect("event source lock poisoned").clone(),
        }
    }
}

impl EventBusSender {
    pub fn output_shutdown(&self) -> tokio_util::sync::CancellationToken {
        self.drain_shutdown.clone()
    }

    pub fn attempt(&self) -> u32 {
        self.attempt
    }

    /// A sender that attributes everything it emits to `source` (WI 0137
    /// §5). Sequence numbers and the log stay shared with the bus; only the
    /// envelope's `source` differs. A per-container sink or a per-step
    /// frontend call site takes one of these instead of the root sender.
    pub fn with_source(&self, source: EventSource) -> Self {
        Self {
            source: Some(SourceStamp::Fixed(source)),
            ..self.clone()
        }
    }

    /// A sender that stamps whatever `cell` holds at the moment of each
    /// emit. A container's output drains are created with one so that the
    /// container name, reported after the drains exist on some runtimes,
    /// still reaches every line.
    pub fn with_shared_source(&self, cell: Arc<Mutex<EventSource>>) -> Self {
        Self {
            source: Some(SourceStamp::Shared(cell)),
            ..self.clone()
        }
    }

    /// The source this sender stamps right now, if any.
    pub fn source(&self) -> Option<EventSource> {
        self.source.as_ref().map(SourceStamp::current)
    }

    /// Spawn an output drain tracked by the bus's completion barrier.
    pub fn spawn_drain<F>(&self, future: F) -> tokio::task::JoinHandle<()>
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        self.drains.spawn(future)
    }

    pub fn emit(&self, payload: EventPayload) {
        // Allocation and both publications are one operation across all
        // producer clones. No cursor can overtake an earlier event.
        let mut publication = self.publication.lock().expect("publication lock poisoned");
        self.publish(&mut publication, payload);
    }

    /// Publish the final status and Done under one lock so even a timed-out
    /// output drain cannot interleave output between the terminal pair.
    pub fn finish(&self, status: EventPayload) {
        debug_assert!(matches!(status, EventPayload::CommandStatus { .. }));
        let mut publication = self.publication.lock().expect("publication lock poisoned");
        self.publish(&mut publication, status);
        self.publish(&mut publication, EventPayload::Done);
    }

    fn publish(&self, publication: &mut Publication, payload: EventPayload) {
        if publication.finished {
            return;
        }
        publication.finished = matches!(payload, EventPayload::Done);
        let event = ExecutionEvent {
            timestamp: chrono::Utc::now(),
            attempt: self.attempt,
            sequence: publication.next_sequence,
            source: self.source(),
            payload,
        };
        publication.next_sequence += 1;
        if let Some(log) = &publication.log {
            let _ = log.send(event.clone());
        }
        let _ = self.tx.send(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrent_publishers_keep_both_consumers_ordered_and_done_fences_output() {
        let bus = EventBus::for_attempt(16_384, 2);
        let mut live = bus.subscribe();
        let mut log = bus.subscribe_log();
        let start = Arc::new(std::sync::Barrier::new(8));
        std::thread::scope(|scope| {
            for producer in 0..8 {
                let sender = bus.sender();
                let start = Arc::clone(&start);
                scope.spawn(move || {
                    start.wait();
                    for line in 0..1_000 {
                        sender.emit(EventPayload::StdoutLine(format!("{producer}:{line}")));
                    }
                });
            }
        });
        bus.sender().emit(EventPayload::Done);
        bus.sender()
            .emit(EventPayload::StdoutLine("too late".into()));
        bus.sender().emit(EventPayload::Done);
        for sequence in 0..=8_000 {
            let event = live.try_recv().unwrap();
            let persisted = log.try_recv().unwrap();
            assert_eq!(
                event.cursor(),
                crate::data::execution_event::EventCursor::new(2, sequence)
            );
            assert_eq!(
                serde_json::to_value(&event).unwrap(),
                serde_json::to_value(persisted).unwrap()
            );
            assert_eq!(
                matches!(event.payload, EventPayload::Done),
                sequence == 8_000
            );
        }
        assert!(live.try_recv().is_err());
        assert!(log.try_recv().is_err());
    }

    #[tokio::test]
    async fn sender_with_source_stamps_its_events_and_shares_the_sequence() {
        use crate::data::execution_event::EventSourcePhase;

        let bus = EventBus::new(16);
        let mut rx = bus.subscribe();
        let root = bus.sender();
        let step = root.with_source(EventSource::step(EventSourcePhase::Main, "tests", 1));

        root.emit(EventPayload::StdoutLine("root".into()));
        step.emit(EventPayload::StdoutLine("step".into()));
        root.emit(EventPayload::Done);

        let first = rx.recv().await.unwrap();
        assert_eq!(first.sequence, 0);
        assert!(first.source.is_none());
        let second = rx.recv().await.unwrap();
        assert_eq!(second.sequence, 1);
        assert_eq!(
            second.source.as_ref().and_then(|s| s.step_name.as_deref()),
            Some("tests")
        );
        assert_eq!(step.source().map(|s| s.step_index), Some(Some(1)));
        let done = rx.recv().await.unwrap();
        assert_eq!(done.sequence, 2);
        assert!(done.source.is_none());
    }

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
