//! Layer 2 error type — `CommandError`.
//!
//! Wraps `EngineError` (Layer 1) and `DataError` (Layer 0) for failures
//! bubbling up from below. Layer 3 wraps `CommandError` in its own
//! user-facing presentation; Layer 2 does not depend on Layer 3 errors.

use std::path::PathBuf;

use thiserror::Error;

use crate::data::error::DataError;
use crate::engine::error::EngineError;

#[derive(Debug, Error)]
pub enum CommandError {
    #[error(transparent)]
    Engine(EngineError),

    #[error(transparent)]
    Data(DataError),

    // ── Dispatch / catalogue ─────────────────────────────────────────────
    /// `suggestions` are full command paths the catalogue considers near
    /// misses for what was typed, nearest first, and may be empty. They are
    /// filled by whoever detects the unknown command — the catalogue knows
    /// the command list, and a frontend must not (WI 0114 F-43). A frontend
    /// chooses how to render them; it does not compute them.
    #[error("unknown command: {path:?}")]
    UnknownCommand {
        path: Vec<String>,
        suggestions: Vec<String>,
    },

    #[error("command '{command}' is not available via the {frontend} frontend")]
    NotAvailableForFrontend { command: String, frontend: String },

    #[error("unknown flag '{flag}' for command {command:?}")]
    UnknownFlag { command: Vec<String>, flag: String },

    #[error("missing required flag '{flag}' for command {command:?}")]
    MissingRequiredFlag { command: Vec<String>, flag: String },

    #[error("missing required argument '{argument}' for command {command:?}")]
    MissingRequiredArgument {
        command: Vec<String>,
        argument: String,
    },

    #[error("unexpected argument '{argument}' for command {command:?}")]
    UnexpectedArgument {
        command: Vec<String>,
        argument: String,
    },

    #[error("flags '{a}' and '{b}' are mutually exclusive on {command:?}")]
    MutuallyExclusive {
        command: Vec<String>,
        a: String,
        b: String,
    },

    #[error("invalid value for flag '{flag}' on {command:?}: {reason}")]
    InvalidFlagValue {
        command: Vec<String>,
        flag: String,
        reason: String,
    },

    #[error("invalid value for argument '{argument}' on {command:?}: {reason}")]
    InvalidArgumentValue {
        command: Vec<String>,
        argument: String,
        reason: String,
    },

    // ── TUI command-box parsing ───────────────────────────────────────────
    #[error("could not parse command-box input: {0}")]
    CommandBoxParse(String),

    // ── Workflow / worktree lifecycle ─────────────────────────────────────
    #[error("command aborted by user")]
    Aborted,

    /// A failed worktree merge-back that is still unresolved after the
    /// merge-fix agent ran. The merge state is left in `git_root`.
    #[error(
        "merge of branch {branch} is still unresolved in {git_root} (worktree at {worktree_path})"
    )]
    MergeConflict {
        branch: String,
        worktree_path: PathBuf,
        git_root: PathBuf,
    },

    #[error("merge target '{target_branch}' has uncommitted changes; worktree for '{branch}' kept at {worktree_path}")]
    MergeTargetDirty {
        target_branch: String,
        branch: String,
        worktree_path: PathBuf,
    },

    #[error("worktree already exists at {path} (branch {branch}); pass --on-existing-worktree resume to continue in it, or --on-existing-worktree recreate to delete it and its branch and start over")]
    WorktreeExists { path: PathBuf, branch: String },

    // ── Remote command ────────────────────────────────────────────────────
    #[error("remote target address is missing or invalid")]
    MissingRemoteAddress,

    #[error("remote API key is missing")]
    MissingApiKey,

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

    // ── API ───────────────────────────────────────────────────────────────
    #[error("API workdir not found: {path}")]
    ApiWorkdirNotFound { path: PathBuf },

    #[error("worker workdir root '{root}' is inside a git repository ({git_root}); list it in daemon.features.worker.push.workDirs.allow instead, or choose a directory that only contains repositories")]
    ApiWorkdirRootInvalid { root: PathBuf, git_root: PathBuf },

    // ── Daemon ────────────────────────────────────────────────────────────
    #[error("the awman daemon is not running; start it with `awman daemon start`")]
    DaemonNotRunning,

    #[error("the awman daemon is already running (PID {pid}); stop it with `awman daemon stop`")]
    DaemonAlreadyRunning { pid: u32 },

    #[error(
        "no daemon feature is enabled; enable one first, for example \
         `awman config set daemon.features.worker.enabled true`"
    )]
    DaemonNoFeatures,

    /// The daemon answered a local request with a 4xx/5xx status; `body` is
    /// the response body as returned.
    #[error("the daemon returned HTTP {status}: {body}")]
    DaemonHttpStatus { status: u16, body: String },

    // ── Session creation (multi-session frontends: API, future desktop, …) ──
    // Typed so that a frontend can map each to the right transport status
    // (e.g. HTTP 400 vs 403) without inspecting the message string. Display
    // text names the failed rule for allowlist refusals.
    #[error("session_type must be 'local' or 'remote'; got '{got}'")]
    SessionInvalidType { got: String },

    #[error("workdir is required when session_type is 'local'")]
    SessionWorkdirRequired,

    #[error("Cannot resolve path: {path}")]
    SessionWorkdirUnresolvable { path: String },

    #[error("Workdir '{requested}' is not allowed: {rule}")]
    SessionWorkdirNotAllowed {
        requested: String,
        rule: crate::command::session_create::WorkdirRefusal,
        allowed: Vec<String>,
        roots: Vec<String>,
    },

    #[error("repo_url is required when session_type is 'remote'")]
    SessionRepoUrlRequired,

    #[error("repo_url must be non-empty")]
    SessionRepoUrlEmpty,

    #[error("repo_url must use http(s), ssh, or git scheme")]
    SessionRepoUrlInvalidScheme { url: String },

    /// `base_ref`, `merge_refs` or `push` given on a local session (WI 0133).
    #[error("{field} is only valid when session_type is 'remote'")]
    SessionFieldRemoteOnly { field: &'static str },

    /// `base_ref`, `merge_refs` or `push` given on a remote session without
    /// `branch`: they all act on the session branch (WI 0133).
    #[error("{field} requires branch on a remote session")]
    SessionFieldRequiresBranch { field: &'static str },

    /// A `branch`, `base_ref` or `merge_refs` entry that is not an acceptable
    /// ref spelling (WI 0133).
    #[error("{field} '{value}' is not a valid git ref name: {reason}")]
    SessionRefInvalid {
        field: &'static str,
        value: String,
        reason: String,
    },

    // ── Session Git credentials and command secrets (WI 0145) ─────────────
    /// `git_auth` on a session whose repository the session helper cannot
    /// serve: not a remote session, or not `https://` (or `http://` to a
    /// loopback host).
    #[error("git_auth is not supported for this session: {reason}")]
    GitAuthUnsupportedUrl { reason: String },

    /// `git_auth` on a platform without the session credential socket.
    #[error("git_auth is not supported on this platform")]
    GitAuthUnsupportedPlatform,

    /// A malformed `git_auth` value. The reason never contains the secret.
    #[error("invalid git_auth: {reason}")]
    GitAuthInvalid { reason: String },

    /// An authenticated Git operation at admission (the merge-ref
    /// advertisement) was refused for its credentials.
    #[error("{message}")]
    GitAuthFailed { message: String },

    /// The session was created with `git_auth`, but the daemon restarted and
    /// no longer holds that in-memory credential.
    #[error(
        "the daemon restarted and this session's Git credentials were lost; open a new session"
    )]
    CredentialsLost,

    /// A queued command had `secrets`, but the daemon restarted before it
    /// ran and no longer holds their in-memory values.
    #[error("the daemon restarted and this command's secrets were lost; submit it again")]
    CommandSecretsLost,

    // ── Remote ────────────────────────────────────────────────────────────
    #[error("no remote session id; pass --session <id> or run `awman remote session start`")]
    RemoteSessionMissing,

    #[error("failed to kill remote session '{session_id}': {reason}")]
    RemoteSessionKillFailed { session_id: String, reason: String },

    // ── Work item / spec ──────────────────────────────────────────────────────
    /// Resolving, staging or creating a work item (WI 0147).
    #[error(transparent)]
    WorkItem(crate::command::work_item::WorkItemError),

    /// Live draft sync or its final upload failed; the draft is kept.
    #[error(transparent)]
    DraftSync(crate::command::work_item::draft_sync::DraftSyncError),

    #[error("work item {number} not found in aspec/work-items/")]
    WorkItemNotFound { number: u32 },

    #[error("spec template missing at {path}; run `awman init --aspec` to create it")]
    SpecTemplateMissing { path: std::path::PathBuf },

    #[error("invalid overlay spec '{spec}': {reason}")]
    InvalidOverlaySpec { spec: String, reason: String },

    #[error("unknown config field '{name}'; similar fields: {suggestions}")]
    UnknownConfigField { name: String, suggestions: String },

    #[error("stdin is not a TTY; provide --{prompt} on the command line")]
    InteractiveInputUnavailable { prompt: String },

    #[error("workflow file not found: {path}")]
    WorkflowFileNotFound { path: std::path::PathBuf },

    // ── Workflow prerequisites (WI 0149) ──────────────────────────────────
    /// A prerequisite is still unsatisfied after prompts and policy, and the
    /// `unresolved` policy is `fail`. Raised before any container starts.
    #[error("workflow prerequisites unresolved: {}", report.summary_line())]
    WorkflowPrerequisitesUnresolved {
        report: Box<crate::data::workflow_prerequisites::WorkflowPrerequisiteReport>,
    },

    /// An API or squad-daemon client passed a `--prerequisite-*` flag that
    /// grants more than the server's global policy allows.
    #[error("flag '{flag}' would grant beyond the server's global prerequisite policy; set workflows.prerequisites.allowApiClientGrants in the server's global config")]
    PrerequisiteWideningRefused { flag: String },

    /// A `--prerequisite-*` value this build cannot honour yet.
    #[error("flag '{flag}' is unsupported until per-command secrets are available")]
    PrerequisiteFlagUnsupported { flag: String },

    // ── Agents (`awman agents …`) ─────────────────────────────────────────
    #[error("no agent matches {reference:?}; run `awman agents ls` to see running agents")]
    AgentNotFound { reference: String },

    #[error("agent {reference:?} is no longer running")]
    AgentNoLongerRunning { reference: String },

    #[error("agent {reference:?} is stopped, not running; `awman clean` removes stopped agents")]
    AgentNotRunning { reference: String },

    #[error(
        "agent {reference:?} was replaced by a newer one with the same name; re-run `awman agents ls`"
    )]
    AgentReplaced { reference: String },

    #[error("agent reference {reference:?} is ambiguous; matches: {}", candidates.join(", "))]
    AgentAmbiguous {
        reference: String,
        candidates: Vec<String>,
    },

    #[error("an agent id, id prefix or name is required")]
    AgentReferenceEmpty,

    #[error(
        "cannot attach to {name}: its launching awman process has exited; {runtime} can only attach while that process is running"
    )]
    AgentOwnerGone { name: String, runtime: String },

    // ── Catch-all ─────────────────────────────────────────────────────────
    #[error("not implemented: {0}")]
    NotImplemented(&'static str),

    #[error("{0}")]
    Other(String),
}

impl CommandError {
    /// Kept for callers of the slice that introduced command secrets.
    pub fn command_secrets_lost() -> Self {
        Self::CommandSecretsLost
    }
    /// A copy of an error `parse_raw_args` can produce, so a parse result held
    /// behind a shared handle can report it from every accessor. `None` for
    /// any other variant, most of which wrap uncloneable engine or data errors.
    pub fn clone_parse_stage(&self) -> Option<CommandError> {
        Some(match self {
            CommandError::UnknownCommand { path, suggestions } => CommandError::UnknownCommand {
                path: path.clone(),
                suggestions: suggestions.clone(),
            },
            CommandError::UnknownFlag { command, flag } => CommandError::UnknownFlag {
                command: command.clone(),
                flag: flag.clone(),
            },
            CommandError::InvalidFlagValue {
                command,
                flag,
                reason,
            } => CommandError::InvalidFlagValue {
                command: command.clone(),
                flag: flag.clone(),
                reason: reason.clone(),
            },
            CommandError::UnexpectedArgument { command, argument } => {
                CommandError::UnexpectedArgument {
                    command: command.clone(),
                    argument: argument.clone(),
                }
            }
            _ => return None,
        })
    }

    /// An unknown command, with the catalogue's near misses attached.
    pub fn unknown_command(path: &[&str]) -> Self {
        CommandError::UnknownCommand {
            path: path.iter().map(|s| s.to_string()).collect(),
            suggestions: crate::command::dispatch::catalogue::CommandCatalogue::get().suggest(path),
        }
    }

    pub fn missing_required_flag(command: &[&str], flag: impl Into<String>) -> Self {
        CommandError::MissingRequiredFlag {
            command: command.iter().map(|s| s.to_string()).collect(),
            flag: flag.into(),
        }
    }

    pub fn missing_required_argument(command: &[&str], argument: impl Into<String>) -> Self {
        CommandError::MissingRequiredArgument {
            command: command.iter().map(|s| s.to_string()).collect(),
            argument: argument.into(),
        }
    }

    pub fn unknown_flag(command: &[&str], flag: impl Into<String>) -> Self {
        CommandError::UnknownFlag {
            command: command.iter().map(|s| s.to_string()).collect(),
            flag: flag.into(),
        }
    }

    pub fn unexpected_argument(command: &[&str], argument: impl Into<String>) -> Self {
        CommandError::UnexpectedArgument {
            command: command.iter().map(|s| s.to_string()).collect(),
            argument: argument.into(),
        }
    }

    pub fn mutually_exclusive(
        command: &[&str],
        a: impl Into<String>,
        b: impl Into<String>,
    ) -> Self {
        CommandError::MutuallyExclusive {
            command: command.iter().map(|s| s.to_string()).collect(),
            a: a.into(),
            b: b.into(),
        }
    }
}

/// `EngineError` bubbles up as `CommandError::Engine` — except for the five
/// remote-transport variants, which have Layer 2 twins this maps onto. The
/// HTTP client moved to Layer 1 in WI 0114 F-28; Layer 2 code (the squad
/// gateway, `remote.rs`) still raises and matches its own `Remote*` variants,
/// and every CLI message and exit code stayed as it was.
impl From<EngineError> for CommandError {
    fn from(err: EngineError) -> Self {
        match err {
            EngineError::RemoteTimeout => CommandError::RemoteTimeout,
            EngineError::RemoteConnectionRefused(reason) => {
                CommandError::RemoteConnectionRefused(reason)
            }
            EngineError::RemoteHttpStatus { status, body } => {
                CommandError::RemoteHttpStatus { status, body }
            }
            EngineError::MalformedSseEvent(msg) => CommandError::MalformedSseEvent(msg),
            EngineError::RemoteTransport(msg) => CommandError::RemoteTransport(msg),
            other => CommandError::Engine(other),
        }
    }
}

/// `DataError` bubbles up as `CommandError::Data` — except for the overlay
/// grammar's rejection, which has a Layer 2 twin so that a malformed
/// `overlays:` entry keeps the invalid-usage exit code and the message the
/// CLI has always printed for it. The grammar moved to Layer 0 in WI 0114
/// F-27; this mapping is what kept that move behaviour-preserving.
impl From<DataError> for CommandError {
    fn from(err: DataError) -> Self {
        match err {
            DataError::InvalidOverlaySpec { spec, reason } => {
                CommandError::InvalidOverlaySpec { spec, reason }
            }
            other => CommandError::Data(other),
        }
    }
}

impl CommandError {
    /// Shared process and API completion classification.
    pub fn exit_code(&self) -> u8 {
        match self {
        CommandError::Aborted => 130,
        CommandError::WorkItem(e) => e.exit_code(),
        CommandError::DraftSync(_) => 1,
        CommandError::DaemonHttpStatus { status: 404, .. } => 4,

        // Exit 2 — invalid usage / parse / flag conflict
        CommandError::UnknownCommand { .. }
        | CommandError::UnknownFlag { .. }
        | CommandError::MissingRequiredFlag { .. }
        | CommandError::MissingRequiredArgument { .. }
        | CommandError::UnexpectedArgument { .. }
        | CommandError::MutuallyExclusive { .. }
        | CommandError::InvalidFlagValue { .. }
        | CommandError::InvalidArgumentValue { .. }
        | CommandError::CommandBoxParse(_)
        | CommandError::InvalidOverlaySpec { .. }
        | CommandError::UnknownConfigField { .. }
        | CommandError::InteractiveInputUnavailable { .. }
        | CommandError::WorkflowPrerequisitesUnresolved { .. }
        | CommandError::PrerequisiteWideningRefused { .. }
        | CommandError::PrerequisiteFlagUnsupported { .. }
        // Session-creation validation failures are invalid-usage class.
        | CommandError::SessionInvalidType { .. }
        | CommandError::SessionWorkdirRequired
        | CommandError::SessionWorkdirUnresolvable { .. }
        | CommandError::SessionWorkdirNotAllowed { .. }
        | CommandError::ApiWorkdirRootInvalid { .. }
        | CommandError::SessionRepoUrlRequired
        | CommandError::SessionRepoUrlEmpty
        | CommandError::SessionRepoUrlInvalidScheme { .. }
        | CommandError::SessionFieldRemoteOnly { .. }
        | CommandError::SessionFieldRequiresBranch { .. }
        | CommandError::SessionRefInvalid { .. }
        | CommandError::GitAuthUnsupportedUrl { .. }
        | CommandError::GitAuthUnsupportedPlatform
        | CommandError::GitAuthInvalid { .. }
        | CommandError::AgentAmbiguous { .. }
        | CommandError::AgentReferenceEmpty => 2,

        // Exit 4 — missing referenced resource
        CommandError::WorkItemNotFound { .. }
        | CommandError::SpecTemplateMissing { .. }
        | CommandError::WorkflowFileNotFound { .. }
        | CommandError::ApiWorkdirNotFound { .. }
        | CommandError::AgentNotFound { .. }
        | CommandError::AgentNoLongerRunning { .. }
        | CommandError::AgentNotRunning { .. }
        | CommandError::AgentReplaced { .. } => 4,

        // Engine failures carry their own class (3 only for a missing or
        // failing Docker/Apple Containers backend, 1 otherwise).
        CommandError::Engine(e) => e.exit_code(),
        CommandError::Data(_) => 1,
        CommandError::MergeConflict { .. } => 1,
        CommandError::MergeTargetDirty { .. } | CommandError::WorktreeExists { .. } => 1,
        CommandError::MissingRemoteAddress
        | CommandError::MissingApiKey
        | CommandError::RemoteTimeout
        | CommandError::RemoteConnectionRefused(_)
        | CommandError::RemoteHttpStatus { .. }
        | CommandError::MalformedSseEvent(_)
        | CommandError::RemoteTransport(_) => 1,
        CommandError::DaemonNotRunning
        | CommandError::DaemonAlreadyRunning { .. }
        | CommandError::DaemonNoFeatures
        | CommandError::DaemonHttpStatus { .. }
        | CommandError::RemoteSessionMissing
        | CommandError::RemoteSessionKillFailed { .. } => 1,
        CommandError::NotImplemented(_) => 1,
        CommandError::NotAvailableForFrontend { .. } => 1,
        CommandError::AgentOwnerGone { .. } => 1,
        CommandError::GitAuthFailed { .. }
        | CommandError::CredentialsLost
        | CommandError::CommandSecretsLost => 1,
        CommandError::Other(_) => 1,
    }
    }

    /// The HTTP status class the API reports for this failure (as
    /// `error_status` in a command's result, and on session creation).
    ///
    /// Agent-definition failures carry their own class: an agent nobody
    /// defined is "not found" (404), and a definition that has not been
    /// approved is "forbidden" (403) — the API never approves one. Everything
    /// else follows the exit-code class: invalid usage is 400, a missing
    /// resource 404, anything else 500.
    pub fn http_status(&self) -> u16 {
        match self {
            CommandError::Engine(EngineError::AgentDefinitionUntrusted { .. }) => 403,
            CommandError::Engine(EngineError::UnknownAgent { .. }) => 404,
            CommandError::SessionWorkdirNotAllowed { .. } => 403,
            CommandError::PrerequisiteWideningRefused { .. } => 403,
            CommandError::WorkflowPrerequisitesUnresolved { .. } => 409,
            CommandError::CredentialsLost => 409,
            CommandError::GitAuthFailed { .. } => 422,
            CommandError::MergeTargetDirty { .. }
            | CommandError::MergeConflict { .. }
            | CommandError::WorktreeExists { .. } => 409,
            _ => match self.exit_code() {
                2 => 400,
                4 => 404,
                _ => 500,
            },
        }
    }
}

/// Failure codes shared by command results and HTTP admission errors.
pub mod failure_codes {
    pub const INVALID_REQUEST: &str = "invalid_request";
    pub const UNKNOWN_FLAG: &str = "unknown_flag";
    pub const UNKNOWN_SUBCOMMAND: &str = "unknown_subcommand";
    pub const FORBIDDEN_WORKDIR: &str = "forbidden_workdir";
    pub const WORKTREE_EXISTS: &str = "worktree_exists";
    pub const MERGE_CONFLICT: &str = "merge_conflict";
    pub const MERGE_TARGET_DIRTY: &str = "merge_target_dirty";
    pub const WORKFLOW_PREREQUISITES_UNRESOLVED: &str = "workflow_prerequisites_unresolved";
    pub const UNKNOWN_AGENT: &str = "unknown_agent";
    pub const AGENT_UNTRUSTED: &str = "agent_untrusted";
}

/// The post-admission stages a queued command can fail in, as the API
/// reports them in `result.failure.stage` (WI 0137 §1). Engine outcomes and
/// post-command stages the queue worker names itself (`step`, `teardown`,
/// `push`, `setup`) use the same spellings.
pub mod failure_stages {
    /// `/v1/worker/runs` session setup, before the command ran.
    pub const SETUP: &str = "setup";
    /// After admission, before any container started: argument resolution,
    /// agent lookup, an existing worktree, a missing resource.
    pub const PREFLIGHT: &str = "preflight";
    /// An unresolved workflow prerequisite (WI 0149).
    pub const WORKFLOW_PREREQUISITES: &str = "workflow_prerequisites";
    /// While the command's own work ran: the `exec prompt` agent, or a
    /// main-phase workflow step.
    pub const STEP: &str = "step";
    /// A workflow teardown step.
    pub const TEARDOWN: &str = "teardown";
    /// Worktree merge-back.
    pub const MERGE: &str = "merge";
    /// Remote-session push-back.
    pub const PUSH: &str = "push";
}

impl CommandError {
    /// The pipeline stage a failure belongs to, reported by the API as
    /// `result.failure.stage` (WI 0137 §1). The legacy `result.stage` keeps
    /// its pre-0137 meaning and is only filled for an unresolved prerequisite.
    ///
    /// Every variant names a stage: usage, lookup and configuration failures
    /// happen before any container starts (`preflight`); merge-back failures
    /// are `merge`; an unresolved prerequisite is `workflow_prerequisites`;
    /// anything raised while the command's work was running — engine and
    /// data failures, an abort, the catch-alls — is `step`. The match is
    /// exhaustive on purpose: a new variant must pick its stage here.
    pub fn stage(&self) -> &'static str {
        use failure_stages::*;
        match self {
            CommandError::WorkflowPrerequisitesUnresolved { .. } => WORKFLOW_PREREQUISITES,

            CommandError::GitAuthFailed { .. } | CommandError::CredentialsLost => {
                crate::data::fs::api_db::failure_stages::GIT_AUTH
            }
            CommandError::CommandSecretsLost => PREFLIGHT,
            CommandError::WorkItem(_) => PREFLIGHT,
            CommandError::DraftSync(_) => STEP,

            CommandError::MergeConflict { .. } | CommandError::MergeTargetDirty { .. } => MERGE,

            CommandError::Engine(EngineError::UnknownAgent { .. })
            | CommandError::Engine(EngineError::AgentDefinitionUntrusted { .. })
            | CommandError::WorktreeExists { .. }
            | CommandError::UnknownCommand { .. }
            | CommandError::NotAvailableForFrontend { .. }
            | CommandError::UnknownFlag { .. }
            | CommandError::MissingRequiredFlag { .. }
            | CommandError::MissingRequiredArgument { .. }
            | CommandError::UnexpectedArgument { .. }
            | CommandError::MutuallyExclusive { .. }
            | CommandError::InvalidFlagValue { .. }
            | CommandError::InvalidArgumentValue { .. }
            | CommandError::CommandBoxParse(_)
            | CommandError::MissingRemoteAddress
            | CommandError::MissingApiKey
            | CommandError::ApiWorkdirNotFound { .. }
            | CommandError::ApiWorkdirRootInvalid { .. }
            | CommandError::DaemonNotRunning
            | CommandError::DaemonAlreadyRunning { .. }
            | CommandError::DaemonNoFeatures
            | CommandError::DaemonHttpStatus { .. }
            | CommandError::SessionInvalidType { .. }
            | CommandError::SessionWorkdirRequired
            | CommandError::SessionWorkdirUnresolvable { .. }
            | CommandError::SessionWorkdirNotAllowed { .. }
            | CommandError::SessionRepoUrlRequired
            | CommandError::SessionRepoUrlEmpty
            | CommandError::SessionRepoUrlInvalidScheme { .. }
            | CommandError::SessionFieldRemoteOnly { .. }
            | CommandError::SessionFieldRequiresBranch { .. }
            | CommandError::SessionRefInvalid { .. }
            | CommandError::GitAuthUnsupportedUrl { .. }
            | CommandError::GitAuthUnsupportedPlatform
            | CommandError::GitAuthInvalid { .. }
            | CommandError::RemoteSessionMissing
            | CommandError::WorkItemNotFound { .. }
            | CommandError::SpecTemplateMissing { .. }
            | CommandError::InvalidOverlaySpec { .. }
            | CommandError::UnknownConfigField { .. }
            | CommandError::InteractiveInputUnavailable { .. }
            | CommandError::WorkflowFileNotFound { .. }
            | CommandError::PrerequisiteWideningRefused { .. }
            | CommandError::PrerequisiteFlagUnsupported { .. }
            | CommandError::AgentNotFound { .. }
            | CommandError::AgentNoLongerRunning { .. }
            | CommandError::AgentNotRunning { .. }
            | CommandError::AgentReplaced { .. }
            | CommandError::AgentAmbiguous { .. }
            | CommandError::AgentReferenceEmpty
            | CommandError::NotImplemented(_) => PREFLIGHT,

            CommandError::Engine(_)
            | CommandError::Data(_)
            | CommandError::Aborted
            | CommandError::RemoteTimeout
            | CommandError::RemoteConnectionRefused(_)
            | CommandError::RemoteHttpStatus { .. }
            | CommandError::MalformedSseEvent(_)
            | CommandError::RemoteTransport(_)
            | CommandError::RemoteSessionKillFailed { .. }
            | CommandError::AgentOwnerGone { .. }
            | CommandError::Other(_) => STEP,
        }
    }

    /// A stable snake_case code for this failure, reported by the API as
    /// `result.failure.code` (WI 0137 §1) so clients stop matching message
    /// text.
    ///
    /// One code per variant, spelled after the variant. Where WI 0138's HTTP
    /// error vocabulary already names the condition, its spelling wins
    /// (`unknown_subcommand`, `unknown_flag`, `forbidden_workdir`), so a
    /// client decodes both with one table. The wrappers `Engine` and `Data`
    /// carry lower-layer failures that are not the client's to act on and
    /// report `internal`, except the two agent-definition failures that are;
    /// `Other` is the untyped catch-all and is `internal` too. `Aborted`
    /// reports `aborted`, the same word as its `outcome` (a cancel request
    /// is `cancelled`, assigned by the queue worker). Codes for outcomes that
    /// are not `CommandError`s (`step_failed`, `teardown_failed`,
    /// `push_failed`, `setup_failed`, `cancelled`) live in
    /// `crate::data::fs::api_db::failure_codes`. Exhaustive on purpose: a
    /// new variant must pick its code here.
    pub fn code(&self) -> &'static str {
        use crate::data::fs::api_db::failure_codes as outcome_codes;
        match self {
            CommandError::Engine(EngineError::UnknownAgent { .. }) => failure_codes::UNKNOWN_AGENT,
            CommandError::Engine(EngineError::AgentDefinitionUntrusted { .. }) => {
                failure_codes::AGENT_UNTRUSTED
            }
            CommandError::Engine(_) | CommandError::Data(_) | CommandError::Other(_) => {
                outcome_codes::INTERNAL
            }

            CommandError::UnknownCommand { .. } => failure_codes::UNKNOWN_SUBCOMMAND,
            CommandError::NotAvailableForFrontend { .. } => "not_available_for_frontend",
            CommandError::UnknownFlag { .. } => failure_codes::UNKNOWN_FLAG,
            CommandError::MissingRequiredFlag { .. } => "missing_required_flag",
            CommandError::MissingRequiredArgument { .. } => "missing_required_argument",
            CommandError::UnexpectedArgument { .. } => "unexpected_argument",
            CommandError::MutuallyExclusive { .. } => "mutually_exclusive_flags",
            CommandError::InvalidFlagValue { .. } => "invalid_flag_value",
            CommandError::InvalidArgumentValue { .. } => "invalid_argument_value",
            CommandError::CommandBoxParse(_) => "command_box_parse_failed",

            CommandError::Aborted => outcome_codes::ABORTED,
            CommandError::MergeConflict { .. } => failure_codes::MERGE_CONFLICT,
            CommandError::MergeTargetDirty { .. } => failure_codes::MERGE_TARGET_DIRTY,
            CommandError::WorktreeExists { .. } => failure_codes::WORKTREE_EXISTS,

            CommandError::MissingRemoteAddress => "missing_remote_address",
            CommandError::MissingApiKey => "missing_api_key",
            CommandError::RemoteTimeout => "remote_timeout",
            CommandError::RemoteConnectionRefused(_) => "remote_connection_refused",
            CommandError::RemoteHttpStatus { .. } => "remote_http_status",
            CommandError::MalformedSseEvent(_) => "malformed_sse_event",
            CommandError::RemoteTransport(_) => "remote_transport_failed",

            CommandError::ApiWorkdirNotFound { .. } => "api_workdir_not_found",
            CommandError::ApiWorkdirRootInvalid { .. } => "api_workdir_root_invalid",
            CommandError::DaemonNotRunning => "daemon_not_running",
            CommandError::DaemonAlreadyRunning { .. } => "daemon_already_running",
            CommandError::DaemonNoFeatures => "daemon_no_features",
            CommandError::DaemonHttpStatus { .. } => "daemon_http_status",

            CommandError::SessionInvalidType { .. } => "session_invalid_type",
            CommandError::SessionWorkdirRequired => "session_workdir_required",
            CommandError::SessionWorkdirUnresolvable { .. } => "session_workdir_unresolvable",
            CommandError::SessionWorkdirNotAllowed { .. } => failure_codes::FORBIDDEN_WORKDIR,
            CommandError::SessionRepoUrlRequired => "session_repo_url_required",
            CommandError::SessionRepoUrlEmpty => "session_repo_url_empty",
            CommandError::SessionRepoUrlInvalidScheme { .. } => "session_repo_url_invalid_scheme",
            CommandError::SessionFieldRemoteOnly { .. } => "session_field_remote_only",
            CommandError::SessionFieldRequiresBranch { .. } => "session_field_requires_branch",
            CommandError::SessionRefInvalid { .. } => "session_ref_invalid",
            CommandError::GitAuthUnsupportedUrl { .. } => "git_auth_unsupported_url",
            CommandError::GitAuthUnsupportedPlatform => "git_auth_unsupported_platform",
            CommandError::GitAuthInvalid { .. } => "invalid_git_auth",
            CommandError::GitAuthFailed { .. } => outcome_codes::GIT_AUTH_FAILED,
            CommandError::CredentialsLost => outcome_codes::CREDENTIALS_LOST,
            CommandError::CommandSecretsLost => "secrets_lost",

            CommandError::RemoteSessionMissing => "remote_session_missing",
            CommandError::RemoteSessionKillFailed { .. } => "remote_session_kill_failed",

            CommandError::WorkItem(e) => e.code(),
            CommandError::DraftSync(_) => "work_item_sync_failed",
            CommandError::WorkItemNotFound { .. } => "work_item_not_found",
            CommandError::SpecTemplateMissing { .. } => "spec_template_missing",
            CommandError::InvalidOverlaySpec { .. } => "invalid_overlay_spec",
            CommandError::UnknownConfigField { .. } => "unknown_config_field",
            CommandError::InteractiveInputUnavailable { .. } => "interactive_input_unavailable",
            CommandError::WorkflowFileNotFound { .. } => "workflow_file_not_found",

            CommandError::WorkflowPrerequisitesUnresolved { .. } => {
                failure_codes::WORKFLOW_PREREQUISITES_UNRESOLVED
            }
            CommandError::PrerequisiteWideningRefused { .. } => "prerequisite_widening_refused",
            CommandError::PrerequisiteFlagUnsupported { .. } => "prerequisite_flag_unsupported",

            CommandError::AgentNotFound { .. } => "agent_not_found",
            CommandError::AgentNoLongerRunning { .. } => "agent_no_longer_running",
            CommandError::AgentNotRunning { .. } => "agent_not_running",
            CommandError::AgentReplaced { .. } => "agent_replaced",
            CommandError::AgentAmbiguous { .. } => "agent_ambiguous",
            CommandError::AgentReferenceEmpty => "agent_reference_empty",
            CommandError::AgentOwnerGone { .. } => "agent_owner_gone",

            CommandError::NotImplemented(_) => "not_implemented",
        }
    }
}

impl From<crate::command::work_item::WorkItemError> for CommandError {
    fn from(err: crate::command::work_item::WorkItemError) -> Self {
        CommandError::WorkItem(err)
    }
}

impl From<crate::command::work_item::draft_sync::DraftSyncError> for CommandError {
    fn from(err: crate::command::work_item::draft_sync::DraftSyncError) -> Self {
        CommandError::DraftSync(err)
    }
}
