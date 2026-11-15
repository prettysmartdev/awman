//! Layer 1 provider transport. Source selection and retry policy belong to Layer 2.
pub mod github;
pub mod http;
pub mod router;
mod transport;

#[cfg(test)]
mod naming_tests;

pub use crate::data::issue::{slugify, Issue, IssueOperation, IssueSourceError};
use crate::data::message::UserMessageSink;
pub use github::GithubIssueSource;
pub use http::HttpIssueSource;
pub use router::{IssueSourceRouter, RoutedIssueSource};
use std::path::Path;

pub trait IssueSource: Send + Sync {
    fn provider_name(&self) -> &str;
    fn provider_prefix(&self) -> &str;
    fn issue_identifier(&self, issue: &Issue) -> String;
    fn fetch_issue(&self, input: &str, git_root: &Path) -> Result<Issue, IssueSourceError>;

    fn fetch_issue_with_progress(
        &self,
        input: &str,
        git_root: &Path,
        _sink: &mut dyn UserMessageSink,
    ) -> Result<Issue, IssueSourceError> {
        self.fetch_issue(input, git_root)
    }
    fn create_issue(
        &self,
        _title: &str,
        _body: &str,
        _git_root: &Path,
    ) -> Result<Issue, IssueSourceError> {
        Err(IssueSourceError::ProviderError {
            provider: self.provider_name().into(),
            detail: "create_issue is not supported".into(),
        })
    }
    fn update_issue(&self, _issue: &Issue, _body: &str) -> Result<Issue, IssueSourceError> {
        Err(IssueSourceError::ProviderError {
            provider: self.provider_name().into(),
            detail: "update_issue is not supported".into(),
        })
    }
    fn title_slug(&self, issue: &Issue) -> String {
        let title = slugify(&issue.title, 40);
        slugify(
            &format!(
                "{}{}-{title}",
                self.provider_prefix(),
                self.issue_identifier(issue)
            ),
            100,
        )
    }
    fn format_as_markdown(&self, issue: &Issue) -> String {
        if issue.body.is_empty() {
            format!("# {}", issue.title)
        } else {
            format!("# {}\n\n{}", issue.title, issue.body)
        }
    }
}
