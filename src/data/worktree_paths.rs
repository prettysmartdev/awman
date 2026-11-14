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

/// Directory name for a session worktree created by `--branch <branch>`:
/// `br-<sanitized>`. Every character outside ASCII `[A-Za-z0-9._-]` becomes
/// `-` (case is preserved). When that changed the name, `-<hash8>` of the
/// original branch name's SHA-256 is appended, so `a/b` and `a-b` map to
/// different directories. Names are never truncated.
pub fn branch_worktree_dir_name(branch: &str) -> String {
    let sanitized: String = branch
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    if sanitized == branch {
        format!("br-{sanitized}")
    } else {
        let hash = sha256_hex(branch.as_bytes());
        format!("br-{sanitized}-{}", &hash[..8])
    }
}

/// Directory name for a session worktree created by `--pr <N>`: `pr-<N>`.
pub fn pull_request_worktree_dir_name(number: u32) -> String {
    format!("pr-{number}")
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
    /// Resolve against the invoking instance, including when a daemon's
    /// own process has a different HOME.
    pub fn from_env(env: &crate::data::config::env::EnvSnapshot) -> Result<Self, DataError> {
        Ok(Self::with_home(
            env.home_dir().ok_or(DataError::HomeNotFound)?,
        ))
    }
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

    /// `~/.awman/worktrees/<dirname>-<hash8>/<name>/`, with `name` joined
    /// verbatim (no `wf-` prefix). The caller must pass one validated, normal
    /// path component; this layer does no validation.
    pub fn named(&self, git_root: &Path, name: &str) -> PathBuf {
        self.repo_dir(&repo_key(git_root)).join(name)
    }

    /// Legacy unhashed `~/.awman/worktrees/<dirname>/<name>/`. Only used to
    /// find worktrees created before the hashed key; never for new ones.
    pub fn legacy_named(&self, git_root: &Path, name: &str) -> PathBuf {
        self.repo_dir(repo_dirname(git_root)).join(name)
    }

    /// `~/.awman/worktrees/<dir>/`.
    fn repo_dir(&self, dir: &str) -> PathBuf {
        self.home.join(".awman").join("worktrees").join(dir)
    }
}
