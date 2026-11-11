//! Unit tests for `engine::git::pull_request`: reference parsing and REST
//! body interpretation. The `gh` lookups (a fake `gh` on the instance PATH)
//! and the remote matching against a real repository are component tests in
//! `tests/component/engine/git.rs`.

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

#[test]
fn gh_repo_argument_names_the_github_host_explicitly() {
    let pr = parse_pull_request_input("owner/repo#17", None).unwrap();
    assert_eq!(gh_repo_argument(&pr), "github.com/owner/repo");
}
