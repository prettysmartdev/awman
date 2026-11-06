//! Typed access to global and per-repo workflow directories.

use std::path::{Path, PathBuf};

use crate::data::config::env::{Env, EnvSnapshot};
use crate::data::error::DataError;

/// Directory name for global workflows under the global home.
pub const GLOBAL_WORKFLOWS_SUBDIR: &str = "workflows";

/// Directory name for per-repo workflows under `<git_root>/.awman/`.
pub const REPO_WORKFLOWS_SUBDIR: &str = "workflows";

/// Resolves global and per-repo workflow directories.
#[derive(Debug, Clone)]
pub struct WorkflowDirs {
    global_home: PathBuf,
    git_root: Option<PathBuf>,
}

impl WorkflowDirs {
    /// Construct from the current process environment.
    pub fn from_process_env(git_root: Option<PathBuf>) -> Result<Self, DataError> {
        Self::from_env(&Env::from_process(), git_root)
    }

    /// Same as [`from_process_env`] but reads from a supplied env snapshot.
    pub fn from_env(env: &EnvSnapshot, git_root: Option<PathBuf>) -> Result<Self, DataError> {
        let global_home = crate::data::fs::data_paths::data_home(env)?;
        Ok(Self {
            global_home,
            git_root,
        })
    }

    /// Path to the global workflows directory.
    pub fn global_dir(&self) -> PathBuf {
        self.global_home.join(GLOBAL_WORKFLOWS_SUBDIR)
    }

    /// Path to the per-repo workflows directory, if a git root is bound.
    pub fn repo_dir(&self) -> Option<PathBuf> {
        self.git_root
            .as_ref()
            .map(|r| r.join(".awman").join(REPO_WORKFLOWS_SUBDIR))
    }

    /// Path to the per-repo workflows directory, given an explicit git root.
    pub fn repo_dir_for(git_root: &Path) -> PathBuf {
        git_root.join(".awman").join(REPO_WORKFLOWS_SUBDIR)
    }

    /// Where a dynamic run stashes the `workflow.toml` its leader generated:
    /// `<git_root>/.awman/workflows/dynamic-NNNN.toml`.
    ///
    /// The leader writes the real file into its own per-invocation context
    /// directory, which is keyed by a random session UUID and so is
    /// unreachable from a later run. Saving a copy here — beside the engine's
    /// `WorkflowState` JSON, inside the run's worktree — is what makes a failed
    /// dynamic run resumable (WI-0115 §2). Both halves die with the worktree,
    /// and a run that finishes cleanly deletes both itself.
    ///
    /// Note this directory is also where a repo keeps its own committed
    /// workflow definitions, so these files are not gitignored for free —
    /// awman writes no ignore rule. They are named `dynamic-NNNN.toml` to stay
    /// obviously machine-written, and they live inside a worktree that is
    /// itself transient.
    pub fn dynamic_workflow_path(git_root: &Path, work_item: u32) -> PathBuf {
        Self::repo_dir_for(git_root).join(format!("dynamic-{work_item:04}.toml"))
    }

    /// Create the global workflows directory on disk, if missing.
    pub fn ensure_global(&self) -> Result<PathBuf, DataError> {
        let dir = self.global_dir();
        std::fs::create_dir_all(&dir).map_err(|e| DataError::io(&dir, e))?;
        Ok(dir)
    }

    /// Create the per-repo workflows directory on disk, if a git root is bound.
    pub fn ensure_repo(&self) -> Result<Option<PathBuf>, DataError> {
        let Some(dir) = self.repo_dir() else {
            return Ok(None);
        };
        std::fs::create_dir_all(&dir).map_err(|e| DataError::io(&dir, e))?;
        Ok(Some(dir))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_dir_for_is_under_dot_awman() {
        assert_eq!(
            WorkflowDirs::repo_dir_for(Path::new("/r/proj")),
            PathBuf::from("/r/proj/.awman/workflows")
        );
    }

    #[test]
    fn dynamic_workflow_path_is_zero_padded_beside_the_state_file() {
        assert_eq!(
            WorkflowDirs::dynamic_workflow_path(Path::new("/r/proj"), 7),
            PathBuf::from("/r/proj/.awman/workflows/dynamic-0007.toml")
        );
    }

    #[test]
    fn dynamic_workflow_paths_differ_per_work_item() {
        let a = WorkflowDirs::dynamic_workflow_path(Path::new("/r/proj"), 7);
        let b = WorkflowDirs::dynamic_workflow_path(Path::new("/r/proj"), 8);
        assert_ne!(a, b);
    }
}
