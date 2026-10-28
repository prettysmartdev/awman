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
