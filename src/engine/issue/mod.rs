//! Layer 1 issue providers.
//!
//! Provider implementations own git, process, and network access. The plain
//! issue values and source errors remain in Layer 0 and are re-exported here
//! so callers have one engine-facing module.

pub mod github;
pub mod router;

#[cfg(test)]
mod naming_tests;

use std::path::Path;

use crate::data::message::UserMessageSink;

pub use crate::data::issue::{slugify, Issue, IssueSourceError, IssueSourceFlags};
pub use github::GithubIssueSource;
pub use router::IssueSourceRouter;

const OVERALL_SLUG_MAX: usize = 100;
const TITLE_SLUG_MAX: usize = 40;

/// Trait for issue source providers.
pub trait IssueSource: Send + Sync {
    /// Human-readable provider name, e.g. "GitHub", "Jira", "Linear".
    fn provider_name(&self) -> &str;

    /// Three-character provider prefix for slugs, e.g. "ghb" for GitHub,
    /// "jra" for Jira, "lnr" for Linear.
    fn provider_prefix(&self) -> &str;

    /// Provider-specific issue identifier as a string. For GitHub this is
    /// the numeric issue number ("84"); for Jira it might be "PROJ-123".
    fn issue_identifier(&self, issue: &Issue) -> String;

    /// Returns true if this provider can handle the given input string.
    /// Must be infallible and perform no I/O — pattern matching only.
    fn can_handle(&self, input: &str) -> bool;

    /// Fetch the issue identified by `input`, using `git_root` for context
    /// (e.g. detecting the remote URL for bare numeric refs).
    fn fetch_issue(&self, input: &str, git_root: &Path) -> Result<Issue, IssueSourceError>;

    /// Fetch using the engine dependencies supplied by the command layer.
    /// Providers that need external systems override this; the default keeps
    /// custom providers source-compatible with the basic trait.
    fn fetch_issue_with_engine(
        &self,
        input: &str,
        git_root: &Path,
        _git_engine: &crate::engine::git::GitEngine,
        _github_token: Option<&str>,
    ) -> Result<Issue, IssueSourceError> {
        self.fetch_issue(input, git_root)
    }

    /// Returns a hyphen-delimited, lowercase slug that uniquely identifies
    /// this issue. Format: `{provider_prefix}{issue_id}-{truncated_title}`.
    /// Used as the slug component of work item filenames and git branch names.
    fn title_slug(&self, issue: &Issue) -> String {
        let id = self.issue_identifier(issue);
        let prefix = format!("{}{}", self.provider_prefix(), id);
        let remaining = OVERALL_SLUG_MAX.saturating_sub(prefix.len() + 1);
        let title_budget = TITLE_SLUG_MAX.min(remaining);
        let title_part = if title_budget == 0 {
            String::new()
        } else {
            slugify(&issue.title, title_budget)
        };
        if title_part.is_empty() {
            prefix
        } else {
            format!("{prefix}-{title_part}")
        }
    }

    /// Like `fetch_issue`, but writes progress messages to the sink so the
    /// user sees which external commands or API requests are being performed.
    /// Default: delegates to `fetch_issue` with no progress output.
    fn fetch_issue_with_progress(
        &self,
        input: &str,
        git_root: &Path,
        _sink: &mut dyn UserMessageSink,
    ) -> Result<Issue, IssueSourceError> {
        self.fetch_issue(input, git_root)
    }

    /// Progress-reporting counterpart to `fetch_issue_with_engine`.
    fn fetch_issue_with_engine_progress(
        &self,
        input: &str,
        git_root: &Path,
        sink: &mut dyn UserMessageSink,
        git_engine: &crate::engine::git::GitEngine,
        github_token: Option<&str>,
    ) -> Result<Issue, IssueSourceError> {
        let _ = (git_engine, github_token);
        self.fetch_issue_with_progress(input, git_root, sink)
    }

    /// Render the issue as markdown for use in prompts and work item files.
    fn format_as_markdown(&self, issue: &Issue) -> String {
        if issue.body.is_empty() {
            format!("# {}", issue.title)
        } else {
            format!("# {}\n\n{}", issue.title, issue.body)
        }
    }
}
