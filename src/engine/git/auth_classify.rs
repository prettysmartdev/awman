//! Classify a failed host Git network operation from its stderr.
//!
//! Only failures *before a remote response* are [`GitFailureClass::Transport`]
//! (the one class a lease push may retry). Credential refusals are
//! [`GitFailureClass::Auth`]; a remote that answered and refused (non-fast-
//! forward, protected branch, hook, other 4xx) is
//! [`GitFailureClass::RemoteRejected`]. Rejection signatures are checked
//! first so a `[rejected]` push is never mistaken for an auth failure.

use crate::data::git_transport::{GitTransportOp, SafeRemote};
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
];
const AUTH: &[&str] = &[
    "authentication failed",
    "could not read username",
    "could not read password",
    // Git 2.44+ with `credential.interactive=false` and no credential.
    "unable to get password from user",
    "permission denied (",
    "host key verification failed",
    "credential helper failed",
    "told us to quit",
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
    // curl connect/resolve timeouts (no response arrived).
    "operation timed out",
    "connection timed out",
    "resolving timed out",
    // OpenSSH could not reach the remote at all.
    "ssh: connect to host",
    "could not resolve hostname",
];

/// Classify a Git failure from its stderr alone. `timed_out` is true when
/// awman killed the process for exceeding its bound; whether a response had
/// arrived by then is decided from the [`ResponseTrace`].
pub fn classify_git_failure(stderr: &str, _timed_out: bool) -> GitFailureClass {
    let text = stderr.to_ascii_lowercase();
    let any = |needles: &[&str]| needles.iter().any(|n| text.contains(n));
    if any(REJECTED) {
        GitFailureClass::RemoteRejected
    } else if any(AUTH)
        || (text.contains("permission to ") && text.contains(" denied to "))
        || text.contains("you are not allowed to push")
    {
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
pub(super) struct ResponseTrace(tempfile::NamedTempFile, tempfile::NamedTempFile);
impl ResponseTrace {
    pub fn attach(command: &mut std::process::Command) -> Result<Self, EngineError> {
        let file = tempfile::NamedTempFile::new()
            .map_err(|e| EngineError::Git(format!("create private Git trace: {e}")))?;
        let packets = tempfile::NamedTempFile::new()
            .map_err(|e| EngineError::Git(format!("create private packet trace: {e}")))?;
        command
            .env("GIT_TRACE_PACKET", packets.path())
            .env("GIT_TRACE_CURL", file.path())
            .env("GIT_TRACE_REDACT", "1")
            .env("GIT_TRACE_CURL_NO_DATA", "1");
        Ok(Self(file, packets))
    }
    pub fn classify(&self, stderr: &str, timed_out: bool) -> GitFailureClass {
        let trace = std::fs::read_to_string(self.0.path()).ok();
        // SSH has no curl trace. An incoming packet is evidence of a remote
        // answer; its contents stay private and are never public diagnostics.
        if trace.as_ref().is_some_and(|t| t.is_empty())
            && std::fs::read_to_string(self.1.path()).map_or(true, |p| {
                p.lines().any(|l| l.contains("packet:") && l.contains('<'))
            })
        {
            let class = classify_git_failure(stderr, timed_out);
            return if matches!(
                class,
                GitFailureClass::Auth | GitFailureClass::RemoteRejected
            ) {
                class
            } else {
                GitFailureClass::Other
            };
        }
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
    // Only the request that failed counts: a push that read the ref
    // advertisement and then lost the connection before any answer to its
    // receive-pack request got no response *to that request*. Whether that
    // push landed anyway is resolved by the publisher's head check before
    // any further push.
    let failed_request = trace
        .rfind("=> send header")
        .map_or(trace.as_str(), |at| &trace[at..]);
    let response_seen =
        failed_request.contains("<= recv header") || failed_request.contains("<= recv data");
    if response_seen {
        return GitFailureClass::Other;
    }
    if class == GitFailureClass::Transport {
        return class;
    }
    // awman killed the process for exceeding its bound before the remote
    // answered (no response in the trace, no `remote:` output on stderr,
    // which classified as rejected above): a connect/read timeout before a
    // response, which is retryable. A timeout after response bytes returned
    // `Other` above and is terminal.
    if timed_out {
        return GitFailureClass::Transport;
    }
    // libcurl distinguishes WSAECONNRESET and WSAECONNABORTED on Windows.
    // Both disconnects still require evidence that no response arrived.
    let stderr = stderr.to_ascii_lowercase();
    if trace.contains("== info:")
        && [
            "connection reset by peer",
            "connection was reset",
            "connection was aborted",
        ]
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

/// A classified transport outcome with no raw remote output in public messages.
pub struct GitTransportFailure {
    pub class: GitFailureClass,
    pub op: GitTransportOp,
    pub remote: SafeRemote,
    pub timed_out: bool,
}
impl GitTransportFailure {
    pub fn message(&self, _sanitized_detail: &str) -> String {
        let reason = match self.class {
            GitFailureClass::Auth => "authentication failed",
            GitFailureClass::Transport if self.timed_out => {
                "no response from the remote before the timeout"
            }
            GitFailureClass::Transport => {
                "connection failed before a remote response (DNS, connect or reset)"
            }
            GitFailureClass::RemoteRejected => "rejected by the remote",
            GitFailureClass::Other if self.timed_out => "operation timed out",
            GitFailureClass::Other => match _sanitized_detail {
                "destination already exists"
                | "no space left on device"
                | "cannot create work tree"
                | "remote branch not found"
                | "repository not found" => _sanitized_detail,
                _ => "remote operation failed",
            },
        };
        format!("git {} to {}: {reason}", self.op.as_str(), self.remote)
    }
    pub fn into_error(self, sanitized_detail: &str) -> EngineError {
        let message = self.message(sanitized_detail);
        let operation = self.op.as_str().to_owned();
        let remote = self.remote.to_string();
        match self.class {
            GitFailureClass::Auth => EngineError::GitAuth {
                operation,
                remote,
                message,
            },
            GitFailureClass::Transport => EngineError::GitTransport {
                operation,
                remote,
                message,
            },
            GitFailureClass::RemoteRejected => EngineError::GitRejected {
                operation,
                remote,
                message,
            },
            GitFailureClass::Other => EngineError::Git(message),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disconnects_need_evidence_of_no_response_on_every_platform() {
        let started = "== Info: Connected to 127.0.0.1\n";
        for message in [
            "Recv failure: Connection reset by peer",
            "Recv failure: Connection was reset",
            "Recv failure: Connection was aborted",
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

#[cfg(test)]
mod timeout_tests {
    use super::*;

    #[test]
    fn a_timeout_is_transport_only_before_any_response() {
        let connected = "== Info: Connected to 127.0.0.1\n";
        // Killed while connecting or waiting for the first byte.
        for trace in ["", connected] {
            assert_eq!(
                classify_response_trace("", true, Some(trace)),
                GitFailureClass::Transport
            );
        }
        // Killed after the remote started answering: terminal.
        for response in [
            "<= Recv header: HTTP/1.1 200 OK\n",
            "<= Recv data: 001e# service=git-receive-pack\n",
        ] {
            let trace = format!("{connected}{response}");
            assert_eq!(
                classify_response_trace("", true, Some(&trace)),
                GitFailureClass::Other
            );
        }
        // Only the failed request counts: the advertisement was answered,
        // the receive-pack request was not.
        let advertised = "=> Send header: GET /r.git/info/refs?service=git-receive-pack HTTP/1.1\n\
                          <= Recv header: HTTP/1.1 200 OK\n";
        let posted = format!("{advertised}=> Send header: POST /r.git/git-receive-pack HTTP/1.1\n");
        assert_eq!(
            classify_response_trace(
                "error: RPC failed; curl 52 Empty reply from server",
                false,
                Some(&posted)
            ),
            GitFailureClass::Transport
        );
        assert_eq!(
            classify_response_trace("", true, Some(&posted)),
            GitFailureClass::Transport
        );
        let answered = format!("{posted}<= Recv header: HTTP/1.1 200 OK\n");
        assert_eq!(
            classify_response_trace(
                "error: RPC failed; curl 52 Empty reply from server",
                false,
                Some(&answered)
            ),
            GitFailureClass::Other
        );
        // SSH has no trace: output from the remote means it answered.
        assert_eq!(
            classify_response_trace("remote: Counting objects", true, Some("")),
            GitFailureClass::RemoteRejected
        );
        // Not killed by awman and no transport signature: terminal.
        assert_eq!(
            classify_response_trace("", false, Some(connected)),
            GitFailureClass::Other
        );
        for stderr in [
            "fatal: unable to access 'x': Failed to connect to h port 1 after 3001 ms: Operation timed out",
            "fatal: unable to access 'x': Connection timed out after 3001 milliseconds",
            "fatal: unable to access 'x': Resolving timed out after 3000 milliseconds",
            "ssh: connect to host example.invalid port 22: Connection refused",
            "ssh: Could not resolve hostname example.invalid: Name or service not known",
        ] {
            assert_eq!(
                classify_response_trace(stderr, false, Some("")),
                GitFailureClass::Transport,
                "{stderr}"
            );
        }
    }

    #[test]
    fn transport_messages_never_carry_stderr() {
        let failure = |timed_out| GitTransportFailure {
            class: GitFailureClass::Transport,
            op: GitTransportOp::Push,
            remote: SafeRemote::from_url("https://user@example.invalid/o/r.git"),
            timed_out,
        };
        assert_eq!(
            failure(true).message("test-secret-detail"),
            "git push to example.invalid/o/r.git: no response from the remote before the timeout"
        );
        assert!(!failure(false)
            .message("test-secret-detail")
            .contains("test-secret"));
    }
}

#[cfg(test)]
mod auth_tests {
    use super::*;
    #[test]
    fn helper_ssh_and_http_refusals_are_auth_and_push_rejections_stay_distinct() {
        for stderr in [
            "fatal: Authentication failed\nerror: failed to push some refs",
            "Permission denied (publickey,password).",
            "ERROR: Permission to org/repo.git denied to alice. fatal: Could not read from remote repository.",
            "GitLab: You are not allowed to push code to this project.",
            "Host key verification failed.",
            "fatal: could not read Username: terminal prompts disabled",
            "fatal: unable to get password from user",
            "credential helper failed",
            "fatal: credential helper selected-by-user told us to quit",
            "HTTP 403",
        ] {
            assert_eq!(
                classify_git_failure(stderr, false),
                GitFailureClass::Auth,
                "{stderr}"
            );
        }
        for stderr in [
            "[rejected] (non-fast-forward)",
            "remote: permission denied\n[remote rejected] (pre-receive hook declined)",
        ] {
            assert_eq!(
                classify_git_failure(stderr, false),
                GitFailureClass::RemoteRejected
            );
        }
    }
}
