//! Provider-generic issue source abstraction.
//!
//! The command layer and workflow engine import only from this module — never
//! from any provider-specific file.

pub mod github;
pub mod router;

use std::fmt;
use std::path::Path;

use crate::data::message::UserMessageSink;

/// Generic output of every `IssueSource`.
#[derive(Debug, Clone)]
pub struct Issue {
    /// Canonical URL of the issue, e.g. "https://github.com/owner/repo/issues/84".
    pub source_id: String,
    pub title: String,
    /// Empty string if the issue has no description.
    pub body: String,
    /// Display name from `IssueSource::provider_name()`.
    pub provider: String,
}

impl Issue {
    /// Parses the last path segment of `source_id` as a u32, if possible.
    /// Returns `Some(84)` for ".../issues/84", `None` for ".../PROJ-123".
    pub fn numeric_id(&self) -> Option<u32> {
        self.source_id
            .rsplit('/')
            .next()
            .and_then(|s| s.parse::<u32>().ok())
    }
}

/// Errors from issue source operations.
#[derive(Debug)]
pub enum IssueSourceError {
    NotFound {
        provider: String,
        source_id: String,
    },
    Unauthorized {
        provider: String,
        /// Provider-supplied hint for resolving the auth issue. Empty if no
        /// hint applies. The trait-level Display does not embed any
        /// provider-specific text — providers populate this field.
        hint: String,
    },
    RateLimited {
        provider: String,
    },
    InvalidRef {
        provider: String,
        input: String,
        hint: String,
    },
    NoRemoteDetected {
        provider: String,
    },
    NoMatchingProvider {
        input: String,
    },
    Network {
        provider: String,
        detail: String,
    },
    ProviderError {
        provider: String,
        detail: String,
    },
}

impl fmt::Display for IssueSourceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IssueSourceError::NotFound {
                provider,
                source_id,
            } => write!(f, "{provider}: issue not found: {source_id}"),
            IssueSourceError::Unauthorized { provider, hint } => {
                if hint.is_empty() {
                    write!(f, "{provider}: unauthorized")
                } else {
                    write!(f, "{provider}: unauthorized — {hint}")
                }
            }
            IssueSourceError::RateLimited { provider } => {
                write!(f, "{provider}: API rate limit exceeded")
            }
            IssueSourceError::InvalidRef {
                provider,
                input,
                hint,
            } => write!(f, "{provider}: invalid issue reference '{input}': {hint}"),
            IssueSourceError::NoRemoteDetected { provider } => {
                write!(
                    f,
                    "{provider}: no {provider} remote detected for this repository"
                )
            }
            IssueSourceError::NoMatchingProvider { input } => {
                write!(f, "no issue provider can handle '{input}'")
            }
            IssueSourceError::Network { provider, detail } => {
                write!(f, "{provider}: network error: {detail}")
            }
            IssueSourceError::ProviderError { provider, detail } => {
                write!(f, "{provider}: {detail}")
            }
        }
    }
}

impl std::error::Error for IssueSourceError {}

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

    /// Render the issue as markdown for use in prompts and work item files.
    fn format_as_markdown(&self, issue: &Issue) -> String {
        if issue.body.is_empty() {
            format!("# {}", issue.title)
        } else {
            format!("# {}\n\n{}", issue.title, issue.body)
        }
    }
}

/// Carries the `--issue` flag value. Composed into command flag structs.
#[derive(Debug, Clone, Default)]
pub struct IssueSourceFlags {
    pub issue: Option<String>,
}

const OVERALL_SLUG_MAX: usize = 100;
const TITLE_SLUG_MAX: usize = 40;

/// Converts arbitrary text to a hyphen-delimited, lowercase slug safe for
/// use in filenames and git branch names.
pub fn slugify(text: &str, max_len: usize) -> String {
    let mut out = String::new();
    let mut last_dash = true;
    for c in text.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    let trimmed = out.trim_matches('-');
    if trimmed.is_empty() {
        return String::new();
    }
    if trimmed.len() <= max_len {
        return trimmed.to_string();
    }
    // Truncate at a word boundary: find the last hyphen within the limit and
    // cut there, so we never split mid-word.
    let cut = &trimmed[..max_len];
    if let Some(last_hyphen) = cut.rfind('-') {
        cut[..last_hyphen].to_string()
    } else {
        cut.trim_end_matches('-').to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numeric_id_parses_last_segment() {
        let issue = Issue {
            source_id: "https://github.com/owner/repo/issues/84".into(),
            title: "test".into(),
            body: String::new(),
            provider: "GitHub".into(),
        };
        assert_eq!(issue.numeric_id(), Some(84));
    }

    #[test]
    fn numeric_id_returns_none_for_non_numeric() {
        let issue = Issue {
            source_id: "https://jira.example.com/PROJ-123".into(),
            title: "test".into(),
            body: String::new(),
            provider: "Jira".into(),
        };
        assert_eq!(issue.numeric_id(), None);
    }

    #[test]
    fn format_as_markdown_default_with_body() {
        struct Dummy;
        impl IssueSource for Dummy {
            fn provider_name(&self) -> &str {
                "Test"
            }
            fn provider_prefix(&self) -> &str {
                "tst"
            }
            fn issue_identifier(&self, _: &Issue) -> String {
                "0".into()
            }
            fn can_handle(&self, _: &str) -> bool {
                false
            }
            fn fetch_issue(&self, _: &str, _: &Path) -> Result<Issue, IssueSourceError> {
                unimplemented!()
            }
        }
        let issue = Issue {
            source_id: String::new(),
            title: "My Title".into(),
            body: "Some body".into(),
            provider: "Test".into(),
        };
        assert_eq!(Dummy.format_as_markdown(&issue), "# My Title\n\nSome body");
    }

    #[test]
    fn format_as_markdown_default_empty_body() {
        struct Dummy;
        impl IssueSource for Dummy {
            fn provider_name(&self) -> &str {
                "Test"
            }
            fn provider_prefix(&self) -> &str {
                "tst"
            }
            fn issue_identifier(&self, _: &Issue) -> String {
                "0".into()
            }
            fn can_handle(&self, _: &str) -> bool {
                false
            }
            fn fetch_issue(&self, _: &str, _: &Path) -> Result<Issue, IssueSourceError> {
                unimplemented!()
            }
        }
        let issue = Issue {
            source_id: String::new(),
            title: "Title Only".into(),
            body: String::new(),
            provider: "Test".into(),
        };
        assert_eq!(Dummy.format_as_markdown(&issue), "# Title Only");
    }

    #[test]
    fn slugify_basic() {
        assert_eq!(slugify("Hello World", 50), "hello-world");
        assert_eq!(slugify("Foo!Bar?Baz", 50), "foo-bar-baz");
        assert_eq!(slugify("  trim  edges  ", 50), "trim-edges");
        assert_eq!(slugify("", 50), "");
    }

    #[test]
    fn slugify_non_ascii() {
        assert_eq!(slugify("café résumé", 50), "caf-r-sum");
    }

    #[test]
    fn slugify_truncation() {
        assert_eq!(
            slugify("github-integration-part-1", 20),
            "github-integration"
        );
        assert_eq!(slugify("abcde-fghij", 6), "abcde");
    }

    #[test]
    fn slugify_all_special() {
        assert_eq!(slugify("!!@@##", 50), "");
    }

    #[test]
    fn slugify_leading_trailing_hyphens() {
        assert_eq!(slugify("---hello---", 50), "hello");
    }

    // ── Additional numeric_id tests ───────────────────────────────────────────

    #[test]
    fn numeric_id_empty_source_id_returns_none() {
        let issue = Issue {
            source_id: String::new(),
            title: String::new(),
            body: String::new(),
            provider: "Test".into(),
        };
        assert_eq!(issue.numeric_id(), None);
    }

    #[test]
    fn numeric_id_no_trailing_slash_segment_returns_none() {
        let issue = Issue {
            source_id: "https://example.com/PROJ-123".into(),
            title: String::new(),
            body: String::new(),
            provider: "Test".into(),
        };
        // "PROJ-123" is not a u32
        assert_eq!(issue.numeric_id(), None);
    }

    // ── Additional format_as_markdown tests ───────────────────────────────────

    #[test]
    fn format_as_markdown_unicode_content() {
        struct Dummy;
        impl IssueSource for Dummy {
            fn provider_name(&self) -> &str {
                "Test"
            }
            fn provider_prefix(&self) -> &str {
                "tst"
            }
            fn issue_identifier(&self, _: &Issue) -> String {
                "0".into()
            }
            fn can_handle(&self, _: &str) -> bool {
                false
            }
            fn fetch_issue(&self, _: &str, _: &Path) -> Result<Issue, IssueSourceError> {
                unimplemented!()
            }
        }
        let issue = Issue {
            source_id: String::new(),
            title: "café résumé".into(),
            body: "Ünïcödé body 🦀".into(),
            provider: "Test".into(),
        };
        let md = Dummy.format_as_markdown(&issue);
        assert_eq!(md, "# café résumé\n\nÜnïcödé body 🦀");
    }

    #[test]
    fn format_as_markdown_special_chars_in_title() {
        struct Dummy;
        impl IssueSource for Dummy {
            fn provider_name(&self) -> &str {
                "Test"
            }
            fn provider_prefix(&self) -> &str {
                "tst"
            }
            fn issue_identifier(&self, _: &Issue) -> String {
                "0".into()
            }
            fn can_handle(&self, _: &str) -> bool {
                false
            }
            fn fetch_issue(&self, _: &str, _: &Path) -> Result<Issue, IssueSourceError> {
                unimplemented!()
            }
        }
        let issue = Issue {
            source_id: String::new(),
            title: "Fix: bug <script>alert(1)</script>".into(),
            body: "Details".into(),
            provider: "Test".into(),
        };
        let md = Dummy.format_as_markdown(&issue);
        assert!(md.starts_with("# Fix: bug"));
        assert!(md.contains("Details"));
    }

    // ── Additional slugify tests ──────────────────────────────────────────────

    #[test]
    fn slugify_consecutive_specials_collapse_to_single_hyphen() {
        assert_eq!(slugify("foo!!!bar", 50), "foo-bar");
        assert_eq!(slugify("a---b", 50), "a-b");
        assert_eq!(slugify("x  y  z", 50), "x-y-z");
    }

    #[test]
    fn slugify_mixed_alphanumeric_and_special_chars() {
        assert_eq!(slugify("foo123-bar456", 50), "foo123-bar456");
        assert_eq!(slugify("abc!@#123", 50), "abc-123");
    }

    #[test]
    fn slugify_max_len_zero_returns_empty() {
        // With max_len = 0, any non-empty input should return empty string.
        let result = slugify("hello", 0);
        assert!(
            result.is_empty(),
            "max_len=0 must return empty string, got: {result:?}"
        );
    }

    #[test]
    fn slugify_exactly_at_max_len_no_truncation() {
        // "hello" is 5 chars — exactly at max_len=5, no truncation.
        assert_eq!(slugify("hello", 5), "hello");
        // "hello-world" is 11 chars — at max_len=11, no truncation.
        assert_eq!(slugify("hello world", 11), "hello-world");
    }
}
