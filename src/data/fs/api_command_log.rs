//! Streaming writer for a single command run's on-disk logs.
//!
//! A command execution emits a stream of [`ExecutionEvent`]s. Two files are
//! written incrementally as they arrive: `events.log` (one JSON object per
//! line, the machine-readable stream) and `output.log` (the human-readable
//! plain-text rendering). This type owns both open file handles so the API
//! frontend's queue worker never touches `tokio::fs` directly
//! (grand-architecture Tenet 2).

use tokio::fs::File;
use tokio::io::AsyncWriteExt;

use crate::data::error::DataError;
use crate::data::execution_event::ExecutionEvent;
use crate::data::fs::api_paths::ApiPaths;

/// Owns the two open log files for one command run and appends to them as
/// events arrive.
pub struct CommandLogWriter {
    events_file: File,
    output_file: File,
}

impl CommandLogWriter {
    /// Create (truncating) both log files for `command_id` under `session_id`.
    pub async fn create(
        paths: &ApiPaths,
        session_id: &str,
        command_id: &str,
    ) -> Result<Self, DataError> {
        let events_path = paths.command_events_log_path(session_id, command_id);
        let output_path = paths.command_log_path(session_id, command_id);
        let events_file = File::create(&events_path)
            .await
            .map_err(|e| DataError::io(&events_path, e))?;
        let output_file = File::create(&output_path)
            .await
            .map_err(|e| DataError::io(&output_path, e))?;
        Ok(Self {
            events_file,
            output_file,
        })
    }

    /// Append one event: its JSON line to `events.log`, and — when the payload
    /// has a plain-text rendering — that text to `output.log`. Write errors are
    /// swallowed (best-effort logging), matching the previous inline behavior.
    pub async fn write_event(&mut self, event: &ExecutionEvent) {
        if let Ok(json) = serde_json::to_string(event) {
            let _ = self
                .events_file
                .write_all(format!("{json}\n").as_bytes())
                .await;
        }
        if let Some(text) = event.payload.to_plain_text() {
            let _ = self
                .output_file
                .write_all(format!("{text}\n").as_bytes())
                .await;
        }
    }

    /// Flush both files to disk.
    pub async fn flush(&mut self) {
        let _ = self.events_file.flush().await;
        let _ = self.output_file.flush().await;
    }
}
