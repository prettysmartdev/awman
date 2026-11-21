//! Following a remote command's event stream.
//!
//! Layer 1 (WI 0114 F-28). `ExecutionEventSink` is the typed surface a caller
//! implements to consume a job's events. [`RemoteClient::follow_job_events`]
//! owns the whole follow contract:
//!
//! - frames come from the bounded byte-level [`SseDecoder`];
//! - every connect, read and reconnect delay is selected against a
//!   [`CancellationToken`], so cancellation never waits for another event;
//! - a follow ends as exactly one of [`FollowEnd::Done`] (the server sent
//!   `done`), [`FollowEnd::Detached`] (the caller stopped following),
//!   [`FollowEnd::Incomplete`] (the transport kept ending before `done`) or
//!   [`FollowEnd::Failed`] (a non-retryable protocol or HTTP error). The end of
//!   a transport is never taken as the end of the command;
//! - an interrupted stream reconnects with `Last-Event-ID` set to the last
//!   delivered `<attempt>:<sequence>` cursor, and any event at or before that
//!   cursor is dropped, matching the server's `ReplayFrom` rule. The command is
//!   never resubmitted.

use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::data::execution_event::{EventCursor, EventPayload, ExecutionEvent};
use crate::engine::error::EngineError;
use crate::engine::remote::client::RemoteClient;
use crate::engine::remote::sse::{SseDecoder, SseFrame, SseItem};

/// Sink for typed `ExecutionEvent`s streaming over SSE from the per-job
/// `/events` endpoint. Returning `true` from `on_event` detaches the follow.
pub trait ExecutionEventSink: Send {
    fn on_event(&mut self, event: ExecutionEvent) -> bool {
        let _ = event;
        false
    }

    /// Called before each reconnect delay.
    fn on_reconnecting(&mut self, notice: &ReconnectNotice) {
        let _ = notice;
    }
}

/// A reconnect about to happen after an incomplete stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconnectNotice {
    /// 1-based count of consecutive reconnects without a newly delivered event.
    pub attempt: u32,
    pub max_attempts: u32,
    pub delay: Duration,
    /// Why the previous connection ended.
    pub reason: String,
    /// The cursor sent as `Last-Event-ID`, if any event carried one.
    pub resume_from: Option<EventCursor>,
}

/// Reconnect budget: one delay per consecutive attempt without progress. A
/// connection that delivers a new event resets the count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconnectPolicy {
    pub delays: Vec<Duration>,
}

impl Default for ReconnectPolicy {
    /// Five attempts after 2, 4, 8, 16 and 30 seconds.
    fn default() -> Self {
        Self {
            delays: [2, 4, 8, 16, 30]
                .into_iter()
                .map(Duration::from_secs)
                .collect(),
        }
    }
}

/// How a follow ended.
#[derive(Debug)]
pub enum FollowEnd {
    /// The server delivered the `done` event.
    Done,
    /// The caller cancelled or the sink asked to stop. Nothing was sent to the
    /// server; the command keeps running there.
    Detached,
    /// Every reconnect in the budget ended before `done`.
    Incomplete { reconnects: u32, last_error: String },
    /// A response that reconnecting cannot fix: an oversized frame
    /// (`RemoteProtocol`) or a client-side HTTP status.
    Failed(EngineError),
}

/// The result of [`RemoteClient::follow_job_events`].
#[derive(Debug)]
pub struct FollowReport {
    pub end: FollowEnd,
    /// The last cursor accepted (delivered, or skipped as malformed).
    pub last_cursor: Option<EventCursor>,
    /// Events delivered to the sink.
    pub delivered: u64,
    /// Complete frames that could not be decoded.
    pub malformed: u64,
    /// The first malformed frame's reason.
    pub first_malformed: Option<String>,
    /// HTTP connections opened, including the first.
    pub connections: u32,
}

/// How one connection ended (internal to the follow loop).
enum StreamEnd {
    Done,
    Detached,
    Incomplete(String),
}

/// What a complete frame means for an execution-event stream.
#[derive(Debug)]
pub enum DecodedFrame {
    /// Id-less transport health failure; never advances the replay cursor.
    RecoveryRequired,
    /// An event, with its `id:` cursor when the frame had one. Frames without
    /// an id are live-only: delivered, never used as a resume cursor.
    Event {
        event: Box<ExecutionEvent>,
        cursor: Option<EventCursor>,
    },
    /// Comments, keepalives and non-event frames (`queue_status`).
    Ignored,
    /// Data that is not an `ExecutionEvent`.
    Malformed {
        cursor: Option<EventCursor>,
        reason: String,
    },
}

/// SSE `event:` names on the events route that do not carry an
/// `ExecutionEvent`.
const NON_EVENT_FRAMES: &[&str] = &["queue_status"];

/// Interpret one decoded SSE item as an execution-event frame.
pub fn decode_execution_frame(item: SseItem) -> DecodedFrame {
    match item {
        SseItem::Malformed { id, reason } => DecodedFrame::Malformed {
            cursor: id.as_deref().and_then(EventCursor::parse),
            reason,
        },
        SseItem::Frame(SseFrame {
            event: Some(event),
            data: Some(_),
            ..
        }) if event == "operation_error" => DecodedFrame::RecoveryRequired,
        SseItem::Frame(SseFrame { data: None, .. }) => DecodedFrame::Ignored,
        SseItem::Frame(SseFrame { event, .. })
            if event
                .as_deref()
                .is_some_and(|name| NON_EVENT_FRAMES.contains(&name)) =>
        {
            DecodedFrame::Ignored
        }
        SseItem::Frame(SseFrame {
            id,
            event,
            data: Some(data),
        }) => {
            let cursor = id.as_deref().and_then(EventCursor::parse);
            match serde_json::from_str::<ExecutionEvent>(&data) {
                Ok(parsed) => DecodedFrame::Event {
                    event: Box::new(parsed),
                    cursor,
                },
                Err(e) => DecodedFrame::Malformed {
                    cursor,
                    reason: format!(
                        "undecodable {} event: {e}",
                        event.as_deref().unwrap_or("message")
                    ),
                },
            }
        }
    }
}

/// Delivery state carried across reconnects.
#[derive(Debug, Default)]
struct FollowState {
    last_cursor: Option<EventCursor>,
    delivered: u64,
    malformed: u64,
    first_malformed: Option<String>,
}

impl FollowState {
    fn advance(&mut self, cursor: EventCursor) {
        if self.last_cursor.is_none_or(|last| cursor > last) {
            self.last_cursor = Some(cursor);
        }
    }

    /// Whether a frame with this cursor was already delivered (server replay
    /// returns only events strictly after `Last-Event-ID`; anything else is a
    /// duplicate from a replay that restarted).
    fn is_duplicate(&self, cursor: Option<EventCursor>) -> bool {
        matches!((cursor, self.last_cursor), (Some(c), Some(last)) if c <= last)
    }
}

/// Whether reconnecting could succeed after this error.
fn is_retryable(err: &EngineError) -> bool {
    match err {
        EngineError::RemoteProtocol(_) => false,
        EngineError::RemoteHttpStatus { status, .. } => {
            *status >= 500 || *status == 408 || *status == 429
        }
        _ => true,
    }
}

/// Run `work` while `signal` may cancel `cancel`, without spawning a task.
/// When `work` finishes first, `signal` is dropped with it, so a Ctrl-C
/// listener never outlives the follow it guards.
pub async fn run_cancellable<T>(
    cancel: &CancellationToken,
    signal: impl std::future::Future<Output = ()>,
    work: impl std::future::Future<Output = T>,
) -> T {
    let finished = CancellationToken::new();
    let watcher = async {
        tokio::select! {
            () = signal => cancel.cancel(),
            () = finished.cancelled() => {}
        }
    };
    let work = async {
        let value = work.await;
        finished.cancel();
        value
    };
    let (value, ()) = tokio::join!(work, watcher);
    value
}

impl RemoteClient {
    /// Follow `GET /v1/worker/commands/{id}/events` until `done`, detachment,
    /// a non-retryable failure or an exhausted reconnect budget. See the
    /// module docs for the contract.
    pub async fn follow_job_events(
        &self,
        job_id: &str,
        sink: &mut dyn ExecutionEventSink,
        cancel: &CancellationToken,
        policy: &ReconnectPolicy,
    ) -> FollowReport {
        let mut state = FollowState::default();
        let mut connections = 0u32;
        let mut without_progress = 0usize;
        let end = loop {
            if cancel.is_cancelled() {
                break FollowEnd::Detached;
            }
            connections += 1;
            let delivered_before = state.delivered;
            let reason = match self.stream_once(job_id, &mut state, sink, cancel).await {
                Ok(StreamEnd::Done) => break FollowEnd::Done,
                Ok(StreamEnd::Detached) => break FollowEnd::Detached,
                Ok(StreamEnd::Incomplete(reason)) => reason,
                Err(e) if !is_retryable(&e) => break FollowEnd::Failed(e),
                Err(e) => e.to_string(),
            };
            if state.delivered > delivered_before {
                without_progress = 0;
            }
            let Some(delay) = policy.delays.get(without_progress).copied() else {
                break FollowEnd::Incomplete {
                    reconnects: without_progress as u32,
                    last_error: reason,
                };
            };
            without_progress += 1;
            sink.on_reconnecting(&ReconnectNotice {
                attempt: without_progress as u32,
                max_attempts: policy.delays.len() as u32,
                delay,
                reason,
                resume_from: state.last_cursor,
            });
            tokio::select! {
                () = cancel.cancelled() => break FollowEnd::Detached,
                () = tokio::time::sleep(delay) => {}
            }
        };
        FollowReport {
            end,
            last_cursor: state.last_cursor,
            delivered: state.delivered,
            malformed: state.malformed,
            first_malformed: state.first_malformed,
            connections,
        }
    }

    /// One connection of a follow.
    async fn stream_once(
        &self,
        job_id: &str,
        state: &mut FollowState,
        sink: &mut dyn ExecutionEventSink,
        cancel: &CancellationToken,
    ) -> Result<StreamEnd, EngineError> {
        use futures_util::StreamExt;

        let url = self.core.url(&["worker", "commands", job_id, "events"]);
        // The client's read timeout covers a whole response; a follow can
        // legitimately last much longer. Cancellation, not this, bounds it.
        let mut request = self
            .core
            .http()
            .get(&url)
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .timeout(Duration::from_secs(86400));
        if let Some(cursor) = state.last_cursor {
            request = request.header("Last-Event-ID", cursor.to_string());
        }
        let resp = tokio::select! {
            () = cancel.cancelled() => return Ok(StreamEnd::Detached),
            resp = request.send() => resp.map_err(Self::map_reqwest_error)?,
        };
        if resp.status().as_u16() >= 400 {
            let status = resp.status().as_u16();
            let body = tokio::select! {
                () = cancel.cancelled() => return Ok(StreamEnd::Detached),
                body = resp.text() => body.unwrap_or_default(),
            };
            return Err(EngineError::RemoteHttpStatus { status, body });
        }

        let mut stream = resp.bytes_stream();
        let mut decoder = SseDecoder::new();
        let mut items = Vec::new();
        loop {
            let next = tokio::select! {
                biased;
                () = cancel.cancelled() => return Ok(StreamEnd::Detached),
                next = stream.next() => next,
            };
            let chunk = match next {
                None if decoder.has_partial_frame() => {
                    return Ok(StreamEnd::Incomplete(format!(
                    "stream ended mid-frame ({} bytes unterminated) before the command finished",
                    decoder.buffered_len()
                )))
                }
                None => {
                    return Ok(StreamEnd::Incomplete(
                        "stream ended before the command finished".into(),
                    ))
                }
                Some(Err(e)) => {
                    return Ok(StreamEnd::Incomplete(format!("stream read failed: {e}")))
                }
                Some(Ok(chunk)) => chunk,
            };
            decoder
                .push(&chunk, &mut items)
                .map_err(|e| EngineError::RemoteProtocol(e.to_string()))?;
            for item in items.drain(..) {
                match decode_execution_frame(item) {
                    DecodedFrame::RecoveryRequired => return Err(EngineError::RemoteProtocol("command storage requires recovery; repair storage and restart the daemon; do not resubmit the task".into())),
                    DecodedFrame::Ignored => {}
                    DecodedFrame::Malformed { cursor, reason } => {
                        state.malformed += 1;
                        state.first_malformed.get_or_insert(reason);
                        if let Some(cursor) = cursor {
                            state.advance(cursor);
                        }
                    }
                    DecodedFrame::Event { event, cursor } => {
                        let is_done = matches!(event.payload, EventPayload::Done);
                        if state.is_duplicate(cursor) {
                            // A replayed `done` still ends the follow.
                            if is_done {
                                return Ok(StreamEnd::Done);
                            }
                            continue;
                        }
                        if cancel.is_cancelled() {
                            return Ok(StreamEnd::Detached);
                        }
                        if let Some(cursor) = cursor {
                            state.advance(cursor);
                        }
                        state.delivered += 1;
                        if sink.on_event(*event) {
                            return Ok(StreamEnd::Detached);
                        }
                        if is_done {
                            return Ok(StreamEnd::Done);
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event_json(attempt: u32, sequence: u64, payload: &str) -> String {
        format!(
            r#"{{"emitted_at":"2026-01-01T00:00:00Z","attempt":{attempt},"sequence":{sequence},"payload":{payload}}}"#
        )
    }

    fn frame(id: Option<&str>, event: Option<&str>, data: Option<String>) -> SseItem {
        SseItem::Frame(SseFrame {
            id: id.map(str::to_string),
            event: event.map(str::to_string),
            data,
        })
    }

    #[test]
    fn an_event_frame_decodes_with_its_cursor() {
        let decoded = decode_execution_frame(frame(
            Some("2:7"),
            Some("done"),
            Some(event_json(2, 7, r#"{"type":"done"}"#)),
        ));
        match decoded {
            DecodedFrame::Event { event, cursor } => {
                assert_eq!(cursor, Some(EventCursor::new(2, 7)));
                assert!(matches!(event.payload, EventPayload::Done));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn comments_and_queue_status_are_ignored_not_malformed() {
        assert!(matches!(
            decode_execution_frame(frame(None, None, None)),
            DecodedFrame::Ignored
        ));
        assert!(matches!(
            decode_execution_frame(frame(
                None,
                Some("queue_status"),
                Some(r#"{"command_id":"x"}"#.into())
            )),
            DecodedFrame::Ignored
        ));
    }

    #[test]
    fn undecodable_data_is_malformed_and_keeps_its_cursor() {
        assert!(matches!(
            decode_execution_frame(frame(Some("1:3"), Some("output"), Some("{not json".into()))),
            DecodedFrame::Malformed { cursor: Some(c), .. } if c == EventCursor::new(1, 3)
        ));
        assert!(matches!(
            decode_execution_frame(SseItem::Malformed {
                id: Some("1:4".into()),
                reason: "bad".into()
            }),
            DecodedFrame::Malformed { cursor: Some(c), .. } if c == EventCursor::new(1, 4)
        ));
    }

    #[test]
    fn duplicates_are_events_at_or_before_the_last_cursor() {
        let mut state = FollowState::default();
        assert!(!state.is_duplicate(Some(EventCursor::new(1, 0))));
        state.advance(EventCursor::new(1, 5));
        assert!(state.is_duplicate(Some(EventCursor::new(1, 5))));
        assert!(state.is_duplicate(Some(EventCursor::new(1, 2))));
        assert!(!state.is_duplicate(Some(EventCursor::new(1, 6))));
        // A re-queued attempt restarts its sequence and is new.
        assert!(!state.is_duplicate(Some(EventCursor::new(2, 0))));
        // Live-only frames are never duplicates.
        assert!(!state.is_duplicate(None));
        state.advance(EventCursor::new(1, 1));
        assert_eq!(state.last_cursor, Some(EventCursor::new(1, 5)));
    }

    #[test]
    fn oversized_frames_and_client_errors_are_not_retried() {
        assert!(!is_retryable(&EngineError::RemoteProtocol("big".into())));
        assert!(!is_retryable(&EngineError::RemoteHttpStatus {
            status: 404,
            body: String::new()
        }));
        assert!(is_retryable(&EngineError::RemoteHttpStatus {
            status: 503,
            body: String::new()
        }));
        assert!(is_retryable(&EngineError::RemoteConnectionRefused(
            "x".into()
        )));
        assert!(is_retryable(&EngineError::RemoteTimeout));
    }

    #[test]
    fn the_default_policy_is_five_growing_delays() {
        let secs: Vec<u64> = ReconnectPolicy::default()
            .delays
            .iter()
            .map(Duration::as_secs)
            .collect();
        assert_eq!(secs, vec![2, 4, 8, 16, 30]);
    }
}

#[cfg(test)]
mod transport_health_tests {
    use super::*;
    #[test]
    fn storage_failure_is_terminal_transport_health_without_a_replay_cursor() {
        let item = SseItem::Frame(SseFrame {
            id: None,
            event: Some("operation_error".into()),
            data: Some(r#"{"code":"storage_error"}"#.into()),
        });
        assert!(matches!(
            decode_execution_frame(item),
            DecodedFrame::RecoveryRequired
        ));
    }
}
