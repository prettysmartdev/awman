//! `engine::git` — `GitEngine`. Consolidates every git operation awman performs.
//!
//! Replaces the free `pub fn`s in `oldsrc/git.rs` with a typed object whose
//! methods are the only public surface. Implements Layer 0's
//! `GitRootResolver` trait so `Session::open` can use it.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use crate::data::error::DataError;
use crate::data::message::{MessageLevel, UserMessage};
use crate::data::session::GitRootResolver;
use crate::data::worktree_paths::{
    worktree_branch_name, worktree_branch_name_for_workflow, WorktreePaths,
};
use crate::engine::error::EngineError;

pub mod frontend;

pub use frontend::{GitCommand, GitFrontend};

/// Run a git command, reporting it to the frontend and streaming its output.
fn run_git_logged(
    args: &[&str],
    cwd: &Path,
    sink: &mut dyn GitFrontend,
) -> Result<std::process::Output, EngineError> {
    let command = GitCommand::new(args);
    let cmd_str = command.display();
    // The engine reports the fact; the frontend draws it. The default impl
    // writes the same `$ git …` line this function used to compose itself
    // (F-45), so nothing changes for a frontend that has not opted in.
    sink.command_started(&command);
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

/// How a file changed relative to `HEAD`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitFileChangeType {
    Added,
    Modified,
    Deleted,
}

/// A single changed file with its per-file line counts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitFileEntry {
    pub path: String,
    pub change: GitFileChangeType,
    pub added: u32,
    pub removed: u32,
    /// `git diff --numstat` reports `-\t-\tpath` for binary files. We surface
    /// these as `+0 -0` with a `(binary)` suffix rather than dropping them.
    pub binary: bool,
}

/// The full diff snapshot returned by [`GitEngine::diff_summary`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitDiffSummary {
    pub branch: Option<String>,
    pub files: Vec<GitFileEntry>,
    pub added: u32,
    pub removed: u32,
}

/// One parsed `git diff --numstat` row. `added`/`removed` are `None` for
/// binary files (git prints `-` in those columns).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NumstatEntry {
    pub path: String,
    pub added: Option<u32>,
    pub removed: Option<u32>,
}

#[derive(Debug, Default, Clone)]
pub struct GitEngine;

/// The committer identity git resolves for a given directory. Either field is
/// `None` when git cannot resolve it at any level (repo, global, system).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GitIdentity {
    pub name: Option<String>,
    pub email: Option<String>,
}

impl GitIdentity {
    /// The `user.*` keys that are not set, in the order a message should list
    /// them. Empty when the identity is complete.
    pub fn missing_keys(&self) -> Vec<&'static str> {
        [
            self.name.is_none().then_some("user.name"),
            self.email.is_none().then_some("user.email"),
        ]
        .into_iter()
        .flatten()
        .collect()
    }

    /// Whether git has everything it needs to create a commit here.
    pub fn is_complete(&self) -> bool {
        self.name.is_some() && self.email.is_some()
    }
}
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

    /// Return the branch, changed files, and line counts for `root`.
    ///
    /// Porcelain status remains the source of truth for the file set, while
    /// numstat supplies tracked-file counts. Untracked files are counted from
    /// the working tree, and a repository without commits reports every
    /// status entry as Added with zero counts, matching the sidebar's former
    /// behavior exactly.
    pub fn diff_summary(&self, root: &Path) -> Result<GitDiffSummary, EngineError> {
        let porcelain_out = run_git_summary(&["status", "--porcelain"], root)?;
        let porcelain = parse_porcelain_status(&porcelain_out);

        // Empty output means detached HEAD; surface it as None so callers can
        // choose their own presentation fallback.
        let branch = run_git_summary(&["branch", "--show-current"], root)
            .ok()
            .map(|out| out.trim().to_string())
            .filter(|name| !name.is_empty());

        // numstat fails when there are no commits (no HEAD). Fall back to an
        // empty list + "no commits" flag so we still render the file set.
        let (numstat, has_commits) = match run_git_summary(&["diff", "--numstat", "HEAD"], root) {
            Ok(out) => (parse_numstat(&out), true),
            Err(numstat_error) => {
                if run_git_summary(&["rev-parse", "--verify", "HEAD"], root).is_ok() {
                    return Err(numstat_error);
                }
                (Vec::new(), false)
            }
        };

        // No-commits fallback: treat every file as Added with 0 line counts.
        if !has_commits {
            let porcelain: Vec<(String, GitFileChangeType)> = porcelain
                .into_iter()
                .map(|(path, _)| (path, GitFileChangeType::Added))
                .collect();
            let mut summary = build_summary(&porcelain, &[], &HashMap::new());
            summary.branch = branch;
            return Ok(summary);
        }

        // Count lines for untracked files (`??`) not covered by numstat.
        let mut untracked_lines = HashMap::new();
        for (path, change) in &porcelain {
            if *change == GitFileChangeType::Added && !numstat.iter().any(|n| &n.path == path) {
                let count = match repo_relative_file_path(root, path) {
                    Some(file_path) => count_file_lines(&file_path),
                    None => 0,
                };
                untracked_lines.insert(path.clone(), count);
            }
        }

        let mut summary = build_summary(&porcelain, &numstat, &untracked_lines);
        summary.branch = branch;
        Ok(summary)
    }

    /// `~/.awman/worktrees/<repo-name>/<NNNN>/` for a work-item.
    pub fn worktree_path(&self, git_root: &Path, work_item: u32) -> Result<PathBuf, EngineError> {
        let p = WorktreePaths::from_home().map_err(EngineError::Data)?;
        Ok(p.for_work_item(git_root, work_item))
    }

    /// `~/.awman/worktrees/<repo-name>/wf-<name>/` for a named workflow.
    pub fn worktree_path_named(&self, git_root: &Path, name: &str) -> Result<PathBuf, EngineError> {
        let p = WorktreePaths::from_home().map_err(EngineError::Data)?;
        Ok(p.for_workflow(git_root, name))
    }

    /// Branch name for a work-item (`awman/work-item-NNNN`).
    pub fn branch_name_for_work_item(&self, work_item: u32) -> String {
        worktree_branch_name(work_item)
    }

    /// Branch name for a named workflow (`awman/workflow-<name>`).
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

    /// `git clone [-b <branch>] <url> <dest>`.
    /// When `branch` is `None`, clones the repository's default branch.
    pub fn clone_repo(
        &self,
        url: &str,
        branch: Option<&str>,
        dest: &Path,
    ) -> Result<(), EngineError> {
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(|e| EngineError::io(parent, e))?;
        }
        let dest_str = dest
            .to_str()
            .ok_or_else(|| EngineError::Git("clone dest path not UTF-8".into()))?;
        let mut args: Vec<&str> = vec!["clone"];
        if let Some(b) = branch {
            args.push("-b");
            args.push(b);
        }
        args.push(url);
        args.push(dest_str);
        let output = Command::new("git")
            .args(&args)
            .output()
            .map_err(|e| EngineError::Git(format!("invoke `git clone`: {e}")))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(EngineError::Git(format!(
                "git clone failed: {}",
                stderr.trim()
            )));
        }
        Ok(())
    }

    /// The URL git would actually use for `remote` in `repo_dir`, with the
    /// user's `url.*.insteadOf` rewrites applied — `git remote get-url`.
    ///
    /// `None` when there is no such remote, or when git cannot be run at all.
    /// This is the value the `context(repo)` directory slug is derived from
    /// (WI 0114 F-29 moved that read out of Layer 0's `ContextDirResolver`),
    /// and it must stay `get-url` rather than [`remote_url`](Self::remote_url):
    /// a user with an `insteadOf` rewrite configured has a context directory
    /// named after the rewritten URL, and switching to the stored value would
    /// silently orphan it.
    pub fn remote_effective_url(&self, repo_dir: &Path, remote: &str) -> Option<String> {
        let output = Command::new("git")
            .args(["remote", "get-url", remote])
            .current_dir(repo_dir)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let url = String::from_utf8_lossy(&output.stdout).trim().to_string();
        (!url.is_empty()).then_some(url)
    }

    /// Read the URL configured for `remote` in `repo_dir`, exactly as stored
    /// in that repository's git config.
    ///
    /// Deliberately uses `git config --get remote.<name>.url` rather than
    /// `git remote get-url`: the latter applies the user's `url.*.insteadOf`
    /// rewrites, which would mask the stored value this is meant to inspect.
    pub fn remote_url(&self, repo_dir: &Path, remote: &str) -> Result<String, EngineError> {
        let key = format!("remote.{remote}.url");
        let output = Command::new("git")
            .args(["config", "--get", &key])
            .current_dir(repo_dir)
            .output()
            .map_err(|e| EngineError::Git(format!("invoke `git config --get {key}`: {e}")))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(EngineError::Git(format!(
                "git config --get {key} failed: {}",
                stderr.trim()
            )));
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    /// Fetch the latest commit from `origin` and reset the worktree to the
    /// remote's default branch. This intentionally discards local changes in
    /// a managed library clone.
    pub fn pull_latest(&self, repo_dir: &Path) -> Result<(), EngineError> {
        let fetch = Command::new("git")
            .args(["fetch", "origin"])
            .current_dir(repo_dir)
            .output()
            .map_err(|e| EngineError::Git(format!("invoke `git fetch origin`: {e}")))?;
        if !fetch.status.success() {
            let stderr = String::from_utf8_lossy(&fetch.stderr);
            return Err(EngineError::Git(format!(
                "git fetch origin failed: {}",
                stderr.trim()
            )));
        }

        let reset = Command::new("git")
            .args(["reset", "--hard", "origin/HEAD"])
            .current_dir(repo_dir)
            .output()
            .map_err(|e| EngineError::Git(format!("invoke `git reset --hard origin/HEAD`: {e}")))?;
        if !reset.status.success() {
            let stderr = String::from_utf8_lossy(&reset.stderr);
            return Err(EngineError::Git(format!(
                "git reset --hard origin/HEAD failed: {}",
                stderr.trim()
            )));
        }
        Ok(())
    }

    /// Check out `branch` if it already exists locally or on a remote;
    /// otherwise create it from HEAD. Returns the disposition:
    /// `"checked-out"` for an existing branch (local or remote-tracking) or
    /// `"created"` for a new one. Errors propagate as `EngineError::Git`.
    pub fn checkout_or_create_branch(
        &self,
        path: &Path,
        branch: &str,
    ) -> Result<&'static str, EngineError> {
        // Plain `git checkout <branch>` succeeds when (a) the local branch
        // exists or (b) exactly one remote has the branch (git auto-creates
        // a tracking branch). Try this first so we don't need a separate
        // remote-branch probe.
        let output = Command::new("git")
            .args(["checkout", branch])
            .current_dir(path)
            .output()
            .map_err(|e| EngineError::Git(format!("invoke `git checkout`: {e}")))?;
        if output.status.success() {
            return Ok("checked-out");
        }

        // Fall back: branch exists neither locally nor on any remote — create
        // it from the current HEAD.
        let output = Command::new("git")
            .args(["checkout", "-b", branch])
            .current_dir(path)
            .output()
            .map_err(|e| EngineError::Git(format!("invoke `git checkout -b`: {e}")))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(EngineError::Git(format!(
                "git checkout -b {branch} failed: {}",
                stderr.trim()
            )));
        }
        Ok("created")
    }

    /// Recursively delete a directory, ignoring missing paths. Used to clean up
    /// a cloned repo when remote-session setup fails.
    pub fn delete_directory(&self, path: &Path) -> Result<(), EngineError> {
        match std::fs::remove_dir_all(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(EngineError::io(path, e)),
        }
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

    /// The `user.name` / `user.email` git resolves **for `path`**.
    ///
    /// `git -C <path> config user.name` applies the normal precedence —
    /// repo-local `.git/config` over `~/.gitconfig` over system — so a repo
    /// that configures its own identity is honoured. A probe run without
    /// `current_dir` (as `exec workflow` did before F-39) only ever saw the
    /// global value and warned about a repo that was in fact configured.
    ///
    /// A field git cannot resolve comes back as `None` rather than an error:
    /// an unset identity is a normal state the caller reports on, not a
    /// failure to interrogate git.
    pub fn identity_configured(&self, path: &Path) -> Result<GitIdentity, EngineError> {
        Ok(GitIdentity {
            name: self.config_value(path, "user.name")?,
            email: self.config_value(path, "user.email")?,
        })
    }

    /// One `git -C <path> config <key>`, `None` when unset or empty.
    fn config_value(&self, path: &Path, key: &str) -> Result<Option<String>, EngineError> {
        let output = Command::new("git")
            .args(["config", key])
            .current_dir(path)
            .stderr(std::process::Stdio::null())
            .output()
            .map_err(|e| EngineError::Git(format!("invoke `git config {key}`: {e}")))?;
        if !output.status.success() {
            // Exit 1 is git's "key not set", which is not an error here.
            return Ok(None);
        }
        let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Ok(if value.is_empty() { None } else { Some(value) })
    }
    /// The commit SHA HEAD points at in `git_root`.
    pub fn head_sha(&self, git_root: &Path) -> Result<String, EngineError> {
        let output = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(git_root)
            .output()
            .map_err(|e| EngineError::Git(format!("invoke `git rev-parse HEAD`: {e}")))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(EngineError::Git(format!(
                "git rev-parse HEAD failed: {}",
                stderr.trim()
            )));
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    // ─── Logged variants ──────────────────────────────────────────────
    //
    // These methods mirror the unlogged methods above but push every git
    // command and its output to a `UserMessageSink`. Used from the
    // `WorktreeLifecycle` command layer so the user can see exactly what
    // awman is doing.

    pub fn uncommitted_files_logged(
        &self,
        path: &Path,
        sink: &mut dyn GitFrontend,
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
        sink: &mut dyn GitFrontend,
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
        sink: &mut dyn GitFrontend,
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
        sink: &mut dyn GitFrontend,
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

    /// Merge `branch` into the current branch. With `squash: true` this stages
    /// the changes via `git merge --squash` and commits `Implement <branch>`;
    /// with `squash: false` it runs a plain `git merge`, preserving the
    /// branch's individual commits (fast-forwarding when possible).
    /// Returns `EngineError::MergeConflict` when the merge fails.
    pub fn merge_branch_logged(
        &self,
        git_root: &Path,
        branch: &str,
        worktree_path: &Path,
        squash: bool,
        sink: &mut dyn GitFrontend,
    ) -> Result<(), EngineError> {
        if !squash {
            let message = format!("Merge {branch}");
            let output = run_git_logged(&["merge", "-m", &message, branch], git_root, sink)?;
            if !output.status.success() {
                return Err(EngineError::MergeConflict {
                    branch: branch.to_string(),
                    worktree_path: worktree_path.to_path_buf(),
                });
            }
            return Ok(());
        }
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
        sink: &mut dyn GitFrontend,
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

    /// Logged variant of [`clone_repo`]. Streams the command line and combined
    /// stdout/stderr through `sink`. Used by the API server's session-setup
    /// path so a remote-clone failure is captured in the server log file.
    pub fn clone_repo_logged(
        &self,
        url: &str,
        branch: Option<&str>,
        dest: &Path,
        sink: &mut dyn GitFrontend,
    ) -> Result<(), EngineError> {
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(|e| EngineError::io(parent, e))?;
        }
        let dest_str = dest
            .to_str()
            .ok_or_else(|| EngineError::Git("clone dest path not UTF-8".into()))?;
        let mut args: Vec<&str> = vec!["clone"];
        if let Some(b) = branch {
            args.push("-b");
            args.push(b);
        }
        args.push(url);
        args.push(dest_str);
        // `git clone` doesn't care about cwd (dest is absolute); pick a path
        // that's guaranteed to exist so `Command::current_dir` doesn't fail.
        let cwd = dest
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(std::env::temp_dir);
        let output = run_git_logged(&args, &cwd, sink)?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(EngineError::Git(format!(
                "git clone failed: {}",
                stderr.trim()
            )));
        }
        Ok(())
    }

    /// Logged variant of [`pull_latest`]. Streams each command and its
    /// combined stdout/stderr through `sink`.
    pub fn pull_latest_logged(
        &self,
        repo_dir: &Path,
        sink: &mut dyn GitFrontend,
    ) -> Result<(), EngineError> {
        let fetch = run_git_logged(&["fetch", "origin"], repo_dir, sink)?;
        if !fetch.status.success() {
            let stderr = String::from_utf8_lossy(&fetch.stderr);
            return Err(EngineError::Git(format!(
                "git fetch origin failed: {}",
                stderr.trim()
            )));
        }

        let reset = run_git_logged(&["reset", "--hard", "origin/HEAD"], repo_dir, sink)?;
        if !reset.status.success() {
            let stderr = String::from_utf8_lossy(&reset.stderr);
            return Err(EngineError::Git(format!(
                "git reset --hard origin/HEAD failed: {}",
                stderr.trim()
            )));
        }
        Ok(())
    }

    /// Logged variant of [`checkout_or_create_branch`]. Forwards every git
    /// invocation and its output to `sink`. The first `git checkout <branch>`
    /// failure is expected (it's how we detect "branch doesn't exist yet") so
    /// its noise is downgraded by [`run_git_logged`] to a warning rather than
    /// surfacing as an error.
    pub fn checkout_or_create_branch_logged(
        &self,
        path: &Path,
        branch: &str,
        sink: &mut dyn GitFrontend,
    ) -> Result<&'static str, EngineError> {
        let output = run_git_logged(&["checkout", branch], path, sink)?;
        if output.status.success() {
            return Ok("checked-out");
        }
        let output = run_git_logged(&["checkout", "-b", branch], path, sink)?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(EngineError::Git(format!(
                "git checkout -b {branch} failed: {}",
                stderr.trim()
            )));
        }
        Ok("created")
    }
}

/// Parse `git status --porcelain` output into `(path, change)` pairs.
pub fn parse_porcelain_status(stdout: &str) -> Vec<(String, GitFileChangeType)> {
    let mut out = Vec::new();
    for line in stdout.lines() {
        // Porcelain v1 lines are `XY<space>PATH`: two status columns, a
        // separator space, then the path. Anything shorter is malformed.
        if line.len() < 4 {
            continue;
        }
        let code = &line[..2];
        let rest = &line[3..];
        let path = rename_target(rest);
        let change = if code == "??" {
            GitFileChangeType::Added
        } else if code.contains('D') {
            GitFileChangeType::Deleted
        } else {
            GitFileChangeType::Modified
        };
        out.push((path, change));
    }
    out
}

/// Resolve a porcelain rename entry (`old -> new`) to its destination path.
fn rename_target(rest: &str) -> String {
    match rest.rfind(" -> ") {
        Some(idx) => rest[idx + 4..].to_string(),
        None => rest.to_string(),
    }
}

/// Parse `git diff --numstat HEAD` output into per-file entries.
pub fn parse_numstat(stdout: &str) -> Vec<NumstatEntry> {
    let mut out = Vec::new();
    for line in stdout.lines() {
        let mut parts = line.splitn(3, '\t');
        let (Some(a), Some(d), Some(p)) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        let added = if a == "-" { None } else { a.parse().ok() };
        let removed = if d == "-" { None } else { d.parse().ok() };
        out.push(NumstatEntry {
            path: resolve_numstat_path(p),
            added,
            removed,
        });
    }
    out
}

/// Resolve a numstat rename path. Handles the two git forms:
/// `prefix{old => new}suffix` and the bare `old => new`.
fn resolve_numstat_path(raw: &str) -> String {
    if let (Some(open), Some(close)) = (raw.find('{'), raw.find('}')) {
        if open < close {
            let prefix = &raw[..open];
            let inner = &raw[open + 1..close];
            let suffix = &raw[close + 1..];
            let new_part = inner
                .split("=>")
                .nth(1)
                .map(str::trim)
                .unwrap_or_else(|| inner.trim());
            return format!("{prefix}{new_part}{suffix}");
        }
    }
    if raw.contains("=>") {
        if let Some(new_part) = raw.split("=>").nth(1) {
            return new_part.trim().to_string();
        }
    }
    raw.to_string()
}

/// Combine porcelain change-types, numstat line counts, and pre-counted
/// untracked-file line totals into a [`GitDiffSummary`].
pub fn build_summary(
    porcelain: &[(String, GitFileChangeType)],
    numstat: &[NumstatEntry],
    untracked_lines: &HashMap<String, u32>,
) -> GitDiffSummary {
    let mut files = Vec::new();
    let mut added = 0u32;
    let mut removed = 0u32;

    for (path, change) in porcelain {
        let (file_added, file_removed, binary) =
            if let Some(entry) = numstat.iter().find(|n| &n.path == path) {
                match (entry.added, entry.removed) {
                    (Some(a), Some(r)) => (a, r, false),
                    // A `-` in either column means git treated it as binary.
                    _ => (0, 0, true),
                }
            } else if let Some(&count) = untracked_lines.get(path) {
                (count, 0, false)
            } else {
                (0, 0, false)
            };

        added = added.saturating_add(file_added);
        removed = removed.saturating_add(file_removed);
        files.push(GitFileEntry {
            path: path.clone(),
            change: *change,
            added: file_added,
            removed: file_removed,
            binary,
        });
    }

    GitDiffSummary {
        branch: None,
        files,
        added,
        removed,
    }
}

/// Run a git command with explicit args in `root`.
fn run_git_summary(args: &[&str], root: &Path) -> Result<String, EngineError> {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .map_err(|e| EngineError::Git(format!("invoke `git {}`: {e}", args.join(" "))))?;
    if !output.status.success() {
        return Err(EngineError::Git(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn repo_relative_file_path(root: &Path, rel: &str) -> Option<PathBuf> {
    let rel = Path::new(rel);
    if rel.is_absolute() {
        return None;
    }
    if rel.components().any(|c| {
        matches!(
            c,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        return None;
    }
    Some(root.join(rel))
}

/// Count the lines in an untracked file. Missing/unreadable files count as 0.
fn count_file_lines(path: &Path) -> u32 {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_file() => {}
        _ => return 0,
    }
    match std::fs::read(path) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).lines().count() as u32,
        Err(_) => 0,
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

/// Resolve the main `.git` directory backing a worktree checkout.
///
/// A worktree's `.git` entry is a *file* containing `gitdir: <path>` where
/// `<path>` points to `.git/worktrees/<name>/` in the main repository.
/// This function reads that pointer and returns the main `.git/` directory
/// (two levels up from the worktree entry).
///
/// Returns `Ok(None)` when `worktree_path/.git` is a directory (regular
/// repo) or does not exist.
pub fn resolve_worktree_git_dir(worktree_path: &Path) -> Result<Option<PathBuf>, EngineError> {
    let dot_git = worktree_path.join(".git");
    if !dot_git.exists() || dot_git.is_dir() {
        return Ok(None);
    }
    let content = std::fs::read_to_string(&dot_git).map_err(|e| EngineError::io(&dot_git, e))?;
    let gitdir_line = content.trim().strip_prefix("gitdir: ").ok_or_else(|| {
        EngineError::Git(format!(
            "unexpected .git file format at {}: {}",
            dot_git.display(),
            content.trim(),
        ))
    })?;
    let gitdir = if Path::new(gitdir_line).is_absolute() {
        PathBuf::from(gitdir_line)
    } else {
        worktree_path.join(gitdir_line)
    };
    // gitdir → .git/worktrees/<name>  →  parent .git/worktrees/  →  parent .git/
    let main_git_dir = gitdir.parent().and_then(|p| p.parent()).ok_or_else(|| {
        EngineError::Git(format!(
            "cannot derive main .git dir from worktree gitdir: {}",
            gitdir.display(),
        ))
    })?;
    Ok(Some(main_git_dir.to_path_buf()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_name_for_work_item_format() {
        let g = GitEngine::new();
        assert_eq!(g.branch_name_for_work_item(7), "awman/work-item-0007");
    }

    #[test]
    fn branch_name_for_workflow_format() {
        let g = GitEngine::new();
        assert_eq!(g.branch_name_for_workflow("x"), "awman/workflow-x");
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
            .args(["config", "user.email", "test@awman.test"])
            .current_dir(dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        Command::new("git")
            .args(["config", "user.name", "awman-test"])
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
        let branch = "awman/test-wt-branch";

        g.create_worktree(repo_tmp.path(), &wt_path, branch)
            .expect("create_worktree should succeed");
        assert!(wt_path.exists(), "worktree directory must exist");

        g.remove_worktree(repo_tmp.path(), &wt_path)
            .expect("remove_worktree should succeed");
        assert!(!wt_path.exists(), "worktree directory must be gone");
    }

    #[test]
    fn resolve_worktree_git_dir_returns_none_for_regular_repo() {
        let tmp = tempfile::tempdir().unwrap();
        init_repo(tmp.path());
        let result = resolve_worktree_git_dir(tmp.path()).unwrap();
        assert!(result.is_none(), "regular repo should return None");
    }

    #[test]
    fn resolve_worktree_git_dir_returns_none_for_non_git_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let result = resolve_worktree_git_dir(tmp.path()).unwrap();
        assert!(result.is_none(), "non-git dir should return None");
    }

    #[test]
    fn resolve_worktree_git_dir_finds_main_git_dir() {
        let repo_tmp = tempfile::tempdir().unwrap();
        let wt_tmp = tempfile::tempdir().unwrap();
        init_repo(repo_tmp.path());
        let g = GitEngine::new();
        let wt_path = wt_tmp.path().join("my-worktree");
        g.create_worktree(repo_tmp.path(), &wt_path, "awman/test-resolve")
            .expect("create_worktree should succeed");

        let result = resolve_worktree_git_dir(&wt_path)
            .expect("should not error")
            .expect("worktree should resolve to Some");

        let expected = repo_tmp.path().join(".git").canonicalize().unwrap();
        let actual = result.canonicalize().unwrap();
        assert_eq!(actual, expected, "should resolve to main repo .git dir");

        g.remove_worktree(repo_tmp.path(), &wt_path).unwrap();
    }

    // ─── pull_latest ──────────────────────────────────────────────────────────

    fn run_git(dir: &std::path::Path, args: &[&str]) {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap_or_else(|e| panic!("failed to invoke git {args:?}: {e}"));
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// Create a local upstream: a `source` working repo pushed to a `bare`
    /// repo. The `bare` repo's `file://` URL is a network-free stand-in for a
    /// GitHub remote. Returns `(source, bare)`; keep both alive for the test.
    fn setup_upstream() -> (tempfile::TempDir, tempfile::TempDir) {
        let source = tempfile::tempdir().unwrap();
        init_repo(source.path());
        let bare = tempfile::tempdir().unwrap();
        run_git(bare.path(), &["init", "--bare"]);
        // Deterministic default branch regardless of the host git version.
        run_git(source.path(), &["branch", "-M", "main"]);
        let bare_url = format!("file://{}", bare.path().display());
        run_git(source.path(), &["remote", "add", "origin", &bare_url]);
        run_git(source.path(), &["push", "-u", "origin", "main"]);
        // Point the bare repo's HEAD at main so `origin/HEAD` resolves on clone.
        run_git(bare.path(), &["symbolic-ref", "HEAD", "refs/heads/main"]);
        (source, bare)
    }

    fn bare_url(bare: &tempfile::TempDir) -> String {
        format!("file://{}", bare.path().display())
    }

    #[test]
    fn pull_latest_picks_up_new_upstream_commit() {
        let (source, bare) = setup_upstream();
        let g = GitEngine::new();
        let work = tempfile::tempdir().unwrap();
        let clone_dir = work.path().join("clone");
        g.clone_repo(&bare_url(&bare), None, &clone_dir).unwrap();
        assert!(
            !clone_dir.join("NEW.md").exists(),
            "new file must not exist before the upstream commit is pulled"
        );

        // Add a commit upstream and push it.
        std::fs::write(source.path().join("NEW.md"), "new upstream content").unwrap();
        run_git(source.path(), &["add", "."]);
        run_git(source.path(), &["commit", "-m", "add NEW.md"]);
        run_git(source.path(), &["push", "origin", "main"]);

        g.pull_latest(&clone_dir)
            .expect("pull_latest must succeed against a reachable remote");
        assert!(
            clone_dir.join("NEW.md").exists(),
            "pull_latest must bring the new upstream commit into the working tree"
        );
    }

    #[test]
    fn pull_latest_hard_resets_dirty_working_tree() {
        let (_source, bare) = setup_upstream();
        let g = GitEngine::new();
        let work = tempfile::tempdir().unwrap();
        let clone_dir = work.path().join("clone");
        g.clone_repo(&bare_url(&bare), None, &clone_dir).unwrap();

        // `init_repo` committed README.md == "init". Dirty it locally.
        std::fs::write(clone_dir.join("README.md"), "LOCAL UNCOMMITTED EDIT").unwrap();

        g.pull_latest(&clone_dir)
            .expect("pull_latest must succeed and discard local changes");
        assert_eq!(
            std::fs::read_to_string(clone_dir.join("README.md")).unwrap(),
            "init",
            "a dirty working tree must be hard-reset back to the upstream content"
        );
    }

    #[test]
    fn pull_latest_errors_on_nonexistent_dir() {
        let g = GitEngine::new();
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("does-not-exist");
        let err = g
            .pull_latest(&missing)
            .expect_err("pull_latest on a non-existent dir must error");
        assert!(
            matches!(err, EngineError::Git(_)),
            "must be EngineError::Git; got {err:?}"
        );
    }

    #[test]
    fn pull_latest_errors_on_non_git_dir() {
        let g = GitEngine::new();
        let tmp = tempfile::tempdir().unwrap();
        // A real directory that is not a git repository.
        let err = g
            .pull_latest(tmp.path())
            .expect_err("pull_latest on a non-git dir must error");
        assert!(
            matches!(err, EngineError::Git(_)),
            "must be EngineError::Git; got {err:?}"
        );
    }

    #[test]
    fn porcelain_parser_preserves_sidebar_mappings() {
        assert_eq!(
            parse_porcelain_status(
                "?? newfile.rs\nD  deleted.rs\n D gone.rs\nM  staged.rs\n M changed.rs\nR  old.rs -> new.rs\nM\n"
            ),
            vec![
                ("newfile.rs".into(), GitFileChangeType::Added),
                ("deleted.rs".into(), GitFileChangeType::Deleted),
                ("gone.rs".into(), GitFileChangeType::Deleted),
                ("staged.rs".into(), GitFileChangeType::Modified),
                ("changed.rs".into(), GitFileChangeType::Modified),
                ("new.rs".into(), GitFileChangeType::Modified),
            ]
        );
    }

    #[test]
    fn numstat_parser_handles_counts_binary_and_renames() {
        assert_eq!(
            parse_numstat(
                "5\t2\tsrc/foo.rs\n-\t-\timg.png\n3\t1\t{old.rs => new.rs}\n1\t0\tsrc/{old => new}/file.rs\n2\t2\told.rs => other.rs\ngarbage\n"
            ),
            vec![
                NumstatEntry {
                    path: "src/foo.rs".into(),
                    added: Some(5),
                    removed: Some(2)
                },
                NumstatEntry {
                    path: "img.png".into(),
                    added: None,
                    removed: None
                },
                NumstatEntry {
                    path: "new.rs".into(),
                    added: Some(3),
                    removed: Some(1)
                },
                NumstatEntry {
                    path: "src/new/file.rs".into(),
                    added: Some(1),
                    removed: Some(0)
                },
                NumstatEntry {
                    path: "other.rs".into(),
                    added: Some(2),
                    removed: Some(2)
                },
            ]
        );
    }

    #[test]
    fn build_summary_combines_tracked_binary_and_untracked_counts() {
        let porcelain = vec![
            ("src/foo.rs".into(), GitFileChangeType::Modified),
            ("img.png".into(), GitFileChangeType::Added),
            ("new.txt".into(), GitFileChangeType::Added),
        ];
        let numstat = vec![
            NumstatEntry {
                path: "src/foo.rs".into(),
                added: Some(5),
                removed: Some(2),
            },
            NumstatEntry {
                path: "img.png".into(),
                added: None,
                removed: None,
            },
        ];
        let mut untracked = HashMap::new();
        untracked.insert("new.txt".into(), 10);
        let summary = build_summary(&porcelain, &numstat, &untracked);

        assert_eq!(summary.added, 15);
        assert_eq!(summary.removed, 2);
        assert_eq!(summary.files[0].change, GitFileChangeType::Modified);
        assert_eq!((summary.files[0].added, summary.files[0].removed), (5, 2));
        assert!(summary.files[1].binary);
        assert_eq!((summary.files[2].added, summary.files[2].removed), (10, 0));
    }

    #[test]
    fn diff_summary_counts_untracked_files() {
        let tmp = tempfile::tempdir().unwrap();
        init_repo(tmp.path());
        std::fs::write(tmp.path().join("new.txt"), "one\ntwo\n").unwrap();

        let summary = GitEngine::new().diff_summary(tmp.path()).unwrap();
        let new_file = summary
            .files
            .iter()
            .find(|file| file.path == "new.txt")
            .expect("untracked file should be included");
        assert_eq!(new_file.change, GitFileChangeType::Added);
        assert_eq!(new_file.added, 2);
        assert_eq!(new_file.removed, 0);
        assert_eq!(summary.added, 2);
    }

    #[test]
    fn diff_summary_empty_repo_marks_everything_added_with_zero_counts() {
        let tmp = tempfile::tempdir().unwrap();
        Command::new("git")
            .args(["init"])
            .current_dir(tmp.path())
            .output()
            .unwrap();
        std::fs::write(tmp.path().join("new.txt"), "one\ntwo\n").unwrap();

        let summary = GitEngine::new().diff_summary(tmp.path()).unwrap();
        assert_eq!(summary.files.len(), 1);
        assert_eq!(summary.files[0].path, "new.txt");
        assert_eq!(summary.files[0].change, GitFileChangeType::Added);
        assert_eq!((summary.added, summary.removed), (0, 0));
        assert_eq!((summary.files[0].added, summary.files[0].removed), (0, 0));
    }

    #[test]
    fn untracked_path_validation_rejects_escape_paths() {
        let root = Path::new("/repo");
        assert!(repo_relative_file_path(root, "/tmp/outside").is_none());
        assert!(repo_relative_file_path(root, "../outside").is_none());
        assert!(repo_relative_file_path(root, "src/../../outside").is_none());
        assert_eq!(
            repo_relative_file_path(root, "src/main.rs").unwrap(),
            PathBuf::from("/repo/src/main.rs")
        );
    }

    #[cfg(unix)]
    #[test]
    fn untracked_line_count_does_not_follow_symlinks() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(outside.path(), "one\ntwo\n").unwrap();
        let link = tmp.path().join("outside-link");
        symlink(outside.path(), &link).unwrap();
        assert_eq!(count_file_lines(&link), 0);
    }
}
