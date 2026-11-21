//! Git transport conformance: awman's [`GitEngine`] against one remote
//! reached over one transport (a local path, smart HTTP, SSH).
//!
//! Contracts (`L1-GIT-*`): clone checks out the remote's default branch at
//! its head; clone of a named branch checks out that branch; pull follows
//! the remote to a newer head; push publishes `HEAD` to a new branch and
//! returns the pushed sha; a remote that refuses the client (missing or
//! wrong credentials) fails fast with a typed git error instead of
//! prompting; an unreachable remote fails without leaving a checkout.

use std::path::{Path, PathBuf};

use super::{ensure, Report};
use crate::data::config::env::EnvSnapshot;
use crate::data::message::RecordingMessageSink;
use crate::engine::error::EngineError;
use crate::engine::git::GitEngine;

/// A branch the world's remote must have besides the default branch.
pub const SIDE_BRANCH: &str = "conformance-side";
/// The branch the push check creates on the remote.
pub const PUSHED_BRANCH: &str = "conformance-pushed";

/// What the world reports about its remote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteFacts {
    /// URL awman clones from (transport-specific).
    pub url: String,
    /// The remote's default branch and its head commit.
    pub default_branch: String,
    pub default_head: String,
    /// Head commit of [`SIDE_BRANCH`].
    pub side_head: String,
}

/// The world a git transport suite runs in.
pub trait GitRemoteWorld {
    /// Name of the transport, e.g. `smart HTTP (fake server)`.
    fn subject(&self) -> String;
    /// Environment for the engine (git identity, credentials, PATH).
    fn env(&self) -> EnvSnapshot;
    /// A fresh directory the suite may clone into.
    fn scratch_dir(&self, name: &str) -> PathBuf;
    /// Create the remote with a default branch and [`SIDE_BRANCH`].
    fn given_remote(&self) -> RemoteFacts;
    /// Add a commit to the remote's default branch; return the new head.
    fn advance_remote(&self) -> String;
    /// Head of `branch` on the remote, if the branch exists.
    fn remote_head(&self, branch: &str) -> Option<String>;
    /// A URL for the same remote that the server refuses (missing or wrong
    /// credentials); `None` when the transport has no access control.
    fn refused_url(&self) -> Option<String>;
    /// A URL on this transport where nothing answers.
    fn unreachable_url(&self) -> String;
    /// Remove whatever the world created.
    fn cleanup(&self);
}

/// Run every git transport check against `world`.
pub fn git_transport(world: &dyn GitRemoteWorld) -> Report {
    let mut report = Report::new(world.subject());
    let git = GitEngine::with_env(world.env());
    let remote = world.given_remote();

    let main_clone = world.scratch_dir("clone-default");
    report.check("clone_checks_out_the_default_branch_at_its_head", {
        check_clone(&git, &remote, None, &main_clone)
    });
    report.check("clone_of_a_named_branch_checks_out_that_branch", {
        check_clone(
            &git,
            &remote,
            Some(SIDE_BRANCH),
            &world.scratch_dir("clone-side"),
        )
    });
    report.check("pull_follows_the_remote_to_its_new_head", {
        check_pull(world, &git, &main_clone)
    });
    report.check("push_publishes_head_to_a_new_branch", {
        check_push(world, &git, &main_clone)
    });
    match world.refused_url() {
        Some(url) => report.check("a_refusing_remote_fails_fast_without_prompting", {
            check_failure(&git, &url, &world.scratch_dir("clone-refused"))
        }),
        None => report.skip(
            "a_refusing_remote_fails_fast_without_prompting",
            "transport has no access control",
        ),
    }
    report.check("an_unreachable_remote_fails_and_leaves_no_checkout", {
        check_failure(
            &git,
            &world.unreachable_url(),
            &world.scratch_dir("clone-unreachable"),
        )
    });
    world.cleanup();
    report
}

fn check_clone(
    git: &GitEngine,
    remote: &RemoteFacts,
    branch: Option<&str>,
    dest: &Path,
) -> Result<(), String> {
    git.clone_repo(&remote.url, branch, dest)
        .map_err(|e| format!("clone failed: {e}"))?;
    let (want_branch, want_head) = match branch {
        Some(b) => (b.to_string(), remote.side_head.clone()),
        None => (remote.default_branch.clone(), remote.default_head.clone()),
    };
    let head = git.head_sha(dest).map_err(|e| format!("head: {e}"))?;
    ensure(head == want_head, || {
        format!("head {head}, remote says {want_head}")
    })?;
    let current = git.current_branch(dest);
    ensure(current.as_deref() == Some(want_branch.as_str()), || {
        format!("checked out {current:?}, expected {want_branch}")
    })
}

fn check_pull(world: &dyn GitRemoteWorld, git: &GitEngine, clone: &Path) -> Result<(), String> {
    let new_head = world.advance_remote();
    git.pull_latest(clone)
        .map_err(|e| format!("pull failed: {e}"))?;
    let head = git.head_sha(clone).map_err(|e| format!("head: {e}"))?;
    ensure(head == new_head, || {
        format!("head {head} after pull, remote is {new_head}")
    })
}

fn check_push(world: &dyn GitRemoteWorld, git: &GitEngine, clone: &Path) -> Result<(), String> {
    std::fs::write(clone.join("conformance-pushed.txt"), "pushed\n")
        .map_err(|e| format!("write: {e}"))?;
    git.commit_all(clone, "conformance push")
        .map_err(|e| format!("commit: {e}"))?;
    let mut sink = RecordingMessageSink::new();
    let pushed = git
        .push_upstream_logged(clone, "origin", PUSHED_BRANCH, &mut sink)
        .map_err(|e| format!("push failed: {e}"))?;
    let local = git.head_sha(clone).map_err(|e| format!("head: {e}"))?;
    ensure(pushed == local, || {
        format!("push returned {pushed}, local head {local}")
    })?;
    let remote = world.remote_head(PUSHED_BRANCH);
    ensure(remote.as_deref() == Some(local.as_str()), || {
        format!("remote {PUSHED_BRANCH} at {remote:?}, expected {local}")
    })
}

fn check_failure(git: &GitEngine, url: &str, dest: &Path) -> Result<(), String> {
    match git.clone_repo(url, None, dest) {
        Ok(()) => Err(format!("clone of {url} succeeded")),
        Err(EngineError::Git(_)) => ensure(!dest.join(".git").exists(), || {
            format!("a failed clone left {}", dest.display())
        }),
        Err(other) => Err(format!("expected a git error, got {other}")),
    }
}
