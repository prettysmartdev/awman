//! `MountScope` — Layer 2 helper that decides whether to mount the entire
//! git root or just the current directory when the two differ.

use std::path::{Path, PathBuf};

use crate::command::error::CommandError;
use crate::engine::message::UserMessageSink;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MountScopeDecision {
    MountGitRoot,
    MountCurrentDirOnly,
    Abort,
}

pub trait MountScopeFrontend: UserMessageSink + Send + Sync {
    /// Prompt the user when cwd is below git_root. The frontend may apply a
    /// safe default (e.g. headless returns `MountGitRoot`).
    fn ask_mount_scope(
        &mut self,
        git_root: &Path,
        cwd: &Path,
    ) -> Result<MountScopeDecision, CommandError>;
}

pub struct MountScope;

impl MountScope {
    /// Resolve the effective mount path. Calls the frontend only when the two
    /// paths differ; otherwise returns `git_root` unconditionally.
    pub fn resolve(
        cwd: &Path,
        git_root: &Path,
        frontend: &mut dyn MountScopeFrontend,
    ) -> Result<PathBuf, CommandError> {
        let cwd_canon = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
        let root_canon = git_root
            .canonicalize()
            .unwrap_or_else(|_| git_root.to_path_buf());
        if cwd_canon == root_canon {
            return Ok(root_canon);
        }
        match frontend.ask_mount_scope(&root_canon, &cwd_canon)? {
            MountScopeDecision::MountGitRoot => Ok(root_canon),
            MountScopeDecision::MountCurrentDirOnly => Ok(cwd_canon),
            MountScopeDecision::Abort => Err(CommandError::Aborted),
        }
    }
}
