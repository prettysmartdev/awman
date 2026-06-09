//! Integration tests for the GitHub issue module — filename/path derivation logic.
//!
//! Hermetic — no network, no Docker, no git daemon. Uses in-process logic only.

use awman::data::issue::github::GithubIssueSource;
use awman::data::issue::{Issue, IssueSource};
use awman::data::worktree_paths::worktree_branch_name_for_workflow;
use std::path::PathBuf;

fn make_issue(source_id: &str, title: &str) -> Issue {
    Issue {
        source_id: source_id.to_string(),
        title: title.to_string(),
        body: String::new(),
        provider: "GitHub".to_string(),
    }
}

// ─── Temp filename format ─────────────────────────────────────────────────────

#[test]
fn issue_overlay_temp_filename_format() {
    // Verifies: awman-issue-{pid}-{slug}.md
    let issue = make_issue("https://github.com/owner/repo/issues/84", "Test Title");
    let slug = GithubIssueSource.title_slug(&issue);
    let pid = std::process::id();
    let filename = format!("awman-issue-{pid}-{slug}.md");

    // Check structure
    assert!(
        filename.starts_with("awman-issue-"),
        "must start with 'awman-issue-'"
    );
    assert!(filename.ends_with(".md"), "must end with '.md'");
    assert!(filename.contains(&slug), "must contain the slug");
    // slug must be safe for filenames
    assert!(
        slug.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'),
        "slug must contain only alphanumerics and hyphens: {slug}"
    );
}

// ─── Container filename format ────────────────────────────────────────────────

#[test]
fn issue_overlay_container_filename_format() {
    // Verifies: {NNNN}-{slug}.md where NNNN = numeric_id() zero-padded to 4 digits.
    let issue = make_issue("https://github.com/owner/repo/issues/84", "Test Title");
    let slug = GithubIssueSource.title_slug(&issue);
    let number = issue.numeric_id().unwrap_or(0);
    let filename = format!("{number:04}-{slug}.md");

    assert_eq!(&filename[..4], "0084", "NNNN prefix must be '0084'");
    assert!(filename.contains(&slug), "filename must contain slug");
    assert!(filename.ends_with(".md"));
}

// ─── Container path derivation ────────────────────────────────────────────────

#[test]
fn issue_overlay_container_path_derivation() {
    // Container path = /workspace + relative_work_items_dir + container_filename.
    let issue = make_issue("https://github.com/owner/repo/issues/84", "Test Title");
    let slug = GithubIssueSource.title_slug(&issue);
    let number = issue.numeric_id().unwrap_or(0);
    let container_filename = format!("{number:04}-{slug}.md");

    let container_path = PathBuf::from("/workspace")
        .join("aspec/work-items")
        .join(&container_filename);

    assert!(
        container_path.starts_with("/workspace/aspec/work-items/"),
        "container path must start with /workspace/aspec/work-items/"
    );
    assert!(
        container_path.to_str().unwrap().ends_with(".md"),
        "container path must end with .md"
    );
}

// ─── Worktree branch name ─────────────────────────────────────────────────────

#[test]
fn issue_worktree_branch_name_uses_title_slug() {
    // Verifies: awman/workflow-{title_slug}
    let issue = make_issue(
        "https://github.com/prettysmartdev/awman/issues/84",
        "GitHub Integration Part 1",
    );
    let slug = GithubIssueSource.title_slug(&issue);
    let branch = worktree_branch_name_for_workflow(&slug);

    assert_eq!(
        branch,
        format!("awman/workflow-{slug}"),
        "branch must follow awman/workflow-{{slug}} pattern"
    );
    // Branch must be git-ref-safe: only alphanumerics, hyphens, and forward slash.
    assert!(
        branch
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '/'),
        "branch name must be git-ref-safe: {branch}"
    );
}

// ─── Zero-padded container filename when numeric_id is None ──────────────────

#[test]
fn issue_numeric_id_none_produces_work_item_number_zero() {
    // When source_id does not end in a numeric segment, numeric_id() returns None.
    let issue = make_issue(
        "https://github.com/owner/repo/issues/not-a-number",
        "Some Title",
    );
    assert_eq!(
        issue.numeric_id(),
        None,
        "non-numeric last segment must return None"
    );

    let slug = GithubIssueSource.title_slug(&issue);
    let filename = format!("{:04}-{slug}.md", issue.numeric_id().unwrap_or(0));
    assert!(
        filename.starts_with("0000-"),
        "filename must start with '0000-' when numeric_id is None, got: {filename}"
    );
}

// ─── Temp file write and cleanup ──────────────────────────────────────────────

#[test]
fn issue_temp_file_write_and_cleanup() {
    // Simulates the exec workflow temp file lifecycle:
    // write → verify exists → delete → verify gone.
    let tmp = tempfile::tempdir().unwrap();
    let issue = make_issue("https://github.com/owner/repo/issues/84", "Test Title");
    let slug = GithubIssueSource.title_slug(&issue);
    let pid = std::process::id();
    let temp_path = tmp.path().join(format!("awman-issue-{pid}-{slug}.md"));
    let content = "# Test Title\n\nBody text.";

    std::fs::write(&temp_path, content).expect("write temp file");
    assert!(temp_path.exists(), "temp file must exist after write");

    let read_back = std::fs::read_to_string(&temp_path).expect("read temp file");
    assert_eq!(
        read_back, content,
        "read-back content must match written content"
    );

    std::fs::remove_file(&temp_path).expect("delete temp file");
    assert!(
        !temp_path.exists(),
        "temp file must not exist after deletion"
    );
}
