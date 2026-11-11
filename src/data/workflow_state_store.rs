//! Engine-level workflow state persistence — Layer 0.
//!
//! Persists `WorkflowState` snapshots under `<git-root>/.awman/workflows/`.
//! The filename pattern is `<repohash8>-[<wi>-]<name>.json`,
//! where `repohash8` is the first 8 hex characters of SHA-256(git_root path).

use std::path::{Path, PathBuf};

use crate::data::error::DataError;
use crate::data::fs::config_file::write_config_file_atomically;
use crate::data::fs::hash::{sanitize_name_for_filename, sha256_hex};
use crate::data::session::Session;
use crate::data::workflow_state::WorkflowState;

/// Persists engine-level `WorkflowState` to `<git_root>/.awman/workflows/`.
#[derive(Debug, Clone)]
pub struct WorkflowStateStore {
    git_root: PathBuf,
}

impl WorkflowStateStore {
    /// Construct a store rooted at `<git_root>/.awman/workflows/`.
    pub fn new(session: &Session) -> Self {
        Self {
            git_root: session.git_root().to_path_buf(),
        }
    }

    /// Construct without a session (used by tests and command setup that
    /// already resolved the git root).
    pub fn at_git_root(git_root: impl Into<PathBuf>) -> Self {
        Self {
            git_root: git_root.into(),
        }
    }

    /// Directory in which state files live.
    pub fn dir(&self) -> PathBuf {
        self.git_root.join(".awman").join("workflows")
    }

    pub fn state_path(&self, work_item: Option<u32>, workflow_name: &str) -> PathBuf {
        let repo_hash = &sha256_hex(self.git_root.to_string_lossy().as_bytes())[..8];
        let name = sanitize_name_for_filename(workflow_name);
        let filename = match work_item {
            Some(wi) => format!("{repo_hash}-{wi:04}-{name}.json"),
            None => format!("{repo_hash}-{name}.json"),
        };
        self.dir().join(filename)
    }

    /// Load a workflow's state by name. Returns `Ok(None)` when no state file
    /// exists.
    pub fn load(
        &self,
        work_item: Option<u32>,
        workflow_name: &str,
    ) -> Result<Option<WorkflowState>, DataError> {
        let path = self.state_path(work_item, workflow_name);
        if !path.exists() {
            return Ok(None);
        }
        let raw = std::fs::read_to_string(&path).map_err(|e| DataError::io(&path, e))?;
        let state: WorkflowState =
            serde_json::from_str(&raw).map_err(|e| DataError::config_parse(&path, e))?;
        Ok(Some(state))
    }

    /// Read a workflow state from an already-recorded engine state path.
    /// Callers that persist this path (such as squad runs) must use this rather
    /// than reconstructing the filename from repository data.
    pub fn read_state_path(path: &Path) -> Result<Option<WorkflowState>, DataError> {
        if !path.exists() {
            return Ok(None);
        }
        let raw = std::fs::read_to_string(path).map_err(|e| DataError::io(path, e))?;
        serde_json::from_str(&raw)
            .map(Some)
            .map_err(|e| DataError::config_parse(path, e))
    }

    /// Persist a workflow's state through a sibling temp file and rename, so a
    /// failed or interrupted save leaves the previous checkpoint readable.
    pub fn save(&self, state: &WorkflowState) -> Result<PathBuf, DataError> {
        let dir = self.dir();
        std::fs::create_dir_all(&dir).map_err(|e| DataError::io(&dir, e))?;
        let path = self.state_path(state.work_item, &state.workflow_name);
        let json = serde_json::to_string_pretty(state)
            .map_err(|e| DataError::ConfigSerialize { source: e })?;
        write_config_file_atomically(&path, &json)?;
        Ok(path)
    }

    /// Delete a workflow's state file. Returns `Ok(())` when the file is absent
    /// (idempotent).
    pub fn delete(&self, work_item: Option<u32>, workflow_name: &str) -> Result<(), DataError> {
        let path = self.state_path(work_item, workflow_name);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(DataError::io(&path, e)),
        }
    }
}
