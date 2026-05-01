//! Worktree path resolution — Layer 0.
//!
//! Resolves `~/.amux/worktrees/<repo-name>/...` paths and the deterministic
//! branch names that go with them. Pure path computation — no git invocation
//! (that's `GitEngine`).

use std::path::{Path, PathBuf};

use crate::data::error::DataError;

/// Branch name for a work-item worktree: `amux/work-item-NNNN`.
pub fn worktree_branch_name(work_item: u32) -> String {
    format!("amux/work-item-{work_item:04}")
}

/// Branch name for a named workflow worktree: `amux/workflow-<name>`.
pub fn worktree_branch_name_for_workflow(name: &str) -> String {
    format!("amux/workflow-{name}")
}

/// Resolves worktree paths beneath `<HOME>/.amux/worktrees/<repo-name>/`.
#[derive(Debug, Clone)]
pub struct WorktreePaths {
    home: PathBuf,
}

impl WorktreePaths {
    /// Construct using the OS home dir. Returns `HomeNotFound` when no home is
    /// resolvable.
    pub fn from_home() -> Result<Self, DataError> {
        let home = dirs::home_dir().ok_or(DataError::HomeNotFound)?;
        Ok(Self { home })
    }

    /// Construct with an explicit home directory (mostly for tests).
    pub fn with_home(home: impl Into<PathBuf>) -> Self {
        Self { home: home.into() }
    }

    /// `~/.amux/worktrees/<repo-name>/<NNNN>/`.
    pub fn for_work_item(&self, git_root: &Path, work_item: u32) -> PathBuf {
        let repo = git_root
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("repo");
        self.home
            .join(".amux")
            .join("worktrees")
            .join(repo)
            .join(format!("{work_item:04}"))
    }

    /// `~/.amux/worktrees/<repo-name>/wf-<name>/`.
    pub fn for_workflow(&self, git_root: &Path, name: &str) -> PathBuf {
        let repo = git_root
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("repo");
        self.home
            .join(".amux")
            .join("worktrees")
            .join(repo)
            .join(format!("wf-{name}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn work_item_path_is_zero_padded() {
        let p = WorktreePaths::with_home("/h");
        let path = p.for_work_item(Path::new("/r/myproj"), 7);
        assert!(path.ends_with("worktrees/myproj/0007"));
    }

    #[test]
    fn workflow_path_uses_wf_prefix() {
        let p = WorktreePaths::with_home("/h");
        let path = p.for_workflow(Path::new("/r/myproj"), "build");
        assert!(path.ends_with("worktrees/myproj/wf-build"));
    }

    #[test]
    fn branch_names_are_stable() {
        assert_eq!(worktree_branch_name(0), "amux/work-item-0000");
        assert_eq!(worktree_branch_name(42), "amux/work-item-0042");
        assert_eq!(worktree_branch_name_for_workflow("x"), "amux/workflow-x");
    }
}
