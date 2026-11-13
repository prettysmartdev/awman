//! `SquadRunPaths` — where one squad task run's output and metadata live.
//!
//! Path arithmetic and directory creation only: no git, no network, no
//! process spawning (Layer 0, decision Q1). WI 0114 F-38 moved this out of
//! `engine::squad::launcher`, where it was a pair of free functions, so the
//! launcher is left holding only what genuinely needs a runtime.

use std::path::{Path, PathBuf};

use crate::data::error::DataError;
use crate::data::fs::RunId;

/// The directories belonging to one run of one task.
///
/// `task_dir` is the durable `<root>/tasks/<task>/workspace` directory, so
/// run data is deliberately its *sibling* rather than content inside the
/// workspace: `<root>/tasks/<task>/runs/<run-id>/`. That keeps transient
/// execution output out of the leader's durable working area.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SquadRunPaths {
    log_dir: PathBuf,
}

impl SquadRunPaths {
    /// Append a terminal lifecycle/control event to this run's durable log.
    /// The daemon's single owner-event writer serializes calls to this method.
    pub fn append_event(
        &self,
        source: Option<crate::data::execution_event::EventSource>,
        payload: crate::data::execution_event::EventPayload,
    ) -> Result<(), DataError> {
        use std::io::Write;
        self.prepare()?;
        let path = self.log_dir.join("events.log");
        let previous = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(DataError::io(&path, e)),
        };
        let sequence = crate::data::execution_event::parse_event_log(&previous)
            .last()
            .map_or(0, |event| event.sequence + 1);
        let event = crate::data::execution_event::ExecutionEvent {
            timestamp: chrono::Utc::now(),
            attempt: 1,
            sequence,
            source,
            payload,
        };
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| DataError::io(&path, e))?;
        if !previous.is_empty() && !previous.ends_with('\n') {
            file.write_all(b"\n").map_err(|e| DataError::io(&path, e))?;
        }
        let mut line = serde_json::to_vec(&event).expect("execution event serializes");
        line.push(b'\n');
        file.write_all(&line).map_err(|e| DataError::io(&path, e))
    }
    /// Resolve the run's directories from the task workspace and the run id.
    ///
    /// Fails when `task_dir` has no parent — a workspace path that is not
    /// `<task-dir>/workspace` cannot name a sibling `runs/` directory.
    pub fn new(task_dir: &Path, run_id: &RunId) -> Result<Self, DataError> {
        let task_root = task_dir.parent().ok_or_else(|| DataError::InvalidPath {
            path: task_dir.to_path_buf(),
            reason: "task workspace has no task-directory parent".to_string(),
        })?;
        Ok(Self {
            log_dir: task_root.join("runs").join(run_id.as_str()),
        })
    }

    /// The directory that owns the output and metadata for this run.
    pub fn log_dir(&self) -> &Path {
        &self.log_dir
    }

    /// Create the log directory before any container for this run starts.
    ///
    /// The scheduler calls this synchronously after reserving the [`RunId`]
    /// and before dispatching evaluation, so output draining never has to
    /// create directories lazily on its first byte.
    pub fn prepare(&self) -> Result<&Path, DataError> {
        std::fs::create_dir_all(&self.log_dir)
            .map_err(|error| DataError::io(&self.log_dir, error))?;
        Ok(&self.log_dir)
    }
}
