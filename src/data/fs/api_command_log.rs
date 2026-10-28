//! Streaming writer for a single command run's on-disk logs.
//!
//! A command execution emits a stream of [`ExecutionEvent`]s. Two files are
//! written incrementally as they arrive: `events.log` (one JSON object per
//! line, the machine-readable stream) and `output.log` (the human-readable
//! plain-text rendering). Both are opened in append mode: a command re-queued
//! after a daemon restart adds its next attempt to the same files (WI 0136).
//! This type owns both open file handles so the API
//! frontend's queue worker never touches `tokio::fs` directly
//! (grand-architecture Tenet 2).

use std::io::SeekFrom;
use std::path::Path;

use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

use crate::data::error::DataError;
use crate::data::execution_event::ExecutionEvent;
use crate::data::fs::api_paths::ApiPaths;
use crate::data::workflow_state::WorkflowState;

/// Owns the filesystem side of a command's live workflow state so the API
/// frontend never touches the filesystem itself. Synchronous because the
/// engine reports state from its synchronous persistence path.
#[derive(Clone)]
pub struct CommandStateWriter {
    worktree: std::sync::Arc<std::sync::Mutex<Option<std::path::PathBuf>>>,
    paths: ApiPaths,
    session_id: String,
    command_id: String,
}

impl CommandStateWriter {
    pub fn new(
        paths: ApiPaths,
        session_id: impl Into<String>,
        command_id: impl Into<String>,
    ) -> Self {
        Self {
            worktree: Default::default(),
            paths,
            session_id: session_id.into(),
            command_id: command_id.into(),
        }
    }

    /// Shared with the queue worker so early errors after preparation still
    /// return the checkout path. Snapshot only surviving directories.
    pub fn record_worktree(&self, path: &std::path::Path) {
        *self.worktree.lock().expect("worktree result poisoned") = Some(path.to_path_buf());
    }

    pub fn clear_worktree(&self) {
        *self.worktree.lock().expect("worktree result poisoned") = None;
    }

    pub fn kept_worktree(&self) -> Option<std::path::PathBuf> {
        self.worktree
            .lock()
            .expect("worktree result poisoned")
            .clone()
            .filter(|path| path.is_dir())
    }

    pub fn write(&self, state: &WorkflowState) -> Result<(), DataError> {
        self.paths
            .write_command_workflow_state(&self.session_id, &self.command_id, state)
    }

    pub fn write_cleanup(
        &self,
        cleanup: &crate::data::workflow_cleanup::WorkflowCleanup,
    ) -> Result<(), DataError> {
        self.paths
            .write_command_cleanup(&self.session_id, &self.command_id, cleanup)
    }
}

/// Owns the two open log files for one command run and appends to them as
/// events arrive.
pub struct CommandLogWriter {
    events_file: File,
    output_file: File,
}

impl CommandLogWriter {
    /// Open both log files for `command_id` under `session_id` for appending,
    /// creating them if absent. A re-queued command (attempt > 1) continues
    /// the same files, so the previous attempt's lines stay. If a file is
    /// non-empty and does not end in `'\n'` (a daemon crash mid-write), a
    /// `'\n'` is written first so the next line is not glued to the torn one.
    pub async fn open_append(
        paths: &ApiPaths,
        session_id: &str,
        command_id: &str,
    ) -> Result<Self, DataError> {
        let events_path = paths.command_events_log_path(session_id, command_id);
        let output_path = paths.command_log_path(session_id, command_id);
        let events_file = open_append_on_line_boundary(&events_path).await?;
        let output_file = open_append_on_line_boundary(&output_path).await?;
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

/// Open `path` for appending (creating it if absent) and make sure the next
/// write starts on a fresh line.
async fn open_append_on_line_boundary(path: &Path) -> Result<File, DataError> {
    let mut file = OpenOptions::new()
        .create(true)
        .read(true)
        .append(true)
        .open(path)
        .await
        .map_err(|e| DataError::io(path, e))?;
    let len = file
        .metadata()
        .await
        .map_err(|e| DataError::io(path, e))?
        .len();
    if len > 0 {
        let mut last = [0u8; 1];
        file.seek(SeekFrom::Start(len - 1))
            .await
            .map_err(|e| DataError::io(path, e))?;
        file.read_exact(&mut last)
            .await
            .map_err(|e| DataError::io(path, e))?;
        // Append mode writes at the end regardless of the read position.
        if last[0] != b'\n' {
            file.write_all(b"\n")
                .await
                .map_err(|e| DataError::io(path, e))?;
        }
    }
    Ok(file)
}

#[cfg(test)]
mod append_tests {
    use super::*;
    use crate::data::execution_event::EventPayload;

    fn event(attempt: u32, sequence: u64, text: &str) -> ExecutionEvent {
        ExecutionEvent {
            timestamp: chrono::Utc::now(),
            attempt,
            sequence,
            payload: EventPayload::StdoutLine(text.into()),
        }
    }

    async fn write_attempt(paths: &ApiPaths, attempt: u32, texts: &[&str]) {
        let mut writer = CommandLogWriter::open_append(paths, "s", "c")
            .await
            .unwrap();
        for (sequence, text) in texts.iter().enumerate() {
            writer
                .write_event(&event(attempt, sequence as u64, text))
                .await;
        }
        writer.flush().await;
    }

    fn setup() -> (tempfile::TempDir, ApiPaths) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = ApiPaths::at_root(tmp.path());
        paths.prepare_command_dir("s", "c").unwrap();
        (tmp, paths)
    }

    #[tokio::test]
    async fn requeue_appends_the_next_attempt_after_the_previous_one() {
        let (_tmp, paths) = setup();
        write_attempt(&paths, 1, &["a", "b"]).await;
        write_attempt(&paths, 2, &["c"]).await;

        let events = paths.read_command_events("s", "c");
        let cursors: Vec<_> = events.iter().map(|e| e.cursor().to_string()).collect();
        assert_eq!(cursors, ["1:0", "1:1", "2:0"]);

        let output = std::fs::read_to_string(paths.command_log_path("s", "c")).unwrap();
        assert_eq!(output, "a\nb\nc\n", "output.log is appended too");
    }

    #[tokio::test]
    async fn a_torn_final_line_is_not_glued_to_the_next_attempt() {
        let (_tmp, paths) = setup();
        write_attempt(&paths, 1, &["a"]).await;
        // Simulate a crash mid-write: half a JSON record, no newline.
        let events_path = paths.command_events_log_path("s", "c");
        let mut raw = std::fs::read(&events_path).unwrap();
        raw.extend_from_slice(br#"{"timestamp":"2026-01-01T00:00:00Z","attempt":1,"seq"#);
        std::fs::write(&events_path, raw).unwrap();

        write_attempt(&paths, 2, &["b"]).await;

        let cursors: Vec<_> = paths
            .read_command_events("s", "c")
            .iter()
            .map(|e| e.cursor().to_string())
            .collect();
        assert_eq!(
            cursors,
            ["1:0", "2:0"],
            "intact lines on both sides survive"
        );
    }

    #[tokio::test]
    async fn a_torn_utf8_record_does_not_hide_later_attempts() {
        let (_tmp, paths) = setup();
        write_attempt(&paths, 1, &["a"]).await;
        let events_path = paths.command_events_log_path("s", "c");
        let mut raw = std::fs::read(&events_path).unwrap();
        raw.extend_from_slice(&[b'{', b'"', 0xE2, 0x82]); // truncated multi-byte char
        std::fs::write(&events_path, raw).unwrap();

        write_attempt(&paths, 2, &["b"]).await;

        let cursors: Vec<_> = paths
            .read_command_events("s", "c")
            .iter()
            .map(|e| e.cursor().to_string())
            .collect();
        assert_eq!(cursors, ["1:0", "2:0"]);
    }

    #[tokio::test]
    async fn a_missing_log_reads_as_empty() {
        let (_tmp, paths) = setup();
        assert!(paths.read_command_events("s", "c").is_empty());
    }
}
