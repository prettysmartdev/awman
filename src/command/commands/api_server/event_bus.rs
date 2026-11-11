//! `EventBus` — ordered publication for worker command execution events.
//!
//! Created once per command/job execution. The API frontend's trait
//! implementations emit events through the sender. SSE connections subscribe
//! to the broadcast channel; the sole logfile writer uses a reliable queue.
//!
//! Layer 2 owns publication; HTTP and foreground frontends use its senders.

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
    awaiting_input: Option<crate::engine::worker::heartbeat::AwaitingInputSignal>,
}

#[derive(Default)]
struct Publication {
    next_sequence: u64,
    finished: bool,
    log: Option<mpsc::UnboundedSender<ExecutionEvent>>,
    followers: Vec<mpsc::UnboundedSender<ExecutionEvent>>,
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
            awaiting_input: None,
        }
    }

    pub fn attempt(&self) -> u32 {
        self.attempt
    }
    pub fn with_awaiting_input(
        mut self,
        signal: crate::engine::worker::heartbeat::AwaitingInputSignal,
    ) -> Self {
        self.awaiting_input = Some(signal);
        self
    }

    pub fn sender(&self) -> EventBusSender {
        EventBusSender {
            tx: self.tx.clone(),
            publication: Arc::clone(&self.publication),
            attempt: self.attempt,
            drains: self.drains.clone(),
            drain_shutdown: self.drain_shutdown.clone(),
            awaiting_input: self.awaiting_input.clone(),
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

    /// Lossless lease forwarding, registered before command publication.
    pub fn subscribe_ordered(&self) -> mpsc::UnboundedReceiver<ExecutionEvent> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.publication
            .lock()
            .expect("publication lock")
            .followers
            .push(tx);
        rx
    }

    pub fn subscribe(&self) -> broadcast::Receiver<ExecutionEvent> {
        self.tx.subscribe()
    }

    /// The sequence the next event of this attempt will carry.
    pub fn next_sequence(&self) -> u64 {
        self.publication
            .lock()
            .expect("publication lock poisoned")
            .next_sequence
    }
}

#[derive(Clone)]
pub struct EventBusSender {
    tx: broadcast::Sender<ExecutionEvent>,
    publication: Arc<Mutex<Publication>>,
    attempt: u32,
    drains: TaskTracker,
    drain_shutdown: tokio_util::sync::CancellationToken,
    awaiting_input: Option<crate::engine::worker::heartbeat::AwaitingInputSignal>,
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
    /// Hold this guard for the duration of a frontend dialog.
    pub fn awaiting_input(&self) -> Option<crate::engine::worker::heartbeat::AwaitingInputGuard> {
        self.awaiting_input.as_ref().map(|signal| signal.hold())
    }
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
        publication
            .followers
            .retain(|tx| tx.send(event.clone()).is_ok());
        let _ = self.tx.send(event);
    }
}
