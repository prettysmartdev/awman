use crate::support;
use awman::{
    data::{ids::LeaseId, message::RecordingMessageSink},
    engine::{
        git::{
            lease_transport::{LeaseTransport, LeaseUrlPolicy},
            GitEngine, MergeRefOutcome,
        },
        worker::lease_workspace::{LeaseCheckout, LeaseWorkspace},
    },
};
use awman_test_support::fixtures::RepoBuilder;
use tokio_util::sync::CancellationToken;

#[test]
fn checkout_merges_declared_refs_and_resumes_the_published_branch() {
    let sandbox = support::sandbox(
        "worker::checkout::checkout_merges_declared_refs_and_resumes_the_published_branch",
    );
    let repo = RepoBuilder::new(&sandbox, "seed")
        .file("base", "base")
        .init();
    repo.create_branch("wsp/dependency")
        .checkout("wsp/dependency");
    repo.write("dependency", "included").commit("dependency");
    repo.checkout("main");
    let bare = repo.bare_remote("origin");
    repo.git(&["push", bare.to_str().unwrap(), "wsp/dependency"]);
    let transport = LeaseTransport::anonymous(LeaseUrlPolicy { allow_local: true });
    let git = GitEngine::with_env(support::env_snapshot(&sandbox));
    let spec = awman_wsp::Repo {
        url: bare.to_str().unwrap().into(),
        base_ref: "main".into(),
        branch: "wsp/item".into(),
        merge_refs: vec!["wsp/dependency".into()],
        credentials: None,
    };
    let workspace =
        LeaseWorkspace::prepare(&sandbox.work().join("leases"), &LeaseId::new()).unwrap();
    let mut sink = RecordingMessageSink::new();
    let first = LeaseCheckout::new(&git, &transport)
        .run(&workspace, &spec, &mut sink, &CancellationToken::new())
        .unwrap();
    assert!(!first.resumed_branch);
    assert!(first.start_point.is_some());
    assert!(matches!(first.merged[0].1, MergeRefOutcome::Merged { .. }));
    assert_eq!(
        std::fs::read_to_string(workspace.clone_dir().join("dependency")).unwrap(),
        "included"
    );
    sandbox
        .command("git")
        .current_dir(workspace.clone_dir())
        .args(["push", bare.to_str().unwrap(), "HEAD:refs/heads/wsp/item"])
        .run()
        .assert_success();
    let second = LeaseWorkspace::prepare(&sandbox.work().join("leases"), &LeaseId::new()).unwrap();
    let resumed = LeaseCheckout::new(&git, &transport)
        .run(&second, &spec, &mut sink, &CancellationToken::new())
        .unwrap();
    assert!(resumed.resumed_branch);
    assert_eq!(resumed.head, first.head);
    second.remove().unwrap();
    workspace.remove().unwrap();
    sandbox.finish();
}

#[test]
fn checkout_refuses_tag_and_missing_merge_refs_after_owned_remote_lookup() {
    let sandbox = support::sandbox(
        "worker::checkout::checkout_refuses_tag_and_missing_merge_refs_after_owned_remote_lookup",
    );
    let repo = RepoBuilder::new(&sandbox, "seed")
        .file("base", "base")
        .init();
    repo.git(&["tag", "v1"]);
    let bare = repo.bare_remote("origin");
    repo.git(&["push", bare.to_str().unwrap(), "v1"]);
    let git = GitEngine::with_env(support::env_snapshot(&sandbox));
    let transport = LeaseTransport::anonymous(LeaseUrlPolicy { allow_local: true });
    for (reference, reason) in [
        ("v1", "is a tag on origin"),
        ("wsp/missing", "does not exist on origin"),
    ] {
        let workspace =
            LeaseWorkspace::prepare(&sandbox.work().join("leases"), &LeaseId::new()).unwrap();
        let spec = awman_wsp::Repo {
            url: bare.to_str().unwrap().into(),
            base_ref: "main".into(),
            branch: "wsp/item".into(),
            merge_refs: vec![reference.into()],
            credentials: None,
        };
        let mut sink = RecordingMessageSink::new();
        let error = LeaseCheckout::new(&git, &transport)
            .run(&workspace, &spec, &mut sink, &CancellationToken::new())
            .unwrap_err();
        assert!(error.to_string().contains(reason), "{reference}: {error}");
        assert!(format!("{sink:?}").contains("ls-remote"));
        workspace.remove().unwrap();
    }
    sandbox.finish();
}

#[test]
fn lease_clone_auth_failure_keeps_operation_remote_and_stable_auth_class() {
    let sandbox = support::sandbox(
        "worker::checkout::lease_clone_auth_failure_keeps_operation_remote_and_stable_auth_class",
    );
    sandbox.require::<awman_test_support::capability::LocalGit>();
    sandbox.require::<awman_test_support::capability::Loopback>();
    let repo = RepoBuilder::new(&sandbox, "seed").file("a", "a").init();
    repo.bare_remote("origin");
    let secret = "test-secret-lease-auth-password";
    let remote =
        awman_test_support::fakes::FakeGitHttp::builder(&sandbox, &sandbox.root().join("remotes"))
            .basic_auth("user", secret)
            .start();
    let url = remote.url("origin.git");
    let transport = LeaseTransport::default();
    let git = GitEngine::with_env(support::env_snapshot(&sandbox));
    let spec = awman_wsp::Repo {
        url: url.clone(),
        base_ref: "main".into(),
        branch: "lease/result".into(),
        merge_refs: vec![],
        credentials: None,
    };
    let workspace =
        LeaseWorkspace::prepare(&sandbox.work().join("leases"), &LeaseId::new()).unwrap();
    let mut sink = RecordingMessageSink::new();
    let error = LeaseCheckout::new(&git, &transport)
        .run(&workspace, &spec, &mut sink, &CancellationToken::new())
        .unwrap_err();
    assert!(
        matches!(&error, awman::engine::error::EngineError::GitAuth { operation, remote, .. }
        if operation == "clone" && remote == awman::data::git_transport::SafeRemote::from_url(&url).as_str())
    );
    assert_eq!(
        awman::command::session_git_transport::git_failure_code(&error),
        Some(("git_auth", "git_auth_failed"))
    );
    assert!(!error.to_string().contains(secret));
    assert!(!format!("{:?}", sink.all()).contains(secret));
    workspace.remove().unwrap();
}
