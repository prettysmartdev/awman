//! Layer 2 choice of push-session transport and stable failure policy.
//!
//! Every push-session network Git operation (setup clone and fetches,
//! merge-ref advertisement at admission, push-back advertisement and push)
//! takes its transport from here, so the credential source is a function of
//! the session alone:
//!
//! - no `git_auth` marker → the invoking user's own helpers (`Ambient`);
//! - marker and an in-memory credential → a one-shot ticket on the daemon's
//!   credential socket (`Helper`), valid while the returned transport lives;
//! - marker but no credential (the daemon restarted) → `credentials_lost`.
use std::sync::Arc;

use crate::command::dispatch::ephemeral_secrets::{EphemeralSecrets, SessionGitCredential};
use crate::command::dispatch::Engines;
use crate::command::error::CommandError;
use crate::data::fs::api_db::{failure_codes, failure_stages};
use crate::engine::error::EngineError;
use crate::engine::git::lease_transport::CredentialScope;
use crate::engine::git::transport::{CredentialSource, HostGitTransport};

/// The transport for a session whose credential (if any) is `credential`.
pub fn transport_for_credential(
    secrets: &EphemeralSecrets,
    private_dir: &std::path::Path,
    credential: Option<&Arc<SessionGitCredential>>,
) -> Result<HostGitTransport, CommandError> {
    let Some(credential) = credential else {
        return Ok(HostGitTransport::user(CredentialSource::Ambient));
    };
    let ticket = secrets
        .host_socket(private_dir)?
        .ticket(Arc::clone(&credential.spec), credential.scope.clone());
    Ok(HostGitTransport::user(CredentialSource::Helper(Arc::new(
        ticket,
    ))))
}

/// The transport of an admitted session. `git_auth_kind` is the session
/// row's persisted marker.
pub fn session_transport(
    engines: &Engines,
    session_id: &str,
    git_auth_kind: Option<&str>,
) -> Result<HostGitTransport, CommandError> {
    if git_auth_kind.is_none() {
        return Ok(HostGitTransport::user(CredentialSource::Ambient));
    }
    let credential = engines
        .ephemeral_secrets
        .session_git_auth(session_id)
        .ok_or(CommandError::CredentialsLost)?;
    transport_for_credential(
        &engines.ephemeral_secrets,
        &credential_socket_dir(engines),
        Some(&credential),
    )
}

/// Whether a session with this marker can still reach its credential.
pub fn check_session_credentials(
    engines: &Engines,
    session_id: &str,
    git_auth_kind: Option<&str>,
) -> Result<(), CommandError> {
    if git_auth_kind.is_some()
        && engines
            .ephemeral_secrets
            .session_git_auth(session_id)
            .is_none()
    {
        return Err(CommandError::CredentialsLost);
    }
    Ok(())
}

/// Owner-only directory holding the daemon's credential sockets. Kept short:
/// a Unix socket path is limited to about 100 bytes.
pub fn credential_socket_dir(engines: &Engines) -> std::path::PathBuf {
    engines.auth_engine.api_paths().root().join("run")
}

/// The credential scope of `repo_url`, or why `git_auth` cannot serve it:
/// the helper answers HTTP credential requests only, so the URL must be
/// `https://` or `http://` to a loopback host.
pub fn git_auth_scope(repo_url: &str) -> Result<CredentialScope, CommandError> {
    let unsupported = |reason: &str| CommandError::GitAuthUnsupportedUrl {
        reason: reason.into(),
    };
    if !crate::engine::git::credential_socket::supported() {
        return Err(CommandError::GitAuthUnsupportedPlatform);
    }
    let parsed = reqwest::Url::parse(repo_url)
        .map_err(|_| unsupported("repo_url must be an https:// URL"))?;
    let loopback = parsed.host_str().is_some_and(|host| {
        let host = host.trim_start_matches('[').trim_end_matches(']');
        host.eq_ignore_ascii_case("localhost")
            || host
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    });
    match parsed.scheme() {
        "https" => {}
        "http" if loopback => {}
        _ => return Err(unsupported("repo_url must be an https:// URL")),
    }
    CredentialScope::for_http_url(repo_url).ok_or_else(|| unsupported("repo_url has no host"))
}

/// Setup and push-back failure policy. `GitTransport` (no remote response)
/// reaches a result only after the push retry loop gave up, so it is
/// `push_unreachable`; a remote that answered and refused is `push_failed`.
pub fn git_failure_code(error: &EngineError) -> Option<(&'static str, &'static str)> {
    match error {
        EngineError::GitAuth { .. } => {
            Some((failure_stages::GIT_AUTH, failure_codes::GIT_AUTH_FAILED))
        }
        EngineError::GitTransport { .. } => {
            Some((failure_stages::PUSH, failure_codes::PUSH_UNREACHABLE))
        }
        EngineError::GitRejected { .. } => Some((failure_stages::PUSH, failure_codes::PUSH_FAILED)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn auth_unreachable_and_refusal_keep_distinct_codes() {
        let auth = EngineError::GitAuth {
            operation: "push".into(),
            remote: "example.invalid/r".into(),
            message: "authentication failed".into(),
        };
        assert_eq!(
            git_failure_code(&auth),
            Some(("git_auth", "git_auth_failed"))
        );
        let rejected = EngineError::GitRejected {
            operation: "push".into(),
            remote: "example.invalid/r".into(),
            message: "rejected".into(),
        };
        assert_eq!(git_failure_code(&rejected), Some(("push", "push_failed")));
        let unreachable = EngineError::GitTransport {
            operation: "push".into(),
            remote: "example.invalid/r".into(),
            message: "unreachable".into(),
        };
        assert_eq!(
            git_failure_code(&unreachable),
            Some(("push", "push_unreachable"))
        );
        assert_eq!(
            git_failure_code(&EngineError::Git("local failure".into())),
            None
        );
    }

    #[cfg(unix)]
    #[test]
    fn git_auth_accepts_https_and_loopback_http_only() {
        for ok in [
            "https://git.example/o/r.git",
            "http://127.0.0.1:8080/o/r.git",
            "http://localhost/o/r",
            "http://[::1]:9/r",
        ] {
            assert!(git_auth_scope(ok).is_ok(), "{ok}");
        }
        for refused in [
            "http://git.example/o/r.git",
            "ssh://git@git.example/o/r.git",
            "git@git.example:o/r.git",
            "git://git.example/r",
        ] {
            assert_eq!(
                git_auth_scope(refused).unwrap_err().code(),
                "git_auth_unsupported_url",
                "{refused}"
            );
        }
    }
}
