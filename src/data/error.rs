//! Typed error enum for Layer 0.
//!
//! Higher layers wrap this in their own enums; Layer 0 never depends on
//! higher-layer error types.

use std::path::PathBuf;

use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Error)]
pub enum DataError {
    #[error("git root not found for working directory {working_dir}")]
    GitRootNotFound { working_dir: PathBuf },

    #[error("git root resolution failed for {working_dir}: {message}")]
    GitRootResolution {
        working_dir: PathBuf,
        message: String,
    },

    #[error("session not found: {id}")]
    SessionNotFound { id: Uuid },

    #[error("session id collision: {id}")]
    SessionIdCollision { id: Uuid },

    #[error("invalid agent name {name:?}: {reason}")]
    InvalidAgentName { name: String, reason: String },

    #[error("config parse error in {path}: {source}")]
    ConfigParse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },

    #[error("config serialize error: {source}")]
    ConfigSerialize {
        #[source]
        source: serde_json::Error,
    },

    #[error("io error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("home directory cannot be determined")]
    HomeNotFound,

    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("workflow step has a missing dependency: step '{step}' depends on '{missing}'")]
    MissingDependency { step: String, missing: String },

    #[error("workflow contains a cycle involving step '{step}'")]
    CyclicDependency { step: String },

    #[error("workflow state error: {0}")]
    WorkflowState(String),

    #[error("Markdown workflow files are no longer supported. Convert to TOML (.toml) or YAML (.yaml/.yml). See docs/04-workflows.md for the current format. File: {}", path.display())]
    MarkdownNoLongerSupported { path: std::path::PathBuf },

    #[error("workflow resume incompatible: {0}")]
    WorkflowResumeIncompatible(String),

    #[error("invalid path {path}: {reason}")]
    InvalidPath { path: PathBuf, reason: String },

    #[error("{0}")]
    Other(String),
}

impl DataError {
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        DataError::Io {
            path: path.into(),
            source,
        }
    }

    pub fn config_parse(path: impl Into<PathBuf>, source: serde_json::Error) -> Self {
        DataError::ConfigParse {
            path: path.into(),
            source,
        }
    }
}
