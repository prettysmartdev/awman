//! Real-git coverage for the session-worktree engine primitives.
//!
//! These tests use local bare repositories only. Keep `real_git` in each test
//! name so `make test-fast` can skip the real git dependency.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use awman::engine::git::{FetchLocalOutcome, GitEngine};

struct Fixture {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    remote: PathBuf,
    engine: GitEngine,
}

impl Fixture {
    fn new() -> Self {
        let tmp = match std::env::var_os("AWMAN_TEST_TMPROOT") {
            Some(root) => {
                std::fs::create_dir_all(&root).unwrap();
                tempfile::Builder::new()
                    .prefix("session-worktree.")
                    .tempdir_in(root)
                    .unwrap()
            }
            None => tempfile::tempdir().unwrap(),
        };
        let root = tmp.path().join("checkout");
        let remote = tmp.path().join("origin.git");
        std::fs::create_dir(&root).unwrap();
        git(&root, &["init", "--initial-branch=main"]);
        configure(&root);
        commit_file(&root, "base.txt", "base\n", "base");
        git(tmp.path(), &["init", "--bare", remote.to_str().unwrap()]);
        git(
            &root,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        git(&root, &["push", "-u", "origin", "main"]);
        Self {
            _tmp: tmp,
            root,
            remote,
            engine: GitEngine::new(),
        }
    }

    fn branch(&self, name: &str, file: &str, text: &str) -> String {
        git(&self.root, &["checkout", "-b", name]);
        commit_file(&self.root, file, text, name);
        git(&self.root, &["push", "origin", name]);
        git(&self.root, &["checkout", "main"]);
        let oid = rev(&self.root, &format!("refs/heads/{name}"));
        git(&self.root, &["branch", "-D", name]);
        oid
    }

    fn target(&self, dir: &str) -> PathBuf {
        self._tmp.path().join(dir)
    }
}

fn git(cwd: &Path, args: &[&str]) -> Output {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn configure(repo: &Path) {
    git(repo, &["config", "user.name", "test"]);
    git(repo, &["config", "user.email", "test@example.com"]);
    git(repo, &["config", "commit.gpgsign", "false"]);
}

fn commit_file(repo: &Path, name: &str, contents: &str, message: &str) {
    std::fs::write(repo.join(name), contents).unwrap();
    git(repo, &["add", name]);
    git(repo, &["commit", "-m", message]);
}

fn rev(repo: &Path, expression: &str) -> String {
    String::from_utf8(git(repo, &["rev-parse", expression]).stdout)
        .unwrap()
        .trim()
        .to_owned()
}

#[test]
fn real_git_branch_remote_yes_local_no_creates_tracking_branch_at_remote_oid() {
    crate::real_git_skip!();
    let f = Fixture::new();
    let expected = f.branch("remote-only", "remote.txt", "remote\n");
    assert!(f
        .engine
        .remote_branch_exists(&f.root, "origin", "remote-only")
        .unwrap());
    assert!(!f
        .engine
        .local_branch_exists(&f.root, "remote-only")
        .unwrap());
    let fetched = f
        .engine
        .fetch_branch_tracking(&f.root, "origin", "remote-only")
        .unwrap();
    assert_eq!(fetched, expected);
    let path = f.target("tracking");
    f.engine
        .create_worktree_tracking(&f.root, &path, "remote-only", "origin")
        .unwrap();
    assert!(f
        .engine
        .local_branch_exists(&f.root, "remote-only")
        .unwrap());
    assert_eq!(rev(&path, "HEAD"), expected);
    assert_eq!(
        String::from_utf8(git(&path, &["rev-parse", "--abbrev-ref", "@{u}"]).stdout)
            .unwrap()
            .trim(),
        "origin/remote-only"
    );
}

#[test]
fn real_git_branch_remote_yes_local_yes_fast_forwards_local_branch() {
    crate::real_git_skip!();
    let f = Fixture::new();
    let expected = f.branch("both", "remote.txt", "remote\n");
    git(&f.root, &["branch", "both", "main"]);
    let before = rev(&f.root, "refs/heads/both");
    assert_ne!(before, expected);
    assert!(f
        .engine
        .remote_branch_exists(&f.root, "origin", "both")
        .unwrap());
    assert!(f.engine.local_branch_exists(&f.root, "both").unwrap());
    assert_eq!(
        f.engine
            .fetch_local_branch_ff(&f.root, "origin", "refs/heads/both", "both")
            .unwrap(),
        FetchLocalOutcome::Updated
    );
    assert_eq!(rev(&f.root, "refs/heads/both"), expected);
}

#[test]
fn real_git_branch_remote_no_local_yes_attaches_existing_branch_without_changing_it() {
    crate::real_git_skip!();
    let f = Fixture::new();
    git(&f.root, &["checkout", "-b", "local-only"]);
    commit_file(&f.root, "local.txt", "local\n", "local branch");
    let expected = rev(&f.root, "HEAD");
    git(&f.root, &["checkout", "main"]);
    assert!(!f
        .engine
        .remote_branch_exists(&f.root, "origin", "local-only")
        .unwrap());
    let path = f.target("existing");
    f.engine
        .add_worktree_existing(&f.root, &path, "local-only")
        .unwrap();
    assert!(f.engine.local_branch_exists(&f.root, "local-only").unwrap());
    assert_eq!(rev(&path, "HEAD"), expected);
    assert_eq!(rev(&f.root, "refs/heads/local-only"), expected);
}

#[test]
fn real_git_branch_remote_no_local_no_creates_from_supplied_invoking_commit() {
    crate::real_git_skip!();
    let f = Fixture::new();
    let base = rev(&f.root, "HEAD");
    assert!(!f
        .engine
        .remote_branch_exists(&f.root, "origin", "new-local")
        .unwrap());
    assert!(!f.engine.local_branch_exists(&f.root, "new-local").unwrap());
    let path = f.target("new");
    f.engine
        .create_worktree_from_commit(&f.root, &path, "new-local", &base)
        .unwrap();
    assert!(f.engine.local_branch_exists(&f.root, "new-local").unwrap());
    assert_eq!(rev(&path, "HEAD"), base);
    assert_eq!(
        rev(&f.root, "HEAD"),
        base,
        "creating a linked tree must not move invoking HEAD"
    );
    let upstream = Command::new("git")
        .args(["config", "--get", "branch.new-local.remote"])
        .current_dir(&path)
        .output()
        .unwrap();
    assert!(
        !upstream.status.success(),
        "new branch unexpectedly has an upstream"
    );
}

#[test]
fn real_git_remote_only_branch_is_tracked_instead_of_created_from_head() {
    crate::real_git_skip!();
    let f = Fixture::new();
    let remote_oid = f.branch("only-on-origin", "remote.txt", "different commit\n");
    let invoking_head = rev(&f.root, "HEAD");
    assert_ne!(remote_oid, invoking_head);
    assert!(!f
        .engine
        .local_branch_exists(&f.root, "only-on-origin")
        .unwrap());
    let fetched = f
        .engine
        .fetch_branch_tracking(&f.root, "origin", "only-on-origin")
        .unwrap();
    let path = f.target("remote-regression");
    f.engine
        .create_worktree_tracking(&f.root, &path, "only-on-origin", "origin")
        .unwrap();
    assert_eq!(rev(&path, "HEAD"), remote_oid);
    assert_ne!(rev(&path, "HEAD"), invoking_head);
    assert_eq!(rev(&f.root, "refs/remotes/origin/only-on-origin"), fetched);
}

#[test]
fn real_git_ls_remote_failure_does_not_create_branch_or_worktree() {
    crate::real_git_skip!();
    let f = Fixture::new();
    git(
        &f.root,
        &[
            "remote",
            "set-url",
            "origin",
            "/definitely/missing/session-worktree-remote",
        ],
    );
    let path = f.target("failed-fetch");
    assert!(f
        .engine
        .remote_branch_exists(&f.root, "origin", "absent")
        .is_err());
    assert!(!f.engine.local_branch_exists(&f.root, "absent").unwrap());
    assert!(!path.exists());
}

#[test]
fn real_git_remote_branch_matching_is_exact_not_suffix_matching() {
    crate::real_git_skip!();
    let f = Fixture::new();
    f.branch("foo/bar", "nested.txt", "nested\n");
    assert!(!f
        .engine
        .remote_branch_exists(&f.root, "origin", "bar")
        .unwrap());
}

#[test]
fn real_git_same_named_tag_is_not_a_local_branch() {
    crate::real_git_skip!();
    let f = Fixture::new();
    git(&f.root, &["tag", "tag-only"]);
    assert!(!f.engine.local_branch_exists(&f.root, "tag-only").unwrap());
    let path = f.target("tag-collision");
    assert!(f
        .engine
        .add_worktree_existing(&f.root, &path, "tag-only")
        .is_err());
    assert!(!path.exists());
}

#[test]
fn real_git_diverged_local_branch_keeps_its_commits() {
    crate::real_git_skip!();
    let f = Fixture::new();
    f.branch("diverged", "remote.txt", "remote\n");
    git(&f.root, &["checkout", "-b", "diverged", "main"]);
    commit_file(&f.root, "local.txt", "local\n", "local-only commit");
    let local_before = rev(&f.root, "refs/heads/diverged");
    git(&f.root, &["checkout", "main"]);
    let outcome = f
        .engine
        .fetch_local_branch_ff(&f.root, "origin", "refs/heads/diverged", "diverged")
        .unwrap();
    assert!(
        matches!(outcome, FetchLocalOutcome::KeptLocal { .. }),
        "got {outcome:?}"
    );
    assert_eq!(rev(&f.root, "refs/heads/diverged"), local_before);
    assert_eq!(
        String::from_utf8(git(&f.root, &["show", "refs/heads/diverged:local.txt"]).stdout).unwrap(),
        "local\n"
    );
}

#[test]
fn real_git_hooks_do_not_run_on_host_during_fetch_or_worktree_add() {
    use std::os::unix::fs::PermissionsExt;
    crate::real_git_skip!();
    let f = Fixture::new();
    let expected = f.branch("hook-target", "hook.txt", "hook test\n");
    let marker = f._tmp.path().join("host-hook-ran");
    let hook_dir = f.root.join("hooks");
    std::fs::create_dir_all(&hook_dir).unwrap();
    for hook_name in ["reference-transaction", "post-checkout"] {
        let hook = hook_dir.join(hook_name);
        std::fs::write(
            &hook,
            format!("#!/bin/sh\necho ran >> '{}'\n", marker.display()),
        )
        .unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    git(
        &f.root,
        &["config", "core.hooksPath", hook_dir.to_str().unwrap()],
    );
    let path = f.target("hook-worktree");
    f.engine
        .fetch_branch_tracking(&f.root, "origin", "hook-target")
        .unwrap();
    f.engine
        .create_worktree_tracking(&f.root, &path, "hook-target", "origin")
        .unwrap();
    assert_eq!(rev(&path, "HEAD"), expected);
    assert!(!marker.exists(), "repository hook executed on host");
}

#[test]
fn real_git_fetches_pull_ref_from_bare_origin_without_using_head_branch_name() {
    crate::real_git_skip!();
    let f = Fixture::new();
    let commit = f.branch("pr-source", "pr.txt", "pull request\n");
    git(&f.remote, &["update-ref", "refs/pull/17/head", &commit]);
    let fetched = f
        .engine
        .fetch_pull_request_head(&f.root, "origin", 17)
        .unwrap();
    assert_eq!(fetched, commit);
    assert_eq!(rev(&f.root, "refs/awman/pull-heads/origin/17"), commit);
    assert!(!f.engine.local_branch_exists(&f.root, "pr-source").unwrap());
}
