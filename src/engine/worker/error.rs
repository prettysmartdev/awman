//! Typed failures of the worker transport and execution primitives.

use awman_wsp::codes;

#[derive(Clone, PartialEq, Eq, thiserror::Error)]
pub enum WorkerEngineError {
    /// `409 lease_lost`: the lease expired and was re-issued. The caller must
    /// stop the run, cancel its containers and discard its result.
    #[error("the work server reports the lease was lost")]
    LeaseLost,
    /// `409 taken`: another worker claimed the item first.
    #[error("the work item was taken by another worker")]
    Taken,
    /// `409 ineligible`: the item no longer matches this worker.
    #[error("the work item is not eligible for this worker")]
    Ineligible,
    /// The refresh token was refused: the link was revoked. Running leases
    /// of this server must be cancelled and claiming must stop.
    #[error("the work server link was revoked")]
    LinkRevoked,
    #[error("lease Git authentication failed")]
    GitAuth,
    /// No (valid) response from the server; safe to retry.
    #[error("work server transport failed")]
    Transport(String),
    #[error("work server returned HTTP {status}")]
    Server {
        status: u16,
        code: String,
        message: String,
    },
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Io(String),
}

// Upstream codes and messages are untrusted and may echo credentials.
impl std::fmt::Debug for WorkerEngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self, f)
    }
}

impl WorkerEngineError {
    /// Whether repeating the same request may succeed (transport failures
    /// and server errors). Fencing, eligibility and revocation never are.
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Transport(_) => true,
            Self::Server { status, .. } => *status >= 500 || *status == 429,
            _ => false,
        }
    }
}

impl From<awman_wsp::Error> for WorkerEngineError {
    fn from(error: awman_wsp::Error) -> Self {
        use awman_wsp::Error as E;
        match error {
            E::LinkRevoked => Self::LinkRevoked,
            E::Conflict { code, .. } if code == codes::LEASE_LOST => Self::LeaseLost,
            E::Conflict { code, .. } if code == codes::TAKEN => Self::Taken,
            E::Conflict { code, .. } if code == codes::INELIGIBLE => Self::Ineligible,
            E::Conflict { code, message } => Self::Server {
                status: 409,
                code,
                message,
            },
            E::Server {
                status,
                code,
                message,
            } => Self::Server {
                status: status.as_u16(),
                code,
                message,
            },
            E::Transport(_) => Self::Transport("request failed".into()),
            #[allow(unreachable_patterns)]
            other => match other.to_string() {
                text if text.contains("terminal transport") => Self::Transport(text),
                text => Self::Invalid(text),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wsp_conflicts_map_to_typed_fencing_errors() {
        let conflict = |code: &str| awman_wsp::Error::Conflict {
            code: code.into(),
            message: String::new(),
        };
        assert_eq!(
            WorkerEngineError::from(conflict("lease_lost")),
            WorkerEngineError::LeaseLost
        );
        assert_eq!(
            WorkerEngineError::from(conflict("taken")),
            WorkerEngineError::Taken
        );
        assert_eq!(
            WorkerEngineError::from(conflict("ineligible")),
            WorkerEngineError::Ineligible
        );
        assert_eq!(
            WorkerEngineError::from(awman_wsp::Error::LinkRevoked),
            WorkerEngineError::LinkRevoked
        );
        assert!(!WorkerEngineError::LeaseLost.is_retryable());
        assert!(WorkerEngineError::Transport("reset".into()).is_retryable());
        assert!(WorkerEngineError::Server {
            status: 503,
            code: "unavailable".into(),
            message: String::new()
        }
        .is_retryable());
    }
}

#[cfg(test)]
mod redaction_tests {
    use super::*;
    #[test]
    fn converted_server_errors_do_not_echo_secrets() {
        for secret in [
            "ACCESS_TOKEN_SENTINEL",
            "REFRESH_TOKEN_SENTINEL",
            "REPO_TOKEN_SENTINEL",
        ] {
            let error = WorkerEngineError::from(awman_wsp::Error::Conflict {
                code: secret.into(),
                message: format!("request failed for token {secret}"),
            });
            let message = format!("heartbeat failed: {error}; {error:?}");
            assert!(!message.contains(secret));
        }
    }
}

#[cfg(test)]
mod logging_redaction_tests {
    use super::*;
    use std::{
        io::Write,
        sync::{Arc, Mutex},
    };
    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<u8>>>);
    impl Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    #[test]
    fn wsp_server_messages_stay_redacted_through_tracing_and_user_messages() {
        for secret in ["REFRESH_SENTINEL", "ACCESS_SENTINEL", "REPO_SENTINEL"] {
            let error = WorkerEngineError::from(awman_wsp::Error::Server {
                status: reqwest::StatusCode::BAD_GATEWAY,
                code: secret.into(),
                message: format!("request failed for token {secret}"),
            });
            let capture = Capture::default();
            let writer = capture.clone();
            let subscriber = tracing_subscriber::fmt()
                .without_time()
                .with_ansi(false)
                .with_writer(move || writer.clone())
                .finish();
            tracing::subscriber::with_default(
                subscriber,
                || tracing::warn!(error = %error, detail = ?error, "lease heartbeat failed"),
            );
            let output = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
            assert!(output.contains("lease heartbeat failed"));
            assert!(!output.contains(secret));
            let message = crate::data::message::UserMessage {
                level: crate::data::message::MessageLevel::Warning,
                text: format!("could not refresh lease repository credentials: {error}"),
            };
            assert!(!format!("{message:?}").contains(secret));
        }
    }
}
