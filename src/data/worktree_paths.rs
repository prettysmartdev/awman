//! Worktree path resolution — Layer 0.
//!
//! Resolves `~/.awman/worktrees/<dirname>-<hash8>/...` paths and the
//! deterministic branch names that go with them. Pure path computation — no
//! git invocation and no canonicalization (that's `GitEngine`): the caller
//! passes the canonical git root.

use std::path::{Path, PathBuf};

use crate::data::error::DataError;
use crate::data::fs::hash::sha256_hex;

/// Branch name for a work-item worktree: `awman/work-item-NNNN`.
pub fn worktree_branch_name(work_item: u32) -> String {
    format!("awman/work-item-{work_item:04}")
}

/// Branch name for a named workflow worktree: `awman/workflow-<name>`.
pub fn worktree_branch_name_for_workflow(name: &str) -> String {
    format!("awman/workflow-{name}")
}

/// `<dirname>-<hash8>`: dirname = last component of `git_root` (or `repo`),
/// hash8 = first 8 lowercase hex digits of SHA-256 over the path's
/// `to_string_lossy()` bytes. Two checkouts that share a directory name get
/// different keys. `git_root` must already be canonical.
pub fn repo_key(git_root: &Path) -> String {
    let hash = sha256_hex(git_root.to_string_lossy().as_bytes());
    format!("{}-{}", repo_dirname(git_root), &hash[..8])
}

/// Last path component of `git_root`, or `repo` when there is none.
fn repo_dirname(git_root: &Path) -> &str {
    git_root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("repo")
}

/// Resolves worktree paths beneath `<HOME>/.awman/worktrees/<dirname>-<hash8>/`.
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

    /// `~/.awman/worktrees/<dirname>-<hash8>/<NNNN>/`.
    pub fn for_work_item(&self, git_root: &Path, work_item: u32) -> PathBuf {
        self.repo_dir(&repo_key(git_root))
            .join(format!("{work_item:04}"))
    }

    /// `~/.awman/worktrees/<dirname>-<hash8>/wf-<name>/`.
    pub fn for_workflow(&self, git_root: &Path, name: &str) -> PathBuf {
        self.repo_dir(&repo_key(git_root))
            .join(format!("wf-{name}"))
    }

    /// Legacy unhashed `~/.awman/worktrees/<dirname>/<NNNN>/`. Only used to
    /// find worktrees created before the hashed key; never for new ones.
    pub fn legacy_for_work_item(&self, git_root: &Path, work_item: u32) -> PathBuf {
        self.repo_dir(repo_dirname(git_root))
            .join(format!("{work_item:04}"))
    }

    /// Legacy unhashed `~/.awman/worktrees/<dirname>/wf-<name>/`. Only used to
    /// find worktrees created before the hashed key; never for new ones.
    pub fn legacy_for_workflow(&self, git_root: &Path, name: &str) -> PathBuf {
        self.repo_dir(repo_dirname(git_root))
            .join(format!("wf-{name}"))
    }

    /// `~/.awman/worktrees/<dir>/`.
    fn repo_dir(&self, dir: &str) -> PathBuf {
        self.home.join(".awman").join("worktrees").join(dir)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn work_item_path_is_zero_padded() {
        let p = WorktreePaths::with_home("/h");
        let root = Path::new("/r/myproj");
        let path = p.for_work_item(root, 7);
        assert!(path.ends_with(format!("worktrees/{}/0007", repo_key(root))));
    }

    #[test]
    fn workflow_path_uses_wf_prefix() {
        let p = WorktreePaths::with_home("/h");
        let root = Path::new("/r/myproj");
        let path = p.for_workflow(root, "build");
        assert!(path.ends_with(format!("worktrees/{}/wf-build", repo_key(root))));
    }

    #[test]
    fn same_directory_name_checkouts_have_distinct_deterministic_keys() {
        let paths = WorktreePaths::with_home("/h");
        let first_root = Path::new("/checkout-a/myrepo");
        let second_root = Path::new("/checkout-b/myrepo");
        let first_key = repo_key(first_root);
        let second_key = repo_key(second_root);

        for key in [&first_key, &second_key] {
            let (dirname, hash) = key.rsplit_once('-').unwrap();
            assert_eq!(dirname, "myrepo");
            assert_eq!(hash.len(), 8);
            assert!(hash
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
        }
        assert_ne!(first_key, second_key);
        assert_eq!(first_key, repo_key(first_root));
        assert_eq!(second_key, repo_key(second_root));

        for (root, key) in [(first_root, &first_key), (second_root, &second_key)] {
            assert!(paths
                .for_work_item(root, 134)
                .ends_with(format!("worktrees/{key}/0134")));
            assert!(paths
                .for_workflow(root, "same")
                .ends_with(format!("worktrees/{key}/wf-same")));
        }
    }

    #[test]
    fn branch_names_are_stable() {
        assert_eq!(worktree_branch_name(0), "awman/work-item-0000");
        assert_eq!(worktree_branch_name(42), "awman/work-item-0042");
        assert_eq!(worktree_branch_name_for_workflow("x"), "awman/workflow-x");
    }
}
