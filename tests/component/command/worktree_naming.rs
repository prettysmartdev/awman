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
fn github_uses_only_the_canonical_issue_name() {
    let sandbox =
        support::sandbox("command::worktree_naming::github_uses_only_the_canonical_issue_name");
    let repo = RepoBuilder::new(&sandbox, "app")
        .file("README", "hello")
        .init();
    let git = Arc::new(GitEngine::with_env(support::env_snapshot(&sandbox)));
    let name =
        WorktreeLifecycle::name_for(Some(&issue("Fix login timeout")), Path::new("run.toml"));
    assert_eq!(
        name,
        WorktreeName::GithubIssue("84-fix-login-timeout".into())
    );
    let fresh = WorktreeLifecycle::open(git.clone(), repo.path().into(), &name).unwrap();
    assert_eq!(fresh.branch(), "awman/84-fix-login-timeout");
    assert_eq!(
        fresh.worktree_path().file_name().unwrap(),
        "84-fix-login-timeout"
    );
    let retired = fresh
        .worktree_path()
        .parent()
        .unwrap()
        .join("wf-ghb84-fix-login-timeout-remote-deadbeef");
    git.create_worktree(
        repo.path(),
        &retired,
        "awman/workflow-ghb84-fix-login-timeout-remote-deadbeef",
    )
    .unwrap();
    let reopened = WorktreeLifecycle::open(git.clone(), repo.path().into(), &name).unwrap();
    assert_eq!(reopened.worktree_path(), fresh.worktree_path());
    assert_eq!(reopened.branch(), fresh.branch());
    assert!(retired.join("README").is_file());
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
        let WorktreeName::GithubIssue(name) = first else {
            panic!("expected GitHub name")
        };
        let WorktreeName::GithubIssue(other) = second else {
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

#[tokio::test]
async fn workflow_names_use_only_the_canonical_spelling_for_every_lifecycle_action() {
    use awman::command::commands::api_server::event_bus::EventBus;
    use awman::command::commands::worktree_lifecycle::{
        ExistingWorktreeChoice, ExistingWorktreeDecision,
    };
    use awman::data::checkout_paths::WorktreePaths;
    use awman::frontend::api::command_frontend::ApiDispatchFrontend;
    let sandbox = support::sandbox("command::worktree_naming::workflow_names_use_only_the_canonical_spelling_for_every_lifecycle_action");
    let repo = RepoBuilder::new(&sandbox, "app")
        .file("README", "hello")
        .init();
    let git = Arc::new(GitEngine::with_env(support::env_snapshot(&sandbox)));
    for name in [
        "../outside",
        "feature/a",
        "feature-a",
        "fix..login",
        "💥",
        "run.lock",
    ] {
        let worktree =
            WorktreeLifecycle::for_workflow(git.clone(), repo.path().into(), name).unwrap();
        git.validate_branch_name(repo.path(), worktree.branch())
            .unwrap();
        let file = worktree
            .worktree_path()
            .file_name()
            .unwrap()
            .to_str()
            .unwrap();
        assert_eq!(
            file.strip_prefix("wf-").unwrap(),
            worktree.branch().strip_prefix("awman/workflow-").unwrap()
        );
        git.create_worktree(repo.path(), worktree.worktree_path(), worktree.branch())
            .unwrap();
        assert_eq!(
            git.current_branch(worktree.worktree_path()).as_deref(),
            Some(worktree.branch())
        );
    }
    let name = "révision";
    let paths = WorktreePaths::from_env(git.env()).unwrap();
    let root = repo.path().canonicalize().unwrap();
    let fresh = paths.for_workflow(&root, name);
    let old = fresh.parent().unwrap().join("wf-révision");
    git.create_worktree(repo.path(), &old, "awman/workflow-révision")
        .unwrap();
    let mut resumed =
        WorktreeLifecycle::for_workflow(git.clone(), repo.path().into(), name).unwrap();
    let fresh_branch = awman::data::checkout_paths::worktree_branch_name_for_workflow(name);
    assert_eq!(resumed.worktree_path(), fresh);
    assert_eq!(resumed.branch(), fresh_branch);
    git.create_worktree(repo.path(), &fresh, &fresh_branch)
        .unwrap();
    let bus = EventBus::new(128);
    let mut frontend = ApiDispatchFrontend::new("exec workflow", &[], bus.sender());
    let kept = resumed
        .prepare_with_existing(
            &mut frontend,
            ExistingWorktreeChoice::Explicit(ExistingWorktreeDecision::Resume),
        )
        .await
        .unwrap();
    assert_eq!(kept, fresh);
    let recreated = resumed
        .prepare_with_existing(
            &mut frontend,
            ExistingWorktreeChoice::Explicit(ExistingWorktreeDecision::Recreate),
        )
        .await
        .unwrap();
    assert_eq!(recreated, fresh);
    assert_eq!(resumed.branch(), fresh_branch);
    assert!(old.join("README").is_file());
    let preferred = WorktreeLifecycle::for_workflow(git, repo.path().into(), name).unwrap();
    assert_eq!(preferred.worktree_path(), fresh);
    assert_eq!(preferred.branch(), fresh_branch);
}
