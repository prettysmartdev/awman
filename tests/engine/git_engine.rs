//! GitEngine unit and integration tests.
//!
//! Pure path-computation tests run without any git installation.
//! Tests touching the real git binary include "real_git" in their name.

use std::path::Path;

use awman::data::worktree_paths::{
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
    assert_eq!(worktree_branch_name(42), "awman/work-item-0042");
}

#[test]
fn worktree_branch_name_9999_no_truncation() {
    assert_eq!(worktree_branch_name(9999), "awman/work-item-9999");
}

#[test]
fn worktree_branch_name_for_workflow_hyphen() {
    assert_eq!(
        worktree_branch_name_for_workflow("my-wf"),
        "awman/workflow-my-wf"
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
    use awman::data::session::GitRootResolver;
    use awman::engine::git::GitEngine;

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
    use awman::engine::git::GitEngine;

    let tmp = tempfile::tempdir().unwrap();
    let git_root = tmp.path();
    init_repo(git_root);

    let engine = GitEngine::new();
    let branch = engine.branch_name_for_work_item(42);
    assert_eq!(branch, "awman/work-item-0042");

    let worktree_path: PathBuf = tmp.path().parent().unwrap().join("awman-test-wt-0042");
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
        subject, "Implement awman/work-item-0042",
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

// ─── Identity probe (F-39) ───────────────────────────────────────────────────
//
// `identity_configured` runs `git config` *at a path*, so a repo-local
// identity is honoured. The pre-F-39 probe in `exec_workflow.rs` ran
// `git config` with no working directory and only ever saw the global value,
// which meant a correctly-configured repo still got the "git identity not
// set" warning before every `commit_changes` teardown.
//
// These tests isolate the global level with `HOME`/`XDG_CONFIG_HOME` pointed
// at a scratch directory and `GIT_CONFIG_*` overrides cleared, so the host's
// real `~/.gitconfig` cannot decide the outcome.

/// A bare repo with no identity of its own, plus a scratch `HOME` whose
/// global git config is whatever `global` says (`None` = no global config).
fn repo_with_isolated_global(
    global: Option<(&str, &str)>,
) -> (tempfile::TempDir, PathBuf, Vec<(&'static str, String)>) {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&repo).unwrap();

    let run = |args: &[&str]| {
        let status = Command::new("git")
            .args(args)
            .current_dir(&repo)
            .env("HOME", &home)
            .status()
            .expect("git invocation");
        assert!(status.success(), "git {args:?} failed");
    };
    run(&["init", "--initial-branch=main"]);

    if let Some((name, email)) = global {
        std::fs::write(
            home.join(".gitconfig"),
            format!("[user]\n\tname = {name}\n\temail = {email}\n"),
        )
        .unwrap();
    }

    // `GitEngine` shells out without overriding the environment, so the
    // scratch HOME has to be installed in this process for the probe to see
    // it. Returned so the caller can restore.
    let saved = vec![
        ("HOME", std::env::var("HOME").unwrap_or_default()),
        (
            "XDG_CONFIG_HOME",
            std::env::var("XDG_CONFIG_HOME").unwrap_or_default(),
        ),
        (
            "GIT_CONFIG_GLOBAL",
            std::env::var("GIT_CONFIG_GLOBAL").unwrap_or_default(),
        ),
        (
            "GIT_CONFIG_SYSTEM",
            std::env::var("GIT_CONFIG_SYSTEM").unwrap_or_default(),
        ),
    ];
    std::env::set_var("HOME", &home);
    std::env::set_var("XDG_CONFIG_HOME", home.join(".config"));
    match global {
        Some(_) => std::env::set_var("GIT_CONFIG_GLOBAL", home.join(".gitconfig")),
        // `/dev/null` is git's documented way to say "no global config".
        None => std::env::set_var("GIT_CONFIG_GLOBAL", "/dev/null"),
    }
    std::env::set_var("GIT_CONFIG_SYSTEM", "/dev/null");

    (tmp, repo, saved)
}

fn restore_env(saved: Vec<(&'static str, String)>) {
    for (key, value) in saved {
        if value.is_empty() {
            std::env::remove_var(key);
        } else {
            std::env::set_var(key, value);
        }
    }
}

/// Serialises the identity tests: they mutate process-wide `HOME`.
static IDENTITY_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn real_git_identity_configured_reads_the_global_identity() {
    use crate::helpers::git_available;
    if !git_available() {
        eprintln!("SKIP: git not available — run on a host with git");
        return;
    }
    use awman::engine::git::GitEngine;

    let _lock = IDENTITY_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (_tmp, repo, saved) = repo_with_isolated_global(Some(("Global User", "global@x.test")));

    let identity = GitEngine::new().identity_configured(&repo).unwrap();
    restore_env(saved);

    assert_eq!(identity.name.as_deref(), Some("Global User"));
    assert_eq!(identity.email.as_deref(), Some("global@x.test"));
    assert!(identity.is_complete());
    assert!(identity.missing_keys().is_empty());
}

#[test]
fn real_git_identity_configured_prefers_the_repo_local_override() {
    use crate::helpers::git_available;
    if !git_available() {
        eprintln!("SKIP: git not available — run on a host with git");
        return;
    }
    use awman::engine::git::GitEngine;

    let _lock = IDENTITY_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (_tmp, repo, saved) = repo_with_isolated_global(Some(("Global User", "global@x.test")));

    for args in [
        ["config", "user.name", "Repo User"],
        ["config", "user.email", "repo@x.test"],
    ] {
        let status = Command::new("git")
            .args(args)
            .current_dir(&repo)
            .status()
            .expect("git invocation");
        assert!(status.success());
    }

    let identity = GitEngine::new().identity_configured(&repo).unwrap();
    restore_env(saved);

    // This is the behaviour change: the old probe ran without a working
    // directory and would have reported the global values here.
    assert_eq!(identity.name.as_deref(), Some("Repo User"));
    assert_eq!(identity.email.as_deref(), Some("repo@x.test"));
}

#[test]
fn real_git_identity_configured_reports_both_keys_missing() {
    use crate::helpers::git_available;
    if !git_available() {
        eprintln!("SKIP: git not available — run on a host with git");
        return;
    }
    use awman::engine::git::GitEngine;

    let _lock = IDENTITY_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (_tmp, repo, saved) = repo_with_isolated_global(None);

    let identity = GitEngine::new().identity_configured(&repo).unwrap();
    restore_env(saved);

    assert_eq!(identity.name, None);
    assert_eq!(identity.email, None);
    assert!(!identity.is_complete());
    assert_eq!(identity.missing_keys(), vec!["user.name", "user.email"]);
}

// ─── remote_effective_url (WI 0114 F-29) ─────────────────────────────────────

/// Real-git: `remote_effective_url` is `git remote get-url`, so it applies the
/// user's `url.*.insteadOf` rewrites, while `remote_url` reports the stored
/// value. The `context(repo)` directory slug is derived from the former —
/// Layer 0's `ContextDirResolver` used to shell `git remote get-url` itself
/// (WI 0114 F-29), and a repository with an `insteadOf` rewrite must keep the
/// context directory it already has.
#[test]
fn real_git_remote_effective_url_applies_insteadof_where_remote_url_does_not() {
    use crate::helpers::git_available;
    if !git_available() {
        eprintln!("SKIP: git not available — run on a host with git");
        return;
    }
    use awman::engine::git::GitEngine;

    let tmp = tempfile::tempdir().unwrap();
    init_repo(tmp.path());
    let run = |args: &[&str]| {
        assert!(Command::new("git")
            .args(args)
            .current_dir(tmp.path())
            .status()
            .expect("git invocation")
            .success());
    };
    run(&["remote", "add", "origin", "gh:org/repo.git"]);
    run(&["config", "url.https://github.com/.insteadOf", "gh:"]);

    let engine = GitEngine::new();
    assert_eq!(
        engine.remote_url(tmp.path(), "origin").unwrap(),
        "gh:org/repo.git",
        "remote_url reports the stored value"
    );
    assert_eq!(
        engine.remote_effective_url(tmp.path(), "origin"),
        Some("https://github.com/org/repo.git".to_string()),
        "remote_effective_url applies the insteadOf rewrite"
    );
}

/// Real-git: no remote at all is `None`, which is what makes the context
/// directory fall back to `_local/{dirname}`.
#[test]
fn real_git_remote_effective_url_is_none_without_a_remote() {
    use crate::helpers::git_available;
    if !git_available() {
        eprintln!("SKIP: git not available — run on a host with git");
        return;
    }
    use awman::engine::git::GitEngine;

    let tmp = tempfile::tempdir().unwrap();
    init_repo(tmp.path());
    assert_eq!(
        GitEngine::new().remote_effective_url(tmp.path(), "origin"),
        None
    );
}
