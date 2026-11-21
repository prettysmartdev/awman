//! Host Git transport policy and identifiers safe for public diagnostics.
use serde::Serialize;
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitTransportOp {
    Clone,
    Fetch,
    LsRemote,
    Push,
}
impl GitTransportOp {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Clone => "clone",
            Self::Fetch => "fetch",
            Self::LsRemote => "ls-remote",
            Self::Push => "push",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct SafeRemote(String);
impl SafeRemote {
    pub fn from_url(url: &str) -> Self {
        let identifier = if let Some((scheme, rest)) = url.split_once("://") {
            if scheme.eq_ignore_ascii_case("file") {
                "local repository"
            } else {
                // Remove even malformed pasted userinfo before delimiters.
                let rest = rest.rsplit_once('@').map_or(rest, |(_, host)| host);
                let rest = rest.split(['?', '#']).next().unwrap_or_default();
                let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
                if !host.starts_with('[')
                    && host
                        .split_once(':')
                        .is_some_and(|(_, port)| port.parse::<u16>().is_err())
                {
                    return Self::bounded("remote repository");
                }
                return Self::bounded(&format!("{host}/{path}"));
            }
        } else if let Some((host, path)) = url.split_once(':') {
            if !host.contains('/') && !host.contains('\\') && host.len() > 1 {
                let host = host.rsplit('@').next().unwrap_or_default();
                return Self::bounded(&format!("{host}:{path}"));
            } else {
                "local repository"
            }
        } else {
            "local repository"
        };
        Self::bounded(identifier)
    }
    fn bounded(text: &str) -> Self {
        let mut value = String::new();
        for ch in text.chars().filter(|c| !c.is_control()) {
            if value.len() + ch.len_utf8() > 256 {
                break;
            }
            value.push(ch);
        }
        Self(value)
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
    pub fn url_has_credentials(url: &str) -> bool {
        if let Some((scheme, rest)) = url.split_once("://") {
            if let Some((user, _)) = rest.rsplit_once('@') {
                return scheme.eq_ignore_ascii_case("http")
                    || scheme.eq_ignore_ascii_case("https")
                    || user.contains([':', '/', '?', '#'])
                    || user.to_ascii_lowercase().contains("%3a");
            }
        }
        false
    }
    pub fn url_has_parameters(url: &str) -> bool {
        url.contains("://") && url.contains(['?', '#'])
    }
}
impl fmt::Display for SafeRemote {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitConfigScope {
    UserGlobal,
    Isolated,
}
#[derive(Debug, Clone)]
pub struct GitTransportOptions {
    pub scope: GitConfigScope,
    /// Local repositories are for trusted callers/tests, never API input.
    pub allow_local: bool,
}

/// How a caller-supplied session credential is presented to Git.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitAuthKind {
    /// A bearer-style token; the username defaults to [`GIT_AUTH_TOKEN_USERNAME`].
    Token,
    /// A username and password.
    Basic,
}
impl GitAuthKind {
    /// Wire spelling (`"token"` | `"basic"`), also the persisted marker.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Token => "token",
            Self::Basic => "basic",
        }
    }
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "token" => Some(Self::Token),
            "basic" => Some(Self::Basic),
            _ => None,
        }
    }
}

/// Username presented with a token when the caller names none.
pub const GIT_AUTH_TOKEN_USERNAME: &str = "x-access-token";
/// Upper bound of a session credential's username.
pub const MAX_GIT_AUTH_USERNAME_BYTES: usize = 256;
/// Upper bound of a session credential's token or password.
pub const MAX_GIT_AUTH_SECRET_BYTES: usize = 8 * 1024;

/// Why a `git_auth` value was refused. Messages never contain the secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitAuthInvalid {
    MissingUsername,
    Empty { field: &'static str },
    TooLarge { field: &'static str },
    LineBreak { field: &'static str },
}
impl fmt::Display for GitAuthInvalid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingUsername => f.write_str("git_auth kind 'basic' requires a username"),
            Self::Empty { field } => write!(f, "git_auth {field} must not be empty"),
            Self::TooLarge { field } => write!(f, "git_auth {field} is too large"),
            Self::LineBreak { field } => {
                write!(f, "git_auth {field} must not contain CR, LF or NUL")
            }
        }
    }
}
impl std::error::Error for GitAuthInvalid {}

/// A push session's caller-supplied Git credential. It lives only in the
/// daemon's memory: never `Serialize`, never `Display`, and `Debug` shows the
/// kind only. The persisted marker is [`GitAuthKind::as_str`].
pub struct GitAuthSpec {
    kind: GitAuthKind,
    username: String,
    secret: crate::data::secret::SecretString,
}
impl fmt::Debug for GitAuthSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GitAuthSpec")
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}
impl GitAuthSpec {
    /// `token`: `username` optional (default [`GIT_AUTH_TOKEN_USERNAME`]);
    /// `basic`: `username` required. Every value is non-empty and free of
    /// CR/LF/NUL; the username is at most 256 bytes, the secret 8 KiB.
    pub fn new(
        kind: GitAuthKind,
        username: Option<String>,
        secret: crate::data::secret::SecretString,
    ) -> Result<Self, GitAuthInvalid> {
        let username = match (kind, username) {
            (_, Some(name)) => name,
            (GitAuthKind::Token, None) => GIT_AUTH_TOKEN_USERNAME.to_string(),
            (GitAuthKind::Basic, None) => return Err(GitAuthInvalid::MissingUsername),
        };
        let field = match kind {
            GitAuthKind::Token => "token",
            GitAuthKind::Basic => "password",
        };
        for (name, value, max) in [
            ("username", username.as_str(), MAX_GIT_AUTH_USERNAME_BYTES),
            (field, secret.expose(), MAX_GIT_AUTH_SECRET_BYTES),
        ] {
            if value.is_empty() {
                return Err(GitAuthInvalid::Empty { field: name });
            }
            if value.len() > max {
                return Err(GitAuthInvalid::TooLarge { field: name });
            }
            if value.contains(['\r', '\n', '\0']) {
                return Err(GitAuthInvalid::LineBreak { field: name });
            }
        }
        Ok(Self {
            kind,
            username,
            secret,
        })
    }
    pub fn kind(&self) -> GitAuthKind {
        self.kind
    }
    /// Not a secret: it is part of the idempotency fingerprint.
    pub fn username(&self) -> &str {
        &self.username
    }
    pub fn secret(&self) -> &crate::data::secret::SecretString {
        &self.secret
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn git_auth_spec_validates_without_echoing_the_secret() {
        use crate::data::secret::SecretString;
        let token = GitAuthSpec::new(
            GitAuthKind::Token,
            None,
            SecretString::new("test-secret-token"),
        )
        .unwrap();
        assert_eq!(token.username(), GIT_AUTH_TOKEN_USERNAME);
        assert!(!format!("{token:?}").contains("test-secret"));
        assert_eq!(
            GitAuthSpec::new(GitAuthKind::Basic, None, SecretString::new("x")).unwrap_err(),
            GitAuthInvalid::MissingUsername
        );
        for (user, secret) in [
            (Some("a".to_string()), "test-secret-\nline"),
            (Some("a\r".to_string()), "test-secret"),
            (Some(String::new()), "test-secret"),
            (Some("a".to_string()), ""),
            (Some("u".repeat(257)), "test-secret"),
        ] {
            let error =
                GitAuthSpec::new(GitAuthKind::Basic, user, SecretString::new(secret)).unwrap_err();
            assert!(!error.to_string().contains("test-secret"));
        }
        assert!(GitAuthSpec::new(
            GitAuthKind::Token,
            None,
            SecretString::new("t".repeat(MAX_GIT_AUTH_SECRET_BYTES + 1))
        )
        .is_err());
        assert_eq!(GitAuthKind::parse("basic"), Some(GitAuthKind::Basic));
        assert_eq!(GitAuthKind::parse("bearer"), None);
    }
    #[test]
    fn identifiers_remove_userinfo_query_fragment_and_controls() {
        for (url, expected, credentials) in [
            (
                "https://user:tok@github.com/o/r.git?x=1",
                "github.com/o/r.git",
                true,
            ),
            ("http://user@host/r#token", "host/r", true),
            ("https://user:abc#def@host/r.git", "host/r.git", true),
            ("https://user:a?b@host/r.git", "host/r.git", true),
            ("https://user:p/ss@host/r.git", "host/r.git", true),
            ("https://user:p@ss@host/r.git", "host/r.git", true),
            ("ssh://git@host:2222/o/r.git", "host:2222/o/r.git", false),
            ("ssh://git:tok@host/r", "host/r", true),
            ("git@github.com:o/r.git", "github.com:o/r.git", false),
            ("git@host:team/a@b.git", "host:team/a@b.git", false),
            ("/local/path", "local repository", false),
            ("file:///p", "local repository", false),
            ("https://host/é\nrepo", "host/érepo", false),
        ] {
            assert_eq!(SafeRemote::from_url(url).as_str(), expected);
            assert_eq!(SafeRemote::url_has_credentials(url), credentials);
        }
        let safe = SafeRemote::from_url(&format!("https://host/{}", "é".repeat(300)));
        assert!(safe.as_str().len() <= 256);
    }
}
