//! SSE event streaming for `RemoteClient`.
//!
//! Layer 1 (WI 0114 F-28). `ExecutionEventSink` is the typed surface a
//! caller implements to consume a job's log stream; the parser that turns
//! `\n\n`-delimited SSE blocks into `ExecutionEvent`s lives beside it.

use std::time::Duration;

use crate::data::execution_event::ExecutionEvent;
use crate::engine::error::EngineError;
use crate::engine::remote::client::RemoteClient;

/// Test-only sink used by the legacy SSE parser test. Production code never
/// uses this; see `ExecutionEventSink` for the typed surface.
#[cfg(test)]
pub trait RemoteEventSink: Send + Sync {
    fn on_event(&mut self, event_type: &str, data: &str);
    fn on_done(&mut self);
}

/// Sink for typed `ExecutionEvent`s streaming over SSE from the per-job
/// `/logs` endpoint. The default impl ignores everything — callers override
/// the methods they care about. Each callback returns `bool`; returning
/// `true` from any callback ends the stream early (e.g. on Ctrl-C).
pub trait ExecutionEventSink: Send {
    fn on_event(&mut self, event: ExecutionEvent) -> bool {
        let _ = event;
        false
    }

    /// Called once when the stream terminates cleanly.
    fn on_stream_end(&mut self) {}
}

impl RemoteClient {
    /// `GET /v1/commands/{id}/logs` (SSE) — stream typed
    /// `ExecutionEvent` values to the sink. Terminates when the server sends
    /// a `Done` event or the sink returns `true` from any callback.
    pub async fn stream_job_logs(
        &self,
        _session_id: &str,
        job_id: &str,
        sink: &mut dyn ExecutionEventSink,
    ) -> Result<(), EngineError> {
        use crate::data::execution_event::EventPayload;
        use futures_util::StreamExt;

        let url = self.core.url(&["commands", job_id, "logs"]);

        let resp = self
            .core
            .http()
            .get(&url)
            .timeout(Duration::from_secs(86400))
            .send()
            .await
            .map_err(Self::map_reqwest_error)?;
        if resp.status().as_u16() >= 400 {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(EngineError::RemoteHttpStatus { status, body });
        }

        let mut stream = resp.bytes_stream();
        let mut buffer = String::new();

        while let Some(chunk_res) = stream.next().await {
            let chunk = chunk_res.map_err(|e| EngineError::RemoteTransport(e.to_string()))?;
            buffer.push_str(&String::from_utf8_lossy(&chunk));

            while let Some(pos) = buffer.find("\n\n") {
                let block: String = buffer.drain(..pos + 2).collect();
                let trimmed = block.trim_end_matches('\n');
                if trimmed.is_empty() {
                    continue;
                }
                // SSE comment lines start with `:` — surface as a sink hook
                // by ignoring them here.
                let mut data_lines: Vec<&str> = Vec::new();
                for line in trimmed.lines() {
                    if let Some(rest) = line.strip_prefix("data: ") {
                        data_lines.push(rest);
                    } else if let Some(rest) = line.strip_prefix("data:") {
                        data_lines.push(rest);
                    }
                    // event: and : (comment) lines are ignored — the typed
                    // payload already includes the event kind.
                }
                let data = data_lines.join("\n");
                if data.is_empty() {
                    continue;
                }
                let event: ExecutionEvent = match serde_json::from_str(&data) {
                    Ok(e) => e,
                    Err(_) => continue, // skip malformed lines
                };
                let is_done = matches!(event.payload, EventPayload::Done);
                if sink.on_event(event) {
                    sink.on_stream_end();
                    return Ok(());
                }
                if is_done {
                    sink.on_stream_end();
                    return Ok(());
                }
            }
        }

        sink.on_stream_end();
        Ok(())
    }
    /// Stream raw SSE events to the given sink. Kept crate-private for tests
    /// of the SSE parser; production code should use `stream_job_logs`.
    #[cfg(test)]
    pub(crate) async fn stream_command_legacy(
        &self,
        path: &[&str],
        _flags: &[(&str, serde_json::Value)],
        sink: &mut dyn RemoteEventSink,
    ) -> Result<(), EngineError> {
        use futures_util::StreamExt;

        let url = self.core.url(path);

        let resp = self
            .core
            .http()
            .get(&url)
            .timeout(Duration::from_secs(86400))
            .send()
            .await
            .map_err(Self::map_reqwest_error)?;

        if resp.status().as_u16() >= 400 {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(EngineError::RemoteHttpStatus { status, body });
        }

        let mut stream = resp.bytes_stream();
        let mut buffer = String::new();

        while let Some(chunk_res) = stream.next().await {
            let chunk = chunk_res.map_err(|e| EngineError::RemoteTransport(e.to_string()))?;
            buffer.push_str(&String::from_utf8_lossy(&chunk));

            // Pull every complete `\n\n`-delimited event block out of the buffer
            // and dispatch it. Whatever's left after the final separator stays
            // in the buffer until more bytes arrive.
            while let Some(pos) = buffer.find("\n\n") {
                let event_block = buffer[..pos].to_string();
                buffer.drain(..pos + 2);
                if Self::dispatch_sse_event(&event_block, sink) {
                    return Ok(());
                }
            }
        }

        // Stream ended without [awman:done] — emit any partial event then close.
        if !buffer.trim().is_empty() {
            let trailing = std::mem::take(&mut buffer);
            if Self::dispatch_sse_event(&trailing, sink) {
                return Ok(());
            }
        }
        sink.on_done();
        Ok(())
    }

    /// Parse one `\n\n`-delimited SSE event block and forward it to the sink.
    /// Returns `true` when the block was the `[awman:done]` sentinel (caller
    /// should stop streaming).
    #[cfg(test)]
    fn dispatch_sse_event(block: &str, sink: &mut dyn RemoteEventSink) -> bool {
        if block.trim().is_empty() {
            return false;
        }
        let mut event_type = "message";
        let mut data_lines: Vec<&str> = Vec::new();
        for line in block.lines() {
            if let Some(rest) = line.strip_prefix("event: ") {
                event_type = rest;
            } else if let Some(rest) = line.strip_prefix("event:") {
                event_type = rest;
            } else if let Some(rest) = line.strip_prefix("data: ") {
                data_lines.push(rest);
            } else if let Some(rest) = line.strip_prefix("data:") {
                data_lines.push(rest);
            }
        }
        let data = data_lines.join("\n");
        if data == "[awman:done]" {
            sink.on_done();
            return true;
        }
        sink.on_event(event_type, &data);
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stream_command_parses_sse_events_and_calls_sink() {
        use wiremock::{matchers, Mock, MockServer, ResponseTemplate};

        let sse_body = "data: hello world\n\ndata: second line\n\ndata: [awman:done]\n\n";

        let server = MockServer::start().await;
        Mock::given(matchers::method("GET"))
            .and(matchers::path("/v1/commands/cmd-1/logs/stream"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse_body),
            )
            .mount(&server)
            .await;

        let client = RemoteClient::new(&server.uri(), None).unwrap();

        struct CollectSink {
            events: Vec<(String, String)>,
            done: bool,
        }
        impl RemoteEventSink for CollectSink {
            fn on_event(&mut self, event_type: &str, data: &str) {
                self.events.push((event_type.to_string(), data.to_string()));
            }
            fn on_done(&mut self) {
                self.done = true;
            }
        }

        let mut sink = CollectSink {
            events: Vec::new(),
            done: false,
        };
        let result = client
            .stream_command_legacy(&["commands", "cmd-1", "logs", "stream"], &[], &mut sink)
            .await;
        assert!(result.is_ok(), "stream_command should succeed: {result:?}");
        assert!(sink.done, "on_done must be called");
        assert_eq!(sink.events.len(), 2);
        assert_eq!(sink.events[0].1, "hello world");
        assert_eq!(sink.events[1].1, "second line");
    }
}
