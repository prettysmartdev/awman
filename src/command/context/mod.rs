//! Typed context policy boundary; HTTP adapters only translate transport.
pub mod change_tracker;
pub mod repo_api;
pub mod retention;
pub mod scope;
pub mod session_binding;
pub mod workflow_api;
#[derive(Debug, thiserror::Error)]
pub enum ContextApiError {
    #[error("{0}")]
    InvalidKey(String),
    #[error("{0}")]
    InvalidPath(String),
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    PreconditionFailed(String),
    #[error("{0}")]
    TooLarge(String),
    #[error("{0}")]
    Conflict(String),
    #[error("{0}")]
    Unavailable(String),
    #[error("{0}")]
    CommandNotFound(String),
    #[error(transparent)]
    Data(#[from] crate::data::error::DataError),
}
impl ContextApiError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidKey(_) | Self::InvalidPath(_) => "invalid_request",
            Self::NotFound(_) => "not_found",
            Self::PreconditionFailed(_) => "precondition_failed",
            Self::TooLarge(_) => "context_too_large",
            Self::Conflict(_) => "conflict",
            Self::Unavailable(_) => "unavailable",
            Self::CommandNotFound(_) => "command_not_found",
            Self::Data(_) => "internal",
        }
    }
}
