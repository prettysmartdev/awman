//! Work item reference grammar (Layer 0, pure).
//!
//! [`WorkItemRef::parse`] classifies the text a user typed for `--work-item`
//! (or `specs amend`). It performs no config lookup, file access, Git call or
//! network request. The first matching rule wins:
//!
//! | Input | Result |
//! |---|---|
//! | `https://github.com/<o>/<r>/issues/<N>` | [`WorkItemRef::Github`] with a repository |
//! | any other `http(s)://` URL | [`WorkItemRef::HttpUrl`] |
//! | `github:<N>` | [`WorkItemRef::Github`] without a repository |
//! | `local:<N>` | [`WorkItemRef::LocalNumber`] |
//! | `http:<name>:<id>` | [`WorkItemRef::Http`] (the id may hold `:`, `/`, `#`) |
//! | `#<N>` | [`WorkItemRef::Github`] without a repository |
//! | `owner/repo#N` | [`WorkItemRef::Github`] with a repository |
//! | a path separator, or a `.md` suffix | [`WorkItemRef::LocalPath`] |
//! | anything else (`42`, `0042`, `WI-42`, `PROJ-123`) | [`WorkItemRef::Bare`] |
//!
//! Only `Bare` follows the configured `workItems.source`; every other form is
//! explicit and wins over it.

use std::fmt;
use std::path::PathBuf;

use reqwest::Url;

/// The `owner/repo` half of a GitHub reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GithubRepository {
    pub owner: String,
    pub repo: String,
}

impl GithubRepository {
    /// GitHub owner and repository names are case-insensitive.
    pub fn same_as(&self, other: &Self) -> bool {
        self.owner.eq_ignore_ascii_case(&other.owner) && self.repo.eq_ignore_ascii_case(&other.repo)
    }
}

/// A parsed work item reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkItemRef {
    /// A GitHub issue. `repository` is `None` for `github:N` / `#N`, which
    /// mean "issue `N` on `origin`".
    Github {
        repository: Option<GithubRepository>,
        number: u32,
    },
    /// An `http(s)` URL that is not a GitHub issue; it needs a configured
    /// provider whose `baseUrl` contains it.
    HttpUrl(Url),
    /// `http:<name>:<id>`; `id` stays raw (it becomes one encoded path segment).
    Http { provider: String, id: String },
    /// `local:<N>`: a numbered file in `workItems.dir`.
    LocalNumber(u32),
    /// A path (separator or `.md` suffix) naming a local file.
    LocalPath(PathBuf),
    /// Anything else; resolved against the configured source.
    Bare(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkItemRefError {
    Empty,
    InvalidGithubReference { input: String },
    InvalidNumber { input: String },
    InvalidProviderName { name: String },
    MissingHttpId,
    InvalidUrl { input: String, reason: String },
    UnsupportedScheme { scheme: String },
    InvalidCharacter,
}

impl fmt::Display for WorkItemRefError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "work item reference is empty"),
            Self::InvalidGithubReference { input } => write!(
                f,
                "invalid GitHub issue reference {:?}; use github:<number>, #<number>, \
                 owner/repo#<number> or a github.com issue URL (numbers are positive integers)",
                redact_input(input)
            ),
            Self::InvalidNumber { input } => {
                write!(f, "invalid work item number in {input:?}; expected digits")
            }
            Self::InvalidProviderName { name } => write!(
                f,
                "invalid provider name {name:?}; names match [a-z0-9][a-z0-9_-]{{0,62}}"
            ),
            Self::MissingHttpId => {
                write!(f, "http reference needs an id; use http:<name>:<id>")
            }
            Self::InvalidUrl { input, reason } => {
                write!(
                    f,
                    "invalid work item URL {:?}: {reason}",
                    redact_input(input)
                )
            }
            Self::UnsupportedScheme { scheme } => write!(
                f,
                "unsupported URL scheme {scheme:?}; only http and https are accepted"
            ),
            Self::InvalidCharacter => {
                write!(f, "work item reference contains a control character")
            }
        }
    }
}

impl std::error::Error for WorkItemRefError {}

/// What an error may echo of a URL-shaped input: never its userinfo, query or
/// fragment, even when it does not parse. Other inputs are echoed whole.
fn redact_input(input: &str) -> String {
    let Some(scheme_end) = input.find("://") else {
        return input.to_string();
    };
    let (scheme, rest) = input.split_at(scheme_end + 3);
    let rest = rest.split(['?', '#']).next().unwrap_or_default();
    let authority_end = rest.find('/').unwrap_or(rest.len());
    let rest = match rest[..authority_end].rfind('@') {
        Some(at) => &rest[at + 1..],
        None => rest,
    };
    format!("{scheme}{rest}")
}

impl WorkItemRef {
    /// Parse `input`; see the module docs for the grammar.
    pub fn parse(input: &str) -> Result<Self, WorkItemRefError> {
        let text = input.trim();
        if text.is_empty() {
            return Err(WorkItemRefError::Empty);
        }
        if text.chars().any(char::is_control) {
            return Err(WorkItemRefError::InvalidCharacter);
        }

        if let Some(scheme) = url_scheme(text) {
            return parse_url(text, scheme);
        }
        // Explicit prefixes come before the hash forms, so an HTTP id such as
        // `http:tracker:team/ticket#7` is never read as `owner/repo#N`.
        if let Some(rest) = strip_prefix_ci(text, "github:") {
            let number = parse_github_number(rest, text)?;
            return Ok(Self::Github {
                repository: None,
                number,
            });
        }
        if let Some(rest) = strip_prefix_ci(text, "local:") {
            return match parse_digits(rest) {
                Some(n) => Ok(Self::LocalNumber(n)),
                None => Err(WorkItemRefError::InvalidNumber {
                    input: text.to_string(),
                }),
            };
        }
        if let Some(rest) = strip_prefix_ci(text, "http:") {
            return parse_http_form(rest);
        }
        if text.contains('#') {
            if let Some(parsed) = parse_hash_form(text)? {
                return Ok(parsed);
            }
        }
        if text.contains('/') || text.contains('\\') || ends_with_md(text) {
            return Ok(Self::LocalPath(PathBuf::from(text)));
        }
        Ok(Self::Bare(text.to_string()))
    }
}

/// `scheme` when `text` starts with `<scheme>://`. A Windows drive path
/// (`C:\x`) has no `//` after the colon, so it is never mistaken for one.
fn url_scheme(text: &str) -> Option<&str> {
    let end = text.find("://")?;
    let scheme = &text[..end];
    let mut chars = scheme.chars();
    let first = chars.next()?;
    if first.is_ascii_alphabetic()
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
    {
        Some(scheme)
    } else {
        None
    }
}

fn parse_url(text: &str, scheme: &str) -> Result<WorkItemRef, WorkItemRefError> {
    if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
        return Err(WorkItemRefError::UnsupportedScheme {
            scheme: scheme.to_ascii_lowercase(),
        });
    }
    let url = Url::parse(text).map_err(|e| WorkItemRefError::InvalidUrl {
        input: text.to_string(),
        reason: e.to_string(),
    })?;
    if url.host_str().is_none() {
        return Err(WorkItemRefError::InvalidUrl {
            input: text.to_string(),
            reason: "missing host".into(),
        });
    }
    let strict_github = url.scheme() == "https"
        && url.host_str() == Some("github.com")
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none();
    if strict_github {
        let segments: Vec<&str> = url.path_segments().map(|s| s.collect()).unwrap_or_default();
        // `/o/r/issues/...` is a GitHub issue reference and must be well formed;
        // any other github.com path is an ordinary URL needing a provider.
        if segments.len() >= 3 && segments[2] == "issues" {
            let malformed = || WorkItemRefError::InvalidGithubReference {
                input: text.to_string(),
            };
            let tail = &segments[3..];
            let number_text = match tail {
                [n] => n,
                [n, ""] => n,
                _ => return Err(malformed()),
            };
            let number = parse_positive(number_text).ok_or_else(malformed)?;
            if !valid_owner(segments[0]) || !valid_repo(segments[1]) {
                return Err(malformed());
            }
            return Ok(WorkItemRef::Github {
                repository: Some(GithubRepository {
                    owner: segments[0].to_string(),
                    repo: segments[1].to_string(),
                }),
                number,
            });
        }
    }
    Ok(WorkItemRef::HttpUrl(url))
}

/// `#N` and `owner/repo#N`. Returns `Ok(None)` when the text is not one of the
/// hash forms (so a path such as `a/b/c#x.md` falls through to the path rule).
fn parse_hash_form(text: &str) -> Result<Option<WorkItemRef>, WorkItemRefError> {
    let malformed = || WorkItemRefError::InvalidGithubReference {
        input: text.to_string(),
    };
    if let Some(rest) = text.strip_prefix('#') {
        let number = parse_positive(rest).ok_or_else(malformed)?;
        return Ok(Some(WorkItemRef::Github {
            repository: None,
            number,
        }));
    }
    let Some((prefix, number_text)) = text.rsplit_once('#') else {
        return Ok(None);
    };
    let mut parts = prefix.split('/');
    let (Some(owner), Some(repo), None) = (parts.next(), parts.next(), parts.next()) else {
        return Ok(None);
    };
    // Exactly one `/` before the `#` is a GitHub attempt: a malformed owner or
    // repo is an error, never a fallback to a local path.
    if owner.is_empty() || repo.is_empty() {
        return Ok(None);
    }
    if !valid_owner(owner) || !valid_repo(repo) {
        return Err(malformed());
    }
    let number = parse_positive(number_text).ok_or_else(malformed)?;
    Ok(Some(WorkItemRef::Github {
        repository: Some(GithubRepository {
            owner: owner.to_string(),
            repo: repo.to_string(),
        }),
        number,
    }))
}

fn parse_http_form(rest: &str) -> Result<WorkItemRef, WorkItemRefError> {
    let Some((name, id)) = rest.split_once(':') else {
        // `http:name` with no id; `http:` alone has an empty name.
        return if valid_provider_name(rest) {
            Err(WorkItemRefError::MissingHttpId)
        } else {
            Err(WorkItemRefError::InvalidProviderName {
                name: rest.to_string(),
            })
        };
    };
    if !valid_provider_name(name) {
        return Err(WorkItemRefError::InvalidProviderName {
            name: name.to_string(),
        });
    }
    if id.trim().is_empty() {
        return Err(WorkItemRefError::MissingHttpId);
    }
    Ok(WorkItemRef::Http {
        provider: name.to_string(),
        id: id.to_string(),
    })
}

fn parse_github_number(rest: &str, whole: &str) -> Result<u32, WorkItemRefError> {
    parse_positive(rest).ok_or_else(|| WorkItemRefError::InvalidGithubReference {
        input: whole.to_string(),
    })
}

/// All-digit text as `u32`; leading zeros are ignored. `None` for empty text,
/// non-digits (including a sign) or overflow.
fn parse_digits(text: &str) -> Option<u32> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

fn parse_positive(text: &str) -> Option<u32> {
    parse_digits(text).filter(|n| *n > 0)
}

fn strip_prefix_ci<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    let head = text.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| &text[prefix.len()..])
}

fn ends_with_md(text: &str) -> bool {
    text.len() >= 3
        && text.is_char_boundary(text.len() - 3)
        && text[text.len() - 3..].eq_ignore_ascii_case(".md")
}

/// Provider names match `[a-z0-9][a-z0-9_-]{0,62}`.
fn valid_provider_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 63
        && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'-'))
}

fn valid_owner(owner: &str) -> bool {
    !owner.is_empty()
        && owner.len() <= 39
        && owner
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

fn valid_repo(repo: &str) -> bool {
    !repo.is_empty()
        && repo.len() <= 100
        && repo != "."
        && repo != ".."
        && repo
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gh(repo: Option<(&str, &str)>, number: u32) -> WorkItemRef {
        WorkItemRef::Github {
            repository: repo.map(|(o, r)| GithubRepository {
                owner: o.into(),
                repo: r.into(),
            }),
            number,
        }
    }

    fn bare(s: &str) -> WorkItemRef {
        WorkItemRef::Bare(s.into())
    }

    #[test]
    fn grammar_table_resolves_every_row() {
        let cases: Vec<(&str, WorkItemRef)> = vec![
            // Row 1: github issue URL (query/fragment do not identify another issue).
            ("https://github.com/o/r/issues/7", gh(Some(("o", "r")), 7)),
            ("https://github.com/o/r/issues/7/", gh(Some(("o", "r")), 7)),
            (
                "https://github.com/o/r/issues/0007",
                gh(Some(("o", "r")), 7),
            ),
            (
                "https://github.com/o/r/issues/7?x=1#c",
                gh(Some(("o", "r")), 7),
            ),
            (
                "  https://github.com/o/r/issues/7  ",
                gh(Some(("o", "r")), 7),
            ),
            // Row 3: owner/repo#N.
            ("owner/repo#42", gh(Some(("owner", "repo")), 42)),
            (
                "my-org/my.repo_x#0042",
                gh(Some(("my-org", "my.repo_x")), 42),
            ),
            // Row 4: github:N and #N.
            ("github:42", gh(None, 42)),
            ("github:0042", gh(None, 42)),
            ("GitHub:42", gh(None, 42)),
            ("#42", gh(None, 42)),
            ("#0042", gh(None, 42)),
            // Row 5: local:N (zero stays valid for compatibility).
            ("local:7", WorkItemRef::LocalNumber(7)),
            ("local:0007", WorkItemRef::LocalNumber(7)),
            ("local:0", WorkItemRef::LocalNumber(0)),
            // Row 6: http:name:id keeps the id raw.
            (
                "http:jira:PROJ-123",
                WorkItemRef::Http {
                    provider: "jira".into(),
                    id: "PROJ-123".into(),
                },
            ),
            (
                "http:wiki:a:b/c d",
                WorkItemRef::Http {
                    provider: "wiki".into(),
                    id: "a:b/c d".into(),
                },
            ),
            // Row 7: path separator or .md.
            ("notes.md", WorkItemRef::LocalPath("notes.md".into())),
            ("NOTES.MD", WorkItemRef::LocalPath("NOTES.MD".into())),
            ("0042-x.md", WorkItemRef::LocalPath("0042-x.md".into())),
            ("a/b", WorkItemRef::LocalPath("a/b".into())),
            ("./0042", WorkItemRef::LocalPath("./0042".into())),
            (
                "../x/0042-y.md",
                WorkItemRef::LocalPath("../x/0042-y.md".into()),
            ),
            ("/abs/file", WorkItemRef::LocalPath("/abs/file".into())),
            (r"dir\file", WorkItemRef::LocalPath(r"dir\file".into())),
            (r"C:\work\x", WorkItemRef::LocalPath(r"C:\work\x".into())),
            ("a/b/c#frag", WorkItemRef::LocalPath("a/b/c#frag".into())),
            // Row 8: bare values follow the configured source.
            ("42", bare("42")),
            ("0042", bare("0042")),
            ("WI-42", bare("WI-42")),
            ("PROJ-123", bare("PROJ-123")),
            ("foo:bar", bare("foo:bar")),
            ("99999999999", bare("99999999999")),
        ];
        for (input, expected) in cases {
            assert_eq!(
                WorkItemRef::parse(input).as_ref(),
                Ok(&expected),
                "input {input:?}"
            );
        }
    }

    #[test]
    fn other_http_urls_become_http_url() {
        for input in [
            "https://tracker.example.com/api/items/9",
            "http://localhost:8080/items/1",
            "https://github.com/o/r/pull/3",
            "https://github.com/o/r",
            "https://gitlab.com/o/r/issues/3",
            // Not the strict GitHub form, so an ordinary URL needing a provider.
            "http://github.com/o/r/issues/3",
            "https://github.com:8443/o/r/issues/3",
            "https://user@github.com/o/r/issues/3",
            "https://notgithub.com/o/r/issues/3",
            "https://github.com.evil.example/o/r/issues/3",
            "https://example.com/doc.md",
        ] {
            match WorkItemRef::parse(input) {
                Ok(WorkItemRef::HttpUrl(url)) => assert!(!url.as_str().is_empty()),
                other => panic!("{input:?} should be HttpUrl, got {other:?}"),
            }
        }
    }

    #[test]
    fn precedence_between_overlapping_rules() {
        // URL beats the `.md` and path-separator rules.
        assert!(matches!(
            WorkItemRef::parse("https://example.com/a/b.md"),
            Ok(WorkItemRef::HttpUrl(_))
        ));
        // A github.com issue URL beats the generic HTTP URL rule.
        assert!(matches!(
            WorkItemRef::parse("https://github.com/o/r/issues/1"),
            Ok(WorkItemRef::Github { .. })
        ));
        // `owner/repo#N` beats the path-separator rule.
        assert_eq!(WorkItemRef::parse("o/r#5"), Ok(gh(Some(("o", "r")), 5)));
        // Explicit prefixes beat `.md`: this is a bad number, not a path.
        assert!(matches!(
            WorkItemRef::parse("local:7.md"),
            Err(WorkItemRefError::InvalidNumber { .. })
        ));
        // `http:` form beats `.md` and path separators inside the id.
        assert_eq!(
            WorkItemRef::parse("http:p:dir/x.md"),
            Ok(WorkItemRef::Http {
                provider: "p".into(),
                id: "dir/x.md".into()
            })
        );
        // `http:` form beats the `owner/repo#N` and `#N` hash forms.
        for id in ["team/ticket#7", "a:b/c#d", "#7", "x/y#z:1"] {
            assert_eq!(
                WorkItemRef::parse(&format!("http:tracker:{id}")),
                Ok(WorkItemRef::Http {
                    provider: "tracker".into(),
                    id: id.into()
                }),
                "{id}"
            );
        }
        // `github:` beats the bare rule even for digits.
        assert_eq!(WorkItemRef::parse("github:5"), Ok(gh(None, 5)));
        // `http://` is a URL, never the `http:<name>:<id>` form.
        assert!(matches!(
            WorkItemRef::parse("http://host/x"),
            Ok(WorkItemRef::HttpUrl(_))
        ));
    }

    #[test]
    fn malformed_values_are_errors_not_fallbacks() {
        use WorkItemRefError as E;
        assert_eq!(WorkItemRef::parse(""), Err(E::Empty));
        assert_eq!(WorkItemRef::parse("   \t "), Err(E::Empty));
        assert_eq!(WorkItemRef::parse("a\nb"), Err(E::InvalidCharacter));
        assert_eq!(WorkItemRef::parse("a\u{0}b"), Err(E::InvalidCharacter));
        for input in [
            "github:abc",
            "github:",
            "github:0",
            "github:-3",
            "github:+3",
            "github:4294967296",
            "#",
            "#abc",
            "#0",
            "o/r#",
            "o/r#abc",
            "o/r#0",
            "bad owner!/r#1",
            "o/..#1",
            "https://github.com/o/r/issues/abc",
            "https://github.com/o/r/issues/0",
            "https://github.com/o/r/issues/",
            "https://github.com/o/r/issues",
            "https://github.com/o/r/issues/1/comments",
            "https://github.com/o/r/issues/%31",
        ] {
            assert!(
                matches!(
                    WorkItemRef::parse(input),
                    Err(E::InvalidGithubReference { .. })
                ),
                "{input:?} -> {:?}",
                WorkItemRef::parse(input)
            );
        }
        assert!(matches!(
            WorkItemRef::parse("local:"),
            Err(E::InvalidNumber { .. })
        ));
        assert!(matches!(
            WorkItemRef::parse("local:abc"),
            Err(E::InvalidNumber { .. })
        ));
        assert!(matches!(
            WorkItemRef::parse("local:-1"),
            Err(E::InvalidNumber { .. })
        ));
        for name in [
            "Upper",
            "-lead",
            "_lead",
            "has space",
            "a.b",
            "",
            &"a".repeat(64),
        ] {
            let input = format!("http:{name}:1");
            assert!(
                matches!(
                    WorkItemRef::parse(&input),
                    Err(E::InvalidProviderName { .. })
                ),
                "{input:?}"
            );
        }
        assert_eq!(WorkItemRef::parse("http:jira"), Err(E::MissingHttpId));
        assert_eq!(WorkItemRef::parse("http:jira:"), Err(E::MissingHttpId));
        assert_eq!(WorkItemRef::parse("http:jira:  "), Err(E::MissingHttpId));
        assert!(matches!(
            WorkItemRef::parse("http:"),
            Err(E::InvalidProviderName { .. })
        ));
        assert!(matches!(
            WorkItemRef::parse("ftp://host/x"),
            Err(E::UnsupportedScheme { .. })
        ));
        assert!(matches!(
            WorkItemRef::parse("file:///etc/passwd"),
            Err(E::UnsupportedScheme { .. })
        ));
        assert!(matches!(
            WorkItemRef::parse("http://"),
            Err(E::InvalidUrl { .. })
        ));
    }

    #[test]
    fn provider_name_at_the_length_limit_is_accepted() {
        let name = "a".repeat(63);
        assert_eq!(
            WorkItemRef::parse(&format!("http:{name}:x")),
            Ok(WorkItemRef::Http {
                provider: name,
                id: "x".into()
            })
        );
    }

    #[test]
    fn error_messages_are_actionable() {
        let msg = WorkItemRef::parse("github:abc").unwrap_err().to_string();
        assert!(msg.contains("github:<number>"), "{msg}");
    }

    #[test]
    fn errors_never_echo_url_credentials_or_queries() {
        for input in [
            "https://user:tok@host:x/items/1?access_key=SECRET",
            "https://github.com/o/r/issues/abc?access_key=SECRET#tok",
        ] {
            let msg = WorkItemRef::parse(input).unwrap_err().to_string();
            assert!(!msg.contains("SECRET") && !msg.contains("tok"), "{msg}");
        }
        assert_eq!(redact_input("o/r#abc"), "o/r#abc");
        assert_eq!(redact_input("https://u:p@h/a/b?q=1#f"), "https://h/a/b");
    }
}
