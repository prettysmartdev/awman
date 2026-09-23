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

#[cfg(test)]
mod tests {
    use super::*;

    fn run_id() -> RunId {
        RunId("run-0001".to_string())
    }

    #[test]
    fn log_dir_is_a_sibling_of_the_workspace_not_a_child() {
        let paths = SquadRunPaths::new(Path::new("/root/tasks/t/workspace"), &run_id()).unwrap();
        assert_eq!(paths.log_dir(), Path::new("/root/tasks/t/runs/run-0001"));
    }

    #[test]
    fn a_task_dir_without_a_parent_is_an_error_naming_the_path() {
        let err = SquadRunPaths::new(Path::new("/"), &run_id()).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("has no task-directory parent") && msg.contains('/'),
            "error must name the path and explain why: {msg}"
        );
    }

    #[test]
    fn prepare_creates_the_directory_and_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let task_dir = tmp.path().join("tasks").join("t").join("workspace");
        let paths = SquadRunPaths::new(&task_dir, &run_id()).unwrap();
        assert!(!paths.log_dir().exists());
        paths.prepare().unwrap();
        assert!(paths.log_dir().is_dir());
        paths.prepare().unwrap();
        assert!(paths.log_dir().is_dir());
    }
}
