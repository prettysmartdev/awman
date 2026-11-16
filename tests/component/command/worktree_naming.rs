use crate::support;
use awman::command::commands::worktree_lifecycle::{WorktreeLifecycle, WorktreeName};
use awman::command::work_item::{ResolvedWorkItem, WorkItemOrigin};
use awman::engine::git::GitEngine;
use awman_test_support::fixtures::RepoBuilder;
use std::path::Path;
use std::sync::Arc;

fn issue(title: &str) -> ResolvedWorkItem {
    ResolvedWorkItem {
        origin: WorkItemOrigin::Remote {
            provider: "github".into(),
            source_id: "github:acme/app#84".into(),
        },
        number: Some(84),
        key: "remote-deadbeef".into(),
        title: title.into(),
        slug: Some("ghb84-fix-login-timeout".into()),
        content: String::new(),
        issue: None,
    }
}

#[test]
fn github_uses_number_and_safe_title_and_resumes_legacy_worktrees() {
    let sandbox = support::sandbox(
        "command::worktree_naming::github_uses_number_and_safe_title_and_resumes_legacy_worktrees",
    );
    let repo = RepoBuilder::new(&sandbox, "app")
        .file("README", "hello")
        .init();
    let git = Arc::new(GitEngine::with_env(support::env_snapshot(&sandbox)));
    let name =
        WorktreeLifecycle::name_for(Some(&issue("Fix login timeout")), Path::new("run.toml"));
    let legacy_name = "ghb84-fix-login-timeout-remote-deadbeef";
    assert_eq!(
        name,
        WorktreeName::GithubIssue {
            name: "84-fix-login-timeout".into(),
            legacy_name: legacy_name.into(),
        }
    );
    let fresh = WorktreeLifecycle::open(git.clone(), repo.path().into(), &name).unwrap();
    assert_eq!(fresh.branch(), "awman/84-fix-login-timeout");
    assert_eq!(
        fresh.worktree_path().file_name().unwrap(),
        "84-fix-login-timeout"
    );
    // An arbitrary old directory must not be mistaken for this repo's worktree.
    let legacy =
        WorktreeLifecycle::for_workflow(git.clone(), repo.path().into(), legacy_name).unwrap();
    std::fs::create_dir_all(legacy.worktree_path()).unwrap();
    let unowned = WorktreeLifecycle::open(git.clone(), repo.path().into(), &name).unwrap();
    assert_eq!(unowned.worktree_path(), fresh.worktree_path());
    std::fs::remove_dir(legacy.worktree_path()).unwrap();
    git.create_worktree(repo.path(), legacy.worktree_path(), legacy.branch())
        .unwrap();
    let resumed = WorktreeLifecycle::open(git.clone(), repo.path().into(), &name).unwrap();
    assert_eq!(resumed.worktree_path(), legacy.worktree_path());
    assert_eq!(resumed.branch(), legacy.branch());
    git.create_worktree(repo.path(), fresh.worktree_path(), fresh.branch())
        .unwrap();
    let preferred = WorktreeLifecycle::open(git, repo.path().into(), &name).unwrap();
    assert_eq!(preferred.worktree_path(), fresh.worktree_path());
    assert_eq!(preferred.branch(), fresh.branch());
}

#[test]
fn github_names_are_bounded_and_do_not_depend_on_repository_identity() {
    for (title, expected) in [
        (
            "../../ Fix [Login] @{timeout}.lock",
            "84-fix-login-timeout-lock",
        ),
        ("💥 / ..", "84"),
        (
            "A very long issue title that needs a shorter branch name for everyday use",
            "84-a-very-long-issue-title-that-needs-a",
        ),
    ] {
        let mut item = issue(title);
        let first = WorktreeLifecycle::name_for(Some(&item), Path::new("run.toml"));
        item.origin = WorkItemOrigin::Remote {
            provider: "github".into(),
            source_id: "github:other/repo#84".into(),
        };
        item.key = "different-key".into();
        let second = WorktreeLifecycle::name_for(Some(&item), Path::new("run.toml"));
        let WorktreeName::GithubIssue { name, .. } = first else {
            panic!("expected GitHub name")
        };
        let WorktreeName::GithubIssue { name: other, .. } = second else {
            panic!("expected GitHub name")
        };
        assert_eq!(name, expected);
        assert_eq!(name, other);
    }
    let mut http = issue("Fix login timeout");
    http.origin = WorkItemOrigin::Remote {
        provider: "http:tracker".into(),
        source_id: "tracker:84".into(),
    };
    assert_eq!(
        WorktreeLifecycle::name_for(Some(&http), Path::new("run.toml")),
        WorktreeName::Workflow("ghb84-fix-login-timeout-remote-deadbeef".into())
    );
}
