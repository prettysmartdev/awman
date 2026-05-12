//! Persists `WorkflowInvocation` to disk.
//!
//! Replaces the free `pub fn`s `workflow_state_path`, `save_workflow_state`,
//! `load_workflow_state`, and `validate_resume_compatibility` from
//! `oldsrc/workflow/mod.rs`.

use std::path::{Path, PathBuf};

use crate::data::error::DataError;
use crate::data::session::{WorkflowInvocation, WorkflowStepRecord};

use super::workflow_dirs::WorkflowDirs;

/// Subdirectory under `<git_root>/.amux/` holding per-workflow state files.
pub const WORKFLOW_STATE_SUBDIR: &str = "workflows";

/// Persists workflow state files under a git root.
#[derive(Debug, Clone)]
pub struct WorkflowStateStore {
    git_root: PathBuf,
}

impl WorkflowStateStore {
    /// Construct a store rooted at `<git_root>/.amux/workflows`.
    pub fn at_git_root(git_root: impl Into<PathBuf>) -> Self {
        Self {
            git_root: git_root.into(),
        }
    }

    /// Directory in which state files live.
    pub fn dir(&self) -> PathBuf {
        WorkflowDirs::repo_dir_for(&self.git_root)
    }

    /// Resolve the on-disk path for the state of a given workflow.
    pub fn state_path(&self, work_item: Option<u32>, workflow_name: &str) -> PathBuf {
        let repo_hash = &sha256_hex(&self.git_root.to_string_lossy())[..8];
        let filename = match work_item {
            Some(wi) => format!("{repo_hash}-{wi:04}-{workflow_name}.json"),
            None => format!("{repo_hash}-{workflow_name}.json"),
        };
        self.dir().join(filename)
    }

    /// Persist a workflow invocation's state to disk.
    pub fn save(&self, invocation: &WorkflowInvocation) -> Result<PathBuf, DataError> {
        let path = self.state_path(invocation.work_item, &invocation.workflow_name);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| DataError::io(dir, e))?;
        }
        let json = serde_json::to_string_pretty(invocation)
            .map_err(|e| DataError::ConfigSerialize { source: e })?;
        std::fs::write(&path, json).map_err(|e| DataError::io(&path, e))?;
        Ok(path)
    }

    /// Load a workflow invocation's state from a specific path.
    pub fn load_path(path: &Path) -> Result<WorkflowInvocation, DataError> {
        let content = std::fs::read_to_string(path).map_err(|e| DataError::io(path, e))?;
        serde_json::from_str(&content).map_err(|e| DataError::config_parse(path, e))
    }

    /// Load a workflow invocation's state by name and work-item.
    pub fn load(
        &self,
        work_item: Option<u32>,
        workflow_name: &str,
    ) -> Result<WorkflowInvocation, DataError> {
        let path = self.state_path(work_item, workflow_name);
        Self::load_path(&path)
    }

    /// Validate that a resume's parsed steps match a saved invocation's step
    /// names and dependency edges.
    ///
    /// Returns `Err(DataError::WorkflowResumeIncompatible)` when the new step
    /// list cannot be safely resumed against the saved state.
    pub fn validate_resume_compatibility(
        saved: &WorkflowInvocation,
        new_steps: &[WorkflowStepRecord],
    ) -> Result<(), DataError> {
        if saved.steps.len() != new_steps.len() {
            return Err(DataError::WorkflowResumeIncompatible(format!(
                "the workflow now has {} steps but the saved state has {}",
                new_steps.len(),
                saved.steps.len()
            )));
        }
        for (saved_step, new_step) in saved.steps.iter().zip(new_steps.iter()) {
            if saved_step.name != new_step.name {
                return Err(DataError::WorkflowResumeIncompatible(format!(
                    "step order changed — expected '{}' but found '{}'",
                    saved_step.name, new_step.name
                )));
            }
            if saved_step.depends_on != new_step.depends_on {
                return Err(DataError::WorkflowResumeIncompatible(format!(
                    "step '{}' depends-on changed from {:?} to {:?}",
                    saved_step.name, saved_step.depends_on, new_step.depends_on
                )));
            }
        }
        Ok(())
    }
}

/// Compute the SHA-256 hash of `data`, returned as a lowercase hex string.
pub fn sha256_hex(data: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(data.as_bytes());
    let result = hasher.finalize();
    result.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn make_invocation(workflow_name: &str) -> WorkflowInvocation {
        WorkflowInvocation {
            id: Uuid::new_v4(),
            title: Some("Test Workflow".to_string()),
            workflow_name: workflow_name.to_string(),
            workflow_hash: sha256_hex("some-workflow-content"),
            work_item: None,
            steps: vec![
                WorkflowStepRecord {
                    name: "step-one".to_string(),
                    depends_on: vec![],
                    prompt_template: "Do thing A".to_string(),
                    status: crate::data::session::StepStatus::Pending,
                    container_id: None,
                    agent: Some("claude".to_string()),
                    model: None,
                },
                WorkflowStepRecord {
                    name: "step-two".to_string(),
                    depends_on: vec!["step-one".to_string()],
                    prompt_template: "Do thing B after A".to_string(),
                    status: crate::data::session::StepStatus::Pending,
                    container_id: None,
                    agent: None,
                    model: Some("claude-3-5-sonnet".to_string()),
                },
            ],
            paused: false,
            yolo: true,
            auto: false,
            current_step: Some(0),
        }
    }

    // ─── save / load round-trip ───────────────────────────────────────────────

    #[test]
    fn save_load_round_trip_preserves_all_fields() {
        let tmp = tempfile::tempdir().unwrap();
        let store = WorkflowStateStore::at_git_root(tmp.path());
        let invocation = make_invocation("my-workflow");

        let saved_path = store.save(&invocation).unwrap();
        assert!(saved_path.exists());

        let loaded = store.load(None, "my-workflow").unwrap();
        assert_eq!(loaded.id, invocation.id);
        assert_eq!(loaded.title, invocation.title);
        assert_eq!(loaded.workflow_name, invocation.workflow_name);
        assert_eq!(loaded.workflow_hash, invocation.workflow_hash);
        assert_eq!(loaded.steps.len(), 2);
        assert_eq!(loaded.steps[0].name, "step-one");
        assert_eq!(loaded.steps[1].name, "step-two");
        assert_eq!(loaded.steps[1].depends_on, vec!["step-one"]);
        assert!(loaded.yolo);
        assert_eq!(loaded.current_step, Some(0));
    }

    #[test]
    fn save_load_with_work_item_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let store = WorkflowStateStore::at_git_root(tmp.path());
        let mut invocation = make_invocation("implement");
        invocation.work_item = Some(42);

        store.save(&invocation).unwrap();
        let loaded = store.load(Some(42), "implement").unwrap();
        assert_eq!(loaded.work_item, Some(42));
        assert_eq!(loaded.workflow_name, "implement");
    }

    #[test]
    fn load_path_reads_from_explicit_file() {
        let tmp = tempfile::tempdir().unwrap();
        let store = WorkflowStateStore::at_git_root(tmp.path());
        let invocation = make_invocation("direct-load");
        let path = store.save(&invocation).unwrap();

        let loaded = WorkflowStateStore::load_path(&path).unwrap();
        assert_eq!(loaded.id, invocation.id);
    }

    // ─── state_path ───────────────────────────────────────────────────────────

    #[test]
    fn state_path_without_work_item_contains_workflow_name() {
        let tmp = tempfile::tempdir().unwrap();
        let store = WorkflowStateStore::at_git_root(tmp.path());
        let path = store.state_path(None, "my-workflow");
        let filename = path.file_name().unwrap().to_str().unwrap();
        assert!(filename.contains("my-workflow"), "filename={filename}");
        assert!(filename.ends_with(".json"));
    }

    #[test]
    fn state_path_with_work_item_contains_zero_padded_number() {
        let tmp = tempfile::tempdir().unwrap();
        let store = WorkflowStateStore::at_git_root(tmp.path());
        let path = store.state_path(Some(66), "implement");
        let filename = path.file_name().unwrap().to_str().unwrap();
        assert!(filename.contains("0066"), "filename={filename}");
        assert!(filename.contains("implement"), "filename={filename}");
    }

    #[test]
    fn state_path_different_git_roots_produce_different_filenames() {
        let tmp1 = tempfile::tempdir().unwrap();
        let tmp2 = tempfile::tempdir().unwrap();
        let store1 = WorkflowStateStore::at_git_root(tmp1.path());
        let store2 = WorkflowStateStore::at_git_root(tmp2.path());
        let path1 = store1.state_path(None, "wf");
        let path2 = store2.state_path(None, "wf");
        // The hash prefix should differ because the git roots differ.
        let name1 = path1.file_name().unwrap().to_str().unwrap();
        let name2 = path2.file_name().unwrap().to_str().unwrap();
        assert_ne!(
            name1, name2,
            "different git roots should yield different state filenames"
        );
    }

    // ─── validate_resume_compatibility ───────────────────────────────────────

    #[test]
    fn validate_resume_compat_same_steps_ok() {
        let inv = make_invocation("wf");
        let same_steps = inv.steps.clone();
        WorkflowStateStore::validate_resume_compatibility(&inv, &same_steps).unwrap();
    }

    #[test]
    fn validate_resume_compat_different_step_count_err() {
        let inv = make_invocation("wf");
        let one_step = vec![inv.steps[0].clone()];
        let err = WorkflowStateStore::validate_resume_compatibility(&inv, &one_step).unwrap_err();
        assert!(
            matches!(err, DataError::WorkflowResumeIncompatible(_)),
            "expected WorkflowResumeIncompatible, got {err:?}"
        );
    }

    #[test]
    fn validate_resume_compat_different_name_err() {
        let inv = make_invocation("wf");
        let mut renamed_steps = inv.steps.clone();
        renamed_steps[0].name = "renamed-step".to_string();
        let err =
            WorkflowStateStore::validate_resume_compatibility(&inv, &renamed_steps).unwrap_err();
        assert!(matches!(err, DataError::WorkflowResumeIncompatible(_)));
    }

    #[test]
    fn validate_resume_compat_different_depends_on_err() {
        let inv = make_invocation("wf");
        let mut changed_deps = inv.steps.clone();
        changed_deps[1].depends_on = vec!["something-else".to_string()];
        let err =
            WorkflowStateStore::validate_resume_compatibility(&inv, &changed_deps).unwrap_err();
        assert!(matches!(err, DataError::WorkflowResumeIncompatible(_)));
    }
}
