//! Layer 1 error type — `EngineError`.
//!
//! Wraps `DataError` for failures bubbling up from Layer 0. Higher layers
//! wrap `EngineError` in their own error types; Layer 1 does not depend on
//! higher-layer errors.

use std::path::PathBuf;

use thiserror::Error;

use crate::data::config::image_source::ImageSourceKind;
use crate::data::error::DataError;
use crate::engine::oci::resolve::ImageBuildGuidance;

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

    /// A Docker or Apple Containers CLI backend failure. Builtin-runtime and
    /// image-builder failures have their own variants: this one exits 3
    /// ("container runtime unavailable"), which is wrong for them.
    #[error("container backend error: {0}")]
    Container(String),
    #[error("agent {id} no longer exists")]
    AgentNotFound { id: String },

    #[error("sandbox backend error: {0}")]
    Sandbox(String),

    #[error("agent '{agent}' does not support ACP (Agent Client Protocol) launch mode")]
    AcpUnsupported { agent: String },

    #[error("ACP protocol error: {0}")]
    Acp(String),

    #[error(
        "runtime {runtime} was given {got}-paradigm options; \
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

    #[error(
        "agent image build requires the project base image first ({tag}); run `awman ready --build`"
    )]
    AgentRequiresProjectImage { tag: String },

    #[error("network error: {0}")]
    Network(String),

    #[error("auth error: {0}")]
    Auth(String),

    #[error("invalid configuration: {0}")]
    Config(String),

    #[error(
        "container runtime '{binary}' not found on PATH; {}",
        container_runtime_install_hint(binary)
    )]
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
    #[error(
        "{operation} is not supported on the {runtime} runtime; \
         change `runtime` in the global config to one that provides it"
    )]
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

    // ── Builtin runtime and image acquisition (WI 0119) ───────────────────
    //
    // The image-source fields are named `source_kind`, not `source`: thiserror
    // treats a field called `source` as the wrapped error.
    /// The builtin microVM runtime cannot run here: built without it, an
    /// unsupported platform, no hypervisor access, a missing entitlement.
    /// Layer 2 treats it like an unreachable Docker daemon.
    #[error("the builtin runtime is unavailable: {reason}")]
    BuiltinRuntimeUnavailable { reason: String },

    #[error(
        "the builtin runtime refuses to start while {variable} is set: it would \
         replace awman's embedded runtime. Unset {variable} and retry."
    )]
    AmbientRuntimeOverride { variable: String },

    /// A session's repo `builtin.stateDir` names a different directory than
    /// the one this process's builtin runtime uses. The state directory is
    /// process-wide, so that session's launch is refused rather than silently
    /// run under the process's directory.
    #[error(
        "invalid configuration: this repository's builtin.stateDir resolves to {}, but this \
         awman process uses the builtin state directory {}; the state directory is \
         process-wide. Remove the repository's builtin.stateDir or set it to {}.",
        session.display(),
        process.display(),
        process.display()
    )]
    BuiltinStateDirMismatch { process: PathBuf, session: PathBuf },

    /// A launch carries an allowed- or disallowed-tools list but the agent
    /// declares no flag to deliver it. Every runtime refuses the launch:
    /// silently dropping a tool restriction is the less safe outcome.
    #[error(
        "invalid configuration: this agent declares no {restriction}-tools flag, so its \
         {restriction}-tools list cannot be applied; awman refuses to launch it without the \
         restriction. Remove the {restriction}-tools list or use an agent that supports it."
    )]
    ToolRestrictionFlagMissing { restriction: ToolRestriction },

    #[error("{source_kind} image source is not supported: {reason}")]
    UnsupportedImageSource {
        source_kind: ImageSourceKind,
        reason: String,
    },

    #[error("{source_kind} image source is blocked: {reason}")]
    ImageSourceBlocked {
        source_kind: ImageSourceKind,
        reason: String,
    },

    /// No source is configured for an image. The guidance follows the
    /// image's builder selection, so it never says the builtin runtime
    /// cannot build when its builder is on.
    #[error("no image source is configured for '{}': {guidance}", guidance.tag)]
    ImageSourceUnconfigured { guidance: Box<ImageBuildGuidance> },

    #[error("digest mismatch for '{reference}': expected {expected}, got {actual}")]
    ImageDigestMismatch {
        reference: String,
        expected: String,
        actual: String,
    },

    #[error("platform mismatch for '{reference}': wanted {wanted}, found {found}")]
    ImagePlatformMismatch {
        reference: String,
        wanted: String,
        found: String,
    },

    #[error("image archive {path} rejected: {reason}")]
    ImageArchiveRejected { path: PathBuf, reason: String },

    #[error("not enough disk space at {path}: {needed} bytes needed, {available} bytes available")]
    InsufficientDiskSpace {
        path: PathBuf,
        needed: u64,
        available: u64,
    },

    #[error("the {runtime} runtime cannot honour {request}: {reason}")]
    UnsupportedResourceRequest {
        runtime: &'static str,
        request: String,
        reason: String,
    },

    #[error(
        "socket path {path} is longer than the {limit}-byte limit; configure a shorter \
         builtin state directory"
    )]
    SocketPathTooLong { path: PathBuf, limit: usize },

    #[error(
        "builtin runtime worker protocol mismatch: expected {expected}, found {found}; \
         stop running agents started by an older awman and retry"
    )]
    WorkerProtocolMismatch { expected: String, found: String },

    /// The whole state directory was created under another protocol, so no
    /// agent in it can be listed or stopped by this awman. Builtin VMs do not
    /// outlive the awman that launched them, so recovery is: quit that awman,
    /// then retire the directory.
    #[error(
        "builtin runtime worker protocol mismatch: the builtin state directory {} was \
         created with protocol {found}, but this awman uses {expected}. Quit any older awman \
         still running agents from it, then move or delete that directory (or set \
         builtin.stateDir to a new one) and run `awman ready` to re-import images",
        state_dir.display()
    )]
    BuiltinCatalogMismatch {
        expected: String,
        found: String,
        state_dir: PathBuf,
    },

    /// A builtin handle outlived its sandbox: the name was reused by a newer
    /// launch, so acting on the old handle would hit the replacement.
    #[error("builtin agent {name} was replaced by a newer sandbox with the same name")]
    BuiltinAgentReplaced { name: String },

    // ── Builtin runtime and builder failures (WI 0126 F1) ─────────────────
    //
    // Typed so every frontend renders the same guidance and the exit code
    // follows the failure: none of these is "Docker is missing" (exit 3).
    /// The host is not a released builtin target.
    #[error(
        "the builtin runtime is unavailable: the experimental builtin runtime is only \
         available on macOS on Apple Silicon (macos/aarch64) in this release; this host is \
         {os}/{arch}. Choose another runtime (docker, apple-containers or \
         docker-sbx-experimental)"
    )]
    BuiltinTargetUnreleased { os: String, arch: String },

    /// The attach endpoint of a builtin agent is gone: only the awman that
    /// launched it can attach.
    #[error(
        "cannot attach to {name}: no live attach endpoint ({detail}); the launching awman must \
         remain alive, and reattachment after its exit is unsupported"
    )]
    BuiltinAttachUnavailable { name: String, detail: String },

    #[error("builtin image {image} is not imported; run `awman ready` to import it")]
    BuiltinImageNotImported { image: String },

    #[error("builtin image {image} needs preparation for the configured source; run `awman ready` before launching it")]
    BuiltinImageNeedsPreparation { image: String },

    #[error("the synchronous builtin runtime API cannot wait under a current-thread Tokio handle; use a multi-thread Tokio runtime for builtin launches")]
    BuiltinBlockingContextUnsupported,

    #[error(
        "a builtin sandbox for agent '{name}' already exists; stop or remove it (`awman clean`) \
         or choose another name"
    )]
    BuiltinSandboxExists { name: String },

    /// A cached-only acquisition found no cached copy.
    #[error("image '{tag}' is not in the builtin image cache ({source_desc}); run `awman ready` to import it")]
    ImageNotCached { tag: String, source_desc: String },

    /// The configured source does not hold the image.
    #[error("image '{reference}' was not found in {location}")]
    SourceImageNotFound { reference: String, location: String },

    #[error("Apple Containers `{route}` failed ({code}): {message}")]
    AppleImageStoreFailed {
        route: String,
        code: String,
        message: String,
    },

    #[error("image acquisition was cancelled")]
    ImageAcquisitionCancelled,

    #[error("image build timed out")]
    ImageBuildTimedOut,

    /// The source-matched image builder cannot be used at all.
    #[error("the {builder} image builder is unavailable: {detail}")]
    BuilderUnavailable { builder: String, detail: String },

    /// The source-matched image builder ran but its result cannot be used.
    #[error("{0}")]
    ImageBuilder(BuilderFailure),

    #[error("{0}")]
    Other(String),
}

/// Why a source-matched image build or inspection could not be used.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum BuilderFailure {
    #[error("Docker returned an unreadable image inspection: {detail}")]
    UnreadableInspection { detail: String },

    #[error("{builder} could not identify its image store (exit {exit_code})")]
    ImageStoreUnknown { builder: String, exit_code: i32 },

    #[error("{builder} did not report its image store; cannot verify the image ID")]
    ImageStoreUnreported { builder: String },

    #[error("{builder} reported an unreadable image store: {detail}")]
    ImageStoreUnreadable { builder: String, detail: String },

    #[error("{builder} could not inspect {reference} (exit {exit_code}): {output}")]
    InspectFailed {
        builder: String,
        reference: String,
        exit_code: i32,
        output: String,
    },

    #[error("{builder} returned no image inspection for {reference}")]
    InspectionMissing { builder: String, reference: String },

    #[error(
        "{builder} reported image {image_id} for {reference}, but inspecting it returned a \
         different image"
    )]
    BuiltImageChanged {
        builder: String,
        image_id: String,
        reference: String,
    },

    #[error(
        "{builder} stored the build of {reference} as a multi-platform index; awman needs a \
         single {platform} image to verify the import"
    )]
    MultiPlatformOutput {
        builder: String,
        reference: String,
        platform: String,
    },

    #[error(
        "{builder} reported image {image_id} for {reference}, but it is not a {platform} image \
         in that store"
    )]
    BuiltImageMissing {
        builder: String,
        image_id: String,
        reference: String,
        platform: String,
    },

    #[error("Apple builder did not report a verifiable output manifest digest")]
    AppleManifestUnreported,

    #[error("Apple builder produced no inspectable image")]
    AppleImageMissing,

    #[error("Apple image {reference} changed between build and source-store inspection")]
    AppleImageChanged { reference: String },

    #[error("create build receipt: {detail}")]
    ImageIdFileCreate { detail: String },

    #[error("read build receipt: {detail}")]
    ImageIdFileRead { detail: String },

    #[error("invalid Docker build receipt: {detail}")]
    ImageIdFileInvalid { detail: String },

    #[error(
        "the Docker builder `default` for {endpoint} uses the `{driver}` driver, which does not \
         store images in that Engine; awman builds only with the `docker` driver"
    )]
    UnsupportedBuildxDriver { endpoint: String, driver: String },

    #[error("`docker buildx inspect default` failed for {endpoint} (exit {exit_code}): {output}")]
    BuildxInspectFailed {
        endpoint: String,
        exit_code: i32,
        output: String,
    },

    #[error("project base image {tag} changed after its build")]
    BaseImageChanged { tag: String },
}

/// Which tool restriction a [`EngineError::ToolRestrictionFlagMissing`] names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolRestriction {
    Allowed,
    Disallowed,
}

impl std::fmt::Display for ToolRestriction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            ToolRestriction::Allowed => "allowed",
            ToolRestriction::Disallowed => "disallowed",
        })
    }
}

/// What to install when a container CLI is missing: the runtime that binary
/// belongs to, not always Docker.
fn container_runtime_install_hint(binary: &str) -> &'static str {
    let name = std::path::Path::new(binary)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(binary);
    match name {
        "container" => "install Apple Containers and retry",
        "docker" | "docker.exe" => "install Docker and retry",
        _ => "install it and retry",
    }
}

impl EngineError {
    /// The process exit code for this failure, per the classes in
    /// `aspec/uxui/cli.md`: 3 only when a Docker or Apple Containers CLI
    /// backend is missing or failing, 1 for every other engine failure. It
    /// lives beside `Display` so every frontend reads the same mapping.
    pub fn exit_code(&self) -> u8 {
        match self {
            EngineError::AgentNotFound { .. } => 4,
            EngineError::Container(_) | EngineError::ContainerRuntimeUnavailable { .. } => 3,
            EngineError::UnknownRuntime { .. } => 2,
            _ => 1,
        }
    }

    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        EngineError::Io {
            path: path.into(),
            source,
        }
    }
}

/// Run a Layer 0 operation that polls a `check` hook, with the hook driven by
/// an engine-level check (cancellation, deadline). When the hook stops the
/// operation, the engine error that stopped it is returned as-is rather than
/// the placeholder `DataError` that carried the stop through Layer 0.
pub(crate) fn run_checked<T>(
    mut check: impl FnMut() -> Result<(), EngineError>,
    op: impl FnOnce(&mut dyn FnMut() -> Result<(), DataError>) -> Result<T, DataError>,
) -> Result<T, EngineError> {
    let mut stopped = None;
    let result = op(&mut || {
        check().map_err(|e| {
            stopped = Some(e);
            DataError::Other("operation stopped".into())
        })
    });
    match (result, stopped) {
        (_, Some(e)) => Err(e),
        (result, None) => result.map_err(EngineError::from),
    }
}

#[cfg(test)]
mod run_checked_tests {
    use super::*;

    #[test]
    fn the_stopping_engine_error_survives_the_layer_0_round_trip() {
        let err = run_checked(
            || Err(EngineError::Network("deadline".into())),
            |check| check().map(|_| 1),
        )
        .unwrap_err();
        assert!(matches!(err, EngineError::Network(m) if m == "deadline"));
    }

    #[test]
    fn a_data_error_is_wrapped_and_success_passes_through() {
        let err =
            run_checked(|| Ok(()), |_| Err::<(), _>(DataError::Other("boom".into()))).unwrap_err();
        assert!(matches!(err, EngineError::Data(_)));
        assert_eq!(
            run_checked(|| Ok(()), |check| check().map(|_| 7)).unwrap(),
            7
        );
    }
}

/// WI 0126 F1: the exact text and exit code every frontend shows for each
/// builtin-runtime and builder failure. The CLI prints `Display` verbatim
/// (prefixed `awman: `), the TUI and the API print it as-is.
#[cfg(test)]
mod user_text_tests {
    use super::*;

    fn s(v: &str) -> String {
        v.to_string()
    }

    #[test]
    fn builtin_and_builder_failures_render_their_guidance_and_exit_1() {
        let cases: Vec<(EngineError, &str)> = vec![
            (
                EngineError::BuiltinTargetUnreleased {
                    os: s("linux"),
                    arch: s("x86_64"),
                },
                "the builtin runtime is unavailable: the experimental builtin runtime is only \
                 available on macOS on Apple Silicon (macos/aarch64) in this release; this host \
                 is linux/x86_64. Choose another runtime (docker, apple-containers or \
                 docker-sbx-experimental)",
            ),
            (
                EngineError::BuiltinAttachUnavailable {
                    name: s("awman-x"),
                    detail: s("connection refused"),
                },
                "cannot attach to awman-x: no live attach endpoint (connection refused); the \
                 launching awman must remain alive, and reattachment after its exit is \
                 unsupported",
            ),
            (
                EngineError::BuiltinImageNotImported {
                    image: s("awman-r-claude:latest"),
                },
                "builtin image awman-r-claude:latest is not imported; run `awman ready` to \
                 import it",
            ),
            (
                EngineError::BuiltinSandboxExists { name: s("claude") },
                "a builtin sandbox for agent 'claude' already exists; stop or remove it \
                 (`awman clean`) or choose another name",
            ),
            (
                EngineError::ImageNotCached {
                    tag: s("t:1"),
                    source_desc: s("archive /a.tar"),
                },
                "image 't:1' is not in the builtin image cache (archive /a.tar); run `awman \
                 ready` to import it",
            ),
            (
                EngineError::SourceImageNotFound {
                    reference: s("team/a:1"),
                    location: s("registry ghcr.io"),
                },
                "image 'team/a:1' was not found in registry ghcr.io",
            ),
            (
                EngineError::AppleImageStoreFailed {
                    route: s("export"),
                    code: s("internal"),
                    message: s("boom"),
                },
                "Apple Containers `export` failed (internal): boom",
            ),
            (
                EngineError::ImageAcquisitionCancelled,
                "image acquisition was cancelled",
            ),
            (EngineError::ImageBuildTimedOut, "image build timed out"),
            (
                EngineError::BuilderUnavailable {
                    builder: s("Docker at unix:///d.sock"),
                    detail: s("`docker info` exited 1: down"),
                },
                "the Docker at unix:///d.sock image builder is unavailable: `docker info` \
                 exited 1: down",
            ),
            (
                EngineError::BuilderUnavailable {
                    builder: s("Apple Containers"),
                    detail: s("the Apple Containers image transport is unavailable"),
                },
                "the Apple Containers image builder is unavailable: the Apple Containers image \
                 transport is unavailable",
            ),
            (
                EngineError::ImageBuilder(BuilderFailure::UnsupportedBuildxDriver {
                    endpoint: s("unix:///d.sock"),
                    driver: s("docker-container"),
                }),
                "the Docker builder `default` for unix:///d.sock uses the `docker-container` \
                 driver, which does not store images in that Engine; awman builds only with the \
                 `docker` driver",
            ),
            (
                EngineError::ImageBuilder(BuilderFailure::BuildxInspectFailed {
                    endpoint: s("unix:///d.sock"),
                    exit_code: 1,
                    output: s("err"),
                }),
                "`docker buildx inspect default` failed for unix:///d.sock (exit 1): err",
            ),
            (
                EngineError::ImageBuilder(BuilderFailure::BaseImageChanged { tag: s("p:1") }),
                "project base image p:1 changed after its build",
            ),
            (
                EngineError::ImageBuilder(BuilderFailure::AppleManifestUnreported),
                "Apple builder did not report a verifiable output manifest digest",
            ),
        ];
        for (error, text) in cases {
            assert_eq!(error.to_string(), text, "{error:?}");
            assert_eq!(error.exit_code(), 1, "{error:?}");
            assert!(
                !text.contains("install Docker"),
                "builtin/builder failures never advise installing Docker: {text}"
            );
        }
    }

    #[test]
    fn every_builder_failure_keeps_its_wording() {
        let b = || s("Docker at unix:///d.sock");
        let cases: Vec<(BuilderFailure, &str)> = vec![
            (
                BuilderFailure::UnreadableInspection { detail: s("eof") },
                "Docker returned an unreadable image inspection: eof",
            ),
            (
                BuilderFailure::ImageStoreUnknown {
                    builder: b(),
                    exit_code: 2,
                },
                "Docker at unix:///d.sock could not identify its image store (exit 2)",
            ),
            (
                BuilderFailure::ImageStoreUnreported { builder: b() },
                "Docker at unix:///d.sock did not report its image store; cannot verify the \
                 image ID",
            ),
            (
                BuilderFailure::ImageStoreUnreadable {
                    builder: b(),
                    detail: s("bad json"),
                },
                "Docker at unix:///d.sock reported an unreadable image store: bad json",
            ),
            (
                BuilderFailure::InspectFailed {
                    builder: b(),
                    reference: s("a:1"),
                    exit_code: 1,
                    output: s("x"),
                },
                "Docker at unix:///d.sock could not inspect a:1 (exit 1): x",
            ),
            (
                BuilderFailure::InspectionMissing {
                    builder: b(),
                    reference: s("a:1"),
                },
                "Docker at unix:///d.sock returned no image inspection for a:1",
            ),
            (
                BuilderFailure::BuiltImageChanged {
                    builder: b(),
                    image_id: s("sha256:1"),
                    reference: s("a:1"),
                },
                "Docker at unix:///d.sock reported image sha256:1 for a:1, but inspecting it \
                 returned a different image",
            ),
            (
                BuilderFailure::MultiPlatformOutput {
                    builder: b(),
                    reference: s("a:1"),
                    platform: s("linux/arm64"),
                },
                "Docker at unix:///d.sock stored the build of a:1 as a multi-platform index; \
                 awman needs a single linux/arm64 image to verify the import",
            ),
            (
                BuilderFailure::BuiltImageMissing {
                    builder: b(),
                    image_id: s("sha256:1"),
                    reference: s("a:1"),
                    platform: s("linux/arm64"),
                },
                "Docker at unix:///d.sock reported image sha256:1 for a:1, but it is not a \
                 linux/arm64 image in that store",
            ),
            (
                BuilderFailure::AppleImageMissing,
                "Apple builder produced no inspectable image",
            ),
            (
                BuilderFailure::AppleImageChanged {
                    reference: s("a:1"),
                },
                "Apple image a:1 changed between build and source-store inspection",
            ),
            (
                BuilderFailure::ImageIdFileCreate {
                    detail: s("denied"),
                },
                "create build receipt: denied",
            ),
            (
                BuilderFailure::ImageIdFileRead { detail: s("gone") },
                "read build receipt: gone",
            ),
            (
                BuilderFailure::ImageIdFileInvalid { detail: s("bad") },
                "invalid Docker build receipt: bad",
            ),
        ];
        for (failure, text) in cases {
            let error = EngineError::ImageBuilder(failure);
            assert_eq!(error.to_string(), text);
            assert_eq!(error.exit_code(), 1);
        }
    }

    #[test]
    fn only_docker_and_apple_backend_failures_exit_3() {
        assert_eq!(EngineError::Container(s("daemon down")).exit_code(), 3);
        assert_eq!(
            EngineError::ContainerRuntimeUnavailable {
                binary: s("docker")
            }
            .exit_code(),
            3
        );
        for error in [
            EngineError::BuiltinRuntimeUnavailable {
                reason: s("no kvm"),
            },
            EngineError::Config(s("bad")),
            EngineError::ImageBuildExitNonzero {
                tag: s("t"),
                exit_code: 2,
            },
            EngineError::Other(s("x")),
        ] {
            assert_eq!(error.exit_code(), 1, "{error:?}");
        }
        // The CLI's old Docker-install advice is no longer part of any
        // engine text, so no frontend can print it for a backend failure.
        assert_eq!(
            EngineError::Container(s("daemon down")).to_string(),
            "container backend error: daemon down"
        );
    }

    /// A missing CLI names the runtime it belongs to: Apple Containers for
    /// `container`, Docker for `docker`.
    #[test]
    fn a_missing_container_cli_names_its_own_runtime() {
        let missing = |binary: &str| {
            EngineError::ContainerRuntimeUnavailable {
                binary: binary.into(),
            }
            .to_string()
        };
        assert!(missing("docker").ends_with("install Docker and retry"));
        assert!(missing("/usr/local/bin/container").ends_with("install Apple Containers and retry"));
        assert!(!missing("container").contains("Docker"));
        assert!(missing("sbx").ends_with("install it and retry"));
    }

    #[test]
    fn guidance_once_only_in_the_cli_is_now_the_engine_text() {
        assert_eq!(
            EngineError::UnsupportedOnRuntime {
                runtime: "builtin",
                operation: "stats",
            }
            .to_string(),
            "stats is not supported on the builtin runtime; change `runtime` in the global \
             config to one that provides it"
        );
        assert_eq!(
            EngineError::AgentRequiresProjectImage { tag: s("p:1") }.to_string(),
            "agent image build requires the project base image first (p:1); run `awman ready \
             --build`"
        );
    }
}
