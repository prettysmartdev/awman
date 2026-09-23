//! Layer 1 error type — `EngineError`.
//!
//! Wraps `DataError` for failures bubbling up from Layer 0. Higher layers
//! wrap `EngineError` in their own error types; Layer 1 does not depend on
//! higher-layer errors.

use std::path::PathBuf;

use thiserror::Error;

use crate::data::error::DataError;

#[derive(Debug, Error)]
pub enum EngineError {
    #[error(transparent)]
    Data(#[from] DataError),

    #[error("io error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("git operation failed: {0}")]
    Git(String),

    #[error(
        "merge conflict on branch '{branch}'; resolve manually in worktree at {worktree_path}"
    )]
    MergeConflict {
        branch: String,
        worktree_path: PathBuf,
    },

    #[error("container backend error: {0}")]
    Container(String),

    #[error("sandbox backend error: {0}")]
    Sandbox(String),

    #[error("agent '{agent}' does not support ACP")]
    AcpUnsupported { agent: String },

    #[error("ACP error: {0}")]
    Acp(String),

    #[error(
        "runtime '{runtime}' was given {got}-paradigm options; \
         this indicates a Layer 2 dispatch bug"
    )]
    OptionVariantMismatch { runtime: String, got: &'static str },

    #[error("image not found: '{image}'. Run `make build` or build the base image manually.")]
    ContainerImageNotFound { image: String },

    #[error("conflicting container options: {0}")]
    ConflictingOptions(String),

    #[error("missing required container option: {0}")]
    MissingRequiredOption(String),

    #[error("container option {option} is not supported by backend {backend}")]
    OptionNotSupportedByBackend { option: String, backend: String },

    #[error("backend {backend} is not supported on platform {platform}")]
    BackendUnsupportedOnPlatform { backend: String, platform: String },

    #[error(
        "invalid runtime '{value}' in global config ($HOME/.awman/config.json); \
         valid values: {valid}"
    )]
    UnknownRuntime { value: String, valid: String },

    #[error("invalid advance action: {0}")]
    InvalidAdvanceAction(String),

    #[error("workflow state schema version {found} is newer than supported version {supported}")]
    UnsupportedWorkflowSchemaVersion { found: u32, supported: u32 },

    #[error("workflow resume incompatible: {0}")]
    WorkflowResumeIncompatible(String),

    #[error("plan mode is not supported by agent {agent}")]
    PlanModeUnsupported { agent: String },

    #[error("agent requires the project base image to be built first ({tag})")]
    AgentRequiresProjectImage { tag: String },

    #[error("network error: {0}")]
    Network(String),

    #[error("auth error: {0}")]
    Auth(String),

    #[error("invalid configuration: {0}")]
    Config(String),

    #[error("container runtime '{binary}' not found on PATH; install Docker and retry")]
    ContainerRuntimeUnavailable { binary: String },

    #[error("failed to download Dockerfile for agent '{agent}': {message}")]
    AgentDockerfileDownloadFailed { agent: String, message: String },

    #[error("agent image build failed for agent '{agent}' (exit code {exit_code})")]
    AgentImageBuildFailed { agent: String, exit_code: i32 },

    #[error("image build for tag '{tag}' exited with code {exit_code}")]
    ImageBuildExitNonzero { tag: String, exit_code: i32 },

    // ── squad daemon lifecycle (WI 0113 F-02) ─────────────────────────────
    //
    // `SquadSupervisor` and `SquadDaemonEngine` live at Layer 1 and must not
    // reach for `CommandError`. Layer 2 maps these through the existing
    // `From<EngineError> for CommandError` impl, so the user-facing text is
    // unchanged from when the supervisor lived one layer up.
    #[error(
        "squad requires a container runtime. The configured runtime \"{runtime}\" cannot mount \
         squad's task directories or run workflow setup/teardown steps. Set runtime to \
         \"docker\" or \"apple-containers\" to use squad."
    )]
    SquadRuntimeUnsupported { runtime: String },

    /// `awman api` already holds the machine. Typed separately from a
    /// startup failure so a frontend can report a conflict as a conflict
    /// without matching on the message text (WI 0113 F-04).
    #[error("{0}")]
    SquadDaemonConflict(String),

    #[error("{0}")]
    SquadDaemonStartup(String),

    #[error("{0}")]
    SquadDaemonUnreachable(String),

    #[error("not implemented: {0}")]
    NotImplemented(&'static str),

    /// A paradigm-specific operation the active runtime does not provide —
    /// a local image probe against the sandbox tier, for instance. Typed
    /// rather than `NotImplemented` so a caller can tell "this runtime
    /// cannot" from "awman has not built this yet" (F-40b).
    #[error("{operation} is not supported on the {runtime} runtime")]
    UnsupportedOnRuntime {
        runtime: &'static str,
        operation: &'static str,
    },

    // ── Remote HTTP transport (WI 0114 F-28) ──────────────────────────────
    // The strings match `CommandError`'s twins exactly: Layer 2 maps these
    // onto them in `From<EngineError> for CommandError`, so a transport
    // failure reads and exits the same whether it surfaced from the engine
    // or from a Layer 2 client of the squad gateway.
    #[error("remote request timed out")]
    RemoteTimeout,

    #[error("remote connection refused: {0}")]
    RemoteConnectionRefused(String),

    #[error("remote returned status {status}: {body}")]
    RemoteHttpStatus { status: u16, body: String },

    #[error("malformed SSE event from remote: {0}")]
    MalformedSseEvent(String),

    #[error("remote transport error: {0}")]
    RemoteTransport(String),

    #[error("{0}")]
    Other(String),
}

impl EngineError {
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        EngineError::Io {
            path: path.into(),
            source,
        }
    }
}
