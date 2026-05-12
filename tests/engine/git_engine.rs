//! GitEngine unit and integration tests.
//!
//! Pure path-computation tests run without any git installation.
//! Tests touching the real git binary include "real_git" in their name.

use std::path::Path;

use amux::data::worktree_paths::{
    worktree_branch_name, worktree_branch_name_for_workflow, WorktreePaths,
};

// ─── Worktree path computation (no git needed) ───────────────────────────────

#[test]
fn worktree_path_for_work_item_42() {
    let wt = WorktreePaths::with_home("/home/user");
    let path = wt.for_work_item(Path::new("/projects/myrepo"), 42);
    assert!(
        path.ends_with("worktrees/myrepo/0042"),
        "unexpected path: {path:?}"
    );
}

#[test]
fn worktree_path_for_work_item_1() {
    let wt = WorktreePaths::with_home("/home/user");
    let path = wt.for_work_item(Path::new("/projects/myrepo"), 1);
    assert!(
        path.ends_with("0001"),
        "expected zero-padded '0001', got {path:?}"
    );
}

#[test]
fn worktree_path_for_workflow_uses_wf_prefix() {
    let wt = WorktreePaths::with_home("/home/user");
    let path = wt.for_workflow(Path::new("/projects/myrepo"), "build-docs");
    assert!(
        path.ends_with("worktrees/myrepo/wf-build-docs"),
        "got {path:?}"
    );
}

#[test]
fn worktree_branch_name_42_is_zero_padded() {
    assert_eq!(worktree_branch_name(42), "amux/work-item-0042");
}

#[test]
fn worktree_branch_name_9999_no_truncation() {
    assert_eq!(worktree_branch_name(9999), "amux/work-item-9999");
}

#[test]
fn worktree_branch_name_for_workflow_hyphen() {
    assert_eq!(
        worktree_branch_name_for_workflow("my-wf"),
        "amux/workflow-my-wf"
    );
}

#[test]
fn worktree_path_home_embedded_in_path() {
    let wt = WorktreePaths::with_home("/my-home");
    let path = wt.for_work_item(Path::new("/r/repo"), 1);
    assert!(
        path.starts_with("/my-home"),
        "path should start with home: {path:?}"
    );
}

// ─── Real git tests (skipped by make test-fast) ─────────────────────────────

use std::path::PathBuf;
use std::process::Command;

/// Initialise a fresh git repository with one initial commit at `dir`.
/// Used by every `real_git_*` test below as the starting point.
fn init_repo(dir: &std::path::Path) {
    let run = |args: &[&str]| {
        let status = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .status()
            .expect("git invocation");
        assert!(status.success(), "git {args:?} failed");
    };
    run(&["init", "--initial-branch=main"]);
    run(&["config", "user.email", "test@example.com"]);
    run(&["config", "user.name", "test"]);
    run(&["config", "commit.gpgsign", "false"]);
    std::fs::write(dir.join("README.md"), "initial\n").unwrap();
    run(&["add", "README.md"]);
    run(&["commit", "-m", "initial"]);
}

/// Real-git: GitEngine resolves the root of a freshly initialised repo.
#[test]
fn real_git_engine_resolves_root_of_fresh_repo() {
    use crate::helpers::git_available;
    if !git_available() {
        eprintln!("SKIP: git not available — run on a host with git");
        return;
    }
    use amux::data::session::GitRootResolver;
    use amux::engine::git::GitEngine;

    let tmp = tempfile::tempdir().unwrap();
    init_repo(tmp.path());

    let engine = GitEngine::new();
    let resolved = engine
        .resolve(tmp.path())
        .expect("resolution must succeed inside a git repo");
    let canonical_input = std::fs::canonicalize(tmp.path()).unwrap();
    let canonical_resolved = std::fs::canonicalize(&resolved).unwrap();
    assert_eq!(
        canonical_resolved, canonical_input,
        "resolved root mismatch"
    );
}

/// Real-git: full prepare → run → finalize → cleanup cycle for a worktree.
/// Exercises `create_worktree`, `merge_branch` (squash + commit), and
/// `remove_worktree` against a real repo, then asserts that the squashed
/// commit message matches the contract documented in §2e item 43.
#[test]
fn real_git_worktree_create_merge_remove_cycle() {
    use crate::helpers::git_available;
    if !git_available() {
        eprintln!("SKIP: git not available — run on a host with git");
        return;
    }
    use amux::engine::git::GitEngine;

    let tmp = tempfile::tempdir().unwrap();
    let git_root = tmp.path();
    init_repo(git_root);

    let engine = GitEngine::new();
    let branch = engine.branch_name_for_work_item(42);
    assert_eq!(branch, "amux/work-item-0042");

    let worktree_path: PathBuf = tmp.path().parent().unwrap().join("amux-test-wt-0042");
    // Clean up any leftover from a previous run.
    let _ = std::fs::remove_dir_all(&worktree_path);

    engine
        .create_worktree(git_root, &worktree_path, &branch)
        .expect("create_worktree must succeed against a fresh repo");
    assert!(worktree_path.exists(), "worktree dir must exist on disk");

    // Make a change inside the worktree and commit it on the work-item branch.
    std::fs::write(worktree_path.join("change.txt"), "hello\n").unwrap();
    let run_in = |dir: &std::path::Path, args: &[&str]| {
        let status = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .status()
            .expect("git invocation");
        assert!(status.success(), "git {args:?} failed in {dir:?}");
    };
    run_in(&worktree_path, &["add", "change.txt"]);
    run_in(&worktree_path, &["commit", "-m", "branch work"]);

    // Squash-merge the branch back into main.
    engine
        .merge_branch(git_root, &branch, &worktree_path)
        .expect("merge_branch must succeed for a non-conflicting change");

    // Confirm the commit on main has the expected `Implement <branch>` message.
    let log = Command::new("git")
        .args(["log", "-1", "--pretty=%s", "main"])
        .current_dir(git_root)
        .output()
        .expect("git log");
    let subject = String::from_utf8_lossy(&log.stdout).trim().to_string();
    assert_eq!(
        subject, "Implement amux/work-item-0042",
        "merge_branch must commit with `Implement <branch>` subject"
    );

    // Tear down the worktree.
    engine
        .remove_worktree(git_root, &worktree_path)
        .expect("remove_worktree must succeed");
    assert!(
        !worktree_path.exists(),
        "worktree dir must be gone after remove_worktree"
    );
}
