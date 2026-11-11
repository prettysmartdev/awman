//! `engine::git` — the host `GitEngine` against real `git` on temporary
//! repositories inside the sandbox (component tier guarantees `LocalGit`).
//!
//! Every engine is built from the sandbox snapshot
//! (`GitEngine::with_env(support::env_snapshot(&sandbox))`), so the host's
//! own `~/.gitconfig`, credential helpers and hooks can never decide an
//! outcome. Remotes are local bare repositories reached over `file://`.

use crate::support;
use awman::data::config::env::EnvSnapshot;
use awman::data::message::RecordingMessageSink;
use awman::data::session::GitRootResolver;
use awman::data::worktree_paths::{repo_key, WorktreePaths};
use awman::engine::error::EngineError;
use awman::engine::git::lease_transport::{
    lease_transport_command, HelperProgram, LeaseCredentialHelper, LeaseCredentials,
    LeaseTransport, LeaseUrlPolicy, LEASE_CREDENTIAL_FILE_ENV,
};
use awman::engine::git::pull_request::{
    match_pull_request_remote, parse_pull_request_input, PullRequestClient, PullRequestInfo,
    PullRequestState,
};
use awman::engine::git::remote_push::LeaseGitGuard;
use awman::engine::git::{
    CommitAllOutcome, CommitIdentity, FetchLocalOutcome, GitEngine, GitFileChangeType, GitIdentity,
    MergeRefOutcome,
};
use awman_test_support::capability::LocalGit;
use awman_test_support::fakes::FakeCli;
use awman_test_support::fixtures::{Repo, RepoBuilder};
use awman_test_support::{Exit, Stdin, TestSandbox};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

// ─── helpers ────────────────────────────────────────────────────────────────

fn sandbox(id: &'static str) -> TestSandbox {
    let sandbox = support::sandbox(id);
    sandbox.require::<LocalGit>();
    sandbox
}

/// The sandbox environment with `overrides` applied, as the engine snapshot.
fn snapshot_with(sandbox: &TestSandbox, overrides: &[(&str, &str)]) -> EnvSnapshot {
    let mut env: BTreeMap<String, String> = sandbox
        .env()
        .iter()
        .map(|(k, v)| {
            (
                k.to_string_lossy().into_owned(),
                v.to_string_lossy().into_owned(),
            )
        })
        .collect();
    for (k, v) in overrides {
        env.insert(k.to_string(), v.to_string());
    }
    EnvSnapshot::from_full_env(env)
        .with_invocation(sandbox.work().to_path_buf(), vec!["awman".to_string()])
}

fn engine(sandbox: &TestSandbox) -> GitEngine {
    GitEngine::with_env(support::env_snapshot(sandbox))
}

/// An engine whose global git config has no identity (the sandbox default
/// config names one), for the fallback-identity contracts.
fn engine_without_global_identity(sandbox: &TestSandbox) -> GitEngine {
    let config = sandbox.root().join("no-identity.gitconfig");
    std::fs::write(&config, "[init]\n\tdefaultBranch = main\n").unwrap();
    GitEngine::with_env(snapshot_with(
        sandbox,
        &[("GIT_CONFIG_GLOBAL", &config.to_string_lossy())],
    ))
}

/// Run git in `dir` through the supervisor (fixed author); stdout trimmed.
fn git(sandbox: &TestSandbox, dir: &Path, args: &[&str]) -> String {
    let out = run_git(sandbox, dir, args);
    out.assert_success();
    out.stdout_text().trim().to_string()
}

fn run_git(sandbox: &TestSandbox, dir: &Path, args: &[&str]) -> awman_test_support::Output {
    sandbox
        .command("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "awman test")
        .env("GIT_AUTHOR_EMAIL", "test@awman.invalid")
        .env("GIT_COMMITTER_NAME", "awman test")
        .env("GIT_COMMITTER_EMAIL", "test@awman.invalid")
        .label("git (test setup)")
        .run()
}

fn sink() -> RecordingMessageSink {
    RecordingMessageSink::new()
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

/// `source` pushed to the bare `origin`, and a clone of origin made by the
/// engine. README.md == "init" on main.
struct Upstream<'a> {
    source: Repo<'a>,
    bare: PathBuf,
    clone: PathBuf,
}

impl<'a> Upstream<'a> {
    fn new(sandbox: &'a TestSandbox, engine: &GitEngine) -> Upstream<'a> {
        let source = RepoBuilder::new(sandbox, "source")
            .file("README.md", "init")
            .init();
        let bare = source.bare_remote("origin");
        let clone = sandbox.work().join("clone");
        engine
            .clone_repo(&file_url(&bare), None, &clone)
            .expect("clone the local upstream");
        Upstream {
            source,
            bare,
            clone,
        }
    }

    /// Commit `file` = `content` on a new upstream branch cut from main.
    fn push_branch(&self, name: &str, file: &str, content: &str) {
        self.source.git(&["checkout", "-q", "-b", name, "main"]);
        self.source.write(file, content);
        self.source.commit(&format!("edit {file} on {name}"));
        self.source.push("origin", name);
        self.source.checkout("main");
    }
}

fn file_url(path: &Path) -> String {
    format!("file://{}", path.display())
}

// ─── root, branches, HEAD ───────────────────────────────────────────────────

#[test]
fn resolve_root_returns_the_repository_root() {
    let sandbox = sandbox("engine::git::resolve_root_returns_the_repository_root");
    let repo = RepoBuilder::new(&sandbox, "app")
        .file("README.md", "x")
        .init();
    std::fs::create_dir_all(repo.path().join("src/deep")).unwrap();
    let g = engine(&sandbox);
    let expected = repo.path().canonicalize().unwrap();
    assert_eq!(
        g.resolve_root(repo.path()).unwrap().canonicalize().unwrap(),
        expected
    );
    assert_eq!(
        g.resolve(&repo.path().join("src/deep"))
            .unwrap()
            .canonicalize()
            .unwrap(),
        expected
    );
    sandbox.finish();
}

#[test]
fn branch_exists_detects_existing_and_missing_branches() {
    let sandbox = sandbox("engine::git::branch_exists_detects_existing_and_missing_branches");
    let repo = RepoBuilder::new(&sandbox, "app")
        .file("README.md", "x")
        .init();
    let g = engine(&sandbox);
    assert!(g.branch_exists(repo.path(), "main"));
    assert!(!g.branch_exists(repo.path(), "branch-that-does-not-exist"));
    sandbox.finish();
}

#[test]
fn detached_head_and_current_branch_follow_checkout_state() {
    let sandbox = sandbox("engine::git::detached_head_and_current_branch_follow_checkout_state");
    let repo = RepoBuilder::new(&sandbox, "app")
        .file("README.md", "x")
        .init();
    let g = engine(&sandbox);
    assert!(!g.is_detached_head(repo.path()));
    assert_eq!(g.current_branch(repo.path()).as_deref(), Some("main"));
    let sha = repo.head();
    repo.git(&["checkout", "-q", "--detach", &sha]);
    assert!(g.is_detached_head(repo.path()));
    assert!(g.current_branch(repo.path()).is_none());
    sandbox.finish();
}

#[test]
fn branch_upstream_is_none_until_one_is_configured() {
    let sandbox = sandbox("engine::git::branch_upstream_is_none_until_one_is_configured");
    let repo = RepoBuilder::new(&sandbox, "app").file("a", "a").init();
    let g = engine(&sandbox);
    assert!(g.branch_upstream(repo.path(), "main").is_none());
    repo.bare_remote("origin");
    repo.git(&["push", "-q", "-u", "origin", "main"]);
    assert_eq!(
        g.branch_upstream(repo.path(), "main").as_deref(),
        Some("origin/main")
    );
    // Option-like branch names are never passed to git.
    assert!(g.branch_upstream(repo.path(), "--all").is_none());
    assert!(g.branch_upstream(repo.path(), "").is_none());
    sandbox.finish();
}

// ─── worktrees ──────────────────────────────────────────────────────────────

#[test]
fn create_then_remove_worktree_leaves_no_directory() {
    let sandbox = sandbox("engine::git::create_then_remove_worktree_leaves_no_directory");
    let repo = RepoBuilder::new(&sandbox, "app")
        .file("README.md", "x")
        .init();
    let g = engine(&sandbox);
    let wt = sandbox.work().join("trees/my-worktree");
    g.create_worktree(repo.path(), &wt, "awman/test-wt-branch")
        .unwrap();
    assert!(wt.join("README.md").exists());
    assert!(g
        .list_worktrees(repo.path())
        .unwrap()
        .iter()
        .any(|p| p.canonicalize().ok() == wt.canonicalize().ok()));
    g.remove_worktree(repo.path(), &wt).unwrap();
    assert!(!wt.exists());
    sandbox.finish();
}

#[test]
fn worktree_create_squash_merge_remove_cycle_commits_implement_subject() {
    let sandbox =
        sandbox("engine::git::worktree_create_squash_merge_remove_cycle_commits_implement_subject");
    let repo = RepoBuilder::new(&sandbox, "app")
        .file("README.md", "initial\n")
        .init();
    let g = engine(&sandbox);
    let branch = g.branch_name_for_work_item(42);
    assert_eq!(branch, "awman/work-item-0042");
    let wt = sandbox.work().join("wt-0042");
    g.create_worktree(repo.path(), &wt, &branch).unwrap();
    std::fs::write(wt.join("change.txt"), "hello\n").unwrap();
    git(&sandbox, &wt, &["add", "change.txt"]);
    git(&sandbox, &wt, &["commit", "-q", "-m", "branch work"]);
    g.merge_branch(repo.path(), &branch, &wt).unwrap();
    assert_eq!(
        repo.git(&["log", "-1", "--pretty=%s", "main"]),
        "Implement awman/work-item-0042"
    );
    assert_eq!(
        std::fs::read_to_string(repo.path().join("change.txt")).unwrap(),
        "hello\n"
    );
    g.remove_worktree(repo.path(), &wt).unwrap();
    assert!(!wt.exists());
    sandbox.finish();
}

#[test]
fn worktree_paths_are_keyed_by_home_repo_and_item() {
    let sandbox = sandbox("engine::git::worktree_paths_are_keyed_by_home_repo_and_item");
    let first = RepoBuilder::at(&sandbox, sandbox.work().join("first/shared-name"))
        .file("a", "a")
        .init();
    let second = RepoBuilder::at(&sandbox, sandbox.work().join("second/shared-name"))
        .file("a", "a")
        .init();
    let g = engine(&sandbox);
    let first_root = first.path().canonicalize().unwrap();
    let second_root = second.path().canonicalize().unwrap();
    let home = sandbox.home();

    let item = g.worktree_path(&first_root, 42).unwrap();
    assert!(item.starts_with(home), "{item:?}");
    assert!(item.ends_with(format!("worktrees/{}/0042", repo_key(&first_root))));
    assert!(g
        .worktree_path(&first_root, 1)
        .unwrap()
        .ends_with(format!("worktrees/{}/0001", repo_key(&first_root))));
    assert!(g
        .worktree_path(&first_root, 9999)
        .unwrap()
        .ends_with("9999"));
    let flow = g.worktree_path_named(&first_root, "build-docs").unwrap();
    assert!(flow.ends_with(format!("worktrees/{}/wf-build-docs", repo_key(&first_root))));

    // Same final directory name, different checkouts: distinct, stable keys.
    let a = g.worktree_path(&first_root, 8136).unwrap();
    let b = g.worktree_path(&second_root, 8136).unwrap();
    assert_ne!(a, b);
    assert_eq!(a, g.worktree_path(&first_root, 8136).unwrap());
    assert_ne!(
        g.worktree_path_named(&first_root, "same-flow").unwrap(),
        g.worktree_path_named(&second_root, "same-flow").unwrap()
    );
    assert_eq!(g.branch_name_for_work_item(9999), "awman/work-item-9999");
    assert_eq!(g.branch_name_for_workflow("my-wf"), "awman/workflow-my-wf");
    sandbox.finish();
}

#[test]
fn legacy_unhashed_worktree_owned_by_this_repository_is_reused() {
    let sandbox =
        sandbox("engine::git::legacy_unhashed_worktree_owned_by_this_repository_is_reused");
    let repo = RepoBuilder::new(&sandbox, "legacy-owner")
        .file("a", "a")
        .init();
    let root = repo.path().canonicalize().unwrap();
    let g = engine(&sandbox);
    let paths = WorktreePaths::with_home(sandbox.home());
    let legacy = paths.legacy_for_work_item(&root, 8134);
    g.create_worktree(&root, &legacy, "awman/test-legacy-owned")
        .unwrap();
    assert_eq!(g.worktree_path(&root, 8134).unwrap(), legacy);
    let legacy_flow = paths.legacy_for_workflow(&root, "old-flow");
    g.create_worktree(&root, &legacy_flow, "awman/test-legacy-workflow")
        .unwrap();
    assert_eq!(
        g.worktree_path_named(&root, "old-flow").unwrap(),
        legacy_flow
    );
    g.remove_worktree(&root, &legacy).unwrap();
    g.remove_worktree(&root, &legacy_flow).unwrap();
    sandbox.finish();
}

#[test]
fn legacy_unhashed_worktree_owned_by_another_repository_is_ignored() {
    let sandbox =
        sandbox("engine::git::legacy_unhashed_worktree_owned_by_another_repository_is_ignored");
    let current = RepoBuilder::at(&sandbox, sandbox.work().join("same-name"))
        .file("a", "a")
        .init();
    let other = RepoBuilder::at(&sandbox, sandbox.work().join("other-parent/same-name"))
        .file("a", "a")
        .init();
    let current_root = current.path().canonicalize().unwrap();
    let g = engine(&sandbox);
    let legacy = WorktreePaths::with_home(sandbox.home()).legacy_for_work_item(&current_root, 8135);
    g.create_worktree(other.path(), &legacy, "awman/test-legacy-foreign")
        .unwrap();
    let hashed = g.worktree_path(&current_root, 8135).unwrap();
    assert_ne!(hashed, legacy);
    assert!(hashed.ends_with(format!("worktrees/{}/8135", repo_key(&current_root))));
    g.create_worktree(&current_root, &hashed, "awman/test-legacy-new")
        .unwrap();
    assert!(hashed.exists());
    g.remove_worktree(&current_root, &hashed).unwrap();
    g.remove_worktree(other.path(), &legacy).unwrap();
    sandbox.finish();
}

#[test]
fn legacy_lookup_rejects_a_stale_registration_replaced_by_a_foreign_checkout() {
    let sandbox = sandbox(
        "engine::git::legacy_lookup_rejects_a_stale_registration_replaced_by_a_foreign_checkout",
    );
    let a = RepoBuilder::new(&sandbox, "a").file("a", "a").init();
    let b = RepoBuilder::new(&sandbox, "b").file("b", "b").init();
    let g = engine(&sandbox);
    for suffix in ["0134", "wf-build"] {
        let legacy = sandbox.work().join("legacy").join(suffix);
        let hashed = sandbox.work().join("hashed").join(suffix);
        g.create_worktree(a.path(), &legacy, &format!("old-a-{suffix}"))
            .unwrap();
        std::fs::remove_dir_all(&legacy).unwrap();
        assert!(
            !g.list_worktrees(a.path()).unwrap().contains(&legacy),
            "prunable records are excluded"
        );
        g.create_worktree(b.path(), &legacy, &format!("new-b-{suffix}"))
            .unwrap();
        // Some Git versions keep the registration without a prunable marker
        // once another checkout occupies the path; ownership rejects it anyway.
        assert!(g.validate_worktree_owner(a.path(), &legacy).is_err());
        assert_eq!(
            g.pick_worktree_path(a.path(), hashed.clone(), legacy.clone()),
            hashed
        );
    }
    sandbox.finish();
}

#[cfg(unix)]
#[test]
fn legacy_lookup_rejects_a_replacement_symlink() {
    let sandbox = sandbox("engine::git::legacy_lookup_rejects_a_replacement_symlink");
    let repo = RepoBuilder::new(&sandbox, "repo").file("a", "a").init();
    let g = engine(&sandbox);
    let legacy = sandbox.work().join("legacy");
    let replacement = sandbox.work().join("replacement");
    g.create_worktree(repo.path(), &legacy, "old").unwrap();
    g.create_worktree(repo.path(), &replacement, "other")
        .unwrap();
    std::fs::remove_dir_all(&legacy).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&replacement, &legacy).unwrap();
    let hashed = sandbox.work().join("hashed");
    assert_eq!(
        g.pick_worktree_path(repo.path(), hashed.clone(), legacy),
        hashed
    );
    sandbox.finish();
}

// APFS refuses file names that are not valid UTF-8.
#[cfg(all(unix, not(target_os = "macos")))]
#[test]
fn legacy_lookup_preserves_tabs_newlines_and_native_path_bytes() {
    let sandbox =
        sandbox("engine::git::legacy_lookup_preserves_tabs_newlines_and_native_path_bytes");
    let repo = RepoBuilder::new(&sandbox, "repo").file("a", "a").init();
    let g = engine(&sandbox);
    #[cfg(unix)]
    for (index, suffix) in ["0134", "wf-build"].iter().enumerate() {
        use std::os::unix::ffi::OsStringExt;
        let ancestor = std::ffi::OsString::from_vec(b"tab\tline\nnonutf8-\xff".to_vec());
        let legacy = sandbox.work().join(ancestor).join(suffix);
        let hashed = sandbox.work().join("hashed").join(suffix);
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        // The engine's own creation needs UTF-8 argv; git accepts native bytes.
        sandbox
            .command("git")
            .args(["worktree", "add", "-q", "-b", &format!("odd-{index}")])
            .arg(&legacy)
            .current_dir(repo.path())
            .run()
            .assert_success();
        assert_eq!(
            g.pick_worktree_path(repo.path(), hashed, legacy.clone()),
            legacy
        );
    }
    sandbox.finish();
}

#[test]
fn worktree_owner_rejects_a_tampered_gitdir_pointer() {
    let sandbox = sandbox("engine::git::worktree_owner_rejects_a_tampered_gitdir_pointer");
    let repo = RepoBuilder::new(&sandbox, "repo").file("a", "a").init();
    let wt = sandbox.work().join("wt");
    let g = engine(&sandbox);
    g.create_worktree(repo.path(), &wt, "test-pointer").unwrap();
    assert_eq!(
        g.validate_worktree_owner(repo.path(), &wt).unwrap(),
        repo.path().join(".git").canonicalize().unwrap()
    );
    std::fs::write(
        wt.join(".git"),
        format!("gitdir: {}/foreign/a/b\n", sandbox.work().display()),
    )
    .unwrap();
    assert!(g.validate_worktree_owner(repo.path(), &wt).is_err());
    sandbox.finish();
}

// ─── host hardening ─────────────────────────────────────────────────────────

/// A repository whose config and attributes name an executable for every
/// hook, filter, monitor and driver git supports; `sentinel` appears if any
/// of them runs on the host.
#[cfg(unix)]
fn hostile_repo<'a>(sandbox: &'a TestSandbox, sentinel: &Path) -> Repo<'a> {
    use std::os::unix::fs::PermissionsExt;
    let repo = RepoBuilder::new(sandbox, "hostile")
        .file("README.md", "x")
        .init();
    let script = sandbox.work().join("evil.sh");
    std::fs::write(
        &script,
        format!("#!/bin/sh\ntouch '{}'\ncat\n", sentinel.display()),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let script_text = script.to_string_lossy().into_owned();
    for key in [
        "core.fsmonitor",
        "filter.evil.clean",
        "filter.evil.smudge",
        "filter.evil.process",
        "merge.evil.driver",
        "diff.evil.textconv",
    ] {
        repo.git(&["config", key, &script_text]);
    }
    // Repository-local hooks path overrides the sandbox's inert global one,
    // so only the engine's own hardening keeps these hooks from running.
    let hooks = repo.path().join(".git/hooks");
    for hook in ["post-checkout", "pre-commit", "reference-transaction"] {
        std::fs::copy(&script, hooks.join(hook)).unwrap();
        std::fs::set_permissions(hooks.join(hook), std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    repo.git(&["config", "core.hooksPath", &hooks.to_string_lossy()]);
    repo.write(".gitattributes", "* filter=evil diff=evil merge=evil\n");
    repo
}

#[cfg(unix)]
#[test]
fn host_git_never_runs_repository_hooks_filters_monitors_or_drivers() {
    let sandbox =
        sandbox("engine::git::host_git_never_runs_repository_hooks_filters_monitors_or_drivers");
    #[cfg(unix)]
    {
        let sentinel = sandbox.work().join("host-executed");
        let repo = hostile_repo(&sandbox, &sentinel);
        repo.write("tracked", "data\n");
        let g = engine(&sandbox);
        g.commit_all(repo.path(), "inert content drivers").unwrap();
        g.uncommitted_files(repo.path()).unwrap();
        g.diff_summary(repo.path()).unwrap();
        let wt = sandbox.work().join("worktree");
        g.create_worktree(repo.path(), &wt, "safe").unwrap();
        assert!(
            !sentinel.exists(),
            "repository commands must not execute on the host"
        );
    }
    sandbox.finish();
}

#[cfg(unix)]
#[test]
fn hooks_do_not_run_on_the_host_during_fetch_or_tracking_worktree_add() {
    let sandbox =
        sandbox("engine::git::hooks_do_not_run_on_the_host_during_fetch_or_tracking_worktree_add");
    #[cfg(unix)]
    {
        let sentinel = sandbox.work().join("host-hook-ran");
        let repo = hostile_repo(&sandbox, &sentinel);
        repo.commit("attributes");
        repo.bare_remote("origin");
        repo.git(&["checkout", "-q", "-b", "hook-target"]);
        repo.write("hook.txt", "hook test\n");
        let expected = repo.commit("hook target");
        repo.push("origin", "hook-target");
        repo.checkout("main");
        repo.git(&["branch", "-D", "hook-target"]);
        // Setup above ran with the hooks inert only by luck of order; start clean.
        let _ = std::fs::remove_file(&sentinel);
        let g = engine(&sandbox);
        g.fetch_branch_tracking(repo.path(), "origin", "hook-target")
            .unwrap();
        let path = sandbox.work().join("hook-worktree");
        g.create_worktree_tracking(repo.path(), &path, "hook-target", "origin")
            .unwrap();
        assert_eq!(git(&sandbox, &path, &["rev-parse", "HEAD"]), expected);
        assert!(!sentinel.exists(), "repository hook executed on host");
    }
    sandbox.finish();
}

// ─── identity ───────────────────────────────────────────────────────────────

#[test]
fn identity_configured_reads_the_global_identity_of_the_snapshot() {
    let sandbox =
        sandbox("engine::git::identity_configured_reads_the_global_identity_of_the_snapshot");
    let repo = RepoBuilder::new(&sandbox, "repo").init();
    let config = sandbox.root().join("global.gitconfig");
    std::fs::write(
        &config,
        "[user]\n\tname = Global User\n\temail = global@x.test\n",
    )
    .unwrap();
    let g = GitEngine::with_env(snapshot_with(
        &sandbox,
        &[("GIT_CONFIG_GLOBAL", &config.to_string_lossy())],
    ));
    let identity = g.identity_configured(repo.path()).unwrap();
    assert_eq!(identity.name.as_deref(), Some("Global User"));
    assert_eq!(identity.email.as_deref(), Some("global@x.test"));
    assert!(identity.is_complete());
    assert!(identity.missing_keys().is_empty());
    sandbox.finish();
}

#[test]
fn identity_configured_prefers_the_repository_local_override() {
    let sandbox = sandbox("engine::git::identity_configured_prefers_the_repository_local_override");
    let repo = RepoBuilder::new(&sandbox, "repo").init();
    repo.git(&["config", "user.name", "Repo User"]);
    repo.git(&["config", "user.email", "repo@x.test"]);
    let identity = engine(&sandbox).identity_configured(repo.path()).unwrap();
    assert_eq!(identity.name.as_deref(), Some("Repo User"));
    assert_eq!(identity.email.as_deref(), Some("repo@x.test"));
    sandbox.finish();
}

#[test]
fn identity_configured_reports_both_keys_missing() {
    let sandbox = sandbox("engine::git::identity_configured_reports_both_keys_missing");
    let repo = RepoBuilder::new(&sandbox, "repo").init();
    let identity = engine_without_global_identity(&sandbox)
        .identity_configured(repo.path())
        .unwrap();
    assert_eq!(identity.name, None);
    assert_eq!(identity.email, None);
    assert!(!identity.is_complete());
    assert_eq!(identity.missing_keys(), vec!["user.name", "user.email"]);
    sandbox.finish();
}

#[test]
fn local_identity_ignores_include_directives_and_global_config() {
    let sandbox =
        sandbox("engine::git::local_identity_ignores_include_directives_and_global_config");
    let repo = RepoBuilder::new(&sandbox, "repo").init();
    let outside = sandbox.work().join("outside.gitconfig");
    std::fs::write(
        &outside,
        "[user]\n\tname = Included\n\temail = inc@x.test\n",
    )
    .unwrap();
    repo.git(&["config", "include.path", &outside.to_string_lossy()]);
    let g = engine(&sandbox);
    let identity = g.local_identity(repo.path()).unwrap();
    assert_eq!(identity, GitIdentity::default(), "{identity:?}");
    assert!(!identity.missing_keys().is_empty());
    g.set_local_identity(repo.path(), "Local", "local@x.test")
        .unwrap();
    let identity = g.local_identity(repo.path()).unwrap();
    assert_eq!(identity.name.as_deref(), Some("Local"));
    assert!(identity.missing_keys().is_empty());
    sandbox.finish();
}

#[test]
fn set_local_identity_writes_only_the_repository_config() {
    let sandbox = sandbox("engine::git::set_local_identity_writes_only_the_repository_config");
    let repo = RepoBuilder::new(&sandbox, "repo").init();
    let global_before = std::fs::read_to_string(sandbox.root().join("gitconfig")).unwrap();
    engine(&sandbox)
        .set_local_identity(repo.path(), "Name With Spaces", "a@b.test")
        .unwrap();
    let config = std::fs::read_to_string(repo.path().join(".git/config")).unwrap();
    assert!(config.contains("Name With Spaces"));
    assert!(config.contains("a@b.test"));
    assert_eq!(
        std::fs::read_to_string(sandbox.root().join("gitconfig")).unwrap(),
        global_before
    );
    sandbox.finish();
}

#[test]
fn local_identity_is_shared_by_linked_worktrees() {
    let sandbox = sandbox("engine::git::local_identity_is_shared_by_linked_worktrees");
    let repo = RepoBuilder::new(&sandbox, "repo").file("a", "a").init();
    repo.git(&["config", "user.name", "awman-test"]);
    repo.git(&["config", "user.email", "test@awman.test"]);
    let g = engine(&sandbox);
    let identity = g.local_identity(repo.path()).unwrap();
    assert_eq!(identity.name.as_deref(), Some("awman-test"));
    let linked = repo.worktree("linked", "linked");
    assert_eq!(g.local_identity(&linked).unwrap(), identity);
    assert_eq!(
        g.git_common_dir(&linked).unwrap().canonicalize().unwrap(),
        repo.path().join(".git").canonicalize().unwrap()
    );
    g.set_local_identity(&linked, "New Name", "new@example.com")
        .unwrap();
    let updated = g.local_identity(repo.path()).unwrap();
    assert_eq!(updated.name.as_deref(), Some("New Name"));
    assert_eq!(updated.email.as_deref(), Some("new@example.com"));
    assert!(g.branch_upstream(repo.path(), "linked").is_none());
    assert!(g
        .remote_url_redacted(repo.path(), "origin")
        .unwrap()
        .is_none());
    sandbox.finish();
}

#[test]
fn set_local_identity_rejects_unsafe_values_and_keeps_the_old_one() {
    let sandbox =
        sandbox("engine::git::set_local_identity_rejects_unsafe_values_and_keeps_the_old_one");
    let repo = RepoBuilder::new(&sandbox, "repo").init();
    repo.git(&["config", "user.name", "awman-test"]);
    let g = engine(&sandbox);
    for (name, email) in [
        ("", "a@b"),
        ("Name\nInjected", "a@b"),
        ("-n", "a@b"),
        ("Name", "no-at-sign"),
        ("Name", "a@b\u{7}"),
    ] {
        assert!(
            g.set_local_identity(repo.path(), name, email).is_err(),
            "{name:?} {email:?}"
        );
    }
    assert_eq!(
        g.local_identity(repo.path()).unwrap().name.as_deref(),
        Some("awman-test")
    );
    sandbox.finish();
}

#[test]
fn commit_identity_uses_the_clone_config_when_present() {
    let sandbox = sandbox("engine::git::commit_identity_uses_the_clone_config_when_present");
    let g = engine_without_global_identity(&sandbox);
    let up = Upstream::new(&sandbox, &g);
    git(&sandbox, &up.clone, &["config", "user.name", "Jane"]);
    git(&sandbox, &up.clone, &["config", "user.email", "jane@x"]);
    let id = g.commit_identity(&up.clone).unwrap();
    assert_eq!((id.name.as_str(), id.email.as_str()), ("Jane", "jane@x"));
    assert!(!id.fallback);
    assert!(id.overrides.is_empty());
    assert!(id.describe().contains("Jane <jane@x> (git config)"));
    sandbox.finish();
}

#[test]
fn commit_identity_falls_back_when_no_identity_is_configured() {
    let sandbox = sandbox("engine::git::commit_identity_falls_back_when_no_identity_is_configured");
    let g = engine_without_global_identity(&sandbox);
    let up = Upstream::new(&sandbox, &g);
    let id = g.commit_identity(&up.clone).unwrap();
    assert!(id.fallback);
    assert_eq!(id.email, "awman@localhost");
    assert!(id
        .overrides
        .contains(&"user.email=awman@localhost".to_string()));
    assert!(id.overrides.contains(&"user.name=awman".to_string()));
    assert!(id.describe().contains("awman <awman@localhost>"));
    sandbox.finish();
}

#[test]
fn commit_identity_overrides_only_the_missing_key() {
    let sandbox = sandbox("engine::git::commit_identity_overrides_only_the_missing_key");
    let g = engine_without_global_identity(&sandbox);
    let up = Upstream::new(&sandbox, &g);
    git(&sandbox, &up.clone, &["config", "user.name", "Jane"]);
    let id = g.commit_identity(&up.clone).unwrap();
    assert_eq!(id.name, "Jane");
    assert_eq!(id.email, "awman@localhost");
    assert_eq!(id.overrides, vec!["-c", "user.email=awman@localhost"]);
    sandbox.finish();
}

// ─── remotes, clone, pull ───────────────────────────────────────────────────

#[test]
fn remote_effective_url_applies_insteadof_where_remote_url_does_not() {
    let sandbox =
        sandbox("engine::git::remote_effective_url_applies_insteadof_where_remote_url_does_not");
    let repo = RepoBuilder::new(&sandbox, "repo").file("a", "a").init();
    let g = engine(&sandbox);
    assert_eq!(g.remote_effective_url(repo.path(), "origin"), None);
    repo.git(&["remote", "add", "origin", "gh:org/repo.git"]);
    repo.git(&["config", "url.https://github.com/.insteadOf", "gh:"]);
    assert_eq!(
        g.remote_url(repo.path(), "origin").unwrap(),
        "gh:org/repo.git"
    );
    assert_eq!(
        g.remote_effective_url(repo.path(), "origin").as_deref(),
        Some("https://github.com/org/repo.git")
    );
    sandbox.finish();
}

#[test]
fn remote_url_redacted_strips_embedded_credentials() {
    let sandbox = sandbox("engine::git::remote_url_redacted_strips_embedded_credentials");
    let repo = RepoBuilder::new(&sandbox, "repo").init();
    let secret = "test-secret-ghp-token";
    sandbox.redact(secret);
    repo.git(&[
        "remote",
        "add",
        "origin",
        &format!("https://x-access-token:{secret}@github.com/o/r.git"),
    ]);
    let g = engine(&sandbox);
    let url = g
        .remote_url_redacted(repo.path(), "origin")
        .unwrap()
        .unwrap();
    assert_eq!(url, "https://github.com/o/r.git");
    assert!(g
        .remote_url_redacted(repo.path(), "nope")
        .unwrap()
        .is_none());
    sandbox.finish();
}

#[test]
fn remote_ref_sha_finds_existing_and_not_missing_refs() {
    let sandbox = sandbox("engine::git::remote_ref_sha_finds_existing_and_not_missing_refs");
    let g = engine(&sandbox);
    let up = Upstream::new(&sandbox, &g);
    assert_eq!(
        g.remote_ref_sha(&up.clone, "origin", "main").as_deref(),
        Some(up.source.head().as_str())
    );
    assert!(g.remote_ref_sha(&up.clone, "origin", "nope").is_none());
    sandbox.finish();
}

#[test]
fn pull_latest_picks_up_a_new_upstream_commit() {
    let sandbox = sandbox("engine::git::pull_latest_picks_up_a_new_upstream_commit");
    let g = engine(&sandbox);
    let up = Upstream::new(&sandbox, &g);
    assert!(!up.clone.join("NEW.md").exists());
    up.source.write("NEW.md", "new upstream content");
    up.source.commit("add NEW.md");
    up.source.push("origin", "main");
    g.pull_latest(&up.clone).unwrap();
    assert!(up.clone.join("NEW.md").exists());
    sandbox.finish();
}

#[test]
fn pull_latest_hard_resets_a_dirty_working_tree() {
    let sandbox = sandbox("engine::git::pull_latest_hard_resets_a_dirty_working_tree");
    let g = engine(&sandbox);
    let up = Upstream::new(&sandbox, &g);
    std::fs::write(up.clone.join("README.md"), "LOCAL UNCOMMITTED EDIT").unwrap();
    g.pull_latest(&up.clone).unwrap();
    assert_eq!(
        std::fs::read_to_string(up.clone.join("README.md")).unwrap(),
        "init"
    );
    sandbox.finish();
}

#[test]
fn pull_latest_on_a_missing_or_non_git_directory_is_a_git_error() {
    let sandbox =
        sandbox("engine::git::pull_latest_on_a_missing_or_non_git_directory_is_a_git_error");
    let g = engine(&sandbox);
    let plain = sandbox.work().join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    for dir in [sandbox.work().join("does-not-exist"), plain] {
        let err = g.pull_latest(&dir).unwrap_err();
        assert!(matches!(err, EngineError::Git(_)), "{dir:?}: {err:?}");
    }
    sandbox.finish();
}

#[test]
fn clone_repo_logged_succeeds_and_cancel_kills_a_clone_from_a_hanging_origin() {
    let sandbox = sandbox(
        "engine::git::clone_repo_logged_succeeds_and_cancel_kills_a_clone_from_a_hanging_origin",
    );
    let g = engine(&sandbox);
    let up = Upstream::new(&sandbox, &g);
    let never = tokio_util::sync::CancellationToken::new();
    let ok = sandbox.work().join("ok");
    g.clone_repo_logged(&file_url(&up.bare), None, &ok, &mut sink(), &never)
        .unwrap();
    assert!(ok.join(".git").exists());

    // An origin that accepts the connection and never answers. The token is
    // cancelled once git is connected (an event, not a delay).
    let listener = sandbox.ports().tcp_listener();
    let url = format!("git://{}/hang.git", listener.local_addr().unwrap());
    let cancel = tokio_util::sync::CancellationToken::new();
    let canceller = cancel.clone();
    let acceptor = listener.try_clone().unwrap();
    let accepted = std::thread::spawn(move || {
        let connection = acceptor.accept().map(|(stream, _)| stream);
        canceller.cancel();
        connection
    });
    let err = g
        .clone_repo_logged(
            &url,
            None,
            &sandbox.work().join("hang"),
            &mut sink(),
            &cancel,
        )
        .unwrap_err();
    assert!(err.to_string().contains("cancelled"), "{err}");
    let connection = accepted.join().unwrap();
    assert!(connection.is_ok(), "git never connected: {connection:?}");
    drop(connection);
    drop(listener);
    sandbox.finish();
}

// ─── diff summary, commits, fingerprints ────────────────────────────────────

#[test]
fn diff_summary_counts_untracked_files() {
    let sandbox = sandbox("engine::git::diff_summary_counts_untracked_files");
    let repo = RepoBuilder::new(&sandbox, "repo")
        .file("README.md", "x")
        .init();
    repo.write("new.txt", "one\ntwo\n");
    let summary = engine(&sandbox).diff_summary(repo.path()).unwrap();
    let new_file = summary.files.iter().find(|f| f.path == "new.txt").unwrap();
    assert_eq!(new_file.change, GitFileChangeType::Added);
    assert_eq!((new_file.added, new_file.removed), (2, 0));
    assert_eq!(summary.added, 2);
    sandbox.finish();
}

#[test]
fn diff_summary_of_an_unborn_repository_marks_everything_added_with_zero_counts() {
    let sandbox = sandbox(
        "engine::git::diff_summary_of_an_unborn_repository_marks_everything_added_with_zero_counts",
    );
    let repo = RepoBuilder::new(&sandbox, "repo").init();
    repo.write("new.txt", "one\ntwo\n");
    let summary = engine(&sandbox).diff_summary(repo.path()).unwrap();
    assert_eq!(summary.files.len(), 1);
    assert_eq!(summary.files[0].path, "new.txt");
    assert_eq!(summary.files[0].change, GitFileChangeType::Added);
    assert_eq!((summary.added, summary.removed), (0, 0));
    assert_eq!((summary.files[0].added, summary.files[0].removed), (0, 0));
    sandbox.finish();
}

#[test]
fn resolve_base_ref_handles_sha_tag_branch_and_missing() {
    let sandbox = sandbox("engine::git::resolve_base_ref_handles_sha_tag_branch_and_missing");
    let g = engine(&sandbox);
    let up = Upstream::new(&sandbox, &g);
    up.push_branch("feature", "f.txt", "f");
    git(&sandbox, &up.clone, &["fetch", "-q", "origin"]);
    git(&sandbox, &up.clone, &["tag", "v1", "origin/main"]);
    let sha = g.head_sha(&up.clone).unwrap();
    let mut s = sink();
    assert_eq!(
        g.resolve_base_ref_logged(&up.clone, &sha, &mut s).unwrap(),
        sha
    );
    assert_eq!(
        g.resolve_base_ref_logged(&up.clone, "v1", &mut s).unwrap(),
        "v1"
    );
    assert_eq!(
        g.resolve_base_ref_logged(&up.clone, "feature", &mut s)
            .unwrap(),
        "origin/feature"
    );
    let err = g
        .resolve_base_ref_logged(&up.clone, "missing", &mut s)
        .unwrap_err();
    assert!(err.to_string().contains("does not resolve"), "{err}");
    sandbox.finish();
}

#[test]
fn create_branch_from_cuts_a_branch_without_an_upstream() {
    let sandbox = sandbox("engine::git::create_branch_from_cuts_a_branch_without_an_upstream");
    let g = engine(&sandbox);
    let up = Upstream::new(&sandbox, &g);
    up.push_branch("base", "base.txt", "b");
    git(&sandbox, &up.clone, &["fetch", "-q", "origin"]);
    let mut s = sink();
    g.create_branch_from_logged(&up.clone, "work", "origin/base", &mut s)
        .unwrap();
    assert!(up.clone.join("base.txt").exists());
    assert!(
        !run_git(&sandbox, &up.clone, &["config", "branch.work.remote"])
            .exit
            .success()
    );
    assert!(g
        .create_branch_from_logged(&up.clone, "x", "origin/nope", &mut s)
        .is_err());
    sandbox.finish();
}

fn parent_count(sandbox: &TestSandbox, dir: &Path) -> usize {
    git(sandbox, dir, &["rev-list", "--parents", "-n", "1", "HEAD"])
        .split_whitespace()
        .count()
        - 1
}

#[test]
fn merge_ref_records_a_merge_commit_and_is_then_up_to_date() {
    let sandbox = sandbox("engine::git::merge_ref_records_a_merge_commit_and_is_then_up_to_date");
    let g = engine(&sandbox);
    let up = Upstream::new(&sandbox, &g);
    up.push_branch("other", "other.txt", "o");
    git(&sandbox, &up.clone, &["fetch", "-q", "origin"]);
    git(&sandbox, &up.clone, &["checkout", "-q", "-b", "work"]);
    std::fs::write(up.clone.join("mine.txt"), "m").unwrap();
    git(&sandbox, &up.clone, &["add", "."]);
    git(&sandbox, &up.clone, &["commit", "-q", "-m", "mine"]);
    let mut s = sink();
    let out = g
        .merge_ref_logged(&up.clone, "other", &test_identity(), &mut s)
        .unwrap();
    let MergeRefOutcome::Merged { sha } = out else {
        panic!("expected Merged, got {out:?}");
    };
    assert_eq!(sha, g.head_sha(&up.clone).unwrap());
    assert!(up.clone.join("other.txt").exists());
    assert_eq!(parent_count(&sandbox, &up.clone), 2);
    assert_eq!(
        g.merge_ref_logged(&up.clone, "other", &test_identity(), &mut s)
            .unwrap(),
        MergeRefOutcome::AlreadyUpToDate
    );
    sandbox.finish();
}

#[test]
fn merge_ref_forces_a_merge_commit_even_when_fast_forward_is_possible() {
    let sandbox =
        sandbox("engine::git::merge_ref_forces_a_merge_commit_even_when_fast_forward_is_possible");
    let g = engine(&sandbox);
    let up = Upstream::new(&sandbox, &g);
    up.push_branch("other", "other.txt", "o");
    git(&sandbox, &up.clone, &["fetch", "-q", "origin"]);
    git(&sandbox, &up.clone, &["checkout", "-q", "-b", "work"]);
    g.merge_ref_logged(&up.clone, "other", &test_identity(), &mut sink())
        .unwrap();
    assert_eq!(parent_count(&sandbox, &up.clone), 2);
    sandbox.finish();
}

#[test]
fn merge_ref_commits_as_the_fallback_identity_when_none_is_configured() {
    let sandbox =
        sandbox("engine::git::merge_ref_commits_as_the_fallback_identity_when_none_is_configured");
    let g = engine_without_global_identity(&sandbox);
    let up = Upstream::new(&sandbox, &g);
    up.push_branch("other", "other.txt", "o");
    git(&sandbox, &up.clone, &["fetch", "-q", "origin"]);
    git(&sandbox, &up.clone, &["checkout", "-q", "-b", "work"]);
    let id = g.commit_identity(&up.clone).unwrap();
    g.merge_ref_logged(&up.clone, "other", &id, &mut sink())
        .unwrap();
    assert_eq!(
        git(&sandbox, &up.clone, &["log", "-1", "--format=%ae"]),
        "awman@localhost"
    );
    sandbox.finish();
}

#[test]
fn merge_ref_of_a_missing_ref_is_an_error() {
    let sandbox = sandbox("engine::git::merge_ref_of_a_missing_ref_is_an_error");
    let g = engine(&sandbox);
    let up = Upstream::new(&sandbox, &g);
    let err = g
        .merge_ref_logged(&up.clone, "ghost", &test_identity(), &mut sink())
        .unwrap_err();
    assert!(
        err.to_string().contains("does not exist on origin"),
        "{err}"
    );
    sandbox.finish();
}

#[test]
fn merge_ref_conflict_aborts_and_lists_the_paths() {
    let sandbox = sandbox("engine::git::merge_ref_conflict_aborts_and_lists_the_paths");
    let g = engine(&sandbox);
    let up = Upstream::new(&sandbox, &g);
    up.push_branch("other", "README.md", "theirs");
    git(&sandbox, &up.clone, &["fetch", "-q", "origin"]);
    git(&sandbox, &up.clone, &["checkout", "-q", "-b", "work"]);
    std::fs::write(up.clone.join("README.md"), "ours").unwrap();
    git(&sandbox, &up.clone, &["add", "."]);
    git(&sandbox, &up.clone, &["commit", "-q", "-m", "ours"]);
    let before = g.head_sha(&up.clone).unwrap();
    let err = g
        .merge_ref_logged(&up.clone, "other", &test_identity(), &mut sink())
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
    assert!(!up.clone.join(".git/MERGE_HEAD").exists());
    assert_eq!(g.head_sha(&up.clone).unwrap(), before);
    assert_eq!(
        std::fs::read_to_string(up.clone.join("README.md")).unwrap(),
        "ours"
    );
    sandbox.finish();
}

#[test]
fn commit_all_commits_changes_and_reports_the_sha() {
    let sandbox = sandbox("engine::git::commit_all_commits_changes_and_reports_the_sha");
    let g = engine(&sandbox);
    let up = Upstream::new(&sandbox, &g);
    std::fs::write(up.clone.join("new.txt"), "n").unwrap();
    let out = g
        .commit_all_with_identity_logged(
            &up.clone,
            "Implement work item 0001",
            &test_identity(),
            &mut sink(),
        )
        .unwrap();
    let CommitAllOutcome::Committed { sha } = out else {
        panic!("expected Committed, got {out:?}");
    };
    assert_eq!(sha, g.head_sha(&up.clone).unwrap());
    assert_eq!(
        git(&sandbox, &up.clone, &["log", "-1", "--format=%s"]),
        "Implement work item 0001"
    );
    sandbox.finish();
}

#[test]
fn commit_all_with_no_changes_is_nothing_to_commit() {
    let sandbox = sandbox("engine::git::commit_all_with_no_changes_is_nothing_to_commit");
    let g = engine(&sandbox);
    let up = Upstream::new(&sandbox, &g);
    let before = g.head_sha(&up.clone).unwrap();
    assert_eq!(
        g.commit_all_with_identity_logged(&up.clone, "msg", &test_identity(), &mut sink())
            .unwrap(),
        CommitAllOutcome::NothingToCommit
    );
    assert_eq!(g.head_sha(&up.clone).unwrap(), before);
    sandbox.finish();
}

#[test]
fn commit_all_respects_gitignore() {
    let sandbox = sandbox("engine::git::commit_all_respects_gitignore");
    let g = engine(&sandbox);
    let up = Upstream::new(&sandbox, &g);
    std::fs::write(up.clone.join(".gitignore"), ".awman/workflows\n").unwrap();
    git(&sandbox, &up.clone, &["add", ".gitignore"]);
    git(
        &sandbox,
        &up.clone,
        &["commit", "-qm", "ignore workflow state"],
    );
    let dir = up.clone.join(".awman/workflows");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("abc-0001-wf.json"), "{}").unwrap();
    std::fs::write(dir.join("dynamic-0001.toml"), "x = 1").unwrap();
    assert_eq!(
        g.commit_all_with_identity_logged(&up.clone, "msg", &test_identity(), &mut sink())
            .unwrap(),
        CommitAllOutcome::NothingToCommit
    );
    sandbox.finish();
}

#[test]
fn commit_all_includes_workflow_files_when_not_ignored() {
    let sandbox = sandbox("engine::git::commit_all_includes_workflow_files_when_not_ignored");
    let g = engine(&sandbox);
    let up = Upstream::new(&sandbox, &g);
    let dir = up.clone.join(".awman/workflows");
    std::fs::create_dir_all(&dir).unwrap();
    for name in ["modified.json", "deleted.json"] {
        std::fs::write(dir.join(name), "original").unwrap();
    }
    git(&sandbox, &up.clone, &["add", "-A"]);
    git(&sandbox, &up.clone, &["commit", "-qm", "baseline"]);
    std::fs::write(dir.join("modified.json"), "modified").unwrap();
    std::fs::remove_file(dir.join("deleted.json")).unwrap();
    std::fs::write(dir.join("new.json"), "new").unwrap();
    std::fs::write(dir.join("dynamic-0042.toml"), "new").unwrap();
    std::fs::write(up.clone.join("work.txt"), "work").unwrap();
    git(&sandbox, &up.clone, &["add", "-A"]);
    let before = g.head_sha(&up.clone).unwrap();
    let result = g
        .commit_all_with_identity_logged(&up.clone, "leftovers", &test_identity(), &mut sink())
        .unwrap();
    assert!(matches!(result, CommitAllOutcome::Committed { .. }));
    assert_eq!(
        git(&sandbox, &up.clone, &["diff", "--name-only", &before, "HEAD"]),
        ".awman/workflows/deleted.json\n.awman/workflows/dynamic-0042.toml\n.awman/workflows/modified.json\n.awman/workflows/new.json\nwork.txt"
    );
    sandbox.finish();
}

#[test]
fn checkout_fingerprint_changes_with_rewrites_modes_untracked_files_and_commits() {
    let sandbox = sandbox(
        "engine::git::checkout_fingerprint_changes_with_rewrites_modes_untracked_files_and_commits",
    );
    let g = engine(&sandbox);
    let up = Upstream::new(&sandbox, &g);
    let clone = &up.clone;
    std::fs::write(clone.join("README.md"), "dirty").unwrap();
    let before = g.checkout_fingerprint(clone).unwrap();
    std::fs::write(clone.join("README.md"), "other").unwrap();
    assert_ne!(before, g.checkout_fingerprint(clone).unwrap());
    std::fs::write(clone.join("untracked"), "one").unwrap();
    let before = g.checkout_fingerprint(clone).unwrap();
    std::fs::write(clone.join("untracked"), "two").unwrap();
    assert_ne!(before, g.checkout_fingerprint(clone).unwrap());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let before = g.checkout_fingerprint(clone).unwrap();
        std::fs::set_permissions(
            clone.join("README.md"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        assert_ne!(before, g.checkout_fingerprint(clone).unwrap());
    }
    let before = g.checkout_fingerprint(clone).unwrap();
    git(
        &sandbox,
        clone,
        &["commit", "--allow-empty", "-qm", "forbidden"],
    );
    assert_ne!(before, g.checkout_fingerprint(clone).unwrap());
    sandbox.finish();
}

#[test]
fn push_upstream_creates_the_branch_on_origin_and_sets_the_upstream() {
    let sandbox =
        sandbox("engine::git::push_upstream_creates_the_branch_on_origin_and_sets_the_upstream");
    let g = engine(&sandbox);
    let up = Upstream::new(&sandbox, &g);
    let mut s = sink();
    g.create_branch_from_logged(&up.clone, "local-name", "origin/HEAD", &mut s)
        .unwrap();
    std::fs::write(up.clone.join("work.txt"), "w").unwrap();
    g.commit_all_with_identity_logged(&up.clone, "work", &test_identity(), &mut s)
        .unwrap();
    let pushed = g
        .push_upstream_logged(&up.clone, "origin", "job/1", &mut s)
        .unwrap();
    assert_eq!(pushed, g.head_sha(&up.clone).unwrap());
    assert_eq!(
        git(&sandbox, &up.bare, &["rev-parse", "refs/heads/job/1"]),
        pushed
    );
    assert_eq!(
        git(&sandbox, &up.clone, &["config", "branch.local-name.remote"]),
        "origin"
    );
    sandbox.finish();
}

#[test]
fn push_upstream_rejection_returns_stderr_and_never_forces() {
    let sandbox = sandbox("engine::git::push_upstream_rejection_returns_stderr_and_never_forces");
    let g = engine(&sandbox);
    let up = Upstream::new(&sandbox, &g);
    up.push_branch("job", "theirs.txt", "t");
    std::fs::write(up.clone.join("mine.txt"), "m").unwrap();
    g.commit_all_with_identity_logged(&up.clone, "mine", &test_identity(), &mut sink())
        .unwrap();
    let before = git(&sandbox, &up.bare, &["rev-parse", "refs/heads/job"]);
    let err = g
        .push_upstream_logged(&up.clone, "origin", "job", &mut sink())
        .unwrap_err();
    assert!(
        err.to_string()
            .contains("git push -u origin HEAD:refs/heads/job failed"),
        "{err}"
    );
    assert_eq!(
        before,
        git(&sandbox, &up.bare, &["rev-parse", "refs/heads/job"])
    );
    sandbox.finish();
}

// ─── session worktrees (branch × remote matrix) ─────────────────────────────

/// A checkout on main with origin (a bare repo); `branch` publishes a branch
/// to origin only.
struct Session<'a> {
    repo: Repo<'a>,
    remote: PathBuf,
}

impl<'a> Session<'a> {
    fn new(sandbox: &'a TestSandbox) -> Session<'a> {
        let repo = RepoBuilder::new(sandbox, "checkout")
            .file("base.txt", "base\n")
            .init();
        let remote = repo.bare_remote("origin");
        repo.git(&["branch", "-q", "--set-upstream-to", "origin/main"]);
        Session { repo, remote }
    }

    fn branch(&self, name: &str, file: &str, text: &str) -> String {
        self.repo.git(&["checkout", "-q", "-b", name]);
        self.repo.write(file, text);
        let oid = self.repo.commit(name);
        self.repo.push("origin", name);
        self.repo.checkout("main");
        self.repo.git(&["branch", "-q", "-D", name]);
        oid
    }

    fn rev(&self, expression: &str) -> String {
        self.repo.git(&["rev-parse", expression])
    }
}

#[test]
fn session_remote_only_branch_becomes_a_tracking_worktree_at_the_remote_commit() {
    let sandbox = sandbox(
        "engine::git::session_remote_only_branch_becomes_a_tracking_worktree_at_the_remote_commit",
    );
    let s = Session::new(&sandbox);
    let g = engine(&sandbox);
    let expected = s.branch("remote-only", "remote.txt", "remote\n");
    let invoking_head = s.rev("HEAD");
    assert_ne!(expected, invoking_head);
    assert!(g
        .remote_branch_exists(s.repo.path(), "origin", "remote-only")
        .unwrap());
    assert!(!g.local_branch_exists(s.repo.path(), "remote-only").unwrap());
    let fetched = g
        .fetch_branch_tracking(s.repo.path(), "origin", "remote-only")
        .unwrap();
    assert_eq!(fetched, expected);
    let path = sandbox.work().join("tracking");
    g.create_worktree_tracking(s.repo.path(), &path, "remote-only", "origin")
        .unwrap();
    assert!(g.local_branch_exists(s.repo.path(), "remote-only").unwrap());
    assert_eq!(git(&sandbox, &path, &["rev-parse", "HEAD"]), expected);
    assert_eq!(
        git(&sandbox, &path, &["rev-parse", "--abbrev-ref", "@{u}"]),
        "origin/remote-only"
    );
    assert_eq!(s.rev("refs/remotes/origin/remote-only"), fetched);
    sandbox.finish();
}

#[test]
fn session_branch_on_both_sides_fast_forwards_the_local_branch() {
    let sandbox =
        sandbox("engine::git::session_branch_on_both_sides_fast_forwards_the_local_branch");
    let s = Session::new(&sandbox);
    let g = engine(&sandbox);
    let expected = s.branch("both", "remote.txt", "remote\n");
    s.repo.git(&["branch", "both", "main"]);
    assert_ne!(s.rev("refs/heads/both"), expected);
    assert!(g
        .remote_branch_exists(s.repo.path(), "origin", "both")
        .unwrap());
    assert!(g.local_branch_exists(s.repo.path(), "both").unwrap());
    assert_eq!(
        g.fetch_local_branch_ff(s.repo.path(), "origin", "refs/heads/both", "both")
            .unwrap(),
        FetchLocalOutcome::Updated
    );
    assert_eq!(s.rev("refs/heads/both"), expected);
    sandbox.finish();
}

#[test]
fn session_local_only_branch_is_attached_without_changing_it() {
    let sandbox = sandbox("engine::git::session_local_only_branch_is_attached_without_changing_it");
    let s = Session::new(&sandbox);
    let g = engine(&sandbox);
    s.repo.git(&["checkout", "-q", "-b", "local-only"]);
    s.repo.write("local.txt", "local\n");
    let expected = s.repo.commit("local branch");
    s.repo.checkout("main");
    assert!(!g
        .remote_branch_exists(s.repo.path(), "origin", "local-only")
        .unwrap());
    let path = sandbox.work().join("existing");
    g.add_worktree_existing(s.repo.path(), &path, "local-only")
        .unwrap();
    assert_eq!(git(&sandbox, &path, &["rev-parse", "HEAD"]), expected);
    assert_eq!(s.rev("refs/heads/local-only"), expected);
    sandbox.finish();
}

#[test]
fn session_new_branch_is_created_from_the_invoking_commit_without_upstream() {
    let sandbox = sandbox(
        "engine::git::session_new_branch_is_created_from_the_invoking_commit_without_upstream",
    );
    let s = Session::new(&sandbox);
    let g = engine(&sandbox);
    let base = s.rev("HEAD");
    assert!(!g
        .remote_branch_exists(s.repo.path(), "origin", "new-local")
        .unwrap());
    assert!(!g.local_branch_exists(s.repo.path(), "new-local").unwrap());
    let path = sandbox.work().join("new");
    g.create_worktree_from_commit(s.repo.path(), &path, "new-local", &base)
        .unwrap();
    assert!(g.local_branch_exists(s.repo.path(), "new-local").unwrap());
    assert_eq!(git(&sandbox, &path, &["rev-parse", "HEAD"]), base);
    assert_eq!(
        s.rev("HEAD"),
        base,
        "creating a linked tree must not move HEAD"
    );
    assert!(!run_git(
        &sandbox,
        &path,
        &["config", "--get", "branch.new-local.remote"]
    )
    .exit
    .success());
    sandbox.finish();
}

#[test]
fn session_ls_remote_failure_creates_no_branch_or_worktree() {
    let sandbox = sandbox("engine::git::session_ls_remote_failure_creates_no_branch_or_worktree");
    let s = Session::new(&sandbox);
    let g = engine(&sandbox);
    let missing = sandbox.work().join("definitely-missing-remote");
    s.repo
        .git(&["remote", "set-url", "origin", &missing.to_string_lossy()]);
    assert!(g
        .remote_branch_exists(s.repo.path(), "origin", "absent")
        .is_err());
    assert!(!g.local_branch_exists(s.repo.path(), "absent").unwrap());
    assert!(!sandbox.work().join("failed-fetch").exists());
    sandbox.finish();
}

#[test]
fn session_remote_branch_matching_is_exact_and_tags_are_not_branches() {
    let sandbox =
        sandbox("engine::git::session_remote_branch_matching_is_exact_and_tags_are_not_branches");
    let s = Session::new(&sandbox);
    let g = engine(&sandbox);
    s.branch("foo/bar", "nested.txt", "nested\n");
    assert!(!g
        .remote_branch_exists(s.repo.path(), "origin", "bar")
        .unwrap());
    s.repo.git(&["tag", "tag-only"]);
    assert!(!g.local_branch_exists(s.repo.path(), "tag-only").unwrap());
    let path = sandbox.work().join("tag-collision");
    assert!(g
        .add_worktree_existing(s.repo.path(), &path, "tag-only")
        .is_err());
    assert!(!path.exists());
    sandbox.finish();
}

#[test]
fn session_diverged_local_branch_keeps_its_commits() {
    let sandbox = sandbox("engine::git::session_diverged_local_branch_keeps_its_commits");
    let s = Session::new(&sandbox);
    let g = engine(&sandbox);
    s.branch("diverged", "remote.txt", "remote\n");
    s.repo.git(&["checkout", "-q", "-b", "diverged", "main"]);
    s.repo.write("local.txt", "local\n");
    let local_before = s.repo.commit("local-only commit");
    s.repo.checkout("main");
    let outcome = g
        .fetch_local_branch_ff(s.repo.path(), "origin", "refs/heads/diverged", "diverged")
        .unwrap();
    assert!(
        matches!(outcome, FetchLocalOutcome::KeptLocal { .. }),
        "got {outcome:?}"
    );
    assert_eq!(s.rev("refs/heads/diverged"), local_before);
    assert_eq!(
        s.repo.git(&["show", "refs/heads/diverged:local.txt"]),
        "local"
    );
    sandbox.finish();
}

#[test]
fn session_fetches_a_pull_ref_without_using_the_head_branch_name() {
    let sandbox =
        sandbox("engine::git::session_fetches_a_pull_ref_without_using_the_head_branch_name");
    let s = Session::new(&sandbox);
    let g = engine(&sandbox);
    let commit = s.branch("pr-source", "pr.txt", "pull request\n");
    git(
        &sandbox,
        &s.remote,
        &["update-ref", "refs/pull/17/head", &commit],
    );
    assert_eq!(
        g.fetch_pull_request_head(s.repo.path(), "origin", 17)
            .unwrap(),
        commit
    );
    assert_eq!(s.rev("refs/awman/pull-heads/origin/17"), commit);
    assert!(!g.local_branch_exists(s.repo.path(), "pr-source").unwrap());
    sandbox.finish();
}

// ─── pull-request metadata through a fake `gh` ──────────────────────────────

/// `gh pr view` answering `body` (the latest rule wins), then one lookup.
fn gh_view(sandbox: &TestSandbox, gh: &FakeCli, body: &str) -> PullRequestInfo {
    gh.on(&["pr", "view"]).stdout(body);
    let client = PullRequestClient::default().with_env(support::env_snapshot(sandbox));
    let pr = parse_pull_request_input("owner/repo#17", None).unwrap();
    client.fetch(&pr, None, &mut sink()).unwrap()
}

fn fake_gh(sandbox: &TestSandbox) -> FakeCli {
    sandbox.install_fake(support::fake_cli_exe(), "gh")
}

#[test]
fn gh_lookup_returns_same_repository_fork_and_deleted_head_metadata() {
    let sandbox =
        sandbox("engine::git::gh_lookup_returns_same_repository_fork_and_deleted_head_metadata");
    let gh = fake_gh(&sandbox);
    let info = gh_view(
        &sandbox,
        &gh,
        r#"{"headRefName":"feature/same","isCrossRepository":false,"state":"OPEN"}"#,
    );
    assert_eq!(info.head_ref_name, "feature/same");
    assert!(!info.is_cross_repository);
    assert_eq!(info.state, PullRequestState::Open);

    let info = gh_view(
        &sandbox,
        &gh,
        r#"{"headRefName":"fork-feature","isCrossRepository":true,"state":"CLOSED"}"#,
    );
    assert_eq!(info.head_ref_name, "fork-feature");
    assert!(info.is_cross_repository);
    assert_eq!(info.state, PullRequestState::Closed);

    // gh reports a deleted head repository as cross-repository; both select
    // refs/pull/N/head.
    let info = gh_view(
        &sandbox,
        &gh,
        r#"{"headRefName":"deleted-branch","isCrossRepository":true,"state":"OPEN"}"#,
    );
    assert!(info.is_cross_repository);
    assert_eq!(info.head_ref_name, "deleted-branch");
    sandbox.finish();
}

#[test]
fn gh_lookup_names_the_github_host_explicitly() {
    let sandbox = sandbox("engine::git::gh_lookup_names_the_github_host_explicitly");
    let gh = fake_gh(&sandbox);
    gh_view(
        &sandbox,
        &gh,
        r#"{"headRefName":"x","isCrossRepository":false,"state":"OPEN"}"#,
    );
    let calls = gh.calls();
    assert_eq!(calls.len(), 1);
    let argv = &calls[0].argv;
    let at = argv
        .iter()
        .position(|a| a == "--repo")
        .expect("--repo passed");
    assert_eq!(argv[at + 1], "github.com/owner/repo");
    sandbox.finish();
}

#[test]
fn a_malicious_head_ref_from_gh_is_rejected_by_branch_validation() {
    let sandbox =
        sandbox("engine::git::a_malicious_head_ref_from_gh_is_rejected_by_branch_validation");
    let marker = sandbox.work().join("pwned");
    let body = serde_json::json!({
        "headRefName": format!("--upload-pack=touch {}", marker.display()),
        "isCrossRepository": true,
        "state": "OPEN"
    })
    .to_string();
    let gh = fake_gh(&sandbox);
    let info = gh_view(&sandbox, &gh, &body);
    let repo = RepoBuilder::new(&sandbox, "repo").init();
    assert!(engine(&sandbox)
        .validate_branch_name(repo.path(), &info.head_ref_name)
        .is_err());
    assert!(!marker.exists());
    sandbox.finish();
}

#[test]
fn a_pull_request_must_match_a_configured_github_remote() {
    let sandbox = sandbox("engine::git::a_pull_request_must_match_a_configured_github_remote");
    let repo = RepoBuilder::new(&sandbox, "repo").init();
    repo.git(&["remote", "add", "origin", "git@github.com:someone/else.git"]);
    let g = engine(&sandbox);
    let other = parse_pull_request_input("requested/repo#17", None).unwrap();
    let err = match_pull_request_remote(&g, repo.path(), &other).unwrap_err();
    assert!(err.to_string().contains("no git remote"), "{err}");
    repo.git(&[
        "remote",
        "add",
        "upstream",
        "https://github.com/Requested/Repo.git",
    ]);
    assert_eq!(
        match_pull_request_remote(&g, repo.path(), &other).unwrap(),
        "upstream"
    );
    sandbox.finish();
}

// ─── lease transport and push-back ──────────────────────────────────────────

const LEASE_SECRET: &str = "test-secret-LEASE-42";

fn bearer() -> LeaseCredentials {
    LeaseCredentials::from_wsp(Some(&awman_wsp::Credentials {
        kind: awman_wsp::CredentialKind::Bearer,
        token: Some(LEASE_SECRET.into()),
        username: None,
        expires_at: None,
    }))
    .unwrap()
}

/// A helper program speaking the awman credential-file protocol, as a
/// script inside the sandbox (git runs it, never the test).
#[cfg(unix)]
fn helper_script(sandbox: &TestSandbox) -> HelperProgram {
    use std::os::unix::fs::PermissionsExt;
    let script = sandbox.work().join("helper.sh");
    std::fs::write(
        &script,
        "#!/bin/sh\n[ \"$2\" = get ] || exit 0\ncat >/dev/null\ngrep -q \"^nonce=$1\\$\" \"$AWMAN_LEASE_CREDENTIAL_FILE\" || exit 0\ngrep -E '^(username|password)=' \"$AWMAN_LEASE_CREDENTIAL_FILE\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    HelperProgram::custom(script, vec![])
}

/// Run, under the supervisor, exactly what a production builder built: its
/// program, arguments and working directory, and an environment that holds
/// only the variables the builder set (host spawns start from a cleared
/// environment, `engine::host_cli::command`).
fn run_built(
    sandbox: &TestSandbox,
    command: &std::process::Command,
    stdin: Stdin,
) -> awman_test_support::Output {
    let mut child = sandbox
        .command(command.get_program())
        .args(command.get_args())
        .current_dir(command.get_current_dir().unwrap_or(&sandbox.work()))
        .stdin(stdin)
        .label("built git command");
    for (key, _) in sandbox.env().iter() {
        child = child.env_remove(key);
    }
    for (key, value) in command.get_envs() {
        child = match value {
            Some(value) => child.env(key, value),
            None => child.env_remove(key),
        };
    }
    child.run()
}

fn args_of(command: &std::process::Command) -> Vec<String> {
    command
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect()
}

fn env_of(command: &std::process::Command, key: &str) -> Option<String> {
    command
        .get_envs()
        .find(|(k, _)| *k == key)
        .and_then(|(_, v)| v.map(|v| v.to_string_lossy().into_owned()))
}

#[cfg(unix)]
#[test]
fn lease_helper_is_invoked_by_real_git_with_global_config_isolated() {
    let sandbox =
        sandbox("engine::git::lease_helper_is_invoked_by_real_git_with_global_config_isolated");
    sandbox.redact(LEASE_SECRET);
    #[cfg(unix)]
    {
        let helper = LeaseCredentialHelper::install(
            &sandbox.work().join("creds"),
            "https://git.example/team/app.git",
            &bearer(),
            helper_script(&sandbox),
            LeaseUrlPolicy::default(),
        )
        .unwrap();
        let transport = LeaseTransport::with_helper(Arc::new(helper), LeaseUrlPolicy::default());
        let env = support::env_snapshot(&sandbox);
        let mut command =
            lease_transport_command(&env, &sandbox.work(), "origin", &transport).unwrap();
        command.args(["credential", "fill"]);
        let out = run_built(
            &sandbox,
            &command,
            Stdin::Bytes(b"protocol=https\nhost=git.example\npath=team/app.git\n\n".to_vec()),
        );
        out.assert_success();
        let stdout = out.stdout_text();
        assert!(stdout.contains(&format!("password={LEASE_SECRET}")));
        assert!(stdout.contains("username=x-access-token"));
    }
    sandbox.finish();
}

/// A bare origin with `main` and `feature`.
fn lease_origin<'a>(sandbox: &'a TestSandbox) -> (Repo<'a>, PathBuf) {
    let seed = RepoBuilder::new(sandbox, "seed").file("a.txt", "a").init();
    seed.git(&["checkout", "-q", "-b", "feature"]);
    seed.write("b.txt", "b");
    seed.commit("feature");
    seed.checkout("main");
    let bare = seed.bare_remote("origin");
    (seed, bare)
}

#[cfg(unix)]
#[test]
fn lease_clone_fetch_ls_remote_and_push_leave_no_secret_in_git_config() {
    let sandbox =
        sandbox("engine::git::lease_clone_fetch_ls_remote_and_push_leave_no_secret_in_git_config");
    sandbox.redact(LEASE_SECRET);
    #[cfg(unix)]
    {
        let (_seed, bare) = lease_origin(&sandbox);
        let url = bare.to_string_lossy().into_owned();
        let policy = LeaseUrlPolicy { allow_local: true };
        // A local remote never asks for credentials; the helper is still
        // installed on every operation.
        let helper = LeaseCredentialHelper::install(
            &sandbox.work().join("creds"),
            "https://git.example/team/app.git",
            &bearer(),
            helper_script(&sandbox),
            policy,
        )
        .unwrap();
        let transport = LeaseTransport::with_helper(Arc::new(helper), policy);
        let g = engine(&sandbox);
        let mut s = sink();
        let cancel = CancellationToken::new();
        let clone = sandbox.work().join("leases/lease_x");
        g.clone_with_lease_transport(&url, Some("main"), &clone, &transport, &mut s, &cancel)
            .unwrap();
        g.fetch_with_lease_transport(
            &clone,
            &["+refs/heads/feature:refs/remotes/origin/feature".into()],
            &transport,
            &mut s,
            &cancel,
        )
        .unwrap();
        let heads = g
            .ls_remote_with_lease_transport(&url, &transport, &mut s, &cancel)
            .unwrap();
        assert!(heads.contains_key("refs/heads/feature"));
        std::fs::write(clone.join("c.txt"), "c").unwrap();
        let push = g
            .isolate_remote_push(&clone, &url)
            .unwrap()
            .with_lease_transport(transport.clone());
        push.commit_leftovers("lease work", &mut s).unwrap();
        let head = push.push("wsp/lease", &mut s).unwrap();
        assert_eq!(
            push.origin_sha("wsp/lease", &mut s).unwrap().as_deref(),
            Some(head.as_str())
        );
        assert_eq!(
            git(&sandbox, &bare, &["rev-parse", "refs/heads/wsp/lease"]),
            head
        );
        let config = std::fs::read_to_string(clone.join(".git/config")).unwrap();
        assert!(
            !config.contains(LEASE_SECRET) && !config.contains("credential"),
            "{config}"
        );
        assert!(!format!("{s:?}").contains(LEASE_SECRET));
    }
    sandbox.finish();
}

#[test]
fn lease_failure_is_classified_and_a_cancelled_token_stops_git() {
    let sandbox =
        sandbox("engine::git::lease_failure_is_classified_and_a_cancelled_token_stops_git");
    let g = engine(&sandbox);
    let transport = LeaseTransport::anonymous(LeaseUrlPolicy { allow_local: true });
    let missing = sandbox.work().join("missing.git");
    let missing = missing.to_string_lossy();
    let err = g
        .ls_remote_with_lease_transport(
            &missing,
            &transport,
            &mut sink(),
            &CancellationToken::new(),
        )
        .unwrap_err();
    assert!(
        matches!(
            err,
            EngineError::Git(_)
                | EngineError::GitTransport { .. }
                | EngineError::GitRejected { .. }
        ),
        "{err:?}"
    );
    let cancel = CancellationToken::new();
    cancel.cancel();
    let err = g
        .ls_remote_with_lease_transport(&missing, &transport, &mut sink(), &cancel)
        .unwrap_err();
    assert!(err.to_string().contains("cancelled"), "{err}");
    sandbox.finish();
}

#[test]
fn lease_git_discards_ambient_config_trace_and_repository_selection() {
    let sandbox =
        sandbox("engine::git::lease_git_discards_ambient_config_trace_and_repository_selection");
    let env = snapshot_with(
        &sandbox,
        &[
            ("GIT_CONFIG_COUNT", "1"),
            ("GIT_CONFIG_KEY_0", "url.https://evil.invalid/.insteadOf"),
            ("GIT_CONFIG_VALUE_0", "https://approved.invalid/"),
            (
                "GIT_CONFIG_PARAMETERS",
                "'http.extraHeader=Authorization: Basic SECRET'",
            ),
            ("GIT_TRACE_CURL", "1"),
            ("GIT_TRACE_REDACT", "0"),
            ("GIT_DIR", "/foreign"),
            ("GIT_WORK_TREE", "/foreign"),
            ("GIT_SSH_COMMAND", "echo stole credentials"),
            ("SSH_ASKPASS", "steal"),
        ],
    );
    let mut command =
        lease_transport_command(&env, &sandbox.work(), "origin", &LeaseTransport::default())
            .unwrap();
    command.args(["config", "--get-regexp", "^url\\."]);
    let out = run_built(&sandbox, &command, Stdin::Null);
    assert_eq!(out.exit, Exit::Code(1), "no url.* rewrite is visible");
    assert!(out.stdout.is_empty() && out.stderr.is_empty());
    sandbox.finish();
}

#[test]
fn lease_push_back_uses_the_same_isolation_helper_and_pinned_programs_as_clone() {
    let sandbox = sandbox(
        "engine::git::lease_push_back_uses_the_same_isolation_helper_and_pinned_programs_as_clone",
    );
    sandbox.redact(LEASE_SECRET);
    let helper = LeaseCredentialHelper::install(
        &sandbox.work().join("c"),
        "https://git.example/team/app.git",
        &bearer(),
        HelperProgram::awman("/bin/awman".into()),
        LeaseUrlPolicy::default(),
    )
    .unwrap();
    let transport = LeaseTransport::with_helper(Arc::new(helper), LeaseUrlPolicy::default());
    let g = engine(&sandbox);
    let command = lease_transport_command(g.env(), &sandbox.work(), "origin", &transport).unwrap();
    let clone = RepoBuilder::new(&sandbox, "clone").init();
    let push = g
        .isolate_remote_push(clone.path(), "https://git.example/team/app.git")
        .unwrap()
        .with_lease_transport(transport.clone());
    let push_command = push.command(true).unwrap();
    let push_args = args_of(&push_command);
    for key in [
        "GIT_CONFIG_GLOBAL",
        "GIT_CONFIG_NOSYSTEM",
        "GIT_TERMINAL_PROMPT",
    ] {
        assert_eq!(env_of(&push_command, key), env_of(&command, key), "{key}");
    }
    assert!(env_of(&push_command, LEASE_CREDENTIAL_FILE_ENV).is_some());
    assert!(env_of(&push_command, "GIT_SSH_COMMAND").is_none());
    for pinned in [
        "credential.useHttpPath=true",
        "credential.helper=",
        "protocol.file.allow=never",
        "remote.origin.uploadpack=git-upload-pack",
        "remote.origin.receivepack=git-receive-pack",
        "core.hooksPath=/dev/null",
        "push.followTags=false",
    ] {
        assert!(
            push_args.iter().any(|a| a == pinned),
            "{pinned}: {push_args:?}"
        );
    }
    assert!(push_args.iter().all(|a| !a.contains(LEASE_SECRET)));
    let ours = |args: &[String]| {
        args.iter()
            .find(|a| a.starts_with("credential.helper=!"))
            .cloned()
    };
    assert_eq!(ours(&push_args), ours(&args_of(&command)));
    sandbox.finish();
}

#[test]
fn isolated_push_back_commit_preserves_the_sha256_object_format() {
    let sandbox =
        sandbox("engine::git::isolated_push_back_commit_preserves_the_sha256_object_format");
    let repo = sandbox.work().join("clone");
    std::fs::create_dir_all(&repo).unwrap();
    git(
        &sandbox,
        &repo,
        &["init", "-q", "-b", "main", "--object-format=sha256"],
    );
    std::fs::write(repo.join("base"), "base").unwrap();
    git(&sandbox, &repo, &["add", "-A"]);
    git(&sandbox, &repo, &["commit", "-qm", "base"]);
    let g = engine(&sandbox);
    let before = g.head_sha(&repo).unwrap();
    assert_eq!(before.len(), 64);
    // The URL is recorded only; nothing is fetched or pushed.
    let push = g
        .isolate_remote_push(&repo, "https://git.example/unused.git")
        .unwrap();
    let mut s = sink();
    assert_eq!(push.head(&mut s).unwrap(), before);
    std::fs::write(repo.join("edit"), "edit").unwrap();
    push.commit_leftovers("leftovers", &mut s).unwrap();
    let after = push.head(&mut s).unwrap();
    assert_eq!(after.len(), 64);
    assert_ne!(before, after);
    assert_eq!(g.head_sha(&repo).unwrap(), after);
    sandbox.finish();
}

#[test]
fn a_guarded_push_to_a_stalled_origin_is_killed_when_the_lease_is_cancelled() {
    let sandbox = sandbox(
        "engine::git::a_guarded_push_to_a_stalled_origin_is_killed_when_the_lease_is_cancelled",
    );
    let repo = RepoBuilder::new(&sandbox, "clone").file("a", "a").init();
    // An origin that accepts and never answers.
    let listener = sandbox.ports().tcp_listener();
    let url = format!("git://{}/stall.git", listener.local_addr().unwrap());
    let g = engine(&sandbox);
    let push = g.isolate_remote_push(repo.path(), &url).unwrap();
    let guard = LeaseGitGuard {
        cancel: CancellationToken::new(),
        expiry: None,
        deadline: chrono::Utc::now() + chrono::Duration::hours(1),
    };
    let canceller = guard.cancel.clone();
    let acceptor = listener.try_clone().unwrap();
    let accepted = std::thread::spawn(move || {
        let connection = acceptor.accept().map(|(stream, _)| stream);
        canceller.cancel();
        connection
    });
    let err = push.push_guarded("job", &mut sink(), &guard).unwrap_err();
    assert!(err.to_string().contains("cancelled or expired"), "{err}");
    let connection = accepted.join().unwrap();
    assert!(connection.is_ok(), "git never connected: {connection:?}");
    drop(connection);
    drop(listener);
    sandbox.finish();
}

#[test]
fn linked_worktree_leftovers_update_the_pinned_admin_and_publish() {
    let sandbox =
        sandbox("engine::git::linked_worktree_leftovers_update_the_pinned_admin_and_publish");
    let repo = RepoBuilder::new(&sandbox, "repo").init();
    repo.commit("base");
    let worktree = repo.worktree("worktree", "lease");
    let remote = sandbox.work().join("remote.git");
    git(
        &sandbox,
        &sandbox.work(),
        &["init", "-q", "--bare", &remote.to_string_lossy()],
    );
    let g = engine(&sandbox);
    let source = g.capture_workflow_git_source(&worktree).unwrap();
    let before = g.head_sha(repo.path()).unwrap();
    let isolated = g
        .isolate_workflow_transport(&source, &remote.to_string_lossy())
        .unwrap();
    std::fs::write(worktree.join("change.txt"), "lease edit").unwrap();
    let mut s = sink();
    isolated.commit_leftovers("lease changes", &mut s).unwrap();
    let head = isolated.head(&mut s).unwrap();
    assert_eq!(g.head_sha(&worktree).unwrap(), head);
    assert_eq!(g.head_sha(repo.path()).unwrap(), before);
    assert!(git(&sandbox, &worktree, &["status", "--porcelain"]).is_empty());
    assert_eq!(isolated.push("result", &mut s).unwrap(), head);
    assert_eq!(
        git(&sandbox, &remote, &["rev-parse", "refs/heads/result"]),
        head
    );
    assert_eq!(
        git(&sandbox, &remote, &["show", "refs/heads/result:change.txt"]),
        "lease edit"
    );
    sandbox.finish();
}

#[test]
fn isolated_workflow_transport_omits_hooks_rewrites_ssh_commands_and_pushurl() {
    let sandbox = sandbox(
        "engine::git::isolated_workflow_transport_omits_hooks_rewrites_ssh_commands_and_pushurl",
    );
    let repo = RepoBuilder::new(&sandbox, "repo").init();
    std::fs::write(
        repo.path().join(".git/config"),
        "[core]\nrepositoryformatversion=0\n[remote \"origin\"]\nurl=https://approved.test/o/r\npushurl=https://evil.test/o/r\n[url \"https://evil.test/\"]\ninsteadOf=https://approved.test/\n[core]\nhooksPath=hooks\nsshCommand=evil\n",
    )
    .unwrap();
    std::fs::create_dir_all(repo.path().join(".git/hooks")).unwrap();
    std::fs::write(repo.path().join(".git/hooks/pre-push"), "echo stolen").unwrap();
    let g = engine(&sandbox);
    let isolated = g
        .isolate_workflow_transport(
            &g.capture_workflow_git_source(repo.path()).unwrap(),
            "https://approved.test/o/r",
        )
        .unwrap();
    let config = std::fs::read_to_string(isolated.metadata_dir().join("config")).unwrap();
    assert!(config.contains("https://approved.test/o/r"));
    for needle in [
        "evil.test",
        "insteadOf",
        "hooksPath",
        "sshCommand",
        "pushurl",
    ] {
        assert!(!config.contains(needle), "{needle}");
    }
    assert!(!isolated.metadata_dir().join("hooks/pre-push").exists());
    assert!(!isolated.metadata_dir().starts_with(repo.path()));
    sandbox.finish();
}

#[test]
fn pinned_workflow_metadata_ignores_rewritten_git_and_commondir_pointers() {
    let sandbox = sandbox(
        "engine::git::pinned_workflow_metadata_ignores_rewritten_git_and_commondir_pointers",
    );
    let repo = RepoBuilder::new(&sandbox, "repo").init();
    repo.commit("initial");
    let foreign = RepoBuilder::new(&sandbox, "foreign").init();
    let worktree = sandbox.work().join("worktree");
    repo.git(&[
        "worktree",
        "add",
        "--detach",
        "-q",
        &worktree.to_string_lossy(),
    ]);
    let admin = std::path::PathBuf::from(git(
        &sandbox,
        &worktree,
        &["rev-parse", "--absolute-git-dir"],
    ));
    let g = engine(&sandbox);
    let source = g.capture_workflow_git_source(&worktree).unwrap();
    let foreign_git = foreign.path().join(".git");
    let foreign_head = std::fs::read(foreign_git.join("HEAD")).unwrap();
    std::fs::write(
        foreign_git.join("config"),
        "[core]\nrepositoryformatversion=0\n[user]\nname=FOREIGN_CANARY\n",
    )
    .unwrap();
    std::fs::write(
        worktree.join(".git"),
        format!("gitdir: {}\n", foreign_git.display()),
    )
    .unwrap();
    std::fs::write(
        admin.join("commondir"),
        format!("{}\n", foreign_git.display()),
    )
    .unwrap();
    let isolated = g
        .isolate_workflow_transport(&source, "https://approved.test/o/r")
        .unwrap();
    assert!(
        !std::fs::read_to_string(isolated.metadata_dir().join("config"))
            .unwrap()
            .contains("FOREIGN_CANARY")
    );
    assert_eq!(
        std::fs::read(isolated.metadata_dir().join("HEAD")).unwrap(),
        std::fs::read(admin.join("HEAD")).unwrap()
    );
    isolated.refresh_workflow_metadata().unwrap();
    isolated.sync_workflow_metadata().unwrap();
    assert_eq!(
        std::fs::read(foreign_git.join("HEAD")).unwrap(),
        foreign_head
    );
    sandbox.finish();
}

#[cfg(unix)]
#[test]
fn isolated_workflow_metadata_refuses_a_linked_config() {
    let sandbox = sandbox("engine::git::isolated_workflow_metadata_refuses_a_linked_config");
    let repo = RepoBuilder::new(&sandbox, "repo").init();
    let g = engine(&sandbox);
    let source = g.capture_workflow_git_source(repo.path()).unwrap();
    let foreign = sandbox.work().join("foreign-config");
    std::fs::write(&foreign, "[core]\n").unwrap();
    std::fs::remove_file(repo.path().join(".git/config")).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&foreign, repo.path().join(".git/config")).unwrap();
    assert!(g
        .isolate_workflow_transport(&source, "https://approved.test/o/r")
        .is_err());
    sandbox.finish();
}
