//! Git primitives for session worktrees (`chat` / `exec prompt` with
//! `--branch`, `--pr` or `--worktree`).
//!
//! Every method is synchronous, never prompts (`GIT_TERMINAL_PROMPT=0` comes
//! from [`host_command`]), and runs host git only through the hardened
//! [`host_command`] builder, so repository hooks, fsmonitor and content
//! drivers never execute. Network operations (`ls-remote`, `fetch`) block;
//! callers run them on a blocking worker.
//!
//! None of these helpers deletes, prunes, resets or force-updates a local
//! branch or a working tree. The decision flow that chooses between them
//! lives in the command layer (`command::commands::session_worktree`).

use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};
use std::process::{Output, Stdio};

use super::{canonical_or_given, host_command, path_from_git_bytes, GitEngine};
use crate::data::worktree_paths::WorktreePaths;
use crate::engine::error::EngineError;

/// One record of `git worktree list --porcelain -z`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeEntry {
    pub path: PathBuf,
    /// The checked-out branch with only a literal `refs/heads/` prefix
    /// stripped; `None` for a detached (or bare) entry.
    pub branch: Option<String>,
    pub bare: bool,
    /// Git reports the registration as stale (its directory is gone).
    pub prunable: bool,
}

/// Result of [`GitEngine::fetch_local_branch_ff`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchLocalOutcome {
    /// The local branch now matches the fetched ref (or already did).
    Updated,
    /// Git rejected the update as non-fast-forward; the local branch keeps
    /// its commits. `detail` is git's rejection message.
    KeptLocal { detail: String },
}

/// Result of [`GitEngine::fast_forward_worktree`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorktreeUpdate {
    /// HEAD already equals the fetched commit.
    Unchanged,
    /// Clean and strictly behind: fast-forwarded with `merge --ff-only`.
    FastForwarded,
    /// Uncommitted or untracked changes; nothing was touched.
    Dirty,
    /// HEAD contains the fetched commit plus local commits; nothing touched.
    Ahead,
    /// HEAD and the fetched commit have diverged; nothing touched.
    Diverged,
}

/// Run hardened host git in `cwd` with a null stdin and captured output.
fn run_git<S: AsRef<OsStr>>(cwd: &Path, args: &[S]) -> Result<Output, EngineError> {
    host_command::command(cwd)
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| EngineError::Git(format!("invoke `git {}`: {e}", describe(args))))
}

/// Run a hardened host git command that contacts `remote`: repository-scoped
/// transport programs are refused, SSH cannot prompt, and the call is bounded
/// by [`host_command::NETWORK_TIMEOUT`].
fn run_git_network<S: AsRef<OsStr>>(
    cwd: &Path,
    remote: &str,
    args: &[S],
) -> Result<Output, EngineError> {
    let mut command = host_command::transport_command(cwd, remote)?;
    command.args(args).current_dir(cwd);
    host_command::bounded_output(
        &mut command,
        host_command::NETWORK_TIMEOUT,
        &format!("`git {}`", describe(args)),
    )
}

fn describe<S: AsRef<OsStr>>(args: &[S]) -> String {
    args.iter()
        .map(|a| a.as_ref().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Git's stderr with credentials removed from any URL it mentions.
fn stderr_text(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr)
        .trim()
        .lines()
        .map(|line| {
            line.split(' ')
                .map(|word| {
                    if word.contains("://") {
                        let quote = |c: char| matches!(c, '\'' | '"');
                        let inner = word.trim_matches(quote);
                        word.replace(inner, &super::strip_url_credentials(inner))
                    } else {
                        word.to_string()
                    }
                })
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn failed<S: AsRef<OsStr>>(args: &[S], output: &Output) -> EngineError {
    EngineError::Git(format!(
        "`git {}` failed: {}",
        describe(args),
        stderr_text(output)
    ))
}

/// `git fetch --no-tags --no-recurse-submodules <remote> <refspec>` through
/// the transport-hardened, time-bounded runner.
fn fetch_ok(repo: &Path, remote: &str, refspec: &str) -> Result<(), EngineError> {
    let args = [
        "fetch",
        "--no-tags",
        "--no-recurse-submodules",
        remote,
        refspec,
    ];
    let output = run_git_network(repo, remote, &args)?;
    if !output.status.success() {
        return Err(failed(&args, &output));
    }
    Ok(())
}

/// Run git and require success; returns trimmed stdout.
fn git_ok<S: AsRef<OsStr>>(cwd: &Path, args: &[S]) -> Result<String, EngineError> {
    let output = run_git(cwd, args)?;
    if !output.status.success() {
        return Err(failed(args, &output));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Validate a remote name before it reaches argv: no option-like, empty,
/// whitespace, control or refspec-significant characters.
fn validate_remote_name(remote: &str) -> Result<(), EngineError> {
    let bad = remote.is_empty()
        || remote.starts_with('-')
        || remote.starts_with('.')
        || remote.contains("..")
        || remote.chars().any(|c| {
            c.is_control()
                || c.is_whitespace()
                || matches!(c, ':' | '^' | '~' | '?' | '*' | '[' | '\\')
        })
        || remote.contains("@{");
    if bad {
        return Err(EngineError::Git(format!("invalid remote name '{remote}'")));
    }
    Ok(())
}

/// The add helpers only accept an absolute target that does not exist at
/// all — not even as an empty directory or a dangling symlink.
fn require_vacant_path(path: &Path) -> Result<(), EngineError> {
    if !path.is_absolute() {
        return Err(EngineError::Git(format!(
            "worktree path must be absolute: {}",
            path.display()
        )));
    }
    match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(EngineError::io(path, e)),
        Ok(_) => {
            return Err(EngineError::Git(format!(
                "{} already exists; awman will not overwrite it",
                path.display()
            )))
        }
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| EngineError::io(parent, e))?;
    }
    Ok(())
}

/// Whether `value` is a full lowercase hex object id (SHA-1 or SHA-256).
fn is_full_oid(value: &str) -> bool {
    matches!(value.len(), 40 | 64)
        && value
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Parse NUL-delimited `git worktree list --porcelain -z` output.
pub(super) fn parse_worktree_entries(stdout: &[u8]) -> Vec<WorktreeEntry> {
    let mut entries = Vec::new();
    let mut current: Option<WorktreeEntry> = None;
    for field in stdout.split(|byte| *byte == 0) {
        if field.is_empty() {
            entries.extend(current.take());
        } else if let Some(bytes) = field.strip_prefix(b"worktree ") {
            entries.extend(current.take());
            current = Some(WorktreeEntry {
                path: path_from_git_bytes(bytes),
                branch: None,
                bare: false,
                prunable: false,
            });
        } else if let Some(entry) = current.as_mut() {
            if let Some(branch) = field.strip_prefix(b"branch ") {
                let branch = String::from_utf8_lossy(branch);
                entry.branch = Some(
                    branch
                        .strip_prefix("refs/heads/")
                        .unwrap_or(&branch)
                        .to_string(),
                );
            } else if field == b"bare" {
                entry.bare = true;
            } else if field == b"prunable" || field.starts_with(b"prunable ") {
                entry.prunable = true;
            }
        }
    }
    entries.extend(current);
    entries
}

impl GitEngine {
    /// Check that `branch` is a literal, valid branch name: not empty, no
    /// leading `-`, no control characters, no `@{…}` / `@` checkout
    /// shorthand, and `git check-ref-format --branch` must return it
    /// unchanged (so nothing is expanded to a different branch).
    pub fn validate_branch_name(&self, repo: &Path, branch: &str) -> Result<(), EngineError> {
        let invalid = |why: &str| {
            EngineError::Git(format!(
                "invalid branch name '{}': {why}",
                branch.escape_debug()
            ))
        };
        if branch.is_empty() {
            return Err(invalid("empty"));
        }
        if branch.starts_with('-') {
            return Err(invalid("must not start with '-'"));
        }
        if branch.chars().any(char::is_control) {
            return Err(invalid("contains control characters"));
        }
        if branch == "@" || branch.contains("@{") || branch == "HEAD" {
            return Err(invalid("checkout shorthand is not a branch name"));
        }
        let output = run_git(repo, &["check-ref-format", "--branch", branch])?;
        if !output.status.success() {
            return Err(invalid("rejected by `git check-ref-format --branch`"));
        }
        if String::from_utf8_lossy(&output.stdout).trim_end_matches('\n') != branch {
            return Err(invalid("git expands it to a different name"));
        }
        Ok(())
    }

    /// Require a non-bare working checkout whose `HEAD` is a real commit.
    /// Missing repository, bare repository and unborn `HEAD` are errors.
    pub fn ensure_committed_worktree(&self, repo: &Path) -> Result<(), EngineError> {
        let output = run_git(
            repo,
            &["rev-parse", "--is-bare-repository", "--is-inside-work-tree"],
        )?;
        if !output.status.success() {
            return Err(EngineError::Git(format!(
                "{} is not inside a git repository: {}",
                repo.display(),
                stderr_text(&output)
            )));
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut lines = stdout.lines();
        if lines.next() != Some("false") || lines.next() != Some("true") {
            return Err(EngineError::Git(format!(
                "{} is a bare repository or not a working checkout",
                repo.display()
            )));
        }
        let head = run_git(repo, &["rev-parse", "--verify", "--quiet", "HEAD^{commit}"])?;
        if !head.status.success() {
            return Err(EngineError::Git(format!(
                "{} has no commits yet; make an initial commit first",
                repo.display()
            )));
        }
        Ok(())
    }

    /// Every record of `git worktree list --porcelain -z`, in git's order
    /// (the main checkout first), including stale (`prunable`) entries.
    /// Paths keep their native bytes.
    pub fn list_worktree_entries(&self, repo: &Path) -> Result<Vec<WorktreeEntry>, EngineError> {
        let args = ["worktree", "list", "--porcelain", "-z"];
        let output = run_git(repo, &args)?;
        if !output.status.success() {
            return Err(failed(&args, &output));
        }
        Ok(parse_worktree_entries(&output.stdout))
    }

    /// `~/.awman/worktrees/<dirname>-<hash8>/<name>/` (or the legacy unhashed
    /// path when it holds a worktree of this repository). `name` must be one
    /// normal path component: empty, `.`, `..`, separators and prefixes are
    /// rejected. Creates nothing.
    pub fn worktree_path_by_name(
        &self,
        git_root: &Path,
        name: &str,
    ) -> Result<PathBuf, EngineError> {
        let invalid = || {
            EngineError::Git(format!(
                "invalid worktree name '{}': expected a single directory name",
                name.escape_debug()
            ))
        };
        if name.is_empty()
            || name == "."
            || name == ".."
            || name.contains(['/', '\\'])
            || name.chars().any(char::is_control)
        {
            return Err(invalid());
        }
        let mut components = Path::new(name).components();
        match (components.next(), components.next()) {
            (Some(Component::Normal(c)), None) if c == OsStr::new(name) => {}
            _ => return Err(invalid()),
        }
        let p = WorktreePaths::from_home().map_err(EngineError::Data)?;
        let root = canonical_or_given(git_root);
        Ok(self.pick_worktree_path(&root, p.named(&root, name), p.legacy_named(&root, name)))
    }

    /// Names of the configured remotes (`git remote`). A failure is an error,
    /// never "no remotes".
    pub fn remote_names(&self, repo: &Path) -> Result<Vec<String>, EngineError> {
        let stdout = git_ok(repo, &["remote"])?;
        Ok(stdout
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect())
    }

    /// Whether `refs/heads/<branch>` exists on `remote`:
    /// `git ls-remote --exit-code <remote> refs/heads/<branch>`. Exit 0 is
    /// "exists" (an exact full-ref line is also required), exit 2 is
    /// "absent", anything else — unreachable remote, authentication failure —
    /// is an error and must never be read as absence.
    pub fn remote_branch_exists(
        &self,
        repo: &Path,
        remote: &str,
        branch: &str,
    ) -> Result<bool, EngineError> {
        validate_remote_name(remote)?;
        self.validate_branch_name(repo, branch)?;
        let full = format!("refs/heads/{branch}");
        let args = ["ls-remote", "--exit-code", remote, full.as_str()];
        let output = run_git_network(repo, remote, &args)?;
        match output.status.code() {
            Some(0) => Ok(String::from_utf8_lossy(&output.stdout)
                .lines()
                .any(|line| line.split_once('\t').is_some_and(|(_, r)| r == full))),
            Some(2) => Ok(false),
            _ => Err(EngineError::Git(format!(
                "could not query remote '{remote}' for branch '{branch}': {}",
                stderr_text(&output)
            ))),
        }
    }

    /// Whether the local branch `refs/heads/<branch>` exists. Tags never
    /// satisfy it; a git failure is an error, not "absent".
    pub fn local_branch_exists(&self, repo: &Path, branch: &str) -> Result<bool, EngineError> {
        self.validate_branch_name(repo, branch)?;
        let full = format!("refs/heads/{branch}");
        let stdout = git_ok(
            repo,
            &["for-each-ref", "--format=%(refname)", full.as_str()],
        )?;
        Ok(stdout.lines().any(|line| line == full))
    }

    /// `git fetch --no-tags <remote> +refs/heads/<b>:refs/remotes/<remote>/<b>`
    /// and return the fetched commit OID. The force applies only to the
    /// remote-tracking ref, never to a local branch.
    pub fn fetch_branch_tracking(
        &self,
        repo: &Path,
        remote: &str,
        branch: &str,
    ) -> Result<String, EngineError> {
        validate_remote_name(remote)?;
        self.validate_branch_name(repo, branch)?;
        let tracking = format!("refs/remotes/{remote}/{branch}");
        self.check_full_ref(repo, &tracking)?;
        let refspec = format!("+refs/heads/{branch}:{tracking}");
        fetch_ok(repo, remote, &refspec)?;
        self.resolve_commit(repo, &tracking)
    }

    /// Fetch `refs/pull/<N>/head` from `remote` into the inert cache ref
    /// `refs/awman/pull-heads/<remote>/<N>` and return its commit OID. No
    /// local branch is touched and no upstream is configured.
    pub fn fetch_pull_request_head(
        &self,
        repo: &Path,
        remote: &str,
        number: u32,
    ) -> Result<String, EngineError> {
        validate_remote_name(remote)?;
        let cache = format!("refs/awman/pull-heads/{remote}/{number}");
        self.check_full_ref(repo, &cache)?;
        let refspec = format!("+refs/pull/{number}/head:{cache}");
        fetch_ok(repo, remote, &refspec)?;
        self.resolve_commit(repo, &cache)
    }

    /// Fast-forward-only fetch into a local branch that is not checked out:
    /// `git fetch --no-tags <remote> <source_ref>:refs/heads/<local_branch>`
    /// (no `+`). `source_ref` is `refs/heads/<b>` or `refs/pull/<N>/head`.
    /// Only git's specific non-fast-forward rejection becomes
    /// [`FetchLocalOutcome::KeptLocal`]; every other failure is an error.
    pub fn fetch_local_branch_ff(
        &self,
        repo: &Path,
        remote: &str,
        source_ref: &str,
        local_branch: &str,
    ) -> Result<FetchLocalOutcome, EngineError> {
        validate_remote_name(remote)?;
        self.validate_branch_name(repo, local_branch)?;
        let source_ok = match source_ref.strip_prefix("refs/heads/") {
            Some(b) => self.validate_branch_name(repo, b).is_ok(),
            None => source_ref
                .strip_prefix("refs/pull/")
                .and_then(|r| r.strip_suffix("/head"))
                .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())),
        };
        if !source_ok {
            return Err(EngineError::Git(format!(
                "invalid source ref '{}'",
                source_ref.escape_debug()
            )));
        }
        self.check_full_ref(repo, source_ref)?;
        let local = format!("refs/heads/{local_branch}");
        let before = self.resolve_commit(repo, &local)?;
        let refspec = format!("{source_ref}:{local}");
        let args = [
            "fetch",
            "--no-tags",
            "--no-recurse-submodules",
            remote,
            refspec.as_str(),
        ];
        let output = run_git_network(repo, remote, &args)?;
        if output.status.success() {
            return Ok(FetchLocalOutcome::Updated);
        }
        let stderr = stderr_text(&output);
        let rejected = stderr.lines().any(|l| {
            l.contains("[rejected]") && l.contains("(non-fast-forward)") && l.contains(local_branch)
        });
        let fatal = stderr.lines().any(|l| l.starts_with("fatal:"));
        if rejected && !fatal && self.resolve_commit(repo, &local)? == before {
            return Ok(FetchLocalOutcome::KeptLocal { detail: stderr });
        }
        Err(EngineError::Git(format!(
            "`git {}` failed: {stderr}",
            describe(&args)
        )))
    }

    /// Bring a reused worktree up to `fetched_commit` only when that is a
    /// pure fast-forward of a clean tree. Dirty, ahead and diverged trees are
    /// reported and left exactly as they are; nothing is reset or forced.
    pub fn fast_forward_worktree(
        &self,
        worktree: &Path,
        branch: &str,
        fetched_commit: &str,
    ) -> Result<WorktreeUpdate, EngineError> {
        self.validate_branch_name(worktree, branch)?;
        let head_ref = git_ok(worktree, &["symbolic-ref", "--quiet", "HEAD"]).map_err(|_| {
            EngineError::Git(format!(
                "{} is not on branch '{branch}'",
                worktree.display()
            ))
        })?;
        if head_ref != format!("refs/heads/{branch}") {
            return Err(EngineError::Git(format!(
                "{} has {head_ref} checked out, not branch '{branch}'",
                worktree.display()
            )));
        }
        let status = git_ok(
            worktree,
            &["status", "--porcelain", "--untracked-files=normal"],
        )?;
        if !status.is_empty() {
            return Ok(WorktreeUpdate::Dirty);
        }
        let target = self.resolve_commit(worktree, fetched_commit)?;
        let head = self.resolve_commit(worktree, "HEAD")?;
        if head == target {
            return Ok(WorktreeUpdate::Unchanged);
        }
        if self.is_ancestor(worktree, &head, &target)? {
            git_ok(
                worktree,
                &["merge", "--ff-only", "--no-edit", target.as_str()],
            )?;
            return Ok(WorktreeUpdate::FastForwarded);
        }
        if self.is_ancestor(worktree, &target, &head)? {
            Ok(WorktreeUpdate::Ahead)
        } else {
            Ok(WorktreeUpdate::Diverged)
        }
    }

    /// Create `<path>` on a new local `branch` that starts at, and tracks,
    /// `refs/remotes/<remote>/<branch>`. Fails if the local branch exists;
    /// never falls back to `HEAD`.
    ///
    /// The worktree is added with `--no-track` from the full tracking ref and
    /// the upstream is then written as `branch.<b>.remote/merge`, which is
    /// what `--track` records, without depending on the remote's configured
    /// fetch refspec mapping that ref.
    pub fn create_worktree_tracking(
        &self,
        repo: &Path,
        path: &Path,
        branch: &str,
        remote: &str,
    ) -> Result<(), EngineError> {
        validate_remote_name(remote)?;
        if self.local_branch_exists(repo, branch)? {
            return Err(EngineError::Git(format!(
                "local branch '{branch}' already exists"
            )));
        }
        let tracking = format!("refs/remotes/{remote}/{branch}");
        let start = self.resolve_commit(repo, &tracking)?;
        require_vacant_path(path)?;
        self.worktree_add_new_branch(repo, path, branch, &start)?;
        git_ok(
            repo,
            &["config", &format!("branch.{branch}.remote"), remote],
        )?;
        git_ok(
            repo,
            &[
                "config",
                &format!("branch.{branch}.merge"),
                &format!("refs/heads/{branch}"),
            ],
        )?;
        self.ensure_upstream_mapped(repo, branch, remote, &tracking)
    }

    /// `branch.<b>.merge` only works as an upstream when a fetch refspec of
    /// the remote maps `refs/heads/<b>` to a remote-tracking ref. Single-branch
    /// clones and narrowed refspecs do not, so add exactly that mapping (the
    /// same thing `git remote set-branches --add` does), keeping every
    /// existing one.
    fn ensure_upstream_mapped(
        &self,
        repo: &Path,
        branch: &str,
        remote: &str,
        tracking: &str,
    ) -> Result<(), EngineError> {
        let local = format!("refs/heads/{branch}");
        let upstream = || -> Result<String, EngineError> {
            git_ok(
                repo,
                &["for-each-ref", "--format=%(upstream)", local.as_str()],
            )
        };
        if !upstream()?.is_empty() {
            return Ok(());
        }
        git_ok(
            repo,
            &[
                "config",
                "--add",
                &format!("remote.{remote}.fetch"),
                &format!("+{local}:{tracking}"),
            ],
        )?;
        let now = upstream()?;
        if now != tracking {
            return Err(EngineError::Git(format!(
                "could not make '{tracking}' the upstream of '{branch}' (git reports '{now}')"
            )));
        }
        Ok(())
    }

    /// Attach the existing local `branch` at `path`; never creates a branch.
    /// The new worktree's `HEAD` must be the symbolic ref
    /// `refs/heads/<branch>` (a same-named tag must not be checked out).
    pub fn add_worktree_existing(
        &self,
        repo: &Path,
        path: &Path,
        branch: &str,
    ) -> Result<(), EngineError> {
        if !self.local_branch_exists(repo, branch)? {
            return Err(EngineError::Git(format!(
                "local branch '{branch}' does not exist"
            )));
        }
        require_vacant_path(path)?;
        let mut args: Vec<&OsStr> = vec![OsStr::new("worktree"), OsStr::new("add")];
        args.push(path.as_os_str());
        args.push(OsStr::new(branch));
        git_ok(repo, &args)?;
        let head = git_ok(path, &["symbolic-ref", "--quiet", "HEAD"]).unwrap_or_default();
        if head != format!("refs/heads/{branch}") {
            return Err(EngineError::Git(format!(
                "worktree at {} did not check out branch '{branch}' (HEAD is {}); it was left in place for inspection",
                path.display(),
                if head.is_empty() { "detached" } else { &head }
            )));
        }
        Ok(())
    }

    /// `git worktree add --no-track -b <branch> <path> <commit>` from a
    /// locally verified commit OID. Used for the only branch-creating cases:
    /// a new branch from the invoking checkout's captured `HEAD`, and a
    /// `pr/<N>` branch from a fetched pull-request head. No upstream is set.
    pub fn create_worktree_from_commit(
        &self,
        repo: &Path,
        path: &Path,
        branch: &str,
        commit: &str,
    ) -> Result<(), EngineError> {
        if !is_full_oid(commit) {
            return Err(EngineError::Git(format!(
                "'{commit}' is not a full commit id"
            )));
        }
        if self.resolve_commit(repo, commit)? != commit {
            return Err(EngineError::Git(format!("'{commit}' is not a commit")));
        }
        if self.local_branch_exists(repo, branch)? {
            return Err(EngineError::Git(format!(
                "local branch '{branch}' already exists"
            )));
        }
        require_vacant_path(path)?;
        self.worktree_add_new_branch(repo, path, branch, commit)
    }

    fn worktree_add_new_branch(
        &self,
        repo: &Path,
        path: &Path,
        branch: &str,
        commit: &str,
    ) -> Result<(), EngineError> {
        let args: Vec<&OsStr> = vec![
            OsStr::new("worktree"),
            OsStr::new("add"),
            OsStr::new("--no-track"),
            OsStr::new("-b"),
            OsStr::new(branch),
            path.as_os_str(),
            OsStr::new(commit),
        ];
        git_ok(repo, &args).map(|_| ())
    }

    /// `git rev-parse --verify --quiet <rev>^{commit}` → full OID.
    fn resolve_commit(&self, repo: &Path, rev: &str) -> Result<String, EngineError> {
        if rev.starts_with('-') {
            return Err(EngineError::Git(format!("invalid revision '{rev}'")));
        }
        let spec = format!("{rev}^{{commit}}");
        let output = run_git(repo, &["rev-parse", "--verify", "--quiet", spec.as_str()])?;
        let oid = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !output.status.success() || !is_full_oid(&oid) {
            return Err(EngineError::Git(format!("'{rev}' does not name a commit")));
        }
        Ok(oid)
    }

    /// `git merge-base --is-ancestor`: exit 0 yes, 1 no, else an error.
    fn is_ancestor(
        &self,
        repo: &Path,
        ancestor: &str,
        descendant: &str,
    ) -> Result<bool, EngineError> {
        let args = ["merge-base", "--is-ancestor", ancestor, descendant];
        let output = run_git(repo, &args)?;
        match output.status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => Err(failed(&args, &output)),
        }
    }

    /// `git check-ref-format <full_ref>` must accept the full ref.
    fn check_full_ref(&self, repo: &Path, full_ref: &str) -> Result<(), EngineError> {
        let output = run_git(repo, &["check-ref-format", full_ref])?;
        if !output.status.success() {
            return Err(EngineError::Git(format!(
                "invalid ref '{}'",
                full_ref.escape_debug()
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_branch_detached_bare_and_prunable_records() {
        let raw = b"worktree /main\0HEAD aaaa\0branch refs/heads/main\0\0\
worktree /wt/a b\0HEAD bbbb\0detached\0\0\
worktree /wt/stale\0HEAD cccc\0branch refs/heads/x/y\0prunable gitdir file points to non-existent location\0\0";
        let entries = parse_worktree_entries(raw);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].path, PathBuf::from("/main"));
        assert_eq!(entries[0].branch.as_deref(), Some("main"));
        assert_eq!(entries[1].path, PathBuf::from("/wt/a b"));
        assert_eq!(entries[1].branch, None);
        assert!(!entries[1].prunable);
        assert_eq!(entries[2].branch.as_deref(), Some("x/y"));
        assert!(entries[2].prunable);

        let bare = parse_worktree_entries(b"worktree /repo.git\0bare\0\0");
        assert!(bare[0].bare);
    }

    #[test]
    fn remote_names_must_not_look_like_options() {
        assert!(validate_remote_name("origin").is_ok());
        assert!(validate_remote_name("up-stream_2").is_ok());
        for bad in [
            "",
            "-x",
            "--upload-pack=x",
            "a b",
            "a:b",
            "a..b",
            ".x",
            "a\nb",
        ] {
            assert!(validate_remote_name(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn full_oid_shape() {
        assert!(is_full_oid(&"a".repeat(40)));
        assert!(is_full_oid(&"0".repeat(64)));
        assert!(!is_full_oid("abc"));
        assert!(!is_full_oid(&"A".repeat(40)));
    }
}
