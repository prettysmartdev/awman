//! Pure DTO and filename/ref-safe naming contracts.
use super::{Issue, IssueSource, IssueSourceError};
use proptest::prelude::*;
use std::path::Path;

struct Naming;
impl IssueSource for Naming {
    fn provider_name(&self) -> &str {
        "example"
    }
    fn provider_prefix(&self) -> &str {
        "ghb"
    }
    fn issue_identifier(&self, issue: &Issue) -> String {
        issue.id.clone()
    }
    fn fetch_issue(&self, _: &str, _: &Path) -> Result<Issue, IssueSourceError> {
        unreachable!()
    }
}
fn issue(id: &str, number: Option<u32>, title: &str) -> Issue {
    Issue {
        source_id: format!("https://example.com/{id}"),
        id: id.into(),
        number,
        title: title.into(),
        body: String::new(),
        provider: "http:example".into(),
    }
}
#[test]
fn optional_number_is_authoritative() {
    assert_eq!(issue("84", None, "Title").numeric_id(), None);
    assert_eq!(issue("opaque", Some(84), "Title").numeric_id(), Some(84));
}
#[test]
fn names_include_the_provider_and_safe_logical_id() {
    let source = Naming;
    assert_eq!(
        source.title_slug(&issue("84", Some(84), "GitHub Integration Part 1")),
        "ghb84-github-integration-part-1"
    );
    assert_eq!(
        source.title_slug(&issue("a/../../unsafe?", None, "Title")),
        "ghba-unsafe-title"
    );
}
#[test]
fn optional_mutations_report_not_supported() {
    assert!(
        matches!(Naming.create_issue("t", "b", Path::new("")), Err(IssueSourceError::ProviderError { detail, .. }) if detail.contains("not supported"))
    );
    assert!(
        matches!(Naming.update_issue(&issue("1", None, "t"), "b"), Err(IssueSourceError::ProviderError { detail, .. }) if detail.contains("not supported"))
    );
}
proptest! {
    #[test]
    fn entire_slug_is_safe(id in ".{1,120}", title in ".{0,120}") {
        let slug = Naming.title_slug(&issue(&id, None, &title));
        prop_assert!(!slug.is_empty());
        prop_assert!(slug.len() <= 100);
        prop_assert!(slug.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));
    }
}
