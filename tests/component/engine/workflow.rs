//! engine::workflow component tests: workflow behaviour that needs a real
//! `git` repository (CI polling reads branch, HEAD and origin through the
//! production `GitEngine`). Everything runs in a per-test sandbox; the
//! instance environment comes from `support::env_snapshot`.

use crate::support;
use awman::data::ci_poll_event::CiPollEvent;
use awman::engine::git::GitEngine;
use awman::engine::workflow::poll_ci::CiPoller;
use awman_test_support::capability::LocalGit;
use awman_test_support::fixtures::RepoBuilder;
use std::sync::Arc;

fn poller(sandbox: &awman_test_support::TestSandbox, token: Option<&str>) -> CiPoller {
    let env = support::env_snapshot(sandbox);
    CiPoller::new(
        Arc::new(GitEngine::with_env(env.clone())),
        token.map(awman::data::secret::SecretString::new),
    )
    .with_env(env)
}

/// Without a granted token, and with host `gh` invisible (test isolation
/// hides a `gh` outside the instance temp dir), polling names both missing
/// authentication paths instead of reaching the network.
#[test]
fn poll_ci_without_token_or_gh_names_both_auth_paths() {
    let sandbox =
        support::sandbox("engine::workflow::poll_ci_without_token_or_gh_names_both_auth_paths");
    sandbox.require::<LocalGit>();
    let repo = RepoBuilder::new(&sandbox, "app")
        .file("f.txt", "x")
        .branch("main")
        .init();
    repo.git(&[
        "remote",
        "add",
        "origin",
        "https://github.com/example/repo.git",
    ]);

    let error = poller(&sandbox, None)
        .fetch_status(repo.path())
        .unwrap_err()
        .to_string();
    assert!(error.contains("GITHUB_TOKEN"), "{error}");
    assert!(error.contains("gh"), "{error}");
    sandbox.finish();
}

/// An origin that changed since prerequisite approval is refused before any
/// credential is used, and the granted secret never reaches the error.
#[test]
fn poll_ci_refuses_a_changed_destination_without_leaking_the_token() {
    let sandbox = support::sandbox(
        "engine::workflow::poll_ci_refuses_a_changed_destination_without_leaking_the_token",
    );
    sandbox.require::<LocalGit>();
    let repo = RepoBuilder::new(&sandbox, "app")
        .file("f.txt", "x")
        .branch("main")
        .init();
    repo.git(&["remote", "add", "origin", "https://evil.test/o/r"]);

    let error = poller(&sandbox, Some("fixture_secret"))
        .with_destination(Some("https://github.com/o/r".into()))
        .fetch_status(repo.path())
        .unwrap_err()
        .to_string();
    assert!(error.contains("destination changed"), "{error}");
    assert!(!error.contains("fixture_secret"), "{error}");
    sandbox.finish();
}

/// An unchanged ssh origin matches its approved redaction; a non-github.com
/// host without a token then stops before any network use.
#[test]
fn poll_ci_accepts_an_unchanged_ssh_destination() {
    let sandbox =
        support::sandbox("engine::workflow::poll_ci_accepts_an_unchanged_ssh_destination");
    sandbox.require::<LocalGit>();
    for (i, url) in ["git@example.test:o/r.git", "ssh://git@example.test/o/r.git"]
        .into_iter()
        .enumerate()
    {
        let repo = RepoBuilder::new(&sandbox, &format!("app{i}"))
            .file("f.txt", "x")
            .branch("main")
            .init();
        repo.git(&["remote", "add", "origin", url]);
        let approved = awman::data::workflow_prerequisites::GitRemoteUrl::parse(url).redacted;
        let error = poller(&sandbox, None)
            .with_destination(Some(approved))
            .fetch_status(repo.path())
            .unwrap_err()
            .to_string();
        assert!(!error.contains("destination changed"), "{url}: {error}");
        assert!(error.contains("explicit token grant"), "{url}: {error}");
    }
    sandbox.finish();
}

fn sh(
    sandbox: &awman_test_support::TestSandbox,
    dir: &std::path::Path,
    cmd: &str,
) -> awman_test_support::process::Output {
    sandbox
        .command("sh")
        .args(["-c", cmd])
        .current_dir(dir)
        .label("sh (step command)")
        .run()
}

/// The `commit_changes` teardown command, run by a real shell and `git`:
/// ignored files stay out, explicitly staged ignored files follow normal Git
/// behaviour, and a clean tree does not fail the step.
#[test]
fn commit_changes_commits_everything_and_tolerates_a_clean_tree() {
    use awman::data::workflow_definition::TeardownStep;
    use awman::engine::workflow::step_commands::teardown_step_to_shell;
    let sandbox = support::sandbox(
        "engine::workflow::commit_changes_commits_everything_and_tolerates_a_clean_tree",
    );
    sandbox.require::<LocalGit>();
    let repo = RepoBuilder::new(&sandbox, "app")
        .file("a", "a\n")
        .file(".gitignore", ".awman/workflows\n")
        .branch("main")
        .init();
    let dir = repo.path();
    sh(
        &sandbox,
        dir,
        "echo b > a && echo new > new && mkdir -p .awman/workflows \
         && echo {} > .awman/workflows/state.json",
    )
    .assert_success();

    let step = TeardownStep::CommitChanges {
        message: "work".to_string(),
        add_all: true,
    };
    let (cmd, _) = teardown_step_to_shell(&step);
    sh(&sandbox, dir, &cmd).assert_success();
    let committed = sh(&sandbox, dir, "git show --name-only --format= HEAD").stdout_text();
    assert_eq!(committed.lines().collect::<Vec<_>>(), ["a", "new"]);

    sh(&sandbox, dir, "git add -f .awman/workflows/state.json").assert_success();
    sh(&sandbox, dir, &cmd).assert_success();
    let committed = sh(&sandbox, dir, "git show --name-only --format= HEAD").stdout_text();
    assert_eq!(committed.trim(), ".awman/workflows/state.json");

    let again = sh(&sandbox, dir, &cmd);
    again.assert_success();
    assert!(again.stdout_text().contains("nothing to commit"));
    sandbox.finish();
}

/// The HTTPS push transport's credential helper speaks git's credential
/// protocol and answers only for the approved destination; the token is
/// expanded from the child environment and never appears in argv.
#[test]
fn https_transport_helper_answers_only_the_approved_destination() {
    use awman::engine::workflow::step_commands::{transport_git, PushAuth, StepCommandOptions};
    let sandbox = support::sandbox(
        "engine::workflow::https_transport_helper_answers_only_the_approved_destination",
    );
    sandbox.require::<LocalGit>();
    let opts = StepCommandOptions {
        push_auth: PushAuth::HttpsToken {
            env_var: "AWMAN_GIT_TOKEN".into(),
        },
        remote_url: Some("https://github.com/o/r.git".into()),
    };
    let git = transport_git(&opts);
    assert!(!git.contains("fixture_secret"));
    for (host, path, allowed) in [
        ("github.com", "o/r.git", true),
        ("evil.test", "o/r.git", false),
        ("github.com", "other/repo", false),
    ] {
        let out = sandbox
            .command("sh")
            .args(["-c", &format!("{git} credential fill")])
            .current_dir(sandbox.work())
            .env("AWMAN_GIT_TOKEN", "fixture_secret")
            .stdin(awman_test_support::Stdin::Bytes(
                format!("protocol=https\nhost={host}\npath={path}\n").into_bytes(),
            ))
            .label("git credential fill")
            .run();
        assert_eq!(
            out.exit == awman_test_support::Exit::Code(0),
            allowed,
            "{host}/{path}: {:?}",
            out
        );
        assert_eq!(
            out.stdout_text().contains("password=fixture_secret"),
            allowed
        );
    }
    sandbox.finish();
}

/// Outside a git repository the poll fails through the real `git`, and only
/// after it has announced the first attempt.
#[test]
fn poll_ci_outside_a_repository_fails_after_the_attempt_banner() {
    let sandbox = support::sandbox(
        "engine::workflow::poll_ci_outside_a_repository_fails_after_the_attempt_banner",
    );
    sandbox.require::<LocalGit>();
    let not_a_repo = sandbox.work().join("plain");
    std::fs::create_dir_all(&not_a_repo).unwrap();
    let mut events = Vec::new();
    let result = poller(&sandbox, None).poll(&not_a_repo, 0, 5, |event| events.push(event));
    assert!(result.is_err());
    assert_eq!(events, vec![CiPollEvent::Attempt { attempt: 1, of: 5 }]);
    sandbox.finish();
}
