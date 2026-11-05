//! Decision flow for retained chat and exec-prompt worktrees.
//!
//! All git/GitHub operations run in one blocking worker. This module never
//! removes a worktree or branch, including when launch or refresh fails.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::command::dispatch::catalogue::SessionWorktreeTarget;
use crate::command::error::CommandError;
use crate::data::fs::remote_slug::RemoteSlug;
use crate::data::message::{MessageLevel, UserMessage, UserMessageSink};
use crate::data::session::Session;
use crate::data::worktree_paths::{
    branch_worktree_dir_name, pull_request_worktree_dir_name, WorktreePaths,
};
use crate::engine::git::pull_request::{
    match_pull_request_remote, parse_pull_request_input, PullRequestClient, PullRequestState,
};
use crate::engine::git::{FetchLocalOutcome, GitEngine, WorktreeEntry, WorktreeUpdate};

#[derive(Debug, Clone)]
pub(crate) struct SessionWorktree {
    pub path: PathBuf,
    pub branch: Option<String>,
    /// The first worktree-list entry, which owns paths and image identity.
    pub invoker_root: PathBuf,
}

impl SessionWorktree {
    /// Revalidate immediately before building launch options. Ownership checks
    /// invoke host git, so this bridge also stays off the async runtime.
    pub(crate) async fn git_directory_overlay(
        &self,
        git: Arc<GitEngine>,
    ) -> Result<Option<crate::engine::overlay::DirectorySpec>, CommandError> {
        let root = self.invoker_root.clone();
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            super::worktree_overlay::worktree_git_overlay(&git, &root, &path).map(|overlay| {
                overlay.map(|overlay| crate::engine::overlay::DirectorySpec {
                    host: overlay.host_path.display().to_string(),
                    container: overlay.container_path.display().to_string(),
                    permission: overlay.permission,
                })
            })
        })
        .await
        .map_err(|e| CommandError::Other(format!("worktree mount validation failed: {e}")))?
        .map_err(CommandError::from)
    }
}

pub(crate) async fn resolve_session_worktree(
    target: &SessionWorktreeTarget,
    invoker: &Session,
    git: Arc<GitEngine>,
    sink: &mut dyn UserMessageSink,
) -> Result<SessionWorktree, CommandError> {
    resolve_session_worktree_with_client(target, invoker, git, PullRequestClient::default(), sink)
        .await
}

pub(crate) async fn resolve_session_worktree_with_client(
    target: &SessionWorktreeTarget,
    invoker: &Session,
    git: Arc<GitEngine>,
    client: PullRequestClient,
    sink: &mut dyn UserMessageSink,
) -> Result<SessionWorktree, CommandError> {
    let target = target.clone();
    let invoker = invoker.clone();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let worker = tokio::task::spawn_blocking(move || {
        let mut sink = BlockingMessageSink(tx);
        resolve_blocking(&target, &invoker, &git, &client, &mut sink)
    });
    while let Some(message) = rx.recv().await {
        sink.write_message(message);
    }
    worker
        .await
        .map_err(|e| CommandError::Other(format!("worktree resolution failed: {e}")))?
}

struct BlockingMessageSink(tokio::sync::mpsc::UnboundedSender<UserMessage>);

impl UserMessageSink for BlockingMessageSink {
    fn write_message(&mut self, message: UserMessage) {
        let _ = self.0.send(message);
    }
    fn replay_queued(&mut self) {}
}

fn message(sink: &mut dyn UserMessageSink, level: MessageLevel, text: impl Into<String>) {
    sink.write_message(UserMessage {
        level,
        text: text.into(),
    });
}

pub(crate) fn report_kept_session_worktree(
    worktree: &SessionWorktree,
    sink: &mut dyn UserMessageSink,
) {
    message(
        sink,
        MessageLevel::Info,
        format!(
            "Worktree kept at {} ({}).",
            worktree.path.display(),
            worktree.branch.as_deref().unwrap_or("detached HEAD")
        ),
    );
}

fn resolve_blocking(
    target: &SessionWorktreeTarget,
    invoker: &Session,
    git: &GitEngine,
    client: &PullRequestClient,
    sink: &mut dyn UserMessageSink,
) -> Result<SessionWorktree, CommandError> {
    if !invoker.is_git_repo() {
        return Err(CommandError::Other(
            "session worktrees require a Git repository".into(),
        ));
    }
    git.ensure_committed_worktree(invoker.git_root())?;
    let paths = git.list_worktrees(invoker.git_root())?;
    let root = paths
        .first()
        .ok_or_else(|| CommandError::Other("Git listed no main checkout".into()))?;
    let root = canonicalize(root)?;
    git.ensure_committed_worktree(&root)?;
    let mut resolver = Resolver {
        invoker,
        root: &root,
        git,
        sink,
    };
    match target {
        SessionWorktreeTarget::Existing(value) => resolver.resolve_existing(value),
        SessionWorktreeTarget::Branch(branch) => {
            git.validate_branch_name(&root, branch)?;
            let remotes = git.remote_names(&root)?;
            let remote = remotes.iter().find(|name| name.as_str() == "origin");
            if remote.is_none() {
                resolver.warn("No origin remote; treating the branch as absent from the remote.");
            }
            resolver.resolve_branch(
                branch,
                remote.map(String::as_str),
                &branch_worktree_dir_name(branch),
                true,
            )
        }
        SessionWorktreeTarget::PullRequest(value) => resolver.resolve_pull_request(value, client),
    }
}

/// How a reused worktree is brought up to date.
enum Refresh<'r> {
    /// No remote copy of the branch: nothing to fetch.
    Nothing,
    /// `refs/heads/<branch>` on this remote.
    Branch(&'r str),
    /// `refs/pull/<N>/head` on this remote.
    PullRequest(&'r str, u32),
}

/// Shared repository context keeps the branch and PR paths on the same policy.
struct Resolver<'a> {
    invoker: &'a Session,
    root: &'a Path,
    git: &'a GitEngine,
    sink: &'a mut dyn UserMessageSink,
}

impl Resolver<'_> {
    fn warn(&mut self, text: impl Into<String>) {
        message(self.sink, MessageLevel::Warning, text);
    }

    fn info(&mut self, text: impl Into<String>) {
        message(self.sink, MessageLevel::Info, text);
    }

    fn result(&self, path: PathBuf, branch: Option<String>) -> SessionWorktree {
        SessionWorktree {
            path,
            branch,
            invoker_root: self.root.to_path_buf(),
        }
    }

    /// Ownership precedes all target-state reads and mounts.
    fn owned_path(&self, path: &Path) -> Result<PathBuf, CommandError> {
        let path = canonicalize(path)?;
        self.git
            .validate_worktree_owner(self.root, &path)
            .map_err(|e| {
                CommandError::Other(format!(
                    "{} is not a linked worktree of this repository: {e}",
                    path.display()
                ))
            })?;
        // Read-only rev-parse checks also catch a broken/unborn target HEAD;
        // do not use status here, since --worktree must not refresh its index.
        self.git.ensure_committed_worktree(&path)?;
        Ok(path)
    }

    fn checked_out(&self, branch: &str) -> Result<Option<SessionWorktree>, CommandError> {
        let entries = self.git.list_worktree_entries(self.root)?;
        let Some(entry) = entries
            .iter()
            .find(|entry| entry.branch.as_deref() == Some(branch))
        else {
            return Ok(None);
        };
        if entry.path == self.root
            || entry
                .path
                .canonicalize()
                .is_ok_and(|path| path == self.root)
        {
            return Err(CommandError::Other(format!(
                "Branch '{branch}' is checked out in the main checkout at {}. Switch that checkout to another branch or run without the worktree selector.",
                self.root.display()
            )));
        }
        if entry.prunable || !entry.path.exists() {
            return Err(CommandError::Other(format!(
                "Branch '{branch}' has a stale worktree registration at {}. Inspect it and run `git worktree prune` manually before retrying.",
                entry.path.display()
            )));
        }
        let path = self.owned_path(&entry.path)?;
        Ok(Some(self.result(path, Some(branch.to_string()))))
    }

    fn announce_reuse(&mut self, worktree: &SessionWorktree) {
        self.info(format!(
            "Reusing worktree {} ({}).",
            worktree.path.display(),
            worktree.branch.as_deref().unwrap_or("detached HEAD")
        ));
    }

    fn refresh(&mut self, worktree: &SessionWorktree, commit: &str) -> Result<(), CommandError> {
        let branch = worktree
            .branch
            .as_deref()
            .expect("branch flow has a branch");
        match self
            .git
            .fast_forward_worktree(&worktree.path, branch, commit)?
        {
            WorktreeUpdate::FastForwarded => {
                self.info(format!("Fast-forwarded '{branch}' to the fetched commit."))
            }
            WorktreeUpdate::Unchanged => {}
            WorktreeUpdate::Dirty => self.warn(format!(
                "Worktree {} has uncommitted changes; leaving it unchanged.",
                worktree.path.display()
            )),
            WorktreeUpdate::Ahead => self.warn(format!(
                "Local branch '{branch}' is ahead of the remote; keeping local commits."
            )),
            WorktreeUpdate::Diverged => self.warn(format!(
                "Local branch '{branch}' has diverged from the remote; keeping local commits."
            )),
        }
        Ok(())
    }

    /// Fetch whatever the reused worktree's branch follows and apply the
    /// clean/strictly-behind fast-forward rule.
    fn refresh_reused(
        &mut self,
        worktree: &SessionWorktree,
        how: &Refresh<'_>,
    ) -> Result<(), CommandError> {
        let branch = worktree
            .branch
            .as_deref()
            .expect("branch flow has a branch");
        let commit = match how {
            Refresh::Nothing => return Ok(()),
            Refresh::Branch(remote) => self.git.fetch_branch_tracking(self.root, remote, branch)?,
            Refresh::PullRequest(remote, number) => self
                .git
                .fetch_pull_request_head(self.root, remote, *number)?,
        };
        self.refresh(worktree, &commit)
    }

    /// A concurrent launch can win between our first lookup and our own path
    /// check or fetch. Look the branch up once more: a valid linked worktree
    /// is reused; otherwise the original error stands. Nothing is overwritten
    /// and no second add is attempted.
    fn reuse_after_collision(
        &mut self,
        branch: &str,
        error: CommandError,
        how: &Refresh<'_>,
    ) -> Result<SessionWorktree, CommandError> {
        match self.checked_out(branch)? {
            Some(worktree) => {
                self.announce_reuse(&worktree);
                self.refresh_reused(&worktree, how)?;
                Ok(worktree)
            }
            None => Err(error),
        }
    }

    fn warn_kept_local(&mut self, outcome: FetchLocalOutcome, branch: &str) {
        if let FetchLocalOutcome::KeptLocal { detail } = outcome {
            self.warn(format!(
                "Keeping local commits on '{branch}'; the remote cannot fast-forward it: {detail}"
            ));
        }
    }

    fn available_path(&self, name: &str) -> Result<PathBuf, CommandError> {
        let path = self.git.worktree_path_by_name(self.root, name)?;
        match std::fs::symlink_metadata(&path) {
            Ok(_) => {
                let entries = self.git.list_worktree_entries(self.root)?;
                let branch = entries
                    .iter()
                    .find(|entry| entry.path == path)
                    .and_then(|entry| entry.branch.as_deref());
                Err(CommandError::Other(format!(
                    "Worktree path {} is already occupied{}; it will not be overwritten.",
                    path.display(),
                    branch
                        .map(|b| format!(" by branch '{b}'"))
                        .unwrap_or_default()
                )))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(path),
            Err(e) => Err(CommandError::Other(format!(
                "Cannot inspect worktree path {}: {e}",
                path.display()
            ))),
        }
    }

    /// Only a failed add is retried, with one fresh registration lookup. No
    /// cleanup or additional add is attempted: a concurrent launch may own it.
    fn finish_add(
        &mut self,
        path: &Path,
        branch: &str,
        added: Result<(), crate::engine::error::EngineError>,
    ) -> Result<(SessionWorktree, bool), CommandError> {
        if let Err(error) = added {
            if let Some(worktree) = self.checked_out(branch)? {
                // A failed checkout/config write can leave our own partially
                // prepared registration behind. Only collision errors prove
                // the other-launcher case; do not hide an unrelated failure.
                if concurrent_add_failure(&error) {
                    self.announce_reuse(&worktree);
                    return Ok((worktree, true));
                }
            }
            return Err(CommandError::Other(format!(
                "Could not add worktree at {}: {error}. If Git reports a stale registration, inspect it and run `git worktree prune` manually.", path.display()
            )));
        }
        let path = self.owned_path(path)?;
        let entries = self.git.list_worktree_entries(self.root)?;
        if !entries.iter().any(|entry| {
            entry.branch.as_deref() == Some(branch)
                && entry.path.canonicalize().is_ok_and(|p| p == path)
        }) {
            return Err(CommandError::Other(format!(
                "Worktree {} is not registered on branch '{branch}'",
                path.display()
            )));
        }
        Ok((self.result(path, Some(branch.to_string())), false))
    }

    fn resolve_branch(
        &mut self,
        branch: &str,
        remote: Option<&str>,
        directory_name: &str,
        allow_head_creation: bool,
    ) -> Result<SessionWorktree, CommandError> {
        self.git.validate_branch_name(self.root, branch)?;
        let base = self.git.head_sha(self.invoker.git_root())?;
        let on_remote = match remote {
            Some(remote) => self.git.remote_branch_exists(self.root, remote, branch)?,
            None => false,
        };
        if !on_remote && !allow_head_creation {
            return Err(CommandError::Other(format!("PR head branch '{branch}' disappeared from the remote; retry to use the PR head ref.")));
        }
        let how = match remote {
            Some(remote) if on_remote => Refresh::Branch(remote),
            _ => Refresh::Nothing,
        };
        if let Some(worktree) = self.checked_out(branch)? {
            self.announce_reuse(&worktree);
            self.refresh_reused(&worktree, &how)?;
            return Ok(worktree);
        }
        #[cfg(test)]
        race_hooks::run(self.root, RaceStage::AfterLookup);
        let path = match self.available_path(directory_name) {
            Ok(path) => path,
            Err(error) => return self.reuse_after_collision(branch, error, &how),
        };
        #[cfg(test)]
        race_hooks::run(self.root, RaceStage::AfterPathCheck);
        let local = self.git.local_branch_exists(self.root, branch)?;
        let added = match (on_remote, local) {
            (true, false) => {
                let remote = remote.expect("remote exists");
                self.git.fetch_branch_tracking(self.root, remote, branch)?;
                self.info(format!(
                    "Checking out '{branch}' tracking {remote}/{branch}."
                ));
                self.git
                    .create_worktree_tracking(self.root, &path, branch, remote)
            }
            (true, true) => {
                let outcome = match self.git.fetch_local_branch_ff(
                    self.root,
                    remote.expect("remote exists"),
                    &format!("refs/heads/{branch}"),
                    branch,
                ) {
                    Ok(outcome) => outcome,
                    Err(error) if concurrent_add_failure(&error) => {
                        return self.reuse_after_collision(branch, error.into(), &how)
                    }
                    Err(error) => return Err(error.into()),
                };
                self.warn_kept_local(outcome, branch);
                self.git.add_worktree_existing(self.root, &path, branch)
            }
            (false, true) => self.git.add_worktree_existing(self.root, &path, branch),
            (false, false) => {
                self.info(format!("Creating branch '{branch}' from invoking HEAD {base}. Uncommitted changes in the invoking checkout are not carried into the worktree."));
                self.git
                    .create_worktree_from_commit(self.root, &path, branch, &base)
            }
        };
        let (worktree, reused) = self.finish_add(&path, branch, added)?;
        // A concurrent launcher may have checked out a different local tip.
        // Apply the same clean/strictly-behind rule as any other reuse.
        if reused {
            self.refresh_reused(&worktree, &how)?;
        }
        Ok(worktree)
    }

    fn resolve_pull_request(
        &mut self,
        value: &str,
        client: &PullRequestClient,
    ) -> Result<SessionWorktree, CommandError> {
        let origin = if value.trim().bytes().all(|c| c.is_ascii_digit()) && !value.trim().is_empty()
        {
            if self
                .git
                .remote_names(self.root)?
                .iter()
                .any(|r| r == "origin")
            {
                RemoteSlug::parse(&self.git.remote_url(self.root, "origin")?)
            } else {
                None
            }
        } else {
            None
        };
        let pr = parse_pull_request_input(value, origin.as_ref())?;
        let remote = match_pull_request_remote(self.git, self.root, &pr)?;
        let info = client.fetch(&pr, self.invoker.env().github_token(), self.sink)?;
        // Even a fork's unused head name must be validated before branching.
        self.git
            .validate_branch_name(self.root, &info.head_ref_name)?;
        match info.state {
            PullRequestState::Open => {}
            PullRequestState::Closed => self.warn(format!(
                "PR #{} is closed; continuing with its code.",
                pr.number
            )),
            PullRequestState::Merged => self.warn(format!(
                "PR #{} is merged; continuing with its code.",
                pr.number
            )),
        }
        let name = pull_request_worktree_dir_name(pr.number);
        if !info.is_cross_repository
            && self
                .git
                .remote_branch_exists(self.root, &remote, &info.head_ref_name)?
        {
            return self.resolve_branch(&info.head_ref_name, Some(&remote), &name, false);
        }
        let branch = format!("pr/{}", pr.number);
        self.git.validate_branch_name(self.root, &branch)?;
        if info.is_cross_repository {
            self.warn(format!("Fork PR #{} uses local branch '{branch}' without tracking the fork branch. Pushing to origin will not update the PR; configure the fork's remote explicitly.", pr.number));
        } else {
            self.warn(format!("PR #{} head branch was deleted; using its pull ref on '{branch}' with no new upstream.", pr.number));
        }
        let how = Refresh::PullRequest(&remote, pr.number);
        if let Some(worktree) = self.checked_out(&branch)? {
            self.announce_reuse(&worktree);
            self.refresh_reused(&worktree, &how)?;
            return Ok(worktree);
        }
        let path = match self.available_path(&name) {
            Ok(path) => path,
            Err(error) => return self.reuse_after_collision(&branch, error, &how),
        };
        let added = if self.git.local_branch_exists(self.root, &branch)? {
            let outcome = match self.git.fetch_local_branch_ff(
                self.root,
                &remote,
                &format!("refs/pull/{}/head", pr.number),
                &branch,
            ) {
                Ok(outcome) => outcome,
                Err(error) if concurrent_add_failure(&error) => {
                    return self.reuse_after_collision(&branch, error.into(), &how)
                }
                Err(error) => return Err(error.into()),
            };
            self.warn_kept_local(outcome, &branch);
            self.git.add_worktree_existing(self.root, &path, &branch)
        } else {
            let commit = self
                .git
                .fetch_pull_request_head(self.root, &remote, pr.number)?;
            self.git
                .create_worktree_from_commit(self.root, &path, &branch, &commit)
        };
        let (worktree, reused) = self.finish_add(&path, &branch, added)?;
        if reused {
            self.refresh_reused(&worktree, &how)?;
        }
        Ok(worktree)
    }

    fn resolve_existing(&mut self, value: &str) -> Result<SessionWorktree, CommandError> {
        let path = match parse_existing_worktree_input(value)? {
            ExistingWorktreeInput::Path(path) => {
                if path.is_absolute() {
                    path
                } else {
                    self.invoker.working_dir().join(path)
                }
            }
            ExistingWorktreeInput::Name(name) => {
                let mut found = None;
                for name in worktree_name_candidates(&name) {
                    let path = self.git.worktree_path_by_name(self.root, &name)?;
                    match std::fs::symlink_metadata(&path) {
                        Ok(_) => {
                            found = Some(path);
                            break;
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                        Err(e) => {
                            return Err(CommandError::Other(format!(
                                "Cannot inspect {}: {e}",
                                path.display()
                            )))
                        }
                    }
                }
                found.ok_or_else(|| self.missing_name(&name))?
            }
        };
        let path = self.owned_path(&path)?;
        let entries = self.git.list_worktree_entries(self.root)?;
        let entry = entries
            .iter()
            .find(|entry| !entry.prunable && entry.path.canonicalize().is_ok_and(|p| p == path))
            .ok_or_else(|| {
                CommandError::Other(format!(
                    "{} is not a registered linked worktree of this repository",
                    path.display()
                ))
            })?;
        if entry.branch.is_none() {
            self.warn(format!(
                "Worktree {} has a detached HEAD; launching without a branch.",
                path.display()
            ));
        }
        Ok(self.result(path, entry.branch.clone()))
    }

    fn missing_name(&self, name: &str) -> CommandError {
        let detail = (|| -> Result<String, CommandError> {
            let paths = WorktreePaths::from_home()?;
            let directories = [
                paths.named(self.root, name),
                paths.legacy_named(self.root, name),
            ]
            .into_iter()
            .filter_map(|path| path.parent().map(Path::to_path_buf))
            .collect::<Vec<_>>();
            let available = self
                .git
                .list_worktree_entries(self.root)?
                .into_iter()
                .filter(|entry: &WorktreeEntry| {
                    !entry.prunable
                        && entry
                            .path
                            .parent()
                            .is_some_and(|p| directories.iter().any(|dir| p == dir))
                })
                .filter(|entry| {
                    self.git
                        .validate_worktree_owner(self.root, &entry.path)
                        .is_ok()
                })
                .filter_map(|entry| {
                    entry
                        .path
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                })
                .collect::<Vec<_>>();
            Ok(format!(
                "Searched {}. Available worktrees: {}.",
                directories
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", "),
                if available.is_empty() {
                    "none".into()
                } else {
                    available.join(", ")
                }
            ))
        })();
        CommandError::Other(format!(
            "No worktree named '{name}'. {}",
            detail.unwrap_or_else(|e| e.to_string())
        ))
    }
}

/// Errors that a concurrent launch of the same branch produces: our add or
/// fetch lost a race to a worktree that now exists.
pub(crate) fn concurrent_add_failure(error: &crate::engine::error::EngineError) -> bool {
    let crate::engine::error::EngineError::Git(detail) = error else {
        return false;
    };
    detail.contains("already exists")
        || detail.contains("already checked out")
        || detail.contains("already used by worktree")
        // `git fetch` into a branch that another launch just checked out.
        || detail.contains("checked out at")
}

/// Test seam: run a callback (standing in for a concurrent launch) at a fixed
/// point of the branch flow, for one repository only.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum RaceStage {
    /// After the first checked-out lookup found nothing.
    AfterLookup,
    /// After the default path was found vacant.
    AfterPathCheck,
}

#[cfg(test)]
pub(crate) mod race_hooks {
    use super::RaceStage;
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    type Hook = Box<dyn FnOnce() + Send>;
    static HOOKS: Mutex<Option<HashMap<(PathBuf, RaceStage), Hook>>> = Mutex::new(None);

    /// Register `hook` to run once when the resolver for `root` reaches `stage`.
    pub(crate) fn set(root: &Path, stage: RaceStage, hook: impl FnOnce() + Send + 'static) {
        let mut hooks = HOOKS.lock().unwrap();
        hooks
            .get_or_insert_with(HashMap::new)
            .insert((root.to_path_buf(), stage), Box::new(hook));
    }

    pub(super) fn run(root: &Path, stage: RaceStage) {
        let hook = HOOKS
            .lock()
            .unwrap()
            .as_mut()
            .and_then(|hooks| hooks.remove(&(root.to_path_buf(), stage)));
        if let Some(hook) = hook {
            hook();
        }
    }
}

fn canonicalize(path: &Path) -> Result<PathBuf, CommandError> {
    path.canonicalize().map_err(|e| {
        CommandError::Other(format!(
            "Cannot resolve worktree path {}: {e}",
            path.display()
        ))
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ExistingWorktreeInput {
    Name(String),
    Path(PathBuf),
}

pub(crate) fn parse_existing_worktree_input(
    value: &str,
) -> Result<ExistingWorktreeInput, CommandError> {
    if value.trim().is_empty() || value == ".." || value.chars().any(char::is_control) {
        return Err(CommandError::Other(format!(
            "Invalid worktree name or path '{value}'"
        )));
    }
    if value.contains('\\') && std::path::MAIN_SEPARATOR != '\\' {
        return Err(CommandError::Other(
            "Backslash worktree paths are unsupported on this platform; use '/' as the separator"
                .into(),
        ));
    }
    if Path::new(value).is_absolute() || value.contains(['/', '\\']) || value.starts_with('.') {
        Ok(ExistingWorktreeInput::Path(PathBuf::from(value)))
    } else {
        Ok(ExistingWorktreeInput::Name(value.to_string()))
    }
}

pub(crate) fn worktree_name_candidates(name: &str) -> Vec<String> {
    let mut candidates = vec![name.to_string()];
    if !name.is_empty() && name.bytes().all(|c| c.is_ascii_digit()) && name.len() < 4 {
        candidates.push(format!("{name:0>4}"));
    }
    candidates
}

/// Relative overlay host paths (from the invoking checkout's config or the
/// command line) mean "relative to where the command ran". The re-rooted
/// session's working directory is the worktree, whose contents a branch or PR
/// controls, so anchor them to the invoking directory before re-rooting.
pub(crate) fn anchor_relative_overlays(
    directories: &mut [crate::engine::overlay::DirectorySpec],
    invoker_dir: &Path,
) -> Result<(), CommandError> {
    use crate::data::fs::overlay_paths::OverlayPathResolver;
    for spec in directories {
        let absolute = OverlayPathResolver::make_absolute_with_cwd(&spec.host, invoker_dir);
        spec.host = absolute
            .to_str()
            .ok_or_else(|| {
                CommandError::Other(format!(
                    "overlay host path {} is not valid UTF-8",
                    absolute.display()
                ))
            })?
            .to_string();
    }
    Ok(())
}
