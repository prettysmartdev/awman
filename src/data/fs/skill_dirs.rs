//! Typed access to global and per-repo skill directories.

use std::path::{Path, PathBuf};

use crate::data::config::env::{Env, EnvSnapshot};
use crate::data::config::global::GlobalConfig;
use crate::data::error::DataError;

/// Directory name for global skills under the global home.
pub const GLOBAL_SKILLS_SUBDIR: &str = "skills";

/// Directory name for per-repo skills under `<git_root>/.claude/`.
pub const REPO_SKILLS_SUBDIR: &str = "skills";

/// Resolves global and per-repo skill directories.
#[derive(Debug, Clone)]
pub struct SkillDirs {
    global_home: PathBuf,
    git_root: Option<PathBuf>,
}

impl SkillDirs {
    /// Construct from the current process environment, resolving the global
    /// home via `AMUX_CONFIG_HOME` (when set) or `$HOME/.amux`.
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

    /// Path to the global skills directory.
    pub fn global_dir(&self) -> PathBuf {
        self.global_home.join(GLOBAL_SKILLS_SUBDIR)
    }

    /// Path to the per-repo skills directory, if a git root is bound.
    pub fn repo_dir(&self) -> Option<PathBuf> {
        self.git_root
            .as_ref()
            .map(|r| r.join(".claude").join(REPO_SKILLS_SUBDIR))
    }

    /// Path to the per-repo skills directory, given an explicit git root.
    pub fn repo_dir_for(git_root: &Path) -> PathBuf {
        git_root.join(".claude").join(REPO_SKILLS_SUBDIR)
    }

    /// Create the global skills directory on disk, if missing.
    pub fn ensure_global(&self) -> Result<PathBuf, DataError> {
        let dir = self.global_dir();
        std::fs::create_dir_all(&dir).map_err(|e| DataError::io(&dir, e))?;
        Ok(dir)
    }

    /// Create the per-repo skills directory on disk, if a git root is bound.
    pub fn ensure_repo(&self) -> Result<Option<PathBuf>, DataError> {
        let Some(dir) = self.repo_dir() else {
            return Ok(None);
        };
        std::fs::create_dir_all(&dir).map_err(|e| DataError::io(&dir, e))?;
        Ok(Some(dir))
    }
}
