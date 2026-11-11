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

#[cfg(test)]
mod tests {
    use super::*;
    use GitFailureClass::*;

    #[test]
    fn classifies_auth_rejection_and_transport_signatures() {
        for (stderr, class) in [
            (
                "remote: Invalid username or password.\nfatal: Authentication failed for 'https://h/r'",
                Auth,
            ),
            (
                "fatal: could not read Username for 'https://h': terminal prompts disabled",
                Auth,
            ),
            ("fatal: unable to get password from user", Auth),
            (
                "fatal: unable to access 'https://h/r/': The requested URL returned error: 403",
                Auth,
            ),
            ("git@h: Permission denied (publickey).", Auth),
            (
                " ! [rejected]        main -> main (non-fast-forward)",
                RemoteRejected,
            ),
            (
                " ! [remote rejected] main -> main (pre-receive hook declined)",
                RemoteRejected,
            ),
            (
                "remote: error: GH006: Protected branch update failed\n ! [remote rejected]",
                RemoteRejected,
            ),
            (
                "fatal: unable to access 'https://h/r/': The requested URL returned error: 404",
                RemoteRejected,
            ),
            (
                "fatal: unable to access 'https://h/': Could not resolve host: h",
                Transport,
            ),
            (
                "fatal: unable to access 'https://h/': Failed to connect to h port 443: Connection refused",
                Transport,
            ),
            (
                "error: RPC failed; curl 56 Recv failure: Connection reset by peer",
                Other,
            ),
            ("fatal: the remote end hung up unexpectedly", Other),
            (
                "fatal: unable to access 'https://h/': The requested URL returned error: 502",
                RemoteRejected,
            ),
            ("fatal: not a git repository", Other),
        ] {
            assert_eq!(classify_git_failure(stderr, false), class, "{stderr}");
        }
        assert_eq!(classify_git_failure("", true), Other);
        // A rejected push that also mentions a hook's auth wording stays rejected.
        assert_eq!(
            classify_git_failure(" ! [remote rejected] x (access denied by hook)", false),
            RemoteRejected
        );
    }

    #[test]
    fn class_round_trips_through_engine_error() {
        for class in [Auth, Transport, RemoteRejected] {
            assert_eq!(
                GitFailureClass::of(&class.into_error("push", "m".into())),
                class
            );
        }
        assert_eq!(
            GitFailureClass::of(&Other.into_error("push", "m".into())),
            Other
        );
    }
}

#[cfg(test)]
mod response_tests {
    use super::*;
    #[test]
    fn responses_take_precedence_over_rpc_and_transport_text() {
        for status in [413, 429, 500, 502, 503] {
            assert_eq!(classify_git_failure(&format!("RPC failed; HTTP {status} curl 22 The requested URL returned error: {status}"), false), GitFailureClass::RemoteRejected);
        }
        assert_eq!(
            classify_git_failure("remote: accepted upload; connection reset by peer", false),
            GitFailureClass::RemoteRejected
        );
        assert_eq!(
            classify_git_failure("operation timed out", true),
            GitFailureClass::Other
        );
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
        let class = classify_git_failure(stderr, timed_out);
        if matches!(
            class,
            GitFailureClass::Auth | GitFailureClass::RemoteRejected
        ) {
            return class;
        }
        let Ok(trace) = std::fs::read_to_string(self.0.path()) else {
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
        if trace.contains("== info:")
            && stderr
                .to_ascii_lowercase()
                .contains("connection reset by peer")
        {
            GitFailureClass::Transport
        } else {
            GitFailureClass::Other
        }
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
mod response_evidence_tests {
    use super::*;
    #[test]
    fn a_final_401_or_403_is_an_auth_failure_whatever_git_printed() {
        // Constructed through the production spawner; never spawned here.
        let mut command = crate::engine::host_cli::command(
            &crate::data::config::env::EnvSnapshot::empty(),
            "git",
        );
        let trace = ResponseTrace::attach(&mut command).unwrap();
        let unknown = "fatal: unable to complete the request";
        std::fs::write(
            trace.0.path(),
            "10:00:00.1 http.c:1 <= Recv header: HTTP/1.1 401 Unauthorized\n",
        )
        .unwrap();
        assert_eq!(trace.classify(unknown, false), GitFailureClass::Auth);
        std::fs::write(trace.0.path(), "<= Recv header: HTTP/2 403 \n").unwrap();
        assert_eq!(trace.classify(unknown, false), GitFailureClass::Auth);
        // A challenge that was then answered is not the failure.
        std::fs::write(
            trace.0.path(),
            "<= Recv header: HTTP/1.1 401 Unauthorized\n<= Recv header: HTTP/1.1 200 OK\n",
        )
        .unwrap();
        assert_eq!(trace.classify(unknown, false), GitFailureClass::Other);
    }
    #[test]
    fn reset_after_response_is_never_retried_and_unknown_evidence_fails_closed() {
        // Constructed through the production spawner; never spawned here.
        let mut command = crate::engine::host_cli::command(
            &crate::data::config::env::EnvSnapshot::empty(),
            "git",
        );
        let trace = ResponseTrace::attach(&mut command).unwrap();
        let reset = "RPC failed; curl 56 Recv failure: Connection reset by peer";
        assert_eq!(trace.classify(reset, false), GitFailureClass::Other);
        std::fs::write(trace.0.path(), "== Info: Trying 192.0.2.1:8080\n").unwrap();
        assert_eq!(trace.classify(reset, false), GitFailureClass::Transport);
        std::fs::write(
            trace.0.path(),
            "== Info: Connected\n<= Recv header: HTTP/1.1 200 OK\n",
        )
        .unwrap();
        assert_eq!(trace.classify(reset, false), GitFailureClass::Other);
        assert_eq!(
            trace.classify("empty reply from server", false),
            GitFailureClass::Other
        );
    }
}
