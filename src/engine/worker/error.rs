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
    #[error("git {operation} to {remote}: authentication failed")]
    GitAuth { operation: String, remote: String },
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
