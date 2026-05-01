//! Engine-level workflow state persistence — Layer 0.
//!
//! Persists `WorkflowState` snapshots under `<git-root>/.amux/workflows/`. The
//! filename pattern matches the legacy on-disk layout (`<repohash8>-...-name.json`)
//! to keep in-flight resumes working across the refactor. Coexists with
//! `fs/workflow_state.rs` (which persists `WorkflowInvocation` for session-level
//! state).

use std::path::{Path, PathBuf};

use crate::data::error::DataError;
use crate::data::fs::workflow_state::sha256_hex;
use crate::data::session::Session;
use crate::data::workflow_state::WorkflowState;

/// Subdirectory under `<git_root>/.amux/` holding engine-level workflow state.
pub const ENGINE_STATE_SUBDIR: &str = "engine-state";

/// Persists engine-level `WorkflowState` to `<git_root>/.amux/workflows/engine-state/`.
#[derive(Debug, Clone)]
pub struct WorkflowStateStore {
    base_dir: PathBuf,
    /// One-time legacy migration source (e.g. `<HOME>/.amux/workflow-state/`).
    /// Scanned on first `load(name)` call.
    legacy_fallback: Option<PathBuf>,
}

impl WorkflowStateStore {
    /// Construct a store rooted at `<git_root>/.amux/workflows/engine-state/`.
    /// The legacy fallback at `<HOME>/.amux/workflow-state/` is consulted on
    /// first load if present.
    pub fn new(session: &Session) -> Self {
        let base_dir = session
            .git_root()
            .join(".amux")
            .join("workflows")
            .join(ENGINE_STATE_SUBDIR);
        let legacy_fallback = dirs::home_dir().map(|h| h.join(".amux").join("workflow-state"));
        Self {
            base_dir,
            legacy_fallback,
        }
    }

    /// Construct without a session (used by tests and command setup that
    /// already resolved the git root).
    pub fn at_git_root(git_root: impl Into<PathBuf>) -> Self {
        let base_dir = git_root
            .into()
            .join(".amux")
            .join("workflows")
            .join(ENGINE_STATE_SUBDIR);
        Self {
            base_dir,
            legacy_fallback: None,
        }
    }

    /// Override the legacy fallback location (mostly for tests).
    pub fn with_legacy_fallback(mut self, dir: Option<PathBuf>) -> Self {
        self.legacy_fallback = dir;
        self
    }

    /// Directory in which state files live.
    pub fn dir(&self) -> &Path {
        &self.base_dir
    }

    fn filename_for(&self, workflow_name: &str) -> PathBuf {
        let key = sha256_hex(&self.base_dir.to_string_lossy())
            .chars()
            .take(8)
            .collect::<String>();
        self.base_dir.join(format!("{key}-{workflow_name}.json"))
    }

    /// Load a workflow's state by name. Returns `Ok(None)` when no state file
    /// exists. On first call, scans `legacy_fallback` and copies any matching
    /// files into `base_dir` (one-time migration).
    pub fn load(&self, workflow_name: &str) -> Result<Option<WorkflowState>, DataError> {
        self.maybe_migrate_legacy()?;
        let path = self.filename_for(workflow_name);
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
        std::fs::create_dir_all(&self.base_dir).map_err(|e| DataError::io(&self.base_dir, e))?;
        let path = self.filename_for(&state.workflow_name);
        let json = serde_json::to_string_pretty(state)
            .map_err(|e| DataError::ConfigSerialize { source: e })?;
        std::fs::write(&path, json).map_err(|e| DataError::io(&path, e))?;
        Ok(path)
    }

    /// Delete a workflow's state file. Returns `Ok(())` when the file is absent
    /// (idempotent).
    pub fn delete(&self, workflow_name: &str) -> Result<(), DataError> {
        let path = self.filename_for(workflow_name);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(DataError::io(&path, e)),
        }
    }

    fn maybe_migrate_legacy(&self) -> Result<(), DataError> {
        let Some(legacy) = self.legacy_fallback.as_ref() else {
            return Ok(());
        };
        if !legacy.is_dir() {
            return Ok(());
        }
        let entries = match std::fs::read_dir(legacy) {
            Ok(e) => e,
            Err(_) => return Ok(()),
        };
        for entry in entries.flatten() {
            let from = entry.path();
            if from.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let name = match from.file_name() {
                Some(n) => n.to_owned(),
                None => continue,
            };
            std::fs::create_dir_all(&self.base_dir)
                .map_err(|e| DataError::io(&self.base_dir, e))?;
            let to = self.base_dir.join(&name);
            if to.exists() {
                continue;
            }
            // Copy rather than move — leaves legacy file in place for any
            // remaining oldsrc readers during the transition.
            if let Err(e) = std::fs::copy(&from, &to) {
                return Err(DataError::io(&to, e));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_state(name: &str) -> WorkflowState {
        WorkflowState::new(name.to_string(), &[], "hash".into())
    }

    #[test]
    fn save_load_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let store = WorkflowStateStore::at_git_root(tmp.path());
        let s = fresh_state("wf");
        store.save(&s).unwrap();
        let loaded = store.load("wf").unwrap().unwrap();
        assert_eq!(loaded.workflow_name, "wf");
    }

    #[test]
    fn load_missing_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        let store = WorkflowStateStore::at_git_root(tmp.path());
        assert!(store.load("nothing").unwrap().is_none());
    }

    #[test]
    fn delete_missing_is_ok() {
        let tmp = tempfile::tempdir().unwrap();
        let store = WorkflowStateStore::at_git_root(tmp.path());
        store.delete("nothing").unwrap();
    }

    #[test]
    fn legacy_fallback_migrates_files_on_first_load() {
        let git = tempfile::tempdir().unwrap();
        let legacy = tempfile::tempdir().unwrap();
        // Write a state file at the legacy location matching the new key format.
        let store_for_key =
            WorkflowStateStore::at_git_root(git.path()).with_legacy_fallback(None);
        let target = store_for_key.filename_for("wf");
        let basename = target.file_name().unwrap();
        let legacy_path = legacy.path().join(basename);
        std::fs::write(
            &legacy_path,
            serde_json::to_string(&fresh_state("wf")).unwrap(),
        )
        .unwrap();

        let store = WorkflowStateStore::at_git_root(git.path())
            .with_legacy_fallback(Some(legacy.path().to_path_buf()));
        let loaded = store.load("wf").unwrap();
        assert!(loaded.is_some());
    }
}
