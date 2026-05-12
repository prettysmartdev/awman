//! Typed access to global and per-repo workflow directories.

use std::path::{Path, PathBuf};

use crate::data::config::env::{Env, EnvSnapshot};
use crate::data::config::global::GlobalConfig;
use crate::data::error::DataError;

/// Directory name for global workflows under the global home.
pub const GLOBAL_WORKFLOWS_SUBDIR: &str = "workflows";

/// Directory name for per-repo workflows under `<git_root>/.amux/`.
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
        let global_home = GlobalConfig::home_dir_with(env)?;
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
            .map(|r| r.join(".amux").join(REPO_WORKFLOWS_SUBDIR))
    }

    /// Path to the per-repo workflows directory, given an explicit git root.
    pub fn repo_dir_for(git_root: &Path) -> PathBuf {
        git_root.join(".amux").join(REPO_WORKFLOWS_SUBDIR)
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
