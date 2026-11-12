//! Classify a failed host Git network operation from its stderr.
//!
//! Only failures *before a remote response* are [`GitFailureClass::Transport`]
//! (the one class a lease push may retry). Credential refusals are
//! [`GitFailureClass::Auth`]; a remote that answered and refused (non-fast-
//! forward, protected branch, hook, other 4xx) is
//! [`GitFailureClass::RemoteRejected`]. Rejection signatures are checked
//! first so a `[rejected]` push is never mistaken for an auth failure.

use crate::engine::error::EngineError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitFailureClass {
    Auth,
    RemoteRejected,
    Transport,
    Other,
}

const REJECTED: &[&str] = &[
    "[rejected]",
    "[remote rejected]",
    "non-fast-forward",
    "pre-receive hook declined",
    "protected branch",
    "fetch first",
    "failed to push some refs",
];
const AUTH: &[&str] = &[
    "authentication failed",
    "could not read username",
    "could not read password",
    // Git 2.44+ with `credential.interactive=false` and no credential.
    "unable to get password from user",
    "permission denied (publickey",
    "http 401",
    "http 403",
    "the requested url returned error: 401",
    "the requested url returned error: 403",
    "invalid username or password",
    "terminal prompts disabled",
    "access denied",
];
const TRANSPORT: &[&str] = &[
    "could not resolve host",
    "connection refused",
    "failed to connect",
    "couldn't connect to server",
    "network is unreachable",
    "empty reply from server",
];

/// Classify a Git failure. `timed_out` is true when awman killed the
/// process for exceeding its bound (no answer arrived).
pub fn classify_git_failure(stderr: &str, _timed_out: bool) -> GitFailureClass {
    let text = stderr.to_ascii_lowercase();
    let any = |needles: &[&str]| needles.iter().any(|n| text.contains(n));
    if any(REJECTED) {
        GitFailureClass::RemoteRejected
    } else if any(AUTH) {
        GitFailureClass::Auth
    } else if text.contains("the requested url returned error:")
        || text.contains("http ")
        || text.contains("http/")
        || text.contains("remote:")
        || text.contains("receiving objects:")
    {
        GitFailureClass::RemoteRejected
    } else if any(TRANSPORT) {
        GitFailureClass::Transport
    } else {
        GitFailureClass::Other
    }
}

impl GitFailureClass {
    /// The typed engine error for this class (`Other` keeps `EngineError::Git`).
    pub fn into_error(self, operation: &str, message: String) -> EngineError {
        let operation = operation.to_owned();
        match self {
            Self::Auth => EngineError::GitAuth { operation, message },
            Self::Transport => EngineError::GitTransport { operation, message },
            Self::RemoteRejected => EngineError::GitRejected { operation, message },
            Self::Other => EngineError::Git(format!("git {operation} failed: {message}")),
        }
    }

    /// The class of a typed engine error, when it carries one.
    pub fn of(error: &EngineError) -> Self {
        match error {
            EngineError::GitAuth { .. } => Self::Auth,
            EngineError::GitTransport { .. } => Self::Transport,
            EngineError::GitRejected { .. } => Self::RemoteRejected,
            _ => Self::Other,
        }
    }
}

/// Private curl evidence lets us distinguish a reset before any response
/// from a reset after the peer started answering. Never emit trace contents.
pub(super) struct ResponseTrace(tempfile::NamedTempFile);
impl ResponseTrace {
    pub fn attach(command: &mut std::process::Command) -> Result<Self, EngineError> {
        let file = tempfile::NamedTempFile::new()
            .map_err(|e| EngineError::Git(format!("create private Git trace: {e}")))?;
        command
            .env("GIT_TRACE_CURL", file.path())
            .env("GIT_TRACE_REDACT", "1")
            .env("GIT_TRACE_CURL_NO_DATA", "1");
        Ok(Self(file))
    }
    pub fn classify(&self, stderr: &str, timed_out: bool) -> GitFailureClass {
        let trace = std::fs::read_to_string(self.0.path()).ok();
        classify_response_trace(stderr, timed_out, trace.as_deref())
    }
}

fn classify_response_trace(stderr: &str, timed_out: bool, trace: Option<&str>) -> GitFailureClass {
    let class = classify_git_failure(stderr, timed_out);
    if matches!(
        class,
        GitFailureClass::Auth | GitFailureClass::RemoteRejected
    ) {
        return class;
    }
    let Some(trace) = trace else {
        return GitFailureClass::Other;
    };
    let trace = trace.to_ascii_lowercase();
    // Git's wording for a refused or missing credential differs between
    // versions; the remote's last answer does not. A final 401 or 403 is
    // an authentication failure whatever stderr says.
    if matches!(last_http_status(&trace), Some(401 | 403)) {
        return GitFailureClass::Auth;
    }
    let response_seen = trace.contains("<= recv header") || trace.contains("<= recv data");
    if response_seen {
        return GitFailureClass::Other;
    }
    if class == GitFailureClass::Transport {
        return class;
    }
    // libcurl renders WSAECONNRESET as "Connection was reset" on Windows.
    // Either spelling still requires evidence that no response arrived.
    let stderr = stderr.to_ascii_lowercase();
    if trace.contains("== info:")
        && ["connection reset by peer", "connection was reset"]
            .iter()
            .any(|reset| stderr.contains(reset))
    {
        GitFailureClass::Transport
    } else {
        GitFailureClass::Other
    }
}

/// The status of the last HTTP response in a lower-cased curl trace.
fn last_http_status(trace: &str) -> Option<u16> {
    trace
        .lines()
        .filter_map(|line| line.split_once("<= recv header: http/"))
        .filter_map(|(_, rest)| rest.split_whitespace().nth(1)?.parse().ok())
        .next_back()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resets_need_evidence_of_no_response_on_every_platform() {
        let started = "== Info: Connected to 127.0.0.1\n";
        for message in [
            "Recv failure: Connection reset by peer",
            "Recv failure: Connection was reset",
        ] {
            for trace in [None, Some("")] {
                assert_eq!(
                    classify_response_trace(message, false, trace),
                    GitFailureClass::Other
                );
            }
            assert_eq!(
                classify_response_trace(message, false, Some(started)),
                GitFailureClass::Transport
            );
            for response in [
                "<= Recv header: HTTP/1.1 200 OK\n",
                "<= Recv data: partial\n",
            ] {
                let trace = format!("{started}{response}");
                assert_eq!(
                    classify_response_trace(message, false, Some(&trace)),
                    GitFailureClass::Other
                );
            }
            let trace = format!("{started}<= Recv header: HTTP/1.1 401 Unauthorized\n");
            assert_eq!(
                classify_response_trace(message, false, Some(&trace)),
                GitFailureClass::Auth
            );
        }
    }
}
