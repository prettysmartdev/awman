//! Unit tests for `engine::git::pull_request`. Seeded by impl-engine with
//! parser smoke tests; the fake-gh / mock-REST suite is added by tests-engine.

use super::*;

fn origin() -> RemoteSlug {
    RemoteSlug::parse("git@github.com:Org/Repo.git").unwrap()
}

#[test]
fn parses_the_three_accepted_forms() {
    let bare = parse_pull_request_input("42", Some(&origin())).unwrap();
    assert_eq!(
        (
            bare.repository.owner.as_str(),
            bare.repository.repo.as_str(),
            bare.number
        ),
        ("Org", "Repo", 42)
    );

    let url = parse_pull_request_input("https://github.com/a-b/c.d_e/pull/7/", None).unwrap();
    assert_eq!(
        (
            url.repository.owner.as_str(),
            url.repository.repo.as_str(),
            url.number
        ),
        ("a-b", "c.d_e", 7)
    );
    assert_eq!(url.repository.host, "github.com");

    let short = parse_pull_request_input("o/r#3", None).unwrap();
    assert_eq!((short.repository.owner.as_str(), short.number), ("o", 3));
}

#[test]
fn rejects_unsupported_forms() {
    for bad in [
        "",
        "0",
        "4294967296",
        "https://github.com/o/r/issues/1",
        "http://github.com/o/r/pull/1",
        "https://gitlab.com/o/r/pull/1",
        "https://user@github.com/o/r/pull/1",
        "https://github.com/o/r/pull/1?x=1",
        "https://github.com/o/r/pull/1#frag",
        "https://github.com/o/r/pull/1/files",
        "https://github.com/o/%2e%2e/pull/1",
        "https://github.com/o/../pull/1",
        "o/r#0",
        "o/r/x#1",
        "o#1",
        "o/..#1",
        "o/r#1\n",
        "o/r",
    ] {
        assert!(
            parse_pull_request_input(bad, Some(&origin())).is_err(),
            "{bad:?}"
        );
    }
}

#[test]
fn bare_number_needs_a_github_origin() {
    assert!(parse_pull_request_input("5", None).is_err());
    let gitlab = RemoteSlug::parse("git@gitlab.com:o/r.git").unwrap();
    assert!(parse_pull_request_input("5", Some(&gitlab)).is_err());
}

#[test]
fn rest_body_with_deleted_head_repo_is_cross_repository() {
    let pr = parse_pull_request_input("o/r#1", None).unwrap();
    let body = serde_json::json!({
        "state": "closed", "merged": true,
        "head": {"ref": "feature", "repo": null},
        "base": {"repo": {"full_name": "o/r"}}
    });
    let info = parse_rest_pull_request(&pr, &body).unwrap();
    assert!(info.is_cross_repository);
    assert_eq!(info.state, PullRequestState::Merged);

    let same = serde_json::json!({
        "state": "open",
        "head": {"ref": "feature", "repo": {"full_name": "O/R"}},
        "base": {"repo": {"full_name": "o/r"}}
    });
    assert!(
        !parse_rest_pull_request(&pr, &same)
            .unwrap()
            .is_cross_repository
    );
    assert!(parse_rest_pull_request(&pr, &serde_json::json!({"state": "open"})).is_err());
}

#[cfg(unix)]
fn fake_gh(body: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("gh");
    std::fs::write(
        &path,
        format!("#!/bin/sh\nprintf '%s' '{}'\n", body.replace('\'', "'\\''")),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    (dir, path)
}

#[cfg(unix)]
fn fetch_with_fake_gh(body: &str) -> PullRequestInfo {
    use crate::data::message::RecordingMessageSink;
    let (_dir, gh) = fake_gh(body);
    let client = PullRequestClient::with_integrations(gh, "http://127.0.0.1:1".into());
    let pr = parse_pull_request_input("owner/repo#17", None).unwrap();
    client
        .fetch(&pr, None, &mut RecordingMessageSink::new())
        .unwrap()
}

#[cfg(unix)]
#[test]
fn fake_gh_returns_same_repository_pr_metadata() {
    let info = fetch_with_fake_gh(
        r#"{"headRefName":"feature/same","isCrossRepository":false,"state":"OPEN"}"#,
    );
    assert_eq!(info.head_ref_name, "feature/same");
    assert!(!info.is_cross_repository);
    assert_eq!(info.state, PullRequestState::Open);
}

#[cfg(unix)]
#[test]
fn fake_gh_returns_fork_pr_metadata() {
    let info = fetch_with_fake_gh(
        r#"{"headRefName":"fork-feature","isCrossRepository":true,"state":"CLOSED"}"#,
    );
    assert_eq!(info.head_ref_name, "fork-feature");
    assert!(info.is_cross_repository);
    assert_eq!(info.state, PullRequestState::Closed);
}

#[cfg(unix)]
#[test]
fn fake_gh_deleted_head_branch_is_routed_as_cross_repository() {
    // gh's isCrossRepository is true for a deleted head repository. REST
    // distinguishes the missing repo as null; both select refs/pull/N/head.
    let info = fetch_with_fake_gh(
        r#"{"headRefName":"deleted-branch","isCrossRepository":true,"state":"OPEN"}"#,
    );
    assert!(info.is_cross_repository);
    assert_eq!(info.head_ref_name, "deleted-branch");
}

#[cfg(unix)]
#[test]
fn real_git_fake_gh_malicious_head_ref_is_returned_as_untrusted_and_git_validation_rejects_it() {
    let info = fetch_with_fake_gh(
        r#"{"headRefName":"--upload-pack=touch /tmp/pwned","isCrossRepository":true,"state":"OPEN"}"#,
    );
    let git = GitEngine::new();
    let temp = tempfile::tempdir().unwrap();
    let init = std::process::Command::new("git")
        .args(["init", "--initial-branch=main"])
        .current_dir(temp.path())
        .output()
        .unwrap();
    assert!(init.status.success());
    assert!(git
        .validate_branch_name(temp.path(), &info.head_ref_name)
        .is_err());
}

#[test]
fn real_git_pull_request_repository_must_match_a_configured_github_remote() {
    use std::path::Path;
    use std::process::Command;
    let temp = tempfile::tempdir().unwrap();
    let output = Command::new("git")
        .args(["init", "--initial-branch=main"])
        .current_dir(temp.path())
        .output()
        .unwrap();
    assert!(output.status.success());
    let run = |args: &[&str]| {
        let result = Command::new("git")
            .args(args)
            .current_dir(temp.path())
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
    };
    run(&["remote", "add", "origin", "git@github.com:someone/else.git"]);
    let pr = parse_pull_request_input("requested/repo#17", None).unwrap();
    assert!(
        match_pull_request_remote(&GitEngine::new(), Path::new(temp.path()), &pr)
            .unwrap_err()
            .to_string()
            .contains("no git remote")
    );
}

#[cfg(unix)]
#[test]
fn gh_lookup_names_the_github_host_explicitly() {
    // An ambient GH_HOST (or gh's default host) must not redirect the lookup
    // to a GitHub Enterprise server with a same-named repository.
    use crate::data::message::RecordingMessageSink;
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let args_file = dir.path().join("args");
    let gh = dir.path().join("gh");
    std::fs::write(
        &gh,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\nprintf '%s' '{{\"headRefName\":\"x\",\"isCrossRepository\":false,\"state\":\"OPEN\"}}'\n",
            args_file.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    let client = PullRequestClient::with_integrations(gh, "http://127.0.0.1:1".into());
    let pr = parse_pull_request_input("owner/repo#17", None).unwrap();
    let mut sink = RecordingMessageSink::new();
    client.fetch(&pr, None, &mut sink).unwrap();
    let args = std::fs::read_to_string(&args_file).unwrap();
    let args: Vec<&str> = args.lines().collect();
    let at = args
        .iter()
        .position(|a| *a == "--repo")
        .expect("--repo passed");
    assert_eq!(args[at + 1], "github.com/owner/repo");
}
