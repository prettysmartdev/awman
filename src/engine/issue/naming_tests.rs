//! Names derived from an issue (re-derived from the legacy
//! `tests/engine/issue_integration.rs`): the provider-prefixed title slug is
//! filename- and ref-safe, the numeric id drives the zero-padded work-item
//! file name, and the workflow branch follows `awman/workflow-<slug>`.
//! In memory only.

use super::{GithubIssueSource, Issue, IssueSource};
use crate::data::worktree_paths::worktree_branch_name_for_workflow;
use proptest::prelude::*;

fn issue(source_id: &str, title: &str) -> Issue {
    Issue {
        source_id: source_id.to_string(),
        title: title.to_string(),
        body: String::new(),
        provider: "GitHub".to_string(),
    }
}

#[test]
fn numeric_id_comes_from_the_last_url_segment() {
    let numbered = issue("https://github.com/owner/repo/issues/84", "Test Title");
    assert_eq!(numbered.numeric_id(), Some(84));
    assert_eq!(format!("{:04}", numbered.numeric_id().unwrap_or(0)), "0084");
    let unnumbered = issue("https://github.com/owner/repo/issues/not-a-number", "T");
    assert_eq!(unnumbered.numeric_id(), None);
    assert_eq!(
        format!("{:04}", unnumbered.numeric_id().unwrap_or(0)),
        "0000"
    );
}

#[test]
fn github_slugs_carry_the_provider_prefix_and_the_number() {
    let i = issue(
        "https://github.com/owner/repo/issues/84",
        "GitHub Integration Part 1",
    );
    let slug = GithubIssueSource.title_slug(&i);
    assert!(slug.starts_with("ghb"), "{slug}");
    assert!(slug.contains("84"), "{slug}");
    let branch = worktree_branch_name_for_workflow(&slug);
    assert_eq!(branch, format!("awman/workflow-{slug}"));
}

proptest! {
    /// Whatever the title, the slug is filename-safe and the derived branch
    /// is git-ref-safe (alphanumerics, `-`, `/`).
    #[test]
    fn slugs_and_branches_are_safe_for_any_title(title in ".{0,120}", n in 0u32..100_000) {
        let i = issue(&format!("https://github.com/o/r/issues/{n}"), &title);
        let slug = GithubIssueSource.title_slug(&i);
        prop_assert!(!slug.is_empty());
        prop_assert!(slug.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'), "{}", slug);
        let branch = worktree_branch_name_for_workflow(&slug);
        prop_assert!(branch
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '/'));
    }
}
