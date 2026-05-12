//! `engine::git` — `GitEngine`. Consolidates every git operation amux performs.
//!
//! Replaces the free `pub fn`s in `oldsrc/git.rs` with a typed object whose
//! methods are the only public surface. Implements Layer 0's
//! `GitRootResolver` trait so `Session::open` can use it.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::data::error::DataError;
use crate::data::session::GitRootResolver;
use crate::data::worktree_paths::{
    worktree_branch_name, worktree_branch_name_for_workflow, WorktreePaths,
};
use crate::engine::error::EngineError;
use crate::engine::message::{MessageLevel, UserMessage, UserMessageSink};

/// Run a git command and log both the command line and output to the sink.
fn run_git_logged(
    args: &[&str],
    cwd: &Path,
    sink: &mut dyn UserMessageSink,
) -> Result<std::process::Output, EngineError> {
    let cmd_str = format!("git {}", args.join(" "));
    sink.write_message(UserMessage {
        level: MessageLevel::Info,
        text: format!("$ {cmd_str}"),
    });
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .map_err(|e| EngineError::Git(format!("invoke `{cmd_str}`: {e}")))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    for line in stdout.lines().chain(stderr.lines()) {
        if !line.trim().is_empty() {
            sink.write_message(UserMessage {
                level: if output.status.success() {
                    MessageLevel::Info
                } else {
                    MessageLevel::Warning
                },
                text: line.to_string(),
            });
        }
    }
    Ok(output)
}

/// Parsed `git --version` result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitVersion {
    pub major: u32,
    pub minor: u32,
}

#[derive(Debug, Default, Clone)]
pub struct GitEngine;

impl GitEngine {
    pub fn new() -> Self {
        Self
    }

    /// Verify `git` is installed and version >= 2.5 (worktree support).
    pub fn version_check(&self) -> Result<GitVersion, EngineError> {
        let output = Command::new("git")
            .args(["--version"])
            .output()
            .map_err(|e| EngineError::Git(format!("invoke `git --version`: {e}")))?;
        let s = String::from_utf8_lossy(&output.stdout);
        let ver_str = s.trim().strip_prefix("git version ").ok_or_else(|| {
            EngineError::Git(format!("could not parse git version from: {}", s.trim()))
        })?;
        let parts: Vec<&str> = ver_str.split('.').collect();
        let major = parts
            .first()
            .and_then(|s| s.parse::<u32>().ok())
            .ok_or_else(|| EngineError::Git(format!("malformed git version: {ver_str}")))?;
        let minor = parts
            .get(1)
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(0);
        if major > 2 || (major == 2 && minor >= 5) {
            Ok(GitVersion { major, minor })
        } else {
            Err(EngineError::Git(format!(
                "git >= 2.5 is required for --worktree support (found {ver_str})"
            )))
        }
    }

    /// Resolve the git root for the given working directory via `git rev-parse
    /// --show-toplevel`.
    pub fn resolve_root(&self, working_dir: &Path) -> Result<PathBuf, EngineError> {
        let output = Command::new("git")
            .args(["rev-parse", "--show-toplevel"])
            .current_dir(working_dir)
            .output()
            .map_err(|e| EngineError::Git(format!("invoke `git rev-parse`: {e}")))?;
        if !output.status.success() {
            return Err(EngineError::Data(DataError::GitRootNotFound {
                working_dir: working_dir.to_path_buf(),
            }));
        }
        let s = String::from_utf8_lossy(&output.stdout);
        Ok(PathBuf::from(s.trim()))
    }

    /// Returns whether the worktree at `path` has zero uncommitted changes.
    pub fn is_clean(&self, path: &Path) -> Result<bool, EngineError> {
        Ok(self.uncommitted_files(path)?.is_empty())
    }

    /// `git status --porcelain` lines (one per uncommitted file).
    pub fn uncommitted_files(&self, path: &Path) -> Result<Vec<String>, EngineError> {
        let output = Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(path)
            .output()
            .map_err(|e| EngineError::Git(format!("invoke `git status --porcelain`: {e}")))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(EngineError::Git(format!(
                "git status failed: {}",
                stderr.trim()
            )));
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        Ok(stdout
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| l.to_string())
            .collect())
    }

    /// `~/.amux/worktrees/<repo-name>/<NNNN>/` for a work-item.
    pub fn worktree_path(&self, git_root: &Path, work_item: u32) -> Result<PathBuf, EngineError> {
        let p = WorktreePaths::from_home().map_err(EngineError::Data)?;
        Ok(p.for_work_item(git_root, work_item))
    }

    /// `~/.amux/worktrees/<repo-name>/wf-<name>/` for a named workflow.
    pub fn worktree_path_named(&self, git_root: &Path, name: &str) -> Result<PathBuf, EngineError> {
        let p = WorktreePaths::from_home().map_err(EngineError::Data)?;
        Ok(p.for_workflow(git_root, name))
    }

    /// Branch name for a work-item (`amux/work-item-NNNN`).
    pub fn branch_name_for_work_item(&self, work_item: u32) -> String {
        worktree_branch_name(work_item)
    }

    /// Branch name for a named workflow (`amux/workflow-<name>`).
    pub fn branch_name_for_workflow(&self, name: &str) -> String {
        worktree_branch_name_for_workflow(name)
    }

    /// `git worktree add <path> [-b] <branch>`.
    pub fn create_worktree(
        &self,
        git_root: &Path,
        worktree_path: &Path,
        branch: &str,
    ) -> Result<(), EngineError> {
        std::fs::create_dir_all(worktree_path.parent().unwrap_or(worktree_path))
            .map_err(|e| EngineError::io(worktree_path, e))?;
        let wt_str = worktree_path
            .to_str()
            .ok_or_else(|| EngineError::Git("worktree path not UTF-8".into()))?;
        let args: Vec<&str> = if self.branch_exists(git_root, branch) {
            vec!["worktree", "add", wt_str, branch]
        } else {
            vec!["worktree", "add", wt_str, "-b", branch]
        };
        let output = Command::new("git")
            .args(&args)
            .current_dir(git_root)
            .output()
            .map_err(|e| EngineError::Git(format!("invoke `git worktree add`: {e}")))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(EngineError::Git(format!(
                "git worktree add failed: {}",
                stderr.trim()
            )));
        }
        Ok(())
    }

    pub fn remove_worktree(
        &self,
        git_root: &Path,
        worktree_path: &Path,
    ) -> Result<(), EngineError> {
        let wt_str = worktree_path
            .to_str()
            .ok_or_else(|| EngineError::Git("worktree path not UTF-8".into()))?;
        let output = Command::new("git")
            .args(["worktree", "remove", "--force", wt_str])
            .current_dir(git_root)
            .output()
            .map_err(|e| EngineError::Git(format!("invoke `git worktree remove`: {e}")))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(EngineError::Git(format!(
                "git worktree remove failed: {}",
                stderr.trim()
            )));
        }
        Ok(())
    }

    /// Squash-merge `branch` into the current branch and commit `Implement <branch>`.
    /// Returns `EngineError::MergeConflict` when the merge produces conflicts.
    pub fn merge_branch(
        &self,
        git_root: &Path,
        branch: &str,
        worktree_path: &Path,
    ) -> Result<(), EngineError> {
        let output = Command::new("git")
            .args(["merge", "--squash", branch])
            .current_dir(git_root)
            .output()
            .map_err(|e| EngineError::Git(format!("invoke `git merge --squash`: {e}")))?;
        if !output.status.success() {
            return Err(EngineError::MergeConflict {
                branch: branch.to_string(),
                worktree_path: worktree_path.to_path_buf(),
            });
        }
        let message = format!("Implement {branch}");
        let output = Command::new("git")
            .args(["commit", "-m", &message])
            .current_dir(git_root)
            .output()
            .map_err(|e| EngineError::Git(format!("invoke `git commit`: {e}")))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(EngineError::Git(format!(
                "git commit failed: {}",
                stderr.trim()
            )));
        }
        Ok(())
    }

    pub fn commit_all(&self, path: &Path, message: &str) -> Result<(), EngineError> {
        let add = Command::new("git")
            .args(["add", "-A"])
            .current_dir(path)
            .output()
            .map_err(|e| EngineError::Git(format!("invoke `git add -A`: {e}")))?;
        if !add.status.success() {
            let stderr = String::from_utf8_lossy(&add.stderr);
            return Err(EngineError::Git(format!(
                "git add -A failed: {}",
                stderr.trim()
            )));
        }
        let commit = Command::new("git")
            .args(["commit", "-m", message])
            .current_dir(path)
            .output()
            .map_err(|e| EngineError::Git(format!("invoke `git commit`: {e}")))?;
        if !commit.status.success() {
            let stderr = String::from_utf8_lossy(&commit.stderr);
            return Err(EngineError::Git(format!(
                "git commit failed: {}",
                stderr.trim()
            )));
        }
        Ok(())
    }

    pub fn delete_branch(&self, git_root: &Path, branch: &str) -> Result<(), EngineError> {
        let output = Command::new("git")
            .args(["branch", "-D", branch])
            .current_dir(git_root)
            .output()
            .map_err(|e| EngineError::Git(format!("invoke `git branch -D`: {e}")))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(EngineError::Git(format!(
                "git branch -D failed: {}",
                stderr.trim()
            )));
        }
        Ok(())
    }

    pub fn branch_exists(&self, git_root: &Path, branch: &str) -> bool {
        Command::new("git")
            .args(["rev-parse", "--verify", &format!("refs/heads/{branch}")])
            .current_dir(git_root)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    pub fn is_detached_head(&self, git_root: &Path) -> bool {
        !Command::new("git")
            .args(["symbolic-ref", "--quiet", "HEAD"])
            .current_dir(git_root)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// Return the current branch name at `git_root` (the branch HEAD points
    /// to). Returns `None` if HEAD is detached or git invocation fails.
    pub fn current_branch(&self, git_root: &Path) -> Option<String> {
        let output = Command::new("git")
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .current_dir(git_root)
            .stderr(std::process::Stdio::null())
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let name = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if name.is_empty() || name == "HEAD" {
            None
        } else {
            Some(name)
        }
    }

    // ─── Logged variants ──────────────────────────────────────────────
    //
    // These methods mirror the unlogged methods above but push every git
    // command and its output to a `UserMessageSink`. Used from the
    // `WorktreeLifecycle` command layer so the user can see exactly what
    // amux is doing.

    pub fn uncommitted_files_logged(
        &self,
        path: &Path,
        sink: &mut dyn UserMessageSink,
    ) -> Result<Vec<String>, EngineError> {
        let output = run_git_logged(&["status", "--porcelain"], path, sink)?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(EngineError::Git(format!(
                "git status failed: {}",
                stderr.trim()
            )));
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        Ok(stdout
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| l.to_string())
            .collect())
    }

    pub fn commit_all_logged(
        &self,
        path: &Path,
        message: &str,
        sink: &mut dyn UserMessageSink,
    ) -> Result<(), EngineError> {
        let add = run_git_logged(&["add", "-A"], path, sink)?;
        if !add.status.success() {
            let stderr = String::from_utf8_lossy(&add.stderr);
            return Err(EngineError::Git(format!(
                "git add -A failed: {}",
                stderr.trim()
            )));
        }
        let commit = run_git_logged(&["commit", "-m", message], path, sink)?;
        if !commit.status.success() {
            let stderr = String::from_utf8_lossy(&commit.stderr);
            return Err(EngineError::Git(format!(
                "git commit failed: {}",
                stderr.trim()
            )));
        }
        Ok(())
    }

    pub fn create_worktree_logged(
        &self,
        git_root: &Path,
        worktree_path: &Path,
        branch: &str,
        sink: &mut dyn UserMessageSink,
    ) -> Result<(), EngineError> {
        std::fs::create_dir_all(worktree_path.parent().unwrap_or(worktree_path))
            .map_err(|e| EngineError::io(worktree_path, e))?;
        let wt_str = worktree_path
            .to_str()
            .ok_or_else(|| EngineError::Git("worktree path not UTF-8".into()))?;
        let args: Vec<&str> = if self.branch_exists(git_root, branch) {
            vec!["worktree", "add", wt_str, branch]
        } else {
            vec!["worktree", "add", wt_str, "-b", branch]
        };
        let output = run_git_logged(&args, git_root, sink)?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(EngineError::Git(format!(
                "git worktree add failed: {}",
                stderr.trim()
            )));
        }
        Ok(())
    }

    pub fn remove_worktree_logged(
        &self,
        git_root: &Path,
        worktree_path: &Path,
        sink: &mut dyn UserMessageSink,
    ) -> Result<(), EngineError> {
        let wt_str = worktree_path
            .to_str()
            .ok_or_else(|| EngineError::Git("worktree path not UTF-8".into()))?;
        let output = run_git_logged(&["worktree", "remove", "--force", wt_str], git_root, sink)?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(EngineError::Git(format!(
                "git worktree remove failed: {}",
                stderr.trim()
            )));
        }
        Ok(())
    }

    pub fn merge_branch_logged(
        &self,
        git_root: &Path,
        branch: &str,
        worktree_path: &Path,
        sink: &mut dyn UserMessageSink,
    ) -> Result<(), EngineError> {
        let output = run_git_logged(&["merge", "--squash", branch], git_root, sink)?;
        if !output.status.success() {
            return Err(EngineError::MergeConflict {
                branch: branch.to_string(),
                worktree_path: worktree_path.to_path_buf(),
            });
        }
        let has_staged = {
            let check = run_git_logged(&["diff", "--cached", "--quiet"], git_root, sink)?;
            !check.status.success()
        };
        if !has_staged {
            sink.write_message(UserMessage {
                level: MessageLevel::Info,
                text: "squash merge staged no changes (branch already up to date)".to_string(),
            });
            return Ok(());
        }
        let message = format!("Implement {branch}");
        let output = run_git_logged(&["commit", "-m", &message], git_root, sink)?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(EngineError::Git(format!(
                "git commit failed: {}",
                stderr.trim()
            )));
        }
        Ok(())
    }

    pub fn delete_branch_logged(
        &self,
        git_root: &Path,
        branch: &str,
        sink: &mut dyn UserMessageSink,
    ) -> Result<(), EngineError> {
        let output = run_git_logged(&["branch", "-D", branch], git_root, sink)?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(EngineError::Git(format!(
                "git branch -D failed: {}",
                stderr.trim()
            )));
        }
        Ok(())
    }
}

impl GitRootResolver for GitEngine {
    fn resolve(&self, working_dir: &Path) -> Result<PathBuf, DataError> {
        match self.resolve_root(working_dir) {
            Ok(p) => Ok(p),
            Err(EngineError::Data(d)) => Err(d),
            Err(e) => Err(DataError::GitRootResolution {
                working_dir: working_dir.to_path_buf(),
                message: e.to_string(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_name_for_work_item_format() {
        let g = GitEngine::new();
        assert_eq!(g.branch_name_for_work_item(7), "amux/work-item-0007");
    }

    #[test]
    fn branch_name_for_workflow_format() {
        let g = GitEngine::new();
        assert_eq!(g.branch_name_for_workflow("x"), "amux/workflow-x");
    }

    fn init_repo(dir: &std::path::Path) {
        Command::new("git")
            .args(["init"])
            .current_dir(dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        Command::new("git")
            .args(["config", "user.email", "test@amux.test"])
            .current_dir(dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        Command::new("git")
            .args(["config", "user.name", "amux-test"])
            .current_dir(dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        // Create an initial commit so branch operations work.
        std::fs::write(dir.join("README.md"), "init").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "initial"])
            .current_dir(dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
    }

    #[test]
    fn resolve_root_returns_input_when_input_is_root() {
        let tmp = tempfile::tempdir().unwrap();
        init_repo(tmp.path());
        let g = GitEngine::new();
        let resolved = g.resolve_root(tmp.path()).unwrap();
        // Canonicalize both to handle any symlink differences.
        assert_eq!(
            resolved.canonicalize().unwrap(),
            tmp.path().canonicalize().unwrap()
        );
    }

    #[test]
    fn branch_exists_detects_existing_branch() {
        let tmp = tempfile::tempdir().unwrap();
        init_repo(tmp.path());
        let g = GitEngine::new();
        // "main" or "master" should exist after init.
        let initial_branch = {
            let out = Command::new("git")
                .args(["rev-parse", "--abbrev-ref", "HEAD"])
                .current_dir(tmp.path())
                .output()
                .unwrap();
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        assert!(
            g.branch_exists(tmp.path(), &initial_branch),
            "default branch must exist"
        );
        assert!(
            !g.branch_exists(tmp.path(), "branch-that-does-not-exist"),
            "nonexistent branch must not be found"
        );
    }

    #[test]
    fn is_detached_head_is_false_on_normal_checkout() {
        let tmp = tempfile::tempdir().unwrap();
        init_repo(tmp.path());
        let g = GitEngine::new();
        assert!(!g.is_detached_head(tmp.path()));
    }

    #[test]
    fn is_detached_head_is_true_in_detached_state() {
        let tmp = tempfile::tempdir().unwrap();
        init_repo(tmp.path());
        // Detach HEAD by checking out the commit hash directly.
        let out = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(tmp.path())
            .output()
            .unwrap();
        let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
        Command::new("git")
            .args(["checkout", "--detach", &sha])
            .current_dir(tmp.path())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        let g = GitEngine::new();
        assert!(g.is_detached_head(tmp.path()));
    }

    #[test]
    fn create_then_remove_worktree_is_idempotent() {
        let repo_tmp = tempfile::tempdir().unwrap();
        let wt_tmp = tempfile::tempdir().unwrap();
        init_repo(repo_tmp.path());
        let g = GitEngine::new();
        let wt_path = wt_tmp.path().join("my-worktree");
        let branch = "amux/test-wt-branch";

        g.create_worktree(repo_tmp.path(), &wt_path, branch)
            .expect("create_worktree should succeed");
        assert!(wt_path.exists(), "worktree directory must exist");

        g.remove_worktree(repo_tmp.path(), &wt_path)
            .expect("remove_worktree should succeed");
        assert!(!wt_path.exists(), "worktree directory must be gone");
    }
}
