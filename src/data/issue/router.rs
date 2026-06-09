//! `IssueSourceRouter` — selects the correct `IssueSource` at runtime.

use std::path::Path;

use crate::engine::message::UserMessageSink;

use super::github::GithubIssueSource;
use super::{Issue, IssueSource, IssueSourceError};

pub struct IssueSourceRouter {
    sources: Vec<Box<dyn IssueSource>>,
}

impl Default for IssueSourceRouter {
    /// Constructs a router with all built-in providers registered.
    /// GitHub is first (claims bare integers). Future providers are added after.
    fn default() -> Self {
        Self {
            sources: vec![Box::new(GithubIssueSource)],
        }
    }
}

impl IssueSourceRouter {
    /// Returns the first provider whose `can_handle(input)` returns true.
    pub fn route(&self, input: &str) -> Result<&dyn IssueSource, IssueSourceError> {
        for source in &self.sources {
            if source.can_handle(input) {
                return Ok(source.as_ref());
            }
        }
        Err(IssueSourceError::NoMatchingProvider {
            input: input.to_string(),
        })
    }

    /// Convenience: route, then fetch. Returns both the Issue and the source.
    pub fn fetch_issue(
        &self,
        input: &str,
        git_root: &Path,
    ) -> Result<(Issue, &dyn IssueSource), IssueSourceError> {
        let source = self.route(input)?;
        let issue = source.fetch_issue(input, git_root)?;
        Ok((issue, source))
    }

    /// Like `fetch_issue`, but writes progress messages to the sink.
    pub fn fetch_issue_with_progress(
        &self,
        input: &str,
        git_root: &Path,
        sink: &mut dyn UserMessageSink,
    ) -> Result<(Issue, &dyn IssueSource), IssueSourceError> {
        let source = self.route(input)?;
        let issue = source.fetch_issue_with_progress(input, git_root, sink)?;
        Ok((issue, source))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn router_routes_bare_integer_to_github() {
        let router = IssueSourceRouter::default();
        let source = router.route("84").unwrap();
        assert_eq!(source.provider_name(), "GitHub");
    }

    #[test]
    fn router_routes_github_url_to_github() {
        let router = IssueSourceRouter::default();
        let source = router
            .route("https://github.com/owner/repo/issues/42")
            .unwrap();
        assert_eq!(source.provider_name(), "GitHub");
    }

    #[test]
    fn router_routes_short_form_to_github() {
        let router = IssueSourceRouter::default();
        let source = router.route("owner/repo#84").unwrap();
        assert_eq!(source.provider_name(), "GitHub");
    }

    #[test]
    fn router_returns_no_matching_provider_for_unknown_input() {
        let router = IssueSourceRouter::default();
        let result = router.route("not-any-provider://something");
        match result {
            Err(IssueSourceError::NoMatchingProvider { input }) => {
                assert_eq!(input, "not-any-provider://something");
            }
            Err(other) => panic!("expected NoMatchingProvider, got error: {other}"),
            Ok(_) => panic!("expected error but got Ok"),
        }
    }

    #[test]
    fn router_returns_github_for_bare_integer_when_ambiguous() {
        // GitHub is registered first and claims bare integers.
        // Verify the priority ordering holds.
        let router = IssueSourceRouter::default();
        let source = router.route("1").unwrap();
        assert_eq!(
            source.provider_name(),
            "GitHub",
            "GitHub must be first in priority order for bare integers"
        );
    }
}
