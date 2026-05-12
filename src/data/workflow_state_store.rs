//! Engine-level workflow state persistence — Layer 0.
//!
//! Persists `WorkflowState` snapshots under `<git-root>/.amux/workflows/`.
//! The filename pattern matches old-amux: `<repohash8>-[<wi>-]<name>.json`,
//! where `repohash8` is the first 8 hex characters of SHA-256(git_root path).

use std::path::{Path, PathBuf};

use crate::data::error::DataError;
use crate::data::fs::workflow_state::sha256_hex;
use crate::data::session::Session;
use crate::data::workflow_state::WorkflowState;

/// Persists engine-level `WorkflowState` to `<git_root>/.amux/workflows/`.
#[derive(Debug, Clone)]
pub struct WorkflowStateStore {
    git_root: PathBuf,
}

impl WorkflowStateStore {
    /// Construct a store rooted at `<git_root>/.amux/workflows/`.
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
        self.git_root.join(".amux").join("workflows")
    }

    fn filename_for(&self, work_item: Option<u32>, workflow_name: &str) -> PathBuf {
        let repo_hash = &sha256_hex(&self.git_root.to_string_lossy())[..8];
        let filename = match work_item {
            Some(wi) => format!("{repo_hash}-{wi:04}-{workflow_name}.json"),
            None => format!("{repo_hash}-{workflow_name}.json"),
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
        let path = self.filename_for(work_item, workflow_name);
        if !path.exists() {
            return Ok(None);
        }
        let raw = std::fs::read_to_string(&path).map_err(|e| DataError::io(&path, e))?;
        let state: WorkflowState =
            serde_json::from_str(&raw).map_err(|e| DataError::config_parse(&path, e))?;
        Ok(Some(state))
    }

    /// Persist a workflow's state.
    pub fn save(&self, state: &WorkflowState) -> Result<PathBuf, DataError> {
        let dir = self.dir();
        std::fs::create_dir_all(&dir).map_err(|e| DataError::io(&dir, e))?;
        let path = self.filename_for(state.work_item, &state.workflow_name);
        let json = serde_json::to_string_pretty(state)
            .map_err(|e| DataError::ConfigSerialize { source: e })?;
        std::fs::write(&path, json).map_err(|e| DataError::io(&path, e))?;
        Ok(path)
    }

    /// Delete a workflow's state file. Returns `Ok(())` when the file is absent
    /// (idempotent).
    pub fn delete(&self, work_item: Option<u32>, workflow_name: &str) -> Result<(), DataError> {
        let path = self.filename_for(work_item, workflow_name);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(DataError::io(&path, e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_state(name: &str) -> WorkflowState {
        WorkflowState::new(name.to_string(), &[], "hash".into(), None)
    }

    #[test]
    fn save_load_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let store = WorkflowStateStore::at_git_root(tmp.path());
        let s = fresh_state("wf");
        store.save(&s).unwrap();
        let loaded = store.load(None, "wf").unwrap().unwrap();
        assert_eq!(loaded.workflow_name, "wf");
    }

    #[test]
    fn load_missing_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        let store = WorkflowStateStore::at_git_root(tmp.path());
        assert!(store.load(None, "nothing").unwrap().is_none());
    }

    #[test]
    fn delete_missing_is_ok() {
        let tmp = tempfile::tempdir().unwrap();
        let store = WorkflowStateStore::at_git_root(tmp.path());
        store.delete(None, "nothing").unwrap();
    }

    #[test]
    fn state_path_without_work_item() {
        let tmp = tempfile::tempdir().unwrap();
        let store = WorkflowStateStore::at_git_root(tmp.path());
        let path = store.filename_for(None, "my-workflow");
        let filename = path.file_name().unwrap().to_str().unwrap();
        assert!(
            filename.ends_with("-my-workflow.json"),
            "filename={filename}"
        );
        assert!(
            !filename.contains("-0"),
            "should not have work_item segment: {filename}"
        );
    }

    #[test]
    fn state_path_with_work_item() {
        let tmp = tempfile::tempdir().unwrap();
        let store = WorkflowStateStore::at_git_root(tmp.path());
        let path = store.filename_for(Some(42), "implement");
        let filename = path.file_name().unwrap().to_str().unwrap();
        assert!(filename.contains("-0042-"), "filename={filename}");
        assert!(filename.ends_with("-implement.json"), "filename={filename}");
    }

    #[test]
    fn state_stored_in_workflows_dir_not_engine_state() {
        let tmp = tempfile::tempdir().unwrap();
        let store = WorkflowStateStore::at_git_root(tmp.path());
        let s = fresh_state("wf");
        let path = store.save(&s).unwrap();
        let parent = path.parent().unwrap();
        assert_eq!(
            parent,
            tmp.path().join(".amux").join("workflows"),
            "state must be stored in .amux/workflows/, not a subdirectory"
        );
    }

    #[test]
    fn different_git_roots_produce_different_filenames() {
        let tmp1 = tempfile::tempdir().unwrap();
        let tmp2 = tempfile::tempdir().unwrap();
        let store1 = WorkflowStateStore::at_git_root(tmp1.path());
        let store2 = WorkflowStateStore::at_git_root(tmp2.path());
        let name1 = store1.filename_for(None, "wf");
        let name2 = store2.filename_for(None, "wf");
        assert_ne!(
            name1.file_name(),
            name2.file_name(),
            "different git roots should yield different filenames"
        );
    }

    #[test]
    fn save_load_with_work_item_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let store = WorkflowStateStore::at_git_root(tmp.path());
        let mut s = fresh_state("implement");
        s.work_item = Some(42);
        store.save(&s).unwrap();
        let loaded = store.load(Some(42), "implement").unwrap().unwrap();
        assert_eq!(loaded.work_item, Some(42));
        assert_eq!(loaded.workflow_name, "implement");
    }
}
