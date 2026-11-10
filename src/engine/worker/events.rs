//! Forward a command's ordered execution-event log to a work server.
//!
//! The same ordered log feeds push SSE and WSP (`POST /v1/leases/{id}/events`),
//! including `workflow_state` snapshots. Delivery is idempotent by
//! `(attempt, sequence)` on the server, so a batch that failed in transport is
//! re-sent unchanged (never reordered) with bounded backoff. A `409
//! lease_lost` stops forwarding at once.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::error::WorkerEngineError;
use super::session::{LeaseApi, LeaseRef};
use crate::data::execution_event::{EventPayload, ExecutionEvent};

/// Largest batch posted at once.
pub const MAX_BATCH: usize = 100;
/// How long a partial batch waits for more events.
pub const BATCH_WINDOW: Duration = Duration::from_millis(500);

/// An ordered source of a command's events. `None` means the log is closed
/// (after the final `done`).
#[async_trait]
pub trait OrderedEventSource: Send {
    async fn next_batch(&mut self, max: usize) -> Option<Vec<ExecutionEvent>>;
}

/// [`OrderedEventSource`] over a channel fed by the command's event bus.
/// Closes after a `done` event or when every sender is dropped.
pub struct ChannelEventSource {
    rx: mpsc::Receiver<ExecutionEvent>,
    window: Duration,
    closed: bool,
}

impl ChannelEventSource {
    pub fn new(rx: mpsc::Receiver<ExecutionEvent>) -> Self {
        Self {
            rx,
            window: BATCH_WINDOW,
            closed: false,
        }
    }

    pub fn with_window(mut self, window: Duration) -> Self {
        self.window = window;
        self
    }
}

#[async_trait]
impl OrderedEventSource for ChannelEventSource {
    async fn next_batch(&mut self, max: usize) -> Option<Vec<ExecutionEvent>> {
        if self.closed {
            return None;
        }
        let first = self.rx.recv().await?;
        let mut batch = vec![first];
        let deadline = tokio::time::Instant::now() + self.window;
        while batch.len() < max
            && !matches!(batch.last().map(|e| &e.payload), Some(EventPayload::Done))
        {
            match tokio::time::timeout_at(deadline, self.rx.recv()).await {
                Ok(Some(event)) => batch.push(event),
                Ok(None) => {
                    self.closed = true;
                    break;
                }
                Err(_) => break,
            }
        }
        if matches!(batch.last().map(|e| &e.payload), Some(EventPayload::Done)) {
            self.closed = true;
        }
        Some(batch)
    }
}

pub struct EventForwarder;

impl EventForwarder {
    /// Forward until the source closes or `stop` fires; returns the number
    /// of events delivered. Events at or before the last delivered
    /// `(attempt, sequence)` are skipped, so a replaying source never sends
    /// a duplicate or an out-of-order event.
    pub async fn run(
        api: Arc<dyn LeaseApi>,
        lease: LeaseRef,
        mut source: Box<dyn OrderedEventSource>,
        stop: CancellationToken,
    ) -> Result<u64, WorkerEngineError> {
        let mut delivered = 0u64;
        let mut last: Option<(u32, u64)> = None;
        loop {
            let batch = tokio::select! {
                biased;
                _ = stop.cancelled() => return Ok(delivered),
                batch = source.next_batch(MAX_BATCH) => batch,
            };
            let Some(batch) = batch else {
                return Ok(delivered);
            };
            let batch: Vec<ExecutionEvent> = batch
                .into_iter()
                .filter(|e| last.is_none_or(|l| (e.attempt, e.sequence) > l))
                .collect();
            if batch.is_empty() {
                continue;
            }
            let mut backoff = Duration::from_millis(250);
            loop {
                let sent = tokio::select! {
                    biased;
                    _ = stop.cancelled() => return Ok(delivered),
                    sent = api.post_events(&lease, &batch) => sent,
                };
                match sent {
                    Ok(_) => break,
                    Err(e) if e.is_retryable() => {
                        tracing::warn!(lease = %lease.server_lease_id, error = %e, "lease events failed; resending");
                        tokio::select! {
                            biased;
                            _ = stop.cancelled() => return Ok(delivered),
                            _ = tokio::time::sleep(backoff) => {}
                        }
                        backoff = (backoff * 2).min(Duration::from_secs(5));
                    }
                    Err(e) => return Err(e),
                }
            }
            let tail = batch.last().expect("non-empty batch");
            last = Some((tail.attempt, tail.sequence));
            delivered += batch.len() as u64;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awman_wsp::{Credentials, HeartbeatRequest, HeartbeatResponse};
    use std::collections::VecDeque;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Recorder {
        posts: Mutex<Vec<Vec<(u32, u64)>>>,
        answers: Mutex<VecDeque<Result<usize, WorkerEngineError>>>,
    }

    #[async_trait]
    impl LeaseApi for Recorder {
        async fn heartbeat(
            &self,
            _: &LeaseRef,
            _: &HeartbeatRequest,
        ) -> Result<HeartbeatResponse, WorkerEngineError> {
            unreachable!()
        }
        async fn post_events(
            &self,
            _: &LeaseRef,
            events: &[ExecutionEvent],
        ) -> Result<usize, WorkerEngineError> {
            self.posts
                .lock()
                .unwrap()
                .push(events.iter().map(|e| (e.attempt, e.sequence)).collect());
            self.answers
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Ok(events.len()))
        }
        async fn credentials(&self, _: &LeaseRef) -> Result<Credentials, WorkerEngineError> {
            unreachable!()
        }
    }

    fn event(attempt: u32, sequence: u64, payload: EventPayload) -> ExecutionEvent {
        ExecutionEvent {
            timestamp: chrono::Utc::now(),
            attempt,
            sequence,
            source: None,
            payload,
        }
    }

    fn lease() -> LeaseRef {
        LeaseRef {
            server_lease_id: "srv".into(),
            epoch: 1,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn resends_the_same_batch_after_transport_errors_and_closes_after_done() {
        let api = Arc::new(Recorder::default());
        api.answers
            .lock()
            .unwrap()
            .extend([Err(WorkerEngineError::Transport("reset".into())), Ok(3)]);
        let (tx, rx) = mpsc::channel(16);
        for (i, payload) in [
            EventPayload::StdoutLine("a".into()),
            EventPayload::StdoutLine("b".into()),
            EventPayload::Done,
        ]
        .into_iter()
        .enumerate()
        {
            tx.send(event(1, i as u64, payload)).await.unwrap();
        }
        let delivered = EventForwarder::run(
            api.clone(),
            lease(),
            Box::new(ChannelEventSource::new(rx)),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(delivered, 3);
        let posts = api.posts.lock().unwrap().clone();
        assert_eq!(posts.len(), 2);
        assert_eq!(posts[0], posts[1], "the failed batch is re-sent unchanged");
        assert_eq!(posts[0], vec![(1, 0), (1, 1), (1, 2)]);
        drop(tx);
    }

    struct Replaying(VecDeque<Vec<ExecutionEvent>>);

    #[async_trait]
    impl OrderedEventSource for Replaying {
        async fn next_batch(&mut self, _: usize) -> Option<Vec<ExecutionEvent>> {
            self.0.pop_front()
        }
    }

    #[tokio::test]
    async fn never_sends_a_duplicate_or_reordered_event_and_stops_on_lease_lost() {
        let api = Arc::new(Recorder::default());
        let line = || EventPayload::StdoutLine("x".into());
        let source = Replaying(VecDeque::from([
            vec![event(1, 0, line()), event(1, 1, line())],
            vec![event(1, 1, line()), event(1, 2, line())],
            vec![event(2, 0, line())],
        ]));
        let delivered = EventForwarder::run(
            api.clone(),
            lease(),
            Box::new(source),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(delivered, 4);
        assert_eq!(
            api.posts.lock().unwrap().concat(),
            vec![(1, 0), (1, 1), (1, 2), (2, 0)]
        );
        let api = Arc::new(Recorder::default());
        api.answers
            .lock()
            .unwrap()
            .push_back(Err(WorkerEngineError::LeaseLost));
        let source = Replaying(VecDeque::from([
            vec![event(1, 0, line())],
            vec![event(1, 1, line())],
        ]));
        let err = EventForwarder::run(
            api.clone(),
            lease(),
            Box::new(source),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert_eq!(err, WorkerEngineError::LeaseLost);
        assert_eq!(api.posts.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn stop_ends_forwarding_while_waiting_for_events() {
        let (_tx, rx) = mpsc::channel(1);
        let stop = CancellationToken::new();
        stop.cancel();
        let delivered = EventForwarder::run(
            Arc::new(Recorder::default()),
            lease(),
            Box::new(ChannelEventSource::new(rx)),
            stop,
        )
        .await
        .unwrap();
        assert_eq!(delivered, 0);
    }
}
