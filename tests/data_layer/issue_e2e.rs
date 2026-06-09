//! End-to-end tests for the GitHub issue integration.
//!
//! Gated by `AWMAN_E2E_ISSUES=1` environment variable.
//! Requires network access to GitHub and a valid GitHub token or gh CLI.
//! Run with: AWMAN_E2E_ISSUES=1 cargo test --test data_layer issue_e2e

use awman::data::issue::router::IssueSourceRouter;
// The trait is needed for `source.title_slug` / `source.format_as_markdown`
// inside the gated tests; unused when `AWMAN_E2E_ISSUES` is unset.
#[allow(unused_imports)]
use awman::data::issue::IssueSource;

// Use a well-known public issue for testing
const TEST_OWNER: &str = "rust-lang";
const TEST_REPO: &str = "rust";
const TEST_ISSUE_NUMBER: u32 = 1;
const TEST_ISSUE_URL: &str = "https://github.com/rust-lang/rust/issues/1";

#[test]
fn e2e_github_fetch_by_bare_integer_real_network() {
    // Bare integers require a git remote in the local repo to resolve to
    // owner/repo. We git-init a temp dir, point origin at the test repo,
    // and exercise the same code path the CLI uses for `--issue 1`.
    if std::env::var("AWMAN_E2E_ISSUES").as_deref() != Ok("1") {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let init = std::process::Command::new("git")
        .arg("init")
        .current_dir(tmp.path())
        .output()
        .expect("git init");
    assert!(init.status.success(), "git init failed: {init:?}");
    let remote = std::process::Command::new("git")
        .args([
            "remote",
            "add",
            "origin",
            &format!("https://github.com/{TEST_OWNER}/{TEST_REPO}.git"),
        ])
        .current_dir(tmp.path())
        .output()
        .expect("git remote add");
    assert!(remote.status.success(), "git remote add failed: {remote:?}");

    let router = IssueSourceRouter::default();
    let result = router.fetch_issue(&format!("{TEST_ISSUE_NUMBER}"), tmp.path());
    assert!(
        result.is_ok(),
        "bare integer fetch failed: {}",
        result
            .as_ref()
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default()
    );
    let (issue, _) = result.unwrap();
    assert!(!issue.title.is_empty(), "issue title must not be empty");
    assert_eq!(issue.provider, "GitHub");
    assert_eq!(
        issue.source_id, TEST_ISSUE_URL,
        "bare-integer resolution must produce the same canonical URL as the full URL form"
    );
}

#[test]
fn e2e_github_fetch_by_short_form_real_network() {
    if std::env::var("AWMAN_E2E_ISSUES").as_deref() != Ok("1") {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let router = IssueSourceRouter::default();
    let input = format!("{TEST_OWNER}/{TEST_REPO}#{TEST_ISSUE_NUMBER}");
    let result = router.fetch_issue(&input, tmp.path());
    assert!(
        result.is_ok(),
        "short form fetch failed: {}",
        result
            .as_ref()
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default()
    );
    let (issue, source) = result.unwrap();
    assert!(!issue.title.is_empty(), "issue title must not be empty");
    assert_eq!(issue.provider, "GitHub");
    let slug = source.title_slug(&issue);
    assert!(
        slug.starts_with("ghb"),
        "slug must start with GitHub provider prefix 'ghb'"
    );
    assert!(slug.contains(&format!("{TEST_ISSUE_NUMBER}")));
    assert!(slug.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));
    let filename = format!("{:04}-{slug}.md", issue.numeric_id().unwrap_or(0));
    assert!(filename.contains(&slug));
}

#[test]
fn e2e_github_fetch_by_full_url_real_network() {
    if std::env::var("AWMAN_E2E_ISSUES").as_deref() != Ok("1") {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let router = IssueSourceRouter::default();
    let result = router.fetch_issue(TEST_ISSUE_URL, tmp.path());
    assert!(
        result.is_ok(),
        "full URL fetch failed: {}",
        result
            .as_ref()
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default()
    );
    let (issue, _) = result.unwrap();
    assert_eq!(
        issue.source_id, TEST_ISSUE_URL,
        "source_id must be canonical URL"
    );
}

#[test]
fn e2e_github_format_as_markdown_produces_valid_output_real_network() {
    if std::env::var("AWMAN_E2E_ISSUES").as_deref() != Ok("1") {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let router = IssueSourceRouter::default();
    let (issue, source) = router.fetch_issue(TEST_ISSUE_URL, tmp.path()).unwrap();
    let md = source.format_as_markdown(&issue);
    assert!(md.starts_with("# "), "markdown must start with H1");
    assert!(md.contains(&issue.title));
}
