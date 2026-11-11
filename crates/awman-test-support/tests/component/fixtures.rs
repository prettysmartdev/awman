//! Fixture builders that need processes (git).

use crate::sandbox;
use awman_test_support::fixtures::{ConfigTree, RepoBuilder, WorkItemBuilder, WorkflowBuilder};

#[test]
fn repositories_have_deterministic_commit_ids() {
    let a = sandbox("fixtures::repositories_have_deterministic_commit_ids");
    let b = sandbox("fixtures::repositories_have_deterministic_commit_ids");
    let build = |s| {
        let repo = RepoBuilder::new(s, "app")
            .file("README.md", "hello\n")
            .init();
        repo.write("src/lib.rs", "pub fn f() {}\n");
        let second = repo.commit("add lib");
        (repo.head(), second)
    };
    assert_eq!(build(&a), build(&b));
}

#[test]
fn branches_worktrees_and_bare_remotes() {
    let sandbox = sandbox("fixtures::branches_worktrees_and_bare_remotes");
    let repo = RepoBuilder::new(&sandbox, "app").file("a.txt", "a").init();
    repo.create_branch("feature").checkout("feature");
    repo.write("b.txt", "b");
    let feature = repo.commit("feature work");
    let worktree = repo.worktree("wt", "wt-branch");
    assert!(worktree.join("a.txt").is_file());
    let bare = repo.bare_remote("origin");
    assert!(bare.join("HEAD").is_file());
    let remote_head = repo.git(&["ls-remote", "origin", "refs/heads/feature"]);
    assert!(remote_head.starts_with(&feature), "{remote_head}");
}

#[test]
fn git_runs_with_the_sandbox_identity_and_config() {
    let sandbox = sandbox("fixtures::git_runs_with_the_sandbox_identity_and_config");
    let repo = RepoBuilder::new(&sandbox, "app").file("a", "a").init();
    assert_eq!(
        repo.git(&["log", "-1", "--format=%an <%ae> %at"]),
        "awman test <test@awman.invalid> 1767225600"
    );
    let origin = repo.git(&[
        "config",
        "--null",
        "--show-origin",
        "--get",
        "core.hooksPath",
    ]);
    assert!(
        origin.contains(
            &sandbox
                .root()
                .join("gitconfig")
                .to_string_lossy()
                .into_owned()
        ),
        "{origin}"
    );
}

#[test]
fn text_fixtures_land_in_the_repository() {
    let sandbox = sandbox("fixtures::text_fixtures_land_in_the_repository");
    let repo = RepoBuilder::new(&sandbox, "app").init();
    let item = WorkItemBuilder::new(7, "Seven").write(repo.path());
    assert!(item.ends_with("aspec/work-items/0007-seven.md"));
    let wf = WorkflowBuilder::new("Flow")
        .step("one", "do it", &[])
        .write(&repo.path().join("aspec/workflows"), "flow");
    assert!(wf.is_file());
    let paths = ConfigTree::new()
        .repo(serde_json::json!({"agent": "codex"}))
        .write(&sandbox, Some(repo.path()));
    assert_eq!(paths.repo.unwrap(), repo.path().join(".awman/config.json"));
}
