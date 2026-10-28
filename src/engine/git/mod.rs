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
use crate::engine::error::{EngineError, MergeFailedDetail};

pub mod frontend;
mod host_command;
pub mod remote_push;

pub use frontend::{GitCommand, GitFrontend};

fn path_from_git_bytes(bytes: &[u8]) -> PathBuf {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        PathBuf::from(std::ffi::OsStr::from_bytes(bytes))
    }
    #[cfg(not(unix))]
    {
        PathBuf::from(String::from_utf8_lossy(bytes).as_ref())
    }
}

/// Run a git command, reporting it to the frontend and streaming its output.
fn run_git_logged(
    args: &[&str],
    cwd: &Path,
    sink: &mut dyn GitFrontend,
) -> Result<std::process::Output, EngineError> {
    run_git_logged_env(args, cwd, &[], sink)
}

/// [`run_git_logged`] with extra environment variables for the child.
fn run_git_logged_env(
    args: &[&str],
    cwd: &Path,
    env: &[(&str, &str)],
    sink: &mut dyn GitFrontend,
) -> Result<std::process::Output, EngineError> {
    let command = GitCommand::new(args);
    let cmd_str = command.display();
    // The engine reports the fact; the frontend draws it. The default impl
    // writes the same `$ git …` line this function used to compose itself
    // (F-45), so nothing changes for a frontend that has not opted in.
    sink.command_started(&command);
    let output = host_command::command(cwd)
        .args(args)
        .envs(env.iter().copied())
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

/// Host-side committer identity awman uses in a clone it owns (WI 0133).
pub const FALLBACK_COMMIT_NAME: &str = "awman";
pub const FALLBACK_COMMIT_EMAIL: &str = "awman@localhost";

/// Pathspecs that keep awman's own bookkeeping out of a leftover-changes commit:
/// the engine's `WorkflowState` JSON and the saved dynamic workflow copy, both of
/// which live under `<root>/.awman/workflows/` and are never gitignored.
pub const AWMAN_BOOKKEEPING_EXCLUDES: &[&str] = &[
    ":(exclude).awman/workflows/*.json",
    ":(exclude).awman/workflows/dynamic-*.toml",
];

/// The identity a host-side commit in a clone will be recorded with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitIdentity {
    pub name: String,
    pub email: String,
    /// `true` when at least one of the two came from awman's fallback rather than
    /// git's own config resolution for the clone.
    pub fallback: bool,
    /// Extra `git` arguments to put *before* the subcommand: `-c user.name=…` and/or
    /// `-c user.email=…` for exactly the keys the clone could not resolve. Empty when
    /// both resolve.
    pub overrides: Vec<String>,
}

impl CommitIdentity {
    /// One line for the setup log / status stream.
    pub fn describe(&self) -> String {
        if self.fallback {
            format!(
                "committing as {} <{}> (no git identity in the clone)",
                self.name, self.email
            )
        } else {
            format!("committing as {} <{}> (git config)", self.name, self.email)
        }
    }
}

/// Result of [`GitEngine::merge_ref_logged`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeRefOutcome {
    Merged { sha: String },
    AlreadyUpToDate,
}

/// Result of [`GitEngine::commit_all_with_identity_logged`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommitAllOutcome {
    Committed { sha: String },
    NothingToCommit,
}

/// The identity overrides followed by `rest`: the full argument list after `git`.
fn with_overrides<'a>(identity: &'a CommitIdentity, rest: &[&'a str]) -> Vec<&'a str> {
    let mut args: Vec<&str> = identity.overrides.iter().map(String::as_str).collect();
    args.extend_from_slice(rest);
    args
}

/// The commit message a squash merge of `branch` is recorded with.
pub fn squash_commit_message(branch: &str) -> String {
    format!("Implement {branch}")
}

/// `EngineError::MergeFailed` for a merge command (`args`, after `git`) that
/// exited non-zero with `output`.
fn merge_failed(
    branch: &str,
    worktree_path: &Path,
    git_root: &Path,
    args: &[&str],
    output: &std::process::Output,
) -> EngineError {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = [stdout.trim(), stderr.trim()]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    EngineError::MergeFailed(Box::new(MergeFailedDetail {
        branch: branch.to_string(),
        worktree_path: worktree_path.to_path_buf(),
        git_root: git_root.to_path_buf(),
        // Quoted where needed so it can be pasted into a shell: the plain
        // merge's `-m "Merge <branch>"` contains a space.
        command: std::iter::once("git".to_string())
            .chain(args.iter().map(|a| {
                if a.contains(char::is_whitespace) {
                    format!("\"{a}\"")
                } else {
                    a.to_string()
                }
            }))
            .collect::<Vec<_>>()
            .join(" "),
        // A signal-terminated git has no exit code; report it as a failure.
        exit_code: output.status.code().unwrap_or(1),
        output: combined,
    }))
}

/// How a sparse clone ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SparseCloneOutcome {
    /// Shallow, blob-less clone with only the requested subdirectory checked out.
    Sparse,
    /// git is older than 2.25; a plain `--depth 1` clone was made instead.
    FellBackToShallow,
}

/// Warning shown when a sparse clone falls back to a plain shallow clone.
pub const SPARSE_FALLBACK_WARNING: &str =
    "this git does not support sparse checkout (git 2.25 or newer is required); fell back to a plain shallow clone, which downloads more data";

/// Whether a clone failure's stderr indicates git lacks `--sparse` / `sparse-checkout`.
fn is_sparse_unsupported(stderr: &str) -> bool {
    let lower = stderr.to_lowercase();
    lower.contains("sparse-checkout")
        || lower.contains("unknown option")
        || lower.contains("--sparse")
        || lower.contains("not a git command")
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

/// Runs one git argv in a working directory (the sparse-clone seam).
type GitRunner<'a> = dyn FnMut(&[&str], &Path) -> Result<std::process::Output, EngineError> + 'a;

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
        let output = host_command::discovery_command(working_dir)
            .args(["rev-parse", "--show-toplevel"])
            .current_dir(working_dir)
            .output()
            .map_err(|e| EngineError::Git(format!("invoke `git rev-parse`: {e}")))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if stderr.starts_with("fatal: not a git repository") {
                return Err(EngineError::Data(DataError::GitRootNotFound {
                    working_dir: working_dir.to_path_buf(),
                }));
            }
            return Err(EngineError::Git(format!(
                "git root discovery failed: {}",
                stderr.trim()
            )));
        }
        let s = String::from_utf8_lossy(&output.stdout);
        Ok(PathBuf::from(s.trim()))
    }

    /// Returns whether the worktree at `path` has zero uncommitted changes.
    pub fn is_clean(&self, path: &Path) -> Result<bool, EngineError> {
        Ok(self.uncommitted_files(path)?.is_empty())
    }

    /// Fingerprint HEAD and file bytes, including dirty/untracked paths. Status
    /// letters alone miss rewrites of already-dirty files and clean commits.
    pub fn checkout_fingerprint(&self, root: &Path) -> Result<Vec<u8>, EngineError> {
        use sha2::{Digest, Sha256};
        fn hash_path(hash: &mut Sha256, path: &Path) -> Result<(), EngineError> {
            let io = |e: std::io::Error| EngineError::Git(e.to_string());
            match std::fs::symlink_metadata(path) {
                Ok(meta) if meta.file_type().is_symlink() => {
                    hash.update(b"symlink");
                    hash.update(
                        std::fs::read_link(path)
                            .map_err(io)?
                            .as_os_str()
                            .as_encoded_bytes(),
                    );
                }
                Ok(meta) if meta.is_file() => {
                    hash.update(b"file");
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        hash.update((meta.permissions().mode() & 0o111).to_le_bytes());
                    }
                    hash.update(meta.len().to_le_bytes());
                    hash.update(std::fs::read(path).map_err(io)?);
                }
                Ok(meta) if meta.is_dir() => {
                    // A tracked directory is a submodule. Its dirty contents
                    // must remain guarded, including an already-dirty submodule.
                    hash.update(b"directory");
                    let mut entries = std::fs::read_dir(path)
                        .map_err(io)?
                        .collect::<Result<Vec<_>, _>>()
                        .map_err(io)?;
                    entries.sort_by_key(|entry| entry.file_name());
                    for entry in entries {
                        if entry.file_name() != ".git" {
                            hash.update(entry.file_name().as_encoded_bytes());
                            hash_path(hash, &entry.path())?;
                        }
                    }
                }
                Ok(_) => {
                    return Err(EngineError::Git(
                        "unsupported file in leader mutation guard".into(),
                    ))
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => hash.update(b"missing"),
                Err(e) => return Err(io(e)),
            }
            Ok(())
        }
        let output = host_command::command(root)
            .args([
                "-c",
                "core.fsmonitor=false",
                "ls-files",
                "-z",
                "--cached",
                "--others",
                "--exclude-standard",
            ])
            .current_dir(root)
            .output()
            .map_err(|e| EngineError::Git(e.to_string()))?;
        if !output.status.success() {
            return Err(EngineError::Git(
                String::from_utf8_lossy(&output.stderr).into(),
            ));
        }
        let mut hash = Sha256::new();
        hash.update(self.head_sha(root)?);
        let mut paths: Vec<&[u8]> = output
            .stdout
            .split(|b| *b == 0)
            .filter(|p| !p.is_empty())
            .collect();
        paths.sort_unstable();
        paths.dedup();
        for path in paths {
            let name = std::str::from_utf8(path).map_err(|e| EngineError::Git(e.to_string()))?;
            hash.update(path.len().to_le_bytes());
            hash.update(path);
            hash_path(&mut hash, &root.join(name))?;
        }
        Ok(hash.finalize().to_vec())
    }

    /// `git status --porcelain` lines (one per uncommitted file).
    pub fn uncommitted_files(&self, path: &Path) -> Result<Vec<String>, EngineError> {
        let output = host_command::command(path)
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

    /// `~/.awman/worktrees/<dirname>-<hash8>/<NNNN>/` for a work-item, or the
    /// legacy unhashed path when it holds a worktree of this repository
    /// (see [`Self::pick_worktree_path`]).
    pub fn worktree_path(&self, git_root: &Path, work_item: u32) -> Result<PathBuf, EngineError> {
        let p = WorktreePaths::from_home().map_err(EngineError::Data)?;
        let root = canonical_or_given(git_root);
        Ok(self.pick_worktree_path(
            &root,
            p.for_work_item(&root, work_item),
            p.legacy_for_work_item(&root, work_item),
        ))
    }

    /// `~/.awman/worktrees/<dirname>-<hash8>/wf-<name>/` for a named workflow,
    /// or the legacy unhashed path when it holds a worktree of this repository
    /// (see [`Self::pick_worktree_path`]).
    pub fn worktree_path_named(&self, git_root: &Path, name: &str) -> Result<PathBuf, EngineError> {
        let p = WorktreePaths::from_home().map_err(EngineError::Data)?;
        let root = canonical_or_given(git_root);
        Ok(self.pick_worktree_path(
            &root,
            p.for_workflow(&root, name),
            p.legacy_for_workflow(&root, name),
        ))
    }

    /// The hashed path, unless it does not exist while the legacy path does
    /// and `git worktree list` in `git_root` names the legacy path — then the
    /// legacy path, so a worktree made before the hashed key is still found.
    /// A legacy directory owned by another checkout with the same dirname is
    /// ignored. A `list_worktrees` failure counts as "not listed".
    fn pick_worktree_path(&self, git_root: &Path, hashed: PathBuf, legacy: PathBuf) -> PathBuf {
        if hashed.exists() || !legacy.exists() {
            return hashed;
        }
        let legacy_canonical = canonical_or_given(&legacy);
        let listed = self
            .list_worktrees(git_root)
            .map(|wts| {
                wts.iter()
                    .any(|wt| canonical_or_given(wt) == legacy_canonical)
            })
            .unwrap_or(false);
        if listed && self.validate_worktree_owner(git_root, &legacy).is_ok() {
            legacy
        } else {
            hashed
        }
    }

    /// Paths of every worktree of `git_root` (including the main checkout),
    /// parsed from the `worktree <path>` lines of
    /// `git worktree list --porcelain`.
    pub fn list_worktrees(&self, git_root: &Path) -> Result<Vec<PathBuf>, EngineError> {
        let output = host_command::command(git_root)
            .args(["worktree", "list", "--porcelain", "-z"])
            .current_dir(git_root)
            .output()
            .map_err(|e| EngineError::Git(format!("invoke `git worktree list`: {e}")))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(EngineError::Git(format!(
                "git worktree list failed: {}",
                stderr.trim()
            )));
        }
        let mut paths = Vec::new();
        let mut path = None;
        let mut prunable = false;
        for field in output.stdout.split(|byte| *byte == 0) {
            if field.is_empty() {
                if !prunable {
                    paths.extend(path.take());
                }
                path = None;
                prunable = false;
            } else if let Some(bytes) = field.strip_prefix(b"worktree ") {
                #[cfg(unix)]
                {
                    use std::os::unix::ffi::OsStrExt;
                    path = Some(PathBuf::from(std::ffi::OsStr::from_bytes(bytes)));
                }
                #[cfg(not(unix))]
                {
                    path = Some(PathBuf::from(String::from_utf8_lossy(bytes).as_ref()));
                }
            } else if field.starts_with(b"prunable") {
                prunable = true;
            }
        }
        Ok(paths)
    }

    /// Validate both sides of a linked-worktree registration. A stale list
    /// entry alone cannot prove ownership of a replaced directory.
    pub fn validate_worktree_owner(
        &self,
        git_root: &Path,
        worktree: &Path,
    ) -> Result<PathBuf, EngineError> {
        let invalid = || {
            EngineError::Git(format!(
                "worktree does not belong to this repository: {}",
                worktree.display()
            ))
        };
        let common_output = host_command::command(git_root)
            .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
            .current_dir(git_root)
            .output()
            .map_err(|_| invalid())?;
        if !common_output.status.success() {
            return Err(invalid());
        }
        let common =
            PathBuf::from(String::from_utf8_lossy(&common_output.stdout).trim_end_matches('\n'))
                .canonicalize()
                .map_err(|_| invalid())?;
        if !std::fs::symlink_metadata(worktree).is_ok_and(|m| m.is_dir()) {
            return Err(invalid());
        }
        let dot_git = worktree.join(".git");
        if !std::fs::symlink_metadata(&dot_git).is_ok_and(|m| m.is_file()) {
            return Err(invalid());
        }
        let pointer = std::fs::read_to_string(&dot_git).map_err(|_| invalid())?;
        let pointer = pointer
            .trim_end_matches('\n')
            .strip_prefix("gitdir: ")
            .ok_or_else(invalid)?;
        let admin = worktree
            .join(pointer)
            .canonicalize()
            .map_err(|_| invalid())?;
        if git_root.join(".git").is_dir()
            && git_root
                .join(".git")
                .canonicalize()
                .map_err(|_| invalid())?
                != common
        {
            return Err(invalid());
        }
        let registrations = common
            .join("worktrees")
            .canonicalize()
            .map_err(|_| invalid())?;
        if registrations != common.join("worktrees")
            || admin.parent() != Some(registrations.as_path())
        {
            return Err(invalid());
        }
        let backlink = std::fs::read(admin.join("gitdir")).map_err(|_| invalid())?;
        if path_from_git_bytes(backlink.strip_suffix(b"\n").unwrap_or(&backlink))
            .canonicalize()
            .map_err(|_| invalid())?
            != dot_git.canonicalize().map_err(|_| invalid())?
        {
            return Err(invalid());
        }
        let commondir = std::fs::read_to_string(admin.join("commondir")).map_err(|_| invalid())?;
        if admin
            .join(commondir.trim())
            .canonicalize()
            .map_err(|_| invalid())?
            != common
        {
            return Err(invalid());
        }
        Ok(common)
    }

    /// Delete the branch before removing its checkout. Detaching HEAD with
    /// update-ref preserves the index and every working file (no checkout,
    /// hooks or filters). Any deletion failure restores the symbolic HEAD.
    pub fn delete_recreated_branch_logged(
        &self,
        git_root: &Path,
        worktree: &Path,
        branch: &str,
        sink: &mut dyn GitFrontend,
    ) -> Result<(), EngineError> {
        let actual = self.current_branch(worktree);
        if actual.as_deref() != Some(branch) {
            return Err(EngineError::Git(format!(
                "cannot recreate {}: branch '{}' is not checked out here (current branch: {}); check whether it is checked out elsewhere",
                worktree.display(), branch, actual.as_deref().unwrap_or("detached HEAD"),
            )));
        }
        {
            let out = run_git_logged(
                &["update-ref", "--no-deref", "HEAD", "HEAD"],
                worktree,
                sink,
            )?;
            if !out.status.success() {
                return Err(EngineError::Git(
                    String::from_utf8_lossy(&out.stderr).into_owned(),
                ));
            }
        }
        if self.branch_exists(git_root, branch) {
            if let Err(error) = self.delete_branch_logged(git_root, branch, sink) {
                {
                    let restore = run_git_logged(
                        &["symbolic-ref", "HEAD", &format!("refs/heads/{branch}")],
                        worktree,
                        sink,
                    )?;
                    if !restore.status.success() {
                        return Err(EngineError::Git(format!("{error}; could not restore original branch; worktree files preserved at {}", worktree.display())));
                    }
                }
                return Err(error);
            }
        }
        Ok(())
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
        let output = host_command::command(git_root)
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
        let output = host_command::command(git_root)
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
    /// Returns `EngineError::MergeFailed` when either command fails.
    pub fn merge_branch(
        &self,
        git_root: &Path,
        branch: &str,
        worktree_path: &Path,
    ) -> Result<(), EngineError> {
        let args = ["merge", "--squash", branch];
        let output = host_command::command(git_root)
            .args(args)
            .current_dir(git_root)
            .output()
            .map_err(|e| EngineError::Git(format!("invoke `git merge --squash`: {e}")))?;
        if !output.status.success() {
            return Err(merge_failed(
                branch,
                worktree_path,
                git_root,
                &args,
                &output,
            ));
        }
        let message = squash_commit_message(branch);
        let args = ["commit", "-m", &message];
        let output = host_command::command(git_root)
            .args(args)
            .current_dir(git_root)
            .output()
            .map_err(|e| EngineError::Git(format!("invoke `git commit`: {e}")))?;
        if !output.status.success() {
            return Err(merge_failed(
                branch,
                worktree_path,
                git_root,
                &args,
                &output,
            ));
        }
        Ok(())
    }

    pub fn commit_all(&self, path: &Path, message: &str) -> Result<(), EngineError> {
        let add = host_command::command(path)
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
        let commit = host_command::command(path)
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
        let output = host_command::command(git_root)
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

    /// The argv `clone_repo_sparse` runs: `(clone_args, sparse_checkout_args)`.
    pub fn sparse_clone_argv(url: &str, dest: &str, subdir: &str) -> (Vec<String>, Vec<String>) {
        let clone = [
            "clone",
            "--depth",
            "1",
            "--filter=blob:none",
            "--sparse",
            url,
            dest,
        ]
        .map(String::from)
        .to_vec();
        let sparse = ["-C", dest, "sparse-checkout", "set", subdir]
            .map(String::from)
            .to_vec();
        (clone, sparse)
    }

    /// Shallow, blob-less, sparse clone of `url` into `dest`, checking out only
    /// `subdir`. When git is too old for `--sparse` / `sparse-checkout` (< 2.25),
    /// retries once as a plain `--depth 1` clone and reports
    /// [`SparseCloneOutcome::FellBackToShallow`].
    pub fn clone_repo_sparse(
        &self,
        url: &str,
        dest: &Path,
        subdir: &str,
    ) -> Result<SparseCloneOutcome, EngineError> {
        let outcome = self.clone_repo_sparse_inner(url, dest, subdir, &mut |args, cwd| {
            host_command::command(cwd)
                .args(args)
                .current_dir(cwd)
                .output()
                .map_err(|e| EngineError::Git(format!("invoke `git {}`: {e}", args.join(" "))))
        })?;
        if outcome == SparseCloneOutcome::FellBackToShallow {
            tracing::warn!("{SPARSE_FALLBACK_WARNING}");
        }
        Ok(outcome)
    }

    /// Logged variant of [`clone_repo_sparse`](Self::clone_repo_sparse).
    pub fn clone_repo_sparse_logged(
        &self,
        url: &str,
        dest: &Path,
        subdir: &str,
        sink: &mut dyn GitFrontend,
    ) -> Result<SparseCloneOutcome, EngineError> {
        let outcome = self.clone_repo_sparse_inner(url, dest, subdir, &mut |args, cwd| {
            run_git_logged(args, cwd, &mut *sink)
        })?;
        if outcome == SparseCloneOutcome::FellBackToShallow {
            sink.write_message(UserMessage {
                level: MessageLevel::Warning,
                text: SPARSE_FALLBACK_WARNING.to_string(),
            });
        }
        Ok(outcome)
    }

    /// Shared clone logic; `run` executes one git argv in a working directory.
    fn clone_repo_sparse_inner(
        &self,
        url: &str,
        dest: &Path,
        subdir: &str,
        run: &mut GitRunner<'_>,
    ) -> Result<SparseCloneOutcome, EngineError> {
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(|e| EngineError::io(parent, e))?;
        }
        let dest_str = dest
            .to_str()
            .ok_or_else(|| EngineError::Git("clone dest path not UTF-8".into()))?;
        let cwd = dest
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(std::env::temp_dir);
        let (clone, sparse) = Self::sparse_clone_argv(url, dest_str, subdir);

        let clone_refs: Vec<&str> = clone.iter().map(String::as_str).collect();
        let output = run(&clone_refs, &cwd)?;
        let failed = if output.status.success() {
            let sparse_refs: Vec<&str> = sparse.iter().map(String::as_str).collect();
            let output = run(&sparse_refs, &cwd)?;
            if output.status.success() {
                return Ok(SparseCloneOutcome::Sparse);
            }
            output
        } else {
            output
        };
        let stderr = String::from_utf8_lossy(&failed.stderr).trim().to_string();
        if !is_sparse_unsupported(&stderr) {
            return Err(EngineError::Git(format!("git clone failed: {stderr}")));
        }
        if dest.exists() {
            std::fs::remove_dir_all(dest).map_err(|e| EngineError::io(dest, e))?;
        }
        let output = run(&["clone", "--depth", "1", url, dest_str], &cwd)?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(EngineError::Git(format!(
                "git clone failed: {}",
                stderr.trim()
            )));
        }
        Ok(SparseCloneOutcome::FellBackToShallow)
    }

    /// Like [`pull_latest`](Self::pull_latest) but keeps the clone shallow:
    /// `git fetch --depth 1 origin`, then `reset --hard origin/HEAD`.
    pub fn pull_latest_shallow(&self, repo_dir: &Path) -> Result<(), EngineError> {
        self.pull_latest_with(repo_dir, &["fetch", "--depth", "1", "origin"])
    }

    /// Logged variant of [`pull_latest_shallow`](Self::pull_latest_shallow).
    pub fn pull_latest_shallow_logged(
        &self,
        repo_dir: &Path,
        sink: &mut dyn GitFrontend,
    ) -> Result<(), EngineError> {
        let fetch_args = ["fetch", "--depth", "1", "origin"];
        let fetch = run_git_logged(&fetch_args, repo_dir, sink)?;
        if !fetch.status.success() {
            let stderr = String::from_utf8_lossy(&fetch.stderr);
            return Err(EngineError::Git(format!(
                "git fetch --depth 1 origin failed: {}",
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

    fn pull_latest_with(&self, repo_dir: &Path, fetch_args: &[&str]) -> Result<(), EngineError> {
        let fetch = host_command::command(repo_dir)
            .args(fetch_args)
            .current_dir(repo_dir)
            .output()
            .map_err(|e| EngineError::Git(format!("invoke `git {}`: {e}", fetch_args.join(" "))))?;
        if !fetch.status.success() {
            let stderr = String::from_utf8_lossy(&fetch.stderr);
            return Err(EngineError::Git(format!(
                "git {} failed: {}",
                fetch_args.join(" "),
                stderr.trim()
            )));
        }
        let reset = host_command::command(repo_dir)
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
        let output = host_command::command(repo_dir)
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
        let output = host_command::command(repo_dir)
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
        let fetch = host_command::command(repo_dir)
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

        let reset = host_command::command(repo_dir)
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
        let output = host_command::command(path)
            .args(["checkout", branch])
            .current_dir(path)
            .output()
            .map_err(|e| EngineError::Git(format!("invoke `git checkout`: {e}")))?;
        if output.status.success() {
            return Ok("checked-out");
        }

        // Fall back: branch exists neither locally nor on any remote — create
        // it from the current HEAD.
        let output = host_command::command(path)
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
        host_command::command(git_root)
            .args(["rev-parse", "--verify", &format!("refs/heads/{branch}")])
            .current_dir(git_root)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    pub fn is_detached_head(&self, git_root: &Path) -> bool {
        !host_command::command(git_root)
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
        let output = host_command::command(git_root)
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
        let output = host_command::command(path)
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

    // ─── Workflow prerequisites (WI 0149) ─────────────────────────────

    /// The identity the **repository** configures: `git config --local` at
    /// `repo`, falling back to the worktree's common-dir config file (a
    /// linked worktree shares it). Never reads global or system config, and
    /// `include.*` directives are not followed, so this is exactly what a
    /// container that sees only the repository would resolve.
    pub fn local_identity(&self, repo: &Path) -> Result<GitIdentity, EngineError> {
        let common_config = self.git_common_dir(repo).ok().map(|d| d.join("config"));
        let read = |key: &str| -> Result<Option<String>, EngineError> {
            if let Some(value) = self.scoped_config_value(repo, &["--local"], key)? {
                return Ok(Some(value));
            }
            match &common_config {
                Some(file) if file.is_file() => {
                    let file = file.to_string_lossy();
                    self.scoped_config_value(repo, &["--file", &file], key)
                }
                _ => Ok(None),
            }
        };
        Ok(GitIdentity {
            name: read("user.name")?,
            email: read("user.email")?,
        })
    }

    /// The host's `git config --global` identity, run from a private scratch
    /// directory so no repository config can contribute. Offered to the user
    /// only; never assumed to reach a container.
    pub fn host_identity(&self) -> Result<GitIdentity, EngineError> {
        let scratch = tempfile::Builder::new()
            .prefix("awman-git-identity-")
            .tempdir()
            .map_err(|e| EngineError::Git(format!("create scratch dir: {e}")))?;
        let read = |key: &str| -> Result<Option<String>, EngineError> {
            let output = host_command::discovery_command(scratch.path())
                .args(["config", "--global", "--get", key])
                .current_dir(scratch.path())
                .stderr(std::process::Stdio::null())
                .output()
                .map_err(|e| {
                    EngineError::Git(format!("invoke `git config --global {key}`: {e}"))
                })?;
            Ok(config_output_value(&output))
        };
        Ok(GitIdentity {
            name: read("user.name")?,
            email: read("user.email")?,
        })
    }

    /// `git config --local user.name <name>` + `user.email <email>` through the
    /// hardened command. Rejects empty values, newlines/control characters, a
    /// leading `-` and an email without `@`. In a linked worktree `--local`
    /// writes the repository's shared config.
    pub fn set_local_identity(
        &self,
        repo: &Path,
        name: &str,
        email: &str,
    ) -> Result<(), EngineError> {
        validate_identity_value("user.name", name)?;
        validate_identity_value("user.email", email)?;
        if !email.contains('@') {
            return Err(EngineError::Git("user.email must contain '@'".to_string()));
        }
        for (key, value) in [("user.name", name), ("user.email", email)] {
            let output = host_command::command(repo)
                .args(["config", "--local", key, value])
                .current_dir(repo)
                .output()
                .map_err(|e| EngineError::Git(format!("invoke `git config --local {key}`: {e}")))?;
            if !output.status.success() {
                return Err(EngineError::Git(format!(
                    "git config --local {key} failed: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                )));
            }
        }
        Ok(())
    }

    /// Rendered `git -C <repo> config --local user.name '<name>' …` shown
    /// before [`Self::set_local_identity`] runs. Display only; never executed.
    pub fn set_local_identity_command_display(repo: &Path, name: &str, email: &str) -> String {
        let repo = shell_words::quote(&repo.to_string_lossy()).into_owned();
        let render = |key: &str, value: &str| {
            let value: String = value
                .chars()
                .map(|c| {
                    if c.is_control() {
                        c.escape_default().to_string()
                    } else {
                        c.to_string()
                    }
                })
                .collect();
            format!(
                "git -C {repo} config --local {key} {}",
                shell_words::quote(&value)
            )
        };
        format!(
            "{} && {}",
            render("user.name", name),
            render("user.email", email)
        )
    }

    /// `git rev-parse --path-format=absolute --git-common-dir`.
    pub fn git_common_dir(&self, repo: &Path) -> Result<PathBuf, EngineError> {
        let output = host_command::command(repo)
            .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
            .current_dir(repo)
            .output()
            .map_err(|e| {
                EngineError::Git(format!("invoke `git rev-parse --git-common-dir`: {e}"))
            })?;
        if !output.status.success() {
            return Err(EngineError::Git(format!(
                "git rev-parse --git-common-dir failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        Ok(PathBuf::from(
            String::from_utf8_lossy(&output.stdout).trim_end_matches('\n'),
        ))
    }

    /// `git rev-parse --abbrev-ref <branch>@{upstream}`; None when unset.
    pub fn branch_upstream(&self, repo: &Path, branch: &str) -> Option<String> {
        if branch.is_empty() || branch.starts_with('-') {
            return None;
        }
        let output = host_command::command(repo)
            .args(["rev-parse", "--abbrev-ref"])
            .arg(format!("{branch}@{{upstream}}"))
            .current_dir(repo)
            .stderr(std::process::Stdio::null())
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let upstream = String::from_utf8_lossy(&output.stdout).trim().to_string();
        (!upstream.is_empty()).then_some(upstream)
    }

    /// The stored URL of `remote` with any embedded credentials removed
    /// ([`strip_url_credentials`]); `None` when the remote is not configured.
    /// Use this, never [`Self::remote_url`], for anything a user may see.
    pub fn remote_url_redacted(
        &self,
        repo: &Path,
        remote: &str,
    ) -> Result<Option<String>, EngineError> {
        let key = format!("remote.{remote}.url");
        let output = host_command::command(repo)
            .args(["config", "--get", &key])
            .current_dir(repo)
            .stderr(std::process::Stdio::null())
            .output()
            .map_err(|e| EngineError::Git(format!("invoke `git config --get {key}`: {e}")))?;
        Ok(config_output_value(&output).map(|url| strip_url_credentials(&url)))
    }

    /// SSH routing identity is pinned in ephemeral engine state, never in a
    /// report or remembered decision. Password-bearing URLs are refused.
    pub fn workflow_ssh_destination(
        &self,
        repo: &Path,
        remote: &str,
    ) -> Result<crate::engine::auth::credential::SecretString, EngineError> {
        let url = if remote.contains(':') {
            remote.to_string()
        } else {
            self.remote_url(repo, remote)?
        };
        if let Some((_, rest)) = url.split_once("://") {
            if rest.split('/').next().is_some_and(|a| {
                a.split_once('@')
                    .is_some_and(|(user, _)| user.contains(':'))
            }) {
                return Err(EngineError::Git(
                    "password-bearing SSH URLs are unsupported".into(),
                ));
            }
        }
        if url.chars().any(char::is_control) {
            return Err(EngineError::Git("invalid SSH destination".into()));
        }
        Ok(crate::engine::auth::credential::SecretString::new(url))
    }

    /// One `git config <scope…> --get <key>` at `path`, `None` when unset or empty.
    fn scoped_config_value(
        &self,
        path: &Path,
        scope: &[&str],
        key: &str,
    ) -> Result<Option<String>, EngineError> {
        let output = host_command::command(path)
            .arg("config")
            .args(scope)
            .args(["--get", key])
            .current_dir(path)
            .stderr(std::process::Stdio::null())
            .output()
            .map_err(|e| EngineError::Git(format!("invoke `git config {key}`: {e}")))?;
        Ok(config_output_value(&output))
    }
    /// The commit SHA HEAD points at in `git_root`.
    pub fn head_sha(&self, git_root: &Path) -> Result<String, EngineError> {
        let output = host_command::command(git_root)
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

    /// `git stash push --include-untracked -m <message>` at `git_root`.
    ///
    /// A non-zero exit is `EngineError::Git` with git's stderr; the caller
    /// treats it as an abort of the merge.
    pub fn stash_push_logged(
        &self,
        git_root: &Path,
        message: &str,
        sink: &mut dyn GitFrontend,
    ) -> Result<(), EngineError> {
        let output = run_git_logged(
            &["stash", "push", "--include-untracked", "-m", message],
            git_root,
            sink,
        )?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(EngineError::Git(format!(
                "git stash failed: {}",
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
    /// Returns `EngineError::MergeFailed` (with the failing command, its exit
    /// code and output) when `git merge` or the squash commit fails.
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
            let args = ["merge", "-m", &message, branch];
            let output = run_git_logged(&args, git_root, sink)?;
            if !output.status.success() {
                return Err(merge_failed(
                    branch,
                    worktree_path,
                    git_root,
                    &args,
                    &output,
                ));
            }
            return Ok(());
        }
        let args = ["merge", "--squash", branch];
        let output = run_git_logged(&args, git_root, sink)?;
        if !output.status.success() {
            return Err(merge_failed(
                branch,
                worktree_path,
                git_root,
                &args,
                &output,
            ));
        }
        if !self.has_staged_changes_logged(git_root, sink)? {
            sink.write_message(UserMessage {
                level: MessageLevel::Info,
                text: "squash merge staged no changes (branch already up to date)".to_string(),
            });
            return Ok(());
        }
        let message = squash_commit_message(branch);
        let args = ["commit", "-m", &message];
        let output = run_git_logged(&args, git_root, sink)?;
        if !output.status.success() {
            return Err(merge_failed(
                branch,
                worktree_path,
                git_root,
                &args,
                &output,
            ));
        }
        Ok(())
    }

    /// Current commit identity, used to detect an agent replacing a pending squash.
    pub fn head_commit_logged(
        &self,
        git_root: &Path,
        sink: &mut dyn GitFrontend,
    ) -> Result<String, EngineError> {
        let output = run_git_logged(&["rev-parse", "--verify", "HEAD"], git_root, sink)?;
        if !output.status.success() {
            return Err(EngineError::Git("reading HEAD failed".into()));
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().into())
    }

    /// Whether the index differs from HEAD (`git diff --cached --quiet`).
    pub fn has_staged_changes_logged(
        &self,
        git_root: &Path,
        sink: &mut dyn GitFrontend,
    ) -> Result<bool, EngineError> {
        let check = run_git_logged(&["diff", "--cached", "--quiet"], git_root, sink)?;
        Ok(!check.status.success())
    }

    /// Paths whose staged contents differ from HEAD, including unmerged paths.
    pub fn staged_paths_logged(
        &self,
        git_root: &Path,
        sink: &mut dyn GitFrontend,
    ) -> Result<Vec<String>, EngineError> {
        let output = run_git_logged(&["diff", "--cached", "--name-only", "-z"], git_root, sink)?;
        if !output.status.success() {
            return Err(EngineError::Git("reading staged paths failed".into()));
        }
        output
            .stdout
            .split(|b| *b == 0)
            .filter(|p| !p.is_empty())
            .map(|p| {
                String::from_utf8(p.to_vec()).map_err(|_| {
                    EngineError::Git(
                        "staged filename is not UTF-8; resolve the merge manually".into(),
                    )
                })
            })
            .collect()
    }

    /// Unmerged paths in the index: `git diff --name-only --diff-filter=U`.
    pub fn unmerged_paths_logged(
        &self,
        git_root: &Path,
        sink: &mut dyn GitFrontend,
    ) -> Result<Vec<String>, EngineError> {
        let output = run_git_logged(
            &["diff", "--name-only", "--diff-filter=U", "-z"],
            git_root,
            sink,
        )?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(EngineError::Git(format!(
                "git diff --diff-filter=U failed: {}",
                stderr.trim()
            )));
        }
        let mut paths: Vec<String> = output
            .stdout
            .split(|byte| *byte == 0)
            .filter(|path| !path.is_empty())
            .map(|path| {
                String::from_utf8(path.to_vec()).map_err(|_| {
                    EngineError::Git(
                        "unmerged filename is not UTF-8; resolve the merge manually".into(),
                    )
                })
            })
            .collect::<Result<_, _>>()?;
        // git lists a path once per conflicting stage on some versions.
        paths.dedup();
        Ok(paths)
    }

    /// Whether a plain merge is in progress (`MERGE_HEAD` exists). A
    /// conflicted squash merge leaves no `MERGE_HEAD`; check
    /// [`Self::unmerged_paths_logged`] too.
    pub fn merge_in_progress(&self, git_root: &Path) -> bool {
        host_command::command(git_root)
            .args(["rev-parse", "-q", "--verify", "MERGE_HEAD"])
            .current_dir(git_root)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// [`Self::merge_in_progress`], with the probe reported to `sink`.
    pub fn merge_in_progress_logged(&self, git_root: &Path, sink: &mut dyn GitFrontend) -> bool {
        run_git_logged(
            &["rev-parse", "-q", "--verify", "MERGE_HEAD"],
            git_root,
            sink,
        )
        .map(|o| o.status.success())
        .unwrap_or(false)
    }

    /// Whether `commit` is an ancestor of (already merged into) `HEAD`.
    pub fn is_merged_into_head(&self, git_root: &Path, commit: &str) -> bool {
        host_command::command(git_root)
            .args(["merge-base", "--is-ancestor", commit, "HEAD"])
            .current_dir(git_root)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// An ancestry check reported through the ordinary git command stream.
    pub fn is_merged_into_head_logged(
        &self,
        git_root: &Path,
        commit: &str,
        sink: &mut dyn GitFrontend,
    ) -> bool {
        run_git_logged(
            &["merge-base", "--is-ancestor", commit, "HEAD"],
            git_root,
            sink,
        )
        .map(|o| o.status.success())
        .unwrap_or(false)
    }

    /// `git --literal-pathspecs add -- <paths>`. A path one side of the merge deleted is staged
    /// as a deletion, as `git add` does for any removed tracked file.
    pub fn stage_paths_logged(
        &self,
        git_root: &Path,
        paths: &[String],
        sink: &mut dyn GitFrontend,
    ) -> Result<(), EngineError> {
        if paths.is_empty() {
            return Ok(());
        }
        let mut args = vec!["--literal-pathspecs", "add", "--"];
        args.extend(paths.iter().map(String::as_str));
        let output = run_git_logged(&args, git_root, sink)?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(EngineError::Git(format!(
                "git add failed: {}",
                stderr.trim()
            )));
        }
        Ok(())
    }

    /// Complete a resolved merge: `git commit --no-edit` when `message` is
    /// `None` (a plain merge, `MERGE_MSG` exists), else `git commit -m
    /// <message>` (the squash commit, [`squash_commit_message`]).
    pub fn commit_merge_logged(
        &self,
        git_root: &Path,
        message: Option<&str>,
        sink: &mut dyn GitFrontend,
    ) -> Result<(), EngineError> {
        let args: Vec<&str> = match message {
            None => vec!["commit", "--no-edit"],
            Some(m) => vec!["commit", "-m", m],
        };
        let output = run_git_logged(&args, git_root, sink)?;
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

    /// Resolve the identity to commit with in `repo`. Each key git cannot resolve
    /// is supplied through `-c`; a key it can resolve is left alone. Never errors on
    /// a missing key, only on a failed `git` spawn.
    pub fn commit_identity(&self, repo: &Path) -> Result<CommitIdentity, EngineError> {
        let configured = self.identity_configured(repo)?;
        let mut overrides = Vec::new();
        let mut fallback = false;
        let name = match configured.name {
            Some(n) => n,
            None => {
                fallback = true;
                overrides.push("-c".to_string());
                overrides.push(format!("user.name={FALLBACK_COMMIT_NAME}"));
                FALLBACK_COMMIT_NAME.to_string()
            }
        };
        let email = match configured.email {
            Some(e) => e,
            None => {
                fallback = true;
                overrides.push("-c".to_string());
                overrides.push(format!("user.email={FALLBACK_COMMIT_EMAIL}"));
                FALLBACK_COMMIT_EMAIL.to_string()
            }
        };
        Ok(CommitIdentity {
            name,
            email,
            fallback,
            overrides,
        })
    }

    /// Check out the named origin branch without allowing a tag to shadow it.
    pub fn checkout_remote_branch_logged(
        &self,
        repo: &Path,
        branch: &str,
        sink: &mut dyn GitFrontend,
    ) -> Result<&'static str, EngineError> {
        let target = format!("refs/remotes/origin/{branch}");
        let output = run_git_logged(&["checkout", "-B", branch, &target], repo, sink)?;
        if !output.status.success() {
            return Err(EngineError::Git(
                String::from_utf8_lossy(&output.stderr).trim().into(),
            ));
        }
        Ok("checked-out")
    }

    /// `git rev-parse --verify --quiet refs/remotes/<remote>/<branch>` → sha, or `None`.
    pub fn remote_ref_sha(&self, repo: &Path, remote: &str, branch: &str) -> Option<String> {
        let output = host_command::command(repo)
            .args(["rev-parse", "--verify", "--quiet"])
            .arg(format!("refs/remotes/{remote}/{branch}^{{commit}}"))
            .current_dir(repo)
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let sha = String::from_utf8_lossy(&output.stdout).trim().to_string();
        (!sha.is_empty()).then_some(sha)
    }

    /// Return the first tag-only merge target. A failed advertisement lookup
    /// cannot establish branch-only validation, so it fails before acceptance.
    pub fn tag_only_merge_ref(
        &self,
        url: &str,
        names: &[String],
    ) -> Result<Option<String>, EngineError> {
        let scratch = tempfile::tempdir().map_err(|e| EngineError::Git(e.to_string()))?;
        let mut command = host_command::command(scratch.path());
        command
            .args(["ls-remote", "--heads", "--tags", url])
            .current_dir(scratch.path())
            .env("GIT_TERMINAL_PROMPT", "0");
        let output = crate::engine::host_cli::checked_output(&mut command, "git ls-remote")
            .map_err(|e| EngineError::Git(format!("cannot validate merge_refs: {e}")))?;
        let listing = String::from_utf8_lossy(&output.stdout);
        let advertised: std::collections::HashSet<&str> = listing
            .lines()
            .filter_map(|line| line.split_once('\t').map(|(_, r)| r))
            .collect();
        Ok(names
            .iter()
            .find(|name| {
                advertised.contains(format!("refs/tags/{name}").as_str())
                    && !advertised.contains(format!("refs/heads/{name}").as_str())
            })
            .cloned())
    }

    /// Resolve a caller-supplied base ref to a spelling git accepts as a start point.
    /// Tries `<base_ref>` first (sha, tag, local branch, `origin/x`), then
    /// `origin/<base_ref>`. Returns the spelling that resolved.
    pub fn resolve_base_ref_logged(
        &self,
        repo: &Path,
        base_ref: &str,
        sink: &mut dyn GitFrontend,
    ) -> Result<String, EngineError> {
        for candidate in [base_ref.to_string(), format!("origin/{base_ref}")] {
            let spec = format!("{candidate}^{{commit}}");
            let output = run_git_logged(&["rev-parse", "--verify", "--quiet", &spec], repo, sink)?;
            if output.status.success() {
                return Ok(candidate);
            }
        }
        Err(EngineError::Git(format!(
            "base_ref '{base_ref}' does not resolve to a commit in the clone \
             (tried '{base_ref}' and 'origin/{base_ref}')"
        )))
    }

    /// `git checkout --no-track -b <branch> <start_point>`. `--no-track` so a branch
    /// cut from `origin/main` does not get `origin/main` as its upstream.
    pub fn create_branch_from_logged(
        &self,
        repo: &Path,
        branch: &str,
        start_point: &str,
        sink: &mut dyn GitFrontend,
    ) -> Result<(), EngineError> {
        let output = run_git_logged(
            &["checkout", "--no-track", "-b", branch, start_point],
            repo,
            sink,
        )?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(EngineError::Git(format!(
                "git checkout -b {branch} {start_point} failed: {}",
                stderr.trim()
            )));
        }
        Ok(())
    }

    /// `git merge --no-ff --no-edit origin/<merge_ref>` in `repo`, committing as
    /// `identity`. A missing `origin/<merge_ref>` is an error; a conflict is aborted
    /// (`git merge --abort`) and reported with the conflicting paths.
    pub fn merge_ref_logged(
        &self,
        repo: &Path,
        merge_ref: &str,
        identity: &CommitIdentity,
        sink: &mut dyn GitFrontend,
    ) -> Result<MergeRefOutcome, EngineError> {
        if self.remote_ref_sha(repo, "origin", merge_ref).is_none() {
            return Err(EngineError::Git(format!(
                "merge ref '{merge_ref}' does not exist on origin"
            )));
        }
        let target = format!("refs/remotes/origin/{merge_ref}");
        let args = with_overrides(identity, &["merge", "--no-ff", "--no-edit", &target]);
        let output = run_git_logged(&args, repo, sink)?;
        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            if stdout.contains("Already up to date") {
                return Ok(MergeRefOutcome::AlreadyUpToDate);
            }
            return Ok(MergeRefOutcome::Merged {
                sha: self.head_sha(repo)?,
            });
        }
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let conflicting_paths = self.unmerged_paths_logged(repo, sink).unwrap_or_default();
        // Leave the clone at the pre-merge HEAD. A failed abort is logged and ignored.
        let _ = run_git_logged(&["merge", "--abort"], repo, sink);
        if conflicting_paths.is_empty() {
            Err(EngineError::Git(format!(
                "git merge {target} failed: {stderr}"
            )))
        } else {
            Err(EngineError::MergeRefConflict {
                merge_ref: merge_ref.to_string(),
                conflicting_paths,
            })
        }
    }

    /// `git add -A -- . <AWMAN_BOOKKEEPING_EXCLUDES>`, then commit with `message` as
    /// `identity` if anything is staged. No changes is `NothingToCommit`, not an error.
    pub fn commit_all_with_identity_logged(
        &self,
        repo: &Path,
        message: &str,
        identity: &CommitIdentity,
        sink: &mut dyn GitFrontend,
    ) -> Result<CommitAllOutcome, EngineError> {
        // Exclusions must apply to entries already staged by an agent as well.
        let output = run_git_logged(
            &[
                "reset",
                "-q",
                "HEAD",
                "--",
                ".awman/workflows/*.json",
                ".awman/workflows/dynamic-*.toml",
            ],
            repo,
            sink,
        )?;
        if !output.status.success() {
            return Err(EngineError::Git(
                String::from_utf8_lossy(&output.stderr).trim().into(),
            ));
        }
        let mut add: Vec<&str> = vec!["add", "-A", "--", "."];
        add.extend_from_slice(AWMAN_BOOKKEEPING_EXCLUDES);
        let output = run_git_logged(&add, repo, sink)?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(EngineError::Git(format!(
                "git add -A failed: {}",
                stderr.trim()
            )));
        }
        if !self.has_staged_changes_logged(repo, sink)? {
            return Ok(CommitAllOutcome::NothingToCommit);
        }
        let args = with_overrides(identity, &["commit", "-m", message]);
        let output = run_git_logged(&args, repo, sink)?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(EngineError::Git(format!(
                "git commit failed: {}",
                stderr.trim()
            )));
        }
        Ok(CommitAllOutcome::Committed {
            sha: self.head_sha(repo)?,
        })
    }

    /// `git push -u <remote> HEAD:<branch>` in `repo` with `GIT_TERMINAL_PROMPT=0`,
    /// so a missing credential fails fast with git's stderr. Never `--force`.
    /// Returns the sha that was pushed.
    pub fn push_upstream_logged(
        &self,
        repo: &Path,
        remote: &str,
        branch: &str,
        sink: &mut dyn GitFrontend,
    ) -> Result<String, EngineError> {
        let refspec = format!("HEAD:refs/heads/{branch}");
        let args = ["push", "-u", remote, refspec.as_str()];
        let output = run_git_logged_env(&args, repo, &[("GIT_TERMINAL_PROMPT", "0")], sink)?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(EngineError::Git(format!(
                "git push -u {remote} HEAD:refs/heads/{branch} failed: {}",
                stderr.trim()
            )));
        }
        self.head_sha(repo)
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
    let output = host_command::command(root)
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
/// `path` canonicalized, or `path` as given when it cannot be (missing,
/// unreadable). Used to key worktrees on the canonical git root.
fn canonical_or_given(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

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

/// The value printed by a successful `git config --get`, `None` when the key
/// is unset (exit 1) or empty.
fn config_output_value(output: &std::process::Output) -> Option<String> {
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!value.is_empty()).then_some(value)
}

fn validate_identity_value(key: &str, value: &str) -> Result<(), EngineError> {
    if value.trim().is_empty() {
        return Err(EngineError::Git(format!("{key} must not be empty")));
    }
    if value.chars().any(char::is_control) {
        return Err(EngineError::Git(format!(
            "{key} must not contain newlines or control characters"
        )));
    }
    if value.starts_with('-') {
        return Err(EngineError::Git(format!("{key} must not start with '-'")));
    }
    Ok(())
}

/// Remove credentials embedded in a remote URL before it is displayed or
/// stored: `https://user:token@host/x` → `https://host/x`; for other schemes
/// only a password is removed (`ssh://git:pw@host` → `ssh://git@host`); query
/// strings and fragments are dropped. scp-style `user@host:path` carries no
/// password and is returned unchanged.
pub fn strip_url_credentials(url: &str) -> String {
    let url = url.trim();
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_string();
    };
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(end);
    let tail = tail.split(['?', '#']).next().unwrap_or_default();
    let authority = match authority.rsplit_once('@') {
        None => authority.to_string(),
        Some((userinfo, host)) => {
            if scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https") {
                host.to_string()
            } else {
                let user = userinfo.split(':').next().unwrap_or_default();
                if user.is_empty() {
                    host.to_string()
                } else {
                    format!("{user}@{host}")
                }
            }
        }
    };
    format!("{scheme}://{authority}{tail}")
}

/// The git queries the workflow-prerequisite preflight makes. `GitEngine` is
/// the production implementation; tests inject a fake. The preflight's
/// `sources.rs` re-exports this trait rather than declaring its own.
pub trait GitProbeSource: Send + Sync {
    fn local_identity(&self, repo: &Path) -> Result<GitIdentity, EngineError>;
    fn host_identity(&self) -> Result<GitIdentity, EngineError>;
    /// Credential-free URL ([`strip_url_credentials`]); `None` when unset.
    fn remote_url(&self, repo: &Path, remote: &str) -> Result<Option<String>, EngineError>;
    fn is_detached_head(&self, repo: &Path) -> bool;
    fn current_branch(&self, repo: &Path) -> Option<String>;
    fn branch_upstream(&self, repo: &Path, branch: &str) -> Option<String>;
}

impl GitProbeSource for GitEngine {
    fn local_identity(&self, repo: &Path) -> Result<GitIdentity, EngineError> {
        GitEngine::local_identity(self, repo)
    }
    fn host_identity(&self) -> Result<GitIdentity, EngineError> {
        GitEngine::host_identity(self)
    }
    fn remote_url(&self, repo: &Path, remote: &str) -> Result<Option<String>, EngineError> {
        self.remote_url_redacted(repo, remote)
    }
    fn is_detached_head(&self, repo: &Path) -> bool {
        GitEngine::is_detached_head(self, repo)
    }
    fn current_branch(&self, repo: &Path) -> Option<String> {
        GitEngine::current_branch(self, repo)
    }
    fn branch_upstream(&self, repo: &Path, branch: &str) -> Option<String> {
        GitEngine::branch_upstream(self, repo, branch)
    }
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

    // ─── sparse clone ─────────────────────────────────────────────────────────

    #[test]
    fn sparse_clone_argv_is_shallow_blobless_sparse_then_sets_subdir() {
        let (clone, sparse) =
            GitEngine::sparse_clone_argv("https://example.test/o/r.git", "/tmp/dest", "skills");
        assert_eq!(
            clone,
            [
                "clone",
                "--depth",
                "1",
                "--filter=blob:none",
                "--sparse",
                "https://example.test/o/r.git",
                "/tmp/dest"
            ]
        );
        assert_eq!(
            sparse,
            ["-C", "/tmp/dest", "sparse-checkout", "set", "skills"]
        );
    }

    /// A failed `git` invocation whose stderr is replaced with `stderr`.
    fn failed_output(stderr: &str) -> std::process::Output {
        let mut out = Command::new("git")
            .arg("--awman-no-such-flag")
            .output()
            .unwrap();
        assert!(!out.status.success());
        out.stderr = stderr.as_bytes().to_vec();
        out
    }

    #[test]
    fn sparse_clone_falls_back_to_plain_shallow_clone_on_old_git() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("lib");
        let mut calls: Vec<Vec<String>> = Vec::new();
        let outcome = GitEngine::new()
            .clone_repo_sparse_inner("file:///nowhere", &dest, "skills", &mut |args, _cwd| {
                calls.push(args.iter().map(|a| a.to_string()).collect());
                if args.contains(&"--sparse") {
                    // Leave a partial clone behind, like a half-supported git would.
                    std::fs::create_dir_all(&dest).unwrap();
                    Ok(failed_output("error: unknown option `sparse'"))
                } else {
                    assert!(!dest.exists(), "partial clone must be removed before retry");
                    Ok(Command::new("git").arg("--version").output().unwrap())
                }
            })
            .unwrap();
        assert_eq!(outcome, SparseCloneOutcome::FellBackToShallow);
        assert_eq!(calls.len(), 2);
        assert_eq!(
            calls[1],
            [
                "clone",
                "--depth",
                "1",
                "file:///nowhere",
                dest.to_str().unwrap()
            ]
        );
    }

    #[test]
    fn sparse_clone_does_not_retry_on_unrelated_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("lib");
        let mut count = 0;
        let err = GitEngine::new()
            .clone_repo_sparse_inner("file:///nowhere", &dest, "skills", &mut |_args, _cwd| {
                count += 1;
                Ok(failed_output(
                    "fatal: unable to access: could not resolve host",
                ))
            })
            .unwrap_err();
        assert_eq!(count, 1);
        assert!(err.to_string().contains("could not resolve host"), "{err}");
    }

    #[test]
    fn sparse_clone_retries_when_sparse_checkout_set_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("lib");
        let mut calls = 0;
        let outcome = GitEngine::new()
            .clone_repo_sparse_inner("file:///nowhere", &dest, "skills", &mut |args, _cwd| {
                calls += 1;
                match calls {
                    1 => Ok(Command::new("git").arg("--version").output().unwrap()),
                    2 => {
                        assert!(args.contains(&"sparse-checkout"));
                        Ok(failed_output("git: 'sparse-checkout' is not a git command"))
                    }
                    _ => Ok(Command::new("git").arg("--version").output().unwrap()),
                }
            })
            .unwrap();
        assert_eq!(outcome, SparseCloneOutcome::FellBackToShallow);
        assert_eq!(calls, 3);
    }

    // ---- WI 0133: merge refs, leftover commit, push ----

    type UpstreamClone = (
        tempfile::TempDir,
        tempfile::TempDir,
        tempfile::TempDir,
        PathBuf,
    );

    /// `(source, bare, work, clone)`: a clone of the local upstream. The clone has no
    /// `user.*` of its own; tests set or rely on explicit identities.
    fn clone_of_upstream() -> UpstreamClone {
        let (source, bare) = setup_upstream();
        let work = tempfile::tempdir().unwrap();
        let clone = work.path().join("clone");
        GitEngine::new()
            .clone_repo(&bare_url(&bare), None, &clone)
            .unwrap();
        (source, bare, work, clone)
    }

    /// Commit `file` = `content` on a new upstream branch `name` cut from main.
    fn push_upstream_branch(source: &Path, name: &str, file: &str, content: &str) {
        run_git(source, &["checkout", "-q", "-b", name, "main"]);
        std::fs::write(source.join(file), content).unwrap();
        run_git(source, &["add", "."]);
        run_git(
            source,
            &["commit", "-q", "-m", &format!("edit {file} on {name}")],
        );
        run_git(source, &["push", "-q", "origin", name]);
        run_git(source, &["checkout", "-q", "main"]);
    }

    fn git_out(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// The fallback assertions only hold when no global identity leaks in.
    fn global_email_unset() -> bool {
        !Command::new("git")
            .args(["config", "--global", "user.email"])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    fn test_identity() -> CommitIdentity {
        CommitIdentity {
            name: "a".into(),
            email: "a@a".into(),
            fallback: true,
            overrides: ["-c", "user.name=a", "-c", "user.email=a@a"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        }
    }

    #[test]
    fn commit_identity_uses_clone_config_when_present() {
        let (_s, _b, _w, clone) = clone_of_upstream();
        run_git(&clone, &["config", "user.name", "Jane"]);
        run_git(&clone, &["config", "user.email", "jane@x"]);
        let id = GitEngine::new().commit_identity(&clone).unwrap();
        assert_eq!(id.name, "Jane");
        assert_eq!(id.email, "jane@x");
        assert!(!id.fallback);
        assert!(id.overrides.is_empty());
        assert!(id.describe().contains("Jane <jane@x> (git config)"));
    }

    #[test]
    fn commit_identity_falls_back_when_clone_has_none() {
        if !global_email_unset() {
            return;
        }
        let (_s, _b, _w, clone) = clone_of_upstream();
        let id = GitEngine::new().commit_identity(&clone).unwrap();
        assert!(id.fallback);
        assert_eq!(id.email, FALLBACK_COMMIT_EMAIL);
        assert!(id
            .overrides
            .contains(&"user.email=awman@localhost".to_string()));
        assert!(id.overrides.contains(&"user.name=awman".to_string()));
        assert!(id.describe().contains("awman <awman@localhost>"));
    }

    #[test]
    fn commit_identity_overrides_only_the_missing_key() {
        if !global_email_unset() {
            return;
        }
        let (_s, _b, _w, clone) = clone_of_upstream();
        run_git(&clone, &["config", "user.name", "Jane"]);
        let id = GitEngine::new().commit_identity(&clone).unwrap();
        assert_eq!(id.name, "Jane");
        assert_eq!(id.email, FALLBACK_COMMIT_EMAIL);
        assert_eq!(id.overrides, vec!["-c", "user.email=awman@localhost"]);
    }

    #[test]
    fn remote_ref_sha_finds_existing_and_not_missing() {
        let (_s, _b, _w, clone) = clone_of_upstream();
        let g = GitEngine::new();
        assert!(g.remote_ref_sha(&clone, "origin", "main").is_some());
        assert!(g.remote_ref_sha(&clone, "origin", "nope").is_none());
    }

    #[test]
    fn resolve_base_ref_handles_sha_tag_branch_and_missing() {
        let (source, _b, _w, clone) = clone_of_upstream();
        push_upstream_branch(source.path(), "feature", "f.txt", "f");
        run_git(&clone, &["fetch", "-q", "origin"]);
        run_git(&clone, &["tag", "v1", "origin/main"]);
        let g = GitEngine::new();
        let mut sink = crate::data::message::RecordingMessageSink::new();
        let sha = g.head_sha(&clone).unwrap();
        assert_eq!(
            g.resolve_base_ref_logged(&clone, &sha, &mut sink).unwrap(),
            sha
        );
        assert_eq!(
            g.resolve_base_ref_logged(&clone, "v1", &mut sink).unwrap(),
            "v1"
        );
        assert_eq!(
            g.resolve_base_ref_logged(&clone, "feature", &mut sink)
                .unwrap(),
            "origin/feature"
        );
        let err = g
            .resolve_base_ref_logged(&clone, "missing", &mut sink)
            .unwrap_err();
        assert!(err.to_string().contains("does not resolve"), "{err}");
    }

    #[test]
    fn create_branch_from_cuts_branch_without_upstream() {
        let (source, _b, _w, clone) = clone_of_upstream();
        push_upstream_branch(source.path(), "base", "base.txt", "b");
        run_git(&clone, &["fetch", "-q", "origin"]);
        let g = GitEngine::new();
        let mut sink = crate::data::message::RecordingMessageSink::new();
        g.create_branch_from_logged(&clone, "work", "origin/base", &mut sink)
            .unwrap();
        assert!(clone.join("base.txt").exists());
        assert_eq!(git_out(&clone, &["config", "branch.work.remote"]), "");
        assert!(g
            .create_branch_from_logged(&clone, "x", "origin/nope", &mut sink)
            .is_err());
    }

    #[test]
    fn merge_ref_merges_with_a_merge_commit() {
        let (source, _b, _w, clone) = clone_of_upstream();
        push_upstream_branch(source.path(), "other", "other.txt", "o");
        run_git(&clone, &["fetch", "-q", "origin"]);
        run_git(&clone, &["checkout", "-q", "-b", "work"]);
        // Diverge so --no-ff has something to record beyond a fast-forward.
        std::fs::write(clone.join("mine.txt"), "m").unwrap();
        run_git(&clone, &["add", "."]);
        run_git(
            &clone,
            &[
                "-c",
                "user.name=a",
                "-c",
                "user.email=a@a",
                "commit",
                "-q",
                "-m",
                "mine",
            ],
        );
        let g = GitEngine::new();
        let id = test_identity();
        let mut sink = crate::data::message::RecordingMessageSink::new();
        let out = g.merge_ref_logged(&clone, "other", &id, &mut sink).unwrap();
        let MergeRefOutcome::Merged { sha } = out else {
            panic!("expected Merged, got {out:?}");
        };
        assert_eq!(sha, g.head_sha(&clone).unwrap());
        assert!(clone.join("other.txt").exists());
        let parents = git_out(&clone, &["rev-list", "--parents", "-n", "1", "HEAD"]);
        assert_eq!(
            parents.split_whitespace().count(),
            3,
            "merge commit has 2 parents"
        );
        let again = g.merge_ref_logged(&clone, "other", &id, &mut sink).unwrap();
        assert_eq!(again, MergeRefOutcome::AlreadyUpToDate);
    }

    #[test]
    fn merge_ref_forces_a_merge_commit_even_when_fast_forward_is_possible() {
        let (source, _b, _w, clone) = clone_of_upstream();
        push_upstream_branch(source.path(), "other", "other.txt", "o");
        run_git(&clone, &["fetch", "-q", "origin"]);
        run_git(&clone, &["checkout", "-q", "-b", "work"]);
        let g = GitEngine::new();
        let mut sink = crate::data::message::RecordingMessageSink::new();
        g.merge_ref_logged(&clone, "other", &test_identity(), &mut sink)
            .unwrap();
        let parents = git_out(&clone, &["rev-list", "--parents", "-n", "1", "HEAD"]);
        assert_eq!(parents.split_whitespace().count(), 3);
    }

    #[test]
    fn merge_ref_commits_as_fallback_identity_when_clone_has_none() {
        if !global_email_unset() {
            return;
        }
        let (source, _b, _w, clone) = clone_of_upstream();
        push_upstream_branch(source.path(), "other", "other.txt", "o");
        run_git(&clone, &["fetch", "-q", "origin"]);
        run_git(&clone, &["checkout", "-q", "-b", "work"]);
        let g = GitEngine::new();
        let id = g.commit_identity(&clone).unwrap();
        let mut sink = crate::data::message::RecordingMessageSink::new();
        g.merge_ref_logged(&clone, "other", &id, &mut sink).unwrap();
        assert_eq!(
            git_out(&clone, &["log", "-1", "--format=%ae"]),
            "awman@localhost"
        );
    }

    #[test]
    fn merge_ref_missing_ref_is_an_error() {
        let (_s, _b, _w, clone) = clone_of_upstream();
        let g = GitEngine::new();
        let mut sink = crate::data::message::RecordingMessageSink::new();
        let err = g
            .merge_ref_logged(&clone, "ghost", &test_identity(), &mut sink)
            .unwrap_err();
        assert!(
            err.to_string().contains("does not exist on origin"),
            "{err}"
        );
    }

    #[test]
    fn merge_ref_conflict_aborts_and_lists_paths() {
        let (source, _b, _w, clone) = clone_of_upstream();
        push_upstream_branch(source.path(), "other", "README.md", "theirs");
        run_git(&clone, &["fetch", "-q", "origin"]);
        run_git(&clone, &["checkout", "-q", "-b", "work"]);
        std::fs::write(clone.join("README.md"), "ours").unwrap();
        run_git(&clone, &["add", "."]);
        run_git(
            &clone,
            &[
                "-c",
                "user.name=a",
                "-c",
                "user.email=a@a",
                "commit",
                "-q",
                "-m",
                "ours",
            ],
        );
        let g = GitEngine::new();
        let before = g.head_sha(&clone).unwrap();
        let mut sink = crate::data::message::RecordingMessageSink::new();
        let err = g
            .merge_ref_logged(&clone, "other", &test_identity(), &mut sink)
            .unwrap_err();
        match &err {
            EngineError::MergeRefConflict {
                merge_ref,
                conflicting_paths,
            } => {
                assert_eq!(merge_ref, "other");
                assert_eq!(conflicting_paths, &vec!["README.md".to_string()]);
            }
            other => panic!("expected MergeRefConflict, got {other:?}"),
        }
        assert!(err.to_string().contains("README.md"));
        assert!(!clone.join(".git/MERGE_HEAD").exists());
        assert_eq!(g.head_sha(&clone).unwrap(), before);
        assert_eq!(
            std::fs::read_to_string(clone.join("README.md")).unwrap(),
            "ours"
        );
    }

    #[test]
    fn commit_all_commits_changes_and_reports_sha() {
        let (_s, _b, _w, clone) = clone_of_upstream();
        let g = GitEngine::new();
        let mut sink = crate::data::message::RecordingMessageSink::new();
        std::fs::write(clone.join("new.txt"), "n").unwrap();
        let out = g
            .commit_all_with_identity_logged(
                &clone,
                "Implement work item 0001",
                &test_identity(),
                &mut sink,
            )
            .unwrap();
        let CommitAllOutcome::Committed { sha } = out else {
            panic!("expected Committed, got {out:?}");
        };
        assert_eq!(sha, g.head_sha(&clone).unwrap());
        assert_eq!(
            git_out(&clone, &["log", "-1", "--format=%s"]),
            "Implement work item 0001"
        );
    }

    #[test]
    fn commit_all_with_no_changes_is_not_an_error() {
        let (_s, _b, _w, clone) = clone_of_upstream();
        let g = GitEngine::new();
        let mut sink = crate::data::message::RecordingMessageSink::new();
        let before = g.head_sha(&clone).unwrap();
        let out = g
            .commit_all_with_identity_logged(&clone, "msg", &test_identity(), &mut sink)
            .unwrap();
        assert_eq!(out, CommitAllOutcome::NothingToCommit);
        assert_eq!(g.head_sha(&clone).unwrap(), before);
    }

    #[test]
    fn commit_all_ignores_awman_workflow_bookkeeping() {
        let (_s, _b, _w, clone) = clone_of_upstream();
        let g = GitEngine::new();
        let mut sink = crate::data::message::RecordingMessageSink::new();
        let dir = clone.join(".awman/workflows");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("abc-0001-wf.json"), "{}").unwrap();
        std::fs::write(dir.join("dynamic-0001.toml"), "x = 1").unwrap();
        let out = g
            .commit_all_with_identity_logged(&clone, "msg", &test_identity(), &mut sink)
            .unwrap();
        assert_eq!(out, CommitAllOutcome::NothingToCommit);
    }

    #[test]
    fn commit_all_excludes_pre_staged_bookkeeping_additions_modifications_and_deletions() {
        for source_edit in [false, true] {
            let (_s, _b, _w, clone) = clone_of_upstream();
            let dir = clone.join(".awman/workflows");
            std::fs::create_dir_all(&dir).unwrap();
            for name in ["modified.json", "deleted.json"] {
                std::fs::write(dir.join(name), "original").unwrap();
            }
            run_git(&clone, &["add", "-A"]);
            run_git(
                &clone,
                &[
                    "-c",
                    "user.name=T",
                    "-c",
                    "user.email=t@t",
                    "commit",
                    "-qm",
                    "baseline",
                ],
            );
            std::fs::write(dir.join("modified.json"), "modified").unwrap();
            std::fs::remove_file(dir.join("deleted.json")).unwrap();
            std::fs::write(dir.join("new.json"), "new").unwrap();
            std::fs::write(dir.join("dynamic-0042.toml"), "new").unwrap();
            if source_edit {
                std::fs::write(clone.join("work.txt"), "work").unwrap();
            }
            run_git(&clone, &["add", "-A"]);
            let before = GitEngine::new().head_sha(&clone).unwrap();
            let result = GitEngine::new()
                .commit_all_with_identity_logged(
                    &clone,
                    "leftovers",
                    &test_identity(),
                    &mut crate::data::message::RecordingMessageSink::new(),
                )
                .unwrap();
            if source_edit {
                assert!(matches!(result, CommitAllOutcome::Committed { .. }));
                assert_eq!(
                    git_out(&clone, &["diff", "--name-only", &before, "HEAD"]),
                    "work.txt"
                );
            } else {
                assert_eq!(result, CommitAllOutcome::NothingToCommit);
                assert_eq!(GitEngine::new().head_sha(&clone).unwrap(), before);
            }
            assert_eq!(
                git_out(&clone, &["show", "HEAD:.awman/workflows/modified.json"]),
                "original"
            );
            assert_eq!(
                git_out(&clone, &["show", "HEAD:.awman/workflows/deleted.json"]),
                "original"
            );
            assert!(dir.join("new.json").exists());
            assert!(!dir.join("deleted.json").exists());
        }
    }

    #[test]
    fn checkout_fingerprint_detects_dirty_rewrites_untracked_rewrites_and_commits() {
        let (_s, _b, _w, clone) = clone_of_upstream();
        let git = GitEngine::new();
        std::fs::write(clone.join("README.md"), "dirty").unwrap();
        let before = git.checkout_fingerprint(&clone).unwrap();
        std::fs::write(clone.join("README.md"), "other").unwrap();
        assert_ne!(before, git.checkout_fingerprint(&clone).unwrap());
        std::fs::write(clone.join("untracked"), "one").unwrap();
        let before = git.checkout_fingerprint(&clone).unwrap();
        std::fs::write(clone.join("untracked"), "two").unwrap();
        assert_ne!(before, git.checkout_fingerprint(&clone).unwrap());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let before = git.checkout_fingerprint(&clone).unwrap();
            std::fs::set_permissions(
                clone.join("README.md"),
                std::fs::Permissions::from_mode(0o755),
            )
            .unwrap();
            assert_ne!(before, git.checkout_fingerprint(&clone).unwrap());
        }
        let before = git.checkout_fingerprint(&clone).unwrap();
        run_git(
            &clone,
            &[
                "-c",
                "user.name=T",
                "-c",
                "user.email=t@t",
                "commit",
                "--allow-empty",
                "-qm",
                "forbidden",
            ],
        );
        assert_ne!(before, git.checkout_fingerprint(&clone).unwrap());
    }

    #[test]
    fn push_upstream_creates_branch_on_origin_and_sets_upstream() {
        let (_s, bare, _w, clone) = clone_of_upstream();
        let g = GitEngine::new();
        let mut sink = crate::data::message::RecordingMessageSink::new();
        g.create_branch_from_logged(&clone, "local-name", "origin/HEAD", &mut sink)
            .unwrap();
        std::fs::write(clone.join("work.txt"), "w").unwrap();
        g.commit_all_with_identity_logged(&clone, "work", &test_identity(), &mut sink)
            .unwrap();
        let pushed = g
            .push_upstream_logged(&clone, "origin", "job/1", &mut sink)
            .unwrap();
        assert_eq!(pushed, g.head_sha(&clone).unwrap());
        assert_eq!(
            git_out(bare.path(), &["rev-parse", "refs/heads/job/1"]),
            pushed
        );
        assert_eq!(
            git_out(&clone, &["config", "branch.local-name.remote"]),
            "origin"
        );
    }

    #[test]
    fn push_upstream_rejection_returns_stderr_and_never_forces() {
        let (source, bare, _w, clone) = clone_of_upstream();
        push_upstream_branch(source.path(), "job", "theirs.txt", "t");
        let g = GitEngine::new();
        let mut sink = crate::data::message::RecordingMessageSink::new();
        std::fs::write(clone.join("mine.txt"), "m").unwrap();
        g.commit_all_with_identity_logged(&clone, "mine", &test_identity(), &mut sink)
            .unwrap();
        let before = git_out(bare.path(), &["rev-parse", "refs/heads/job"]);
        let err = g
            .push_upstream_logged(&clone, "origin", "job", &mut sink)
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("git push -u origin HEAD:refs/heads/job failed"),
            "{err}"
        );
        assert_eq!(
            before,
            git_out(bare.path(), &["rev-parse", "refs/heads/job"]),
            "a non-fast-forward push must not overwrite the remote branch"
        );
    }
    #[test]
    fn legacy_lookup_rejects_stale_registration_replaced_by_foreign_checkout() {
        for suffix in ["0134", "wf-build"] {
            let tmp = tempfile::tempdir().unwrap();
            let a = tmp.path().join("a");
            let b = tmp.path().join("b");
            std::fs::create_dir_all(&a).unwrap();
            std::fs::create_dir_all(&b).unwrap();
            init_repo(&a);
            init_repo(&b);
            let legacy = tmp.path().join("legacy").join(suffix);
            let hashed = tmp.path().join("hashed").join(suffix);
            let git = GitEngine::new();
            git.create_worktree(&a, &legacy, "old-a").unwrap();
            std::fs::remove_dir_all(&legacy).unwrap();
            assert!(
                !git.list_worktrees(&a).unwrap().contains(&legacy),
                "prunable records are excluded"
            );
            git.create_worktree(&b, &legacy, "new-b").unwrap();
            // Some Git versions retain this registration without a prunable
            // marker once another checkout occupies the path. Ownership must
            // reject it independently of that marker.
            assert!(git.validate_worktree_owner(&a, &legacy).is_err());
            assert_eq!(
                git.pick_worktree_path(&a, hashed.clone(), legacy.clone()),
                hashed
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn legacy_lookup_rejects_replacement_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);
        let git = GitEngine::new();
        let legacy = tmp.path().join("legacy");
        let replacement = tmp.path().join("replacement");
        git.create_worktree(&repo, &legacy, "old").unwrap();
        git.create_worktree(&repo, &replacement, "other").unwrap();
        std::fs::remove_dir_all(&legacy).unwrap();
        std::os::unix::fs::symlink(&replacement, &legacy).unwrap();
        let hashed = tmp.path().join("hashed");
        assert_eq!(
            git.pick_worktree_path(&repo, hashed.clone(), legacy),
            hashed
        );
    }

    #[cfg(unix)]
    #[test]
    fn legacy_lookup_preserves_tabs_newlines_and_native_path_bytes() {
        use std::os::unix::ffi::OsStringExt;
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);
        for (index, suffix) in ["0134", "wf-build"].iter().enumerate() {
            let ancestor = std::ffi::OsString::from_vec(b"tab\tline\nnonutf8-\xff".to_vec());
            let legacy = tmp.path().join(ancestor).join(suffix);
            let hashed = tmp.path().join("hashed").join(suffix);
            let git = GitEngine::new();
            // GitEngine creation currently requires UTF-8 argv; Git itself
            // accepts native bytes and the listing must preserve them.
            std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
            assert!(Command::new("git")
                .args(["worktree", "add", "-b", &format!("odd-{index}")])
                .arg(&legacy)
                .current_dir(&repo)
                .output()
                .unwrap()
                .status
                .success());
            assert_eq!(
                git.pick_worktree_path(&repo, hashed, legacy.clone()),
                legacy
            );
        }
    }

    #[test]
    fn worktree_owner_rejects_tampered_pointer_without_deriving_overlay_from_it() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);
        let wt = tmp.path().join("wt");
        let git = GitEngine::new();
        git.create_worktree(&repo, &wt, "test-pointer").unwrap();
        assert_eq!(
            git.validate_worktree_owner(&repo, &wt).unwrap(),
            repo.join(".git")
        );
        std::fs::write(
            wt.join(".git"),
            format!("gitdir: {}/foreign/a/b\n", tmp.path().display()),
        )
        .unwrap();
        assert!(git.validate_worktree_owner(&repo, &wt).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn host_git_disables_repository_hooks_filters_fsmonitor_and_merge_drivers() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        init_repo(tmp.path());
        let sentinel = tmp.path().join("host-executed");
        let script = tmp.path().join("evil.sh");
        std::fs::write(
            &script,
            format!("#!/bin/sh\ntouch '{}'\ncat\n", sentinel.display()),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        for key in [
            "core.fsmonitor",
            "filter.evil.clean",
            "filter.evil.smudge",
            "filter.evil.process",
            "merge.evil.driver",
            "diff.evil.textconv",
        ] {
            assert!(Command::new("git")
                .args(["config", key])
                .arg(&script)
                .current_dir(tmp.path())
                .status()
                .unwrap()
                .success());
        }
        let hook = tmp.path().join(".git/hooks/post-checkout");
        std::fs::copy(&script, &hook).unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(
            tmp.path().join(".gitattributes"),
            "* filter=evil diff=evil merge=evil\n",
        )
        .unwrap();
        std::fs::write(tmp.path().join("tracked"), "data\n").unwrap();
        let git = GitEngine::new();
        git.commit_all(tmp.path(), "inert content drivers").unwrap();
        git.uncommitted_files(tmp.path()).unwrap();
        let wt = tmp.path().join("worktree");
        git.create_worktree(tmp.path(), &wt, "safe").unwrap();
        assert!(
            !sentinel.exists(),
            "repository commands must not execute on the host"
        );
    }

    #[test]
    fn strip_url_credentials_removes_userinfo_secrets_and_queries() {
        for (input, expected) in [
            (
                "https://x-access-token:ghp_abc@github.com/o/r.git",
                "https://github.com/o/r.git",
            ),
            ("https://ghp_abc@github.com/o/r", "https://github.com/o/r"),
            (
                "https://github.com/o/r?token=1#frag",
                "https://github.com/o/r",
            ),
            (
                "ssh://git:pw@host.example/o/r",
                "ssh://git@host.example/o/r",
            ),
            (
                "ssh://git@host.example:22/o/r",
                "ssh://git@host.example:22/o/r",
            ),
            ("git@github.com:o/r.git", "git@github.com:o/r.git"),
            ("/srv/repo.git", "/srv/repo.git"),
        ] {
            assert_eq!(strip_url_credentials(input), expected, "{input}");
        }
    }

    #[test]
    fn local_identity_reads_repo_config_and_linked_worktrees_share_it() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        init_repo(&repo);
        let git = GitEngine::new();
        let identity = git.local_identity(&repo).unwrap();
        assert_eq!(identity.name.as_deref(), Some("awman-test"));
        assert_eq!(identity.email.as_deref(), Some("test@awman.test"));

        let linked = tmp.path().join("linked");
        Command::new("git")
            .args(["worktree", "add", "-q", "-b", "linked"])
            .arg(&linked)
            .current_dir(&repo)
            .status()
            .unwrap();
        assert_eq!(git.local_identity(&linked).unwrap(), identity);
        assert_eq!(
            git.git_common_dir(&linked).unwrap().canonicalize().unwrap(),
            repo.join(".git").canonicalize().unwrap()
        );

        git.set_local_identity(&linked, "New Name", "new@example.com")
            .unwrap();
        let updated = git.local_identity(&repo).unwrap();
        assert_eq!(updated.name.as_deref(), Some("New Name"));
        assert_eq!(updated.email.as_deref(), Some("new@example.com"));
        assert!(git.branch_upstream(&repo, "linked").is_none());
        assert!(git.remote_url_redacted(&repo, "origin").unwrap().is_none());
    }

    #[test]
    fn set_local_identity_rejects_unsafe_values() {
        let tmp = tempfile::tempdir().unwrap();
        init_repo(tmp.path());
        let git = GitEngine::new();
        for (name, email) in [
            ("", "a@b"),
            ("Name\nInjected", "a@b"),
            ("-n", "a@b"),
            ("Name", "no-at-sign"),
            ("Name", "a@b\u{7}"),
        ] {
            assert!(
                git.set_local_identity(tmp.path(), name, email).is_err(),
                "{name:?} {email:?}"
            );
        }
        assert_eq!(
            git.local_identity(tmp.path()).unwrap().name.as_deref(),
            Some("awman-test")
        );
        let display = GitEngine::set_local_identity_command_display(Path::new("/r"), "A B", "a@b");
        assert_eq!(
            display,
            "git -C /r config --local user.name 'A B' && git -C /r config --local user.email a@b"
        );
    }
}

#[cfg(test)]
mod prerequisite_probe_tests {
    use super::*;
    use std::process::Command;

    fn git(repo: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(repo)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    fn bare_repo() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        git(tmp.path(), &["init", "-q", "-b", "main"]);
        tmp
    }

    #[test]
    fn local_identity_ignores_include_directives_and_global_config() {
        let tmp = bare_repo();
        let outside = tmp.path().join("outside.gitconfig");
        std::fs::write(
            &outside,
            "[user]\n\tname = Included\n\temail = inc@x.test\n",
        )
        .unwrap();
        let config = tmp.path().join(".git/config");
        let mut text = std::fs::read_to_string(&config).unwrap();
        text.push_str(&format!("[include]\n\tpath = {}\n", outside.display()));
        std::fs::write(&config, text).unwrap();
        let identity = GitEngine::new().local_identity(tmp.path()).unwrap();
        assert_eq!(identity, GitIdentity::default(), "{identity:?}");
        assert!(!identity.missing_keys().is_empty());
        // Setting the local identity makes it visible.
        GitEngine::new()
            .set_local_identity(tmp.path(), "Local", "local@x.test")
            .unwrap();
        let identity = GitEngine::new().local_identity(tmp.path()).unwrap();
        assert_eq!(identity.name.as_deref(), Some("Local"));
        assert!(identity.missing_keys().is_empty());
    }

    #[test]
    fn set_local_identity_writes_only_the_repo_config() {
        let tmp = bare_repo();
        GitEngine::new()
            .set_local_identity(tmp.path(), "Name With Spaces", "a@b.test")
            .unwrap();
        let config = std::fs::read_to_string(tmp.path().join(".git/config")).unwrap();
        assert!(config.contains("Name With Spaces"));
        assert!(config.contains("a@b.test"));
    }

    #[test]
    fn remote_url_redacted_strips_embedded_credentials() {
        let tmp = bare_repo();
        git(
            tmp.path(),
            &[
                "remote",
                "add",
                "origin",
                "https://x-access-token:ghp_SECRET@github.com/o/r.git",
            ],
        );
        let git_engine = GitEngine::new();
        let url = git_engine
            .remote_url_redacted(tmp.path(), "origin")
            .unwrap()
            .unwrap();
        assert_eq!(url, "https://github.com/o/r.git");
        assert!(!url.contains("ghp_SECRET"));
        assert!(git_engine
            .remote_url_redacted(tmp.path(), "nope")
            .unwrap()
            .is_none());
    }

    #[test]
    fn branch_upstream_is_none_until_one_is_configured() {
        let tmp = bare_repo();
        git(
            tmp.path(),
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@x",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "init",
            ],
        );
        let upstream = tempfile::tempdir().unwrap();
        git(upstream.path(), &["init", "-q", "--bare", "-b", "main"]);
        git(
            tmp.path(),
            &[
                "remote",
                "add",
                "origin",
                &upstream.path().display().to_string(),
            ],
        );
        let engine = GitEngine::new();
        assert!(engine.branch_upstream(tmp.path(), "main").is_none());
        git(tmp.path(), &["push", "-q", "-u", "origin", "main"]);
        assert_eq!(
            engine.branch_upstream(tmp.path(), "main").as_deref(),
            Some("origin/main")
        );
        // Option-like branch names are never passed to git.
        assert!(engine.branch_upstream(tmp.path(), "--all").is_none());
        assert!(engine.branch_upstream(tmp.path(), "").is_none());
    }

    #[test]
    fn detached_head_probe_reports_detached_and_attached() {
        let tmp = bare_repo();
        git(
            tmp.path(),
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@x",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "init",
            ],
        );
        let engine = GitEngine::new();
        assert!(!engine.is_detached_head(tmp.path()));
        assert_eq!(engine.current_branch(tmp.path()).as_deref(), Some("main"));
        git(tmp.path(), &["checkout", "-q", "--detach"]);
        assert!(engine.is_detached_head(tmp.path()));
        assert!(engine.current_branch(tmp.path()).is_none());
    }
}
