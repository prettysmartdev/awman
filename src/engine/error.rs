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

/// The failed merge behind [`EngineError::MergeFailed`].
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("merge of '{branch}' into the checkout at {git_root} failed: `{command}` exited {exit_code}; worktree kept at {worktree_path}")]
pub struct MergeFailedDetail {
    pub branch: String,
    pub worktree_path: PathBuf,
    pub git_root: PathBuf,
    /// The exact command that failed, as a user would type it.
    pub command: String,
    pub exit_code: i32,
    /// stdout then stderr of that command, trimmed.
    pub output: String,
}

/// Retry classification assigned by an image source before diagnostics are formatted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageRetryClass {
    Transient,
    Permanent,
}

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

    /// Host Git transport: the remote refused the credentials. Never
    /// retried; the command layer reports `failure.stage: "git_auth"`,
    /// `failure.code: "git_auth_failed"`.
    #[error("{message}")]
    GitAuth {
        operation: String,
        remote: String,
        message: String,
    },

    /// Host Git transport: no response from the remote (connect, reset,
    /// timeout). The only class a lease push retries.
    #[error("{message}")]
    GitTransport {
        operation: String,
        remote: String,
        message: String,
    },

    /// Host Git transport: the remote answered and refused the operation
    /// (non-fast-forward, protected branch, hook). Never retried.
    #[error("{message}")]
    GitRejected {
        operation: String,
        remote: String,
        message: String,
    },

    /// A `git merge` (or the squash merge's commit) exited non-zero. Carries
    /// the exact command and its output so the merge-fix agent's prompt and
    /// the frontend can show what failed. Conflicts and failures that never
    /// touched the index both land here. Boxed to keep `EngineError` small.
    #[error("{0}")]
    MergeFailed(Box<MergeFailedDetail>),

    /// A setup merge of `origin/<merge_ref>` hit conflicts; the merge was aborted and the
    /// clone is back at the pre-merge HEAD.
    #[error("merge of origin/{merge_ref} conflicts in: {}", conflicting_paths.join(", "))]
    MergeRefConflict {
        merge_ref: String,
        conflicting_paths: Vec<String>,
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

    /// Internal acquisition failure. The retry loop uses `retry`, never the
    /// redacted diagnostic. Acquisition exposes the existing `Network` error
    /// once its retry policy is finished.
    #[error("network error: {diagnostic}")]
    ImageSourceFailure {
        retry: ImageRetryClass,
        diagnostic: String,
    },

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

    /// A conflicting daemon already holds the machine. Typed separately from a
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

    /// A `--yolo` launch would carry the configured `yoloDisallowedTools`
    /// list, but this agent (or this runtime's delivery path for it) has no
    /// way to enforce a disallowed-tools list. Refused like
    /// [`Self::ToolRestrictionFlagMissing`], with the config key named so the
    /// user knows where the list came from.
    #[error(
        "invalid configuration: yoloDisallowedTools ({}) is set, but agent '{agent}' has no way \
         to enforce a disallowed-tools list here, so awman refuses to launch it with --yolo. \
         Use an agent that supports it (claude), run without --yolo, or clear the list for this \
         repository with `awman config set yoloDisallowedTools \"\"`.",
        tools.join(", ")
    )]
    YoloDisallowedToolsUnsupported { agent: String, tools: Vec<String> },

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

    /// The state directory can be upgraded in place from protocol `found`,
    /// but a session of the older awman that created it may still be alive
    /// (it holds a sandbox lifecycle lock, an image lease or the image-import
    /// lock). Nothing was changed, or the attempt was rolled back.
    #[error(
        "the builtin state directory {} was created by an older awman (protocol {found}) \
         that is still running or launching agents, or importing or removing an image, from \
         it; quit that older awman, then retry so this awman can upgrade the directory in place",
        state_dir.display()
    )]
    BuiltinCatalogUpgradeBlocked { state_dir: PathBuf, found: String },

    /// An earlier in-place upgrade left a catalog backup behind, and nothing
    /// proves that restoring it is safe: it may not be a complete awman
    /// backup, or the catalog was never migrated. The backup may be the only
    /// good copy of something, so it is never overwritten or removed.
    #[error(
        "cannot upgrade the builtin state directory {} from protocol {found}: a catalog backup \
         from an earlier, interrupted upgrade already exists at {}, and awman cannot prove it is \
         safe to restore automatically. If db/ still holds your catalog, move the backup aside \
         and retry; if db/ is missing or empty, restore it from the backup first",
        state_dir.display(),
        backup.display()
    )]
    BuiltinCatalogBackupExists {
        state_dir: PathBuf,
        found: String,
        backup: PathBuf,
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

    #[error("Apple Containers image '{reference}' is missing content required for {platform} ({detail}). Rebuild or re-pull that image in Apple Containers, then retry the import. If `builtin.builder.enabled` is true, `awman ready --build` can rebuild it")]
    AppleImageContentMissing {
        reference: String,
        platform: String,
        detail: String,
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

    // ── agent definitions (WI 0130) ──────────────────────────────────────
    /// unknown agent 'foo'; first-party: claude, codex, …; custom: pi (repo);
    /// define one with: awman new agent foo — followed by one line per note.
    #[error("{}", render_unknown_agent(name, first_party, custom, notes))]
    UnknownAgent {
        name: String,
        first_party: Vec<String>,
        /// Pre-rendered `"<name> (<scope>)"` entries.
        custom: Vec<String>,
        notes: Vec<String>,
    },

    /// `<path>: <problem>`, one line per problem.
    #[error("{}", render_definition_problems(path, problems))]
    AgentDefinitionInvalid {
        path: PathBuf,
        problems: Vec<String>,
    },

    #[error(
        "agent definition '{name}' ({}) has not been approved for this repo; run 'awman ready --agent {name}' interactively to review it",
        path.display()
    )]
    AgentDefinitionUntrusted {
        name: String,
        path: PathBuf,
        sha256: String,
    },

    #[error(
        "custom agent '{name}' runs only on container runtimes (runtime.kind docker, apple or builtin); '{runtime}' is not supported for custom agents yet"
    )]
    CustomAgentRuntimeUnsupported { name: String, runtime: String },
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

/// Render [`EngineError::UnknownAgent`].
fn render_unknown_agent(
    name: &str,
    first_party: &[String],
    custom: &[String],
    notes: &[String],
) -> String {
    let custom = if custom.is_empty() {
        "(none)".to_string()
    } else {
        custom.join(", ")
    };
    let mut out = format!(
        "unknown agent '{name}'; first-party: {}; custom: {custom}; define one with: awman new agent {name}",
        first_party.join(", ")
    );
    for note in notes {
        out.push('\n');
        out.push_str(note);
    }
    out
}

/// Render [`EngineError::AgentDefinitionInvalid`].
fn render_definition_problems(path: &std::path::Path, problems: &[String]) -> String {
    // Problems quote untrusted keys and paths: never let one reach a terminal raw.
    use crate::data::config::agent_definition::escape_for_display;
    let path = escape_for_display(&path.display().to_string());
    problems
        .iter()
        .map(|p| format!("{path}: {}", escape_for_display(p)))
        .collect::<Vec<_>>()
        .join("\n")
}

impl From<crate::data::config::agent_definition::AgentDefinitionInvalid> for EngineError {
    fn from(e: crate::data::config::agent_definition::AgentDefinitionInvalid) -> Self {
        EngineError::AgentDefinitionInvalid {
            path: e.path,
            problems: e.problems,
        }
    }
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
    /// backend is missing or failing, 2 for input/runtime refusals, 4 for missing
    /// agents, and 1 for other engine failures. It
    /// lives beside `Display` so every frontend reads the same mapping.
    pub fn exit_code(&self) -> u8 {
        match self {
            // A referenced agent that does not exist is "resource missing",
            // the same class as a vanished agent container.
            EngineError::AgentNotFound { .. } | EngineError::UnknownAgent { .. } => 4,
            EngineError::Container(_) | EngineError::ContainerRuntimeUnavailable { .. } => 3,
            EngineError::UnknownRuntime { .. }
            | EngineError::AgentDefinitionUntrusted { .. }
            | EngineError::CustomAgentRuntimeUnsupported { .. } => 2,
            // A refused or failed in-place catalog upgrade is an ordinary
            // engine failure, never "Docker is missing" (3).
            EngineError::BuiltinCatalogMismatch { .. }
            | EngineError::BuiltinCatalogUpgradeBlocked { .. }
            | EngineError::BuiltinCatalogBackupExists { .. }
            | EngineError::MergeRefConflict { .. } => 1,
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
