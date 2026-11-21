//! Plain provider values and errors shared by Layers 1 and 2.
use std::fmt;

#[derive(Clone)]
pub struct Issue {
    pub source_id: String,
    pub id: String,
    pub number: Option<u32>,
    pub title: String,
    /// Raw markdown, without a prepended title.
    pub body: String,
    /// Stable routing key: `github` or `http:<name>`.
    pub provider: String,
}

impl fmt::Debug for Issue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Issue")
            .field("source_id", &display_url(&self.source_id))
            .field("number", &self.number)
            .field("body", &"[redacted]")
            .field("provider", &self.provider)
            .finish_non_exhaustive()
    }
}
impl Issue {
    pub fn numeric_id(&self) -> Option<u32> {
        self.number
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IssueOperation {
    Fetch,
    Create,
    Update,
}

pub enum IssueSourceError {
    NotFound {
        provider: String,
        source_id: String,
    },
    Unauthorized {
        provider: String,
        hint: String,
    },
    Forbidden {
        provider: String,
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
    Timeout {
        provider: String,
        operation: IssueOperation,
    },
    CreateOutcomeUnknown {
        provider: String,
        detail: String,
    },
    AmbiguousProvider {
        providers: Vec<String>,
    },
    UnsafeUrl {
        provider: String,
        reason: String,
    },
    ResponseTooLarge {
        provider: String,
        limit: u64,
    },
}
impl fmt::Debug for IssueSourceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

/// Diagnostic URLs never include credentials, queries or fragments. Used for
/// every user-visible or persisted remote `source_id`.
pub fn display_url(input: &str) -> String {
    if let Ok(mut url) = reqwest::Url::parse(input) {
        url.set_query(None);
        url.set_fragment(None);
        let _ = url.set_username("");
        let _ = url.set_password(None);
        url.to_string()
    } else {
        input
            .split(['?', '#'])
            .next()
            .unwrap_or_default()
            .to_string()
    }
}
impl fmt::Display for IssueSourceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound {
                provider,
                source_id,
            } => write!(f, "{provider}: issue not found: {}", display_url(source_id)),
            Self::Unauthorized { provider, hint } => write!(f, "{provider}: unauthorized — {hint}"),
            Self::Forbidden { provider, hint } => {
                write!(f, "{provider}: write permission denied — {hint}")
            }
            Self::RateLimited { provider } => write!(f, "{provider}: API rate limit exceeded"),
            Self::InvalidRef {
                provider,
                input,
                hint,
            } => write!(
                f,
                "{provider}: invalid issue reference '{}': {hint}",
                display_url(input)
            ),
            Self::NoRemoteDetected { provider } => write!(
                f,
                "{provider}: no {provider} remote detected for this repository"
            ),
            Self::NoMatchingProvider { input } => {
                write!(f, "no issue provider can handle '{}'", display_url(input))
            }
            Self::Network { provider, detail } => write!(f, "{provider}: network error: {detail}"),
            Self::ProviderError { provider, detail } => write!(f, "{provider}: {detail}"),
            Self::Timeout {
                provider,
                operation,
            } => write!(f, "{provider}: {operation:?} timed out"),
            Self::CreateOutcomeUnknown { provider, detail } => write!(
                f,
                "{provider}: create outcome unknown; check the tracker before retrying: {detail}"
            ),
            Self::AmbiguousProvider { providers } => {
                write!(f, "ambiguous work item providers: {}", providers.join(", "))
            }
            Self::UnsafeUrl { provider, reason } => write!(f, "{provider}: unsafe URL: {reason}"),
            Self::ResponseTooLarge { provider, limit } => {
                write!(f, "{provider}: response exceeds {limit} bytes")
            }
        }
    }
}
impl std::error::Error for IssueSourceError {}

/// Convert text to a filename/ref-safe ASCII slug.
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
    if trimmed.len() <= max_len {
        return trimmed.to_string();
    }
    let cut = &trimmed[..max_len];
    match cut.rfind('-') {
        Some(pos) => cut[..pos].to_string(),
        None => cut.trim_end_matches('-').to_string(),
    }
}
