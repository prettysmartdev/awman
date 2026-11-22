//! `EventBus` — ordered publication for worker command execution events.
//!
//! Created once per command/job execution. The API frontend's trait
//! implementations emit events through the sender. SSE connections subscribe
//! to the broadcast channel. The ordered logfile writer acknowledges lifecycle
//! flushes before completion reaches any subscriber.
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
    redaction: crate::engine::auth::redaction::RedactionSet,
    next_sequence: u64,
    finished: bool,
    log: Option<mpsc::UnboundedSender<ExecutionEvent>>,
    followers: Vec<mpsc::UnboundedSender<ExecutionEvent>>,
    durable_log: Option<Box<dyn crate::data::fs::api_command_log::EventLog>>,
    failure: Option<String>,
    recovery_required: bool,
    reserved: Option<crate::data::automation_store::LifecycleProjection>,
    deferred: Vec<ExecutionEvent>,
    deferred_spool: Option<std::fs::File>,
    deferred_bytes: usize,
}

impl EventBus {
    pub fn redaction(&self) -> crate::engine::auth::redaction::RedactionSet {
        self.sender().redaction()
    }
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

    /// Install the sole ordered writer before the first event.
    pub fn set_log_writer(
        &self,
        writer: Result<
            Box<dyn crate::data::fs::api_command_log::EventLog>,
            crate::data::error::DataError,
        >,
    ) {
        let mut p = self.publication.lock().expect("publication lock");
        assert!(p.next_sequence == 0 && p.durable_log.is_none());
        match writer {
            Ok(writer) => p.durable_log = Some(writer),
            Err(e) => p.failure = Some(e.to_string()),
        }
    }

    pub fn recovery_required(&self) -> bool {
        self.publication
            .lock()
            .expect("publication lock")
            .recovery_required
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
    /// Transport health only: never allocates an event cursor or fabricates Done.
    pub(crate) fn require_recovery(&self) {
        self.publication
            .lock()
            .expect("publication lock")
            .recovery_required = true;
    }

    pub fn redaction(&self) -> crate::engine::auth::redaction::RedactionSet {
        self.publication
            .lock()
            .expect("publication lock")
            .redaction
            .clone()
    }
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
        if publication.failure.is_some() || publication.finished {
            return;
        }
        if publication.reserved.is_some() {
            let event = ExecutionEvent {
                timestamp: chrono::Utc::now(),
                attempt: self.attempt,
                sequence: publication.next_sequence,
                source: self
                    .source()
                    .and_then(|s| publication.redaction.redact_source(s)),
                payload: publication.redaction.redact_event(payload),
            };
            let bytes = serde_json::to_vec(&event).map_or(usize::MAX, |v| v.len());
            let written = (|| -> std::io::Result<()> {
                use std::io::Write as _;
                if publication.deferred_spool.is_none()
                    && (publication.deferred.len() >= 1024
                        || bytes
                            > (8 * 1024 * 1024usize).saturating_sub(publication.deferred_bytes))
                {
                    let mut file = tempfile::tempfile()?;
                    for previous in std::mem::take(&mut publication.deferred) {
                        serde_json::to_writer(&mut file, &previous)?;
                        file.write_all(b"\n")?;
                    }
                    publication.deferred_bytes = 0;
                    publication.deferred_spool = Some(file);
                }
                if let Some(file) = &mut publication.deferred_spool {
                    serde_json::to_writer(&mut *file, &event)?;
                    file.write_all(b"\n")?;
                } else {
                    publication.deferred_bytes += bytes;
                    publication.deferred.push(event);
                }
                Ok(())
            })();
            match written {
                Ok(()) => publication.next_sequence += 1,
                Err(error) => {
                    publication.failure = Some(format!("deferred output storage failed: {error}"))
                }
            }
            return;
        }
        self.publish(&mut publication, payload);
    }

    /// Publish the final status and Done under one lock so even a timed-out
    /// output drain cannot interleave output between the terminal pair.
    pub fn finish(&self, status: EventPayload) {
        debug_assert!(matches!(status, EventPayload::CommandStatus { .. }));
        let mut publication = self.publication.lock().expect("publication lock poisoned");
        self.publish(&mut publication, status);
        self.publish(&mut publication, EventPayload::Done);
        publication.redaction.clear();
    }

    /// Fan out only the terminal records already flushed by in-process recovery.
    pub(crate) fn recovered_terminal(&self, events: impl IntoIterator<Item = ExecutionEvent>) {
        let mut p = self.publication.lock().expect("publication lock");
        for event in events {
            if event.attempt == self.attempt
                && matches!(
                    event.payload,
                    EventPayload::CommandStatus { .. } | EventPayload::Done
                )
            {
                p.followers.retain(|tx| tx.send(event.clone()).is_ok());
                let _ = self.tx.send(event);
            }
        }
        p.finished = true;
    }
    pub fn try_finish(&self, status: EventPayload) -> Result<(), crate::data::error::DataError> {
        self.finish(status);
        let p = self.publication.lock().expect("publication lock");
        match &p.failure {
            Some(e) => Err(crate::data::error::DataError::Other(e.clone())),
            None if p.reserved.is_some() => Err(crate::data::error::DataError::Other(
                "lifecycle publication pending".into(),
            )),
            None => Ok(()),
        }
    }

    fn publish(&self, publication: &mut Publication, payload: EventPayload) {
        if publication.finished || publication.failure.is_some() || publication.reserved.is_some() {
            return;
        }
        publication.finished = matches!(payload, EventPayload::Done);
        let payload = publication.redaction.redact_event(payload);
        let event = ExecutionEvent {
            timestamp: chrono::Utc::now(),
            attempt: self.attempt,
            sequence: publication.next_sequence,
            source: self
                .source()
                .and_then(|source| publication.redaction.redact_source(source)),
            payload,
        };
        publication.next_sequence += 1;
        self.publish_event(publication, event);
    }

    fn publish_event(&self, publication: &mut Publication, event: ExecutionEvent) {
        if let Some(log) = &mut publication.durable_log {
            let saved = log.append(&event).and_then(|()| {
                if matches!(
                    event.payload,
                    EventPayload::CommandStatus { .. } | EventPayload::Done
                ) {
                    log.flush()
                } else {
                    Ok(())
                }
            });
            if let Err(e) = saved {
                publication.failure = Some(e.to_string());
                return;
            }
        }
        if let Some(log) = &publication.log {
            let _ = log.send(event.clone());
        }
        publication
            .followers
            .retain(|tx| tx.send(event.clone()).is_ok());
        let _ = self.tx.send(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn sender_clones_redact_before_log_ordered_and_broadcast_consumers() {
        let bus = EventBus::new(16);
        let mut log = bus.subscribe_log();
        let mut ordered = bus.subscribe_ordered();
        let mut broadcast = bus.subscribe();
        let sender = bus.sender();
        let clone = sender.clone();
        clone
            .redaction()
            .add(crate::data::secret::SecretString::new("test-secret-bus"));
        let mut source = EventSource::phase(crate::data::execution_event::EventSourcePhase::Main);
        source.step_name = Some("test-secret-bus".into());
        let sender = sender.with_source(source);
        sender.emit(EventPayload::StdoutLine(
            "test-secret-bus test-secret-bus".into(),
        ));
        for event in [
            log.recv().await.unwrap(),
            ordered.recv().await.unwrap(),
            broadcast.recv().await.unwrap(),
        ] {
            assert!(
                matches!(event.payload, EventPayload::StdoutLine(ref line) if line == "*** ***")
            );
            assert_eq!(event.sequence, 0);
            assert_eq!(event.source.unwrap().step_name.as_deref(), Some("***"));
        }
    }
}

/// A reservation is never observable until both its database transaction and log
/// flush succeeded. Dropped/failed reservations fence this bus until recovery.
#[derive(Clone)]
pub struct LifecyclePublisher {
    sender: Arc<EventBusSender>,
}
impl EventBusSender {
    pub fn lifecycle_publisher(&self) -> LifecyclePublisher {
        LifecyclePublisher {
            sender: Arc::new(self.clone()),
        }
    }
}
impl LifecyclePublisher {
    pub fn ensure_available(&self) -> Result<(), crate::data::error::DataError> {
        let p = self.sender.publication.lock().expect("publication lock");
        if p.finished || p.failure.is_some() || p.durable_log.is_none() || p.reserved.is_some() {
            return Err(crate::data::error::DataError::Other(format!(
                "lifecycle writer unavailable: {}",
                p.failure
                    .as_deref()
                    .unwrap_or("missing writer or pending publication")
            )));
        }
        Ok(())
    }
    pub fn emit(&self, payload: EventPayload) {
        self.sender.emit(payload);
    }
    /// The command set must also redact the result before its immutable commit.
    pub fn redaction(&self) -> crate::engine::auth::redaction::RedactionSet {
        self.sender.redaction()
    }

    pub fn abort(&self, error: &str) {
        self.sender
            .publication
            .lock()
            .expect("publication lock")
            .failure = Some(error.to_owned());
    }
    pub fn reserve(
        &self,
        payloads: Vec<EventPayload>,
    ) -> Result<crate::data::automation_store::LifecycleProjection, crate::data::error::DataError>
    {
        use crate::data::{automation_store::LifecycleProjection, error::DataError};
        let mut p = self.sender.publication.lock().expect("publication lock");
        if p.finished || p.reserved.is_some() || p.failure.is_some() || p.durable_log.is_none() {
            return Err(DataError::Other(format!(
                "lifecycle publication unavailable: {}",
                p.failure
                    .as_deref()
                    .unwrap_or("missing writer or outstanding reservation")
            )));
        }
        if payloads.is_empty() || payloads.len() > 8 {
            return Err(DataError::Other("invalid lifecycle batch".into()));
        }
        let mut events = Vec::new();
        for payload in payloads {
            events.push(ExecutionEvent {
                timestamp: chrono::Utc::now(),
                attempt: self.sender.attempt,
                sequence: p.next_sequence,
                source: self
                    .sender
                    .source()
                    .and_then(|s| p.redaction.redact_source(s)),
                payload: p.redaction.redact_event(payload),
            });
            p.next_sequence += 1;
        }
        let batch = LifecycleProjection {
            cursor: events.last().expect("nonempty").cursor(),
            events,
        };
        if serde_json::to_vec(&batch.events)
            .map_err(|e| DataError::Other(e.to_string()))?
            .len()
            > 16 * 1024 * 1024
        {
            return Err(DataError::Other("lifecycle batch exceeds bound".into()));
        }
        p.reserved = Some(batch.clone());
        Ok(batch)
    }
    pub fn publish(
        &self,
        batch: &crate::data::automation_store::LifecycleProjection,
    ) -> Result<(), crate::data::error::DataError> {
        use crate::data::error::DataError;
        let mut p = self.sender.publication.lock().expect("publication lock");
        if p.failure.is_some()
            || p.reserved.as_ref().map(|b| b.cursor) != Some(batch.cursor)
            || serde_json::to_value(p.reserved.as_ref().map(|b| &b.events))
                .map_err(|e| DataError::Other(e.to_string()))?
                != serde_json::to_value(&batch.events)
                    .map_err(|e| DataError::Other(e.to_string()))?
        {
            return Err(DataError::Other(
                "invalid or failed lifecycle reservation".into(),
            ));
        }
        let log = p
            .durable_log
            .as_mut()
            .ok_or_else(|| DataError::Other("missing lifecycle writer".into()))?;
        let saved = batch
            .events
            .iter()
            .try_for_each(|event| log.append(event))
            .and_then(|()| log.flush());
        if let Err(e) = saved {
            p.failure = Some(e.to_string());
            return Err(DataError::Other(format!(
                "completion recorded, not yet published: {e}"
            )));
        }
        for event in &batch.events {
            if let Some(log) = &p.log {
                let _ = log.send(event.clone());
            }
            p.followers.retain(|tx| tx.send(event.clone()).is_ok());
            let _ = self.sender.tx.send(event.clone());
        }
        p.reserved = None;
        p.deferred_bytes = 0;
        for event in std::mem::take(&mut p.deferred) {
            if p.failure.is_some() {
                break;
            }
            self.sender.publish_event(&mut p, event);
        }
        if let Some(mut file) = p.deferred_spool.take() {
            use std::io::{BufRead as _, Seek as _};
            let replay = (|| -> std::io::Result<()> {
                file.rewind()?;
                for line in std::io::BufReader::new(file).lines() {
                    let event = serde_json::from_str(&line?)?;
                    self.sender.publish_event(&mut p, event);
                    if p.failure.is_some() {
                        break;
                    }
                }
                Ok(())
            })();
            if let Err(error) = replay {
                p.failure = Some(format!("deferred output replay failed: {error}"));
            }
        }
        // Completion itself was flushed and delivered. A later output error
        // fences future publications; it cannot retract this acknowledgement.
        Ok(())
    }
}

#[cfg(test)]
mod deferred_output_tests {
    use super::*;
    use crate::data::fs::api_command_log::OrderedCommandLog;
    use crate::data::fs::api_paths::ApiPaths;

    #[test]
    fn output_burst_spills_without_poisoning_committed_completion() {
        let s = awman_test_support::TestSandbox::for_test(awman_test_support::TestCase::component(
            "event_bus::output_burst_spills_without_poisoning_committed_completion",
        ));
        let paths = ApiPaths::from_root(s.tmp().join("api"));
        let bus = EventBus::new(16);
        bus.set_log_writer(Ok(Box::new(
            OrderedCommandLog::open(&paths, "session", "command").unwrap(),
        )));
        let mut observed = bus.subscribe_ordered();
        let publisher = bus.sender().lifecycle_publisher();
        let batch = publisher
            .reserve(vec![EventPayload::StatusMessage {
                phase: "completion".into(),
                message: "verified".into(),
            }])
            .unwrap();
        for n in 0..1100 {
            bus.sender()
                .emit(EventPayload::StdoutLine(format!("line-{n}")));
        }
        assert!(
            observed.try_recv().is_err(),
            "output cannot overtake the commit"
        );
        publisher.publish(&batch).unwrap();
        let events: Vec<_> = std::iter::from_fn(|| observed.try_recv().ok()).collect();
        assert_eq!(events.len(), 1101);
        assert!(events
            .windows(2)
            .all(|pair| pair[0].sequence < pair[1].sequence));
        assert!(
            matches!(&events.last().unwrap().payload, EventPayload::StdoutLine(line) if line == "line-1099")
        );
        assert!(publisher
            .reserve(vec![EventPayload::StatusMessage {
                phase: "next".into(),
                message: "healthy".into()
            }])
            .is_ok());
        drop(observed);
        drop(publisher);
        drop(bus);
        s.finish();
    }
}
