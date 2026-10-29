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

    #[error("API workdir root '{root}' is inside a git repository ({git_root}); list it with --workdirs instead, or choose a directory that only contains repositories")]
    ApiWorkdirRootInvalid { root: PathBuf, git_root: PathBuf },

    #[error("API server already running on PID {pid}")]
    ApiServerAlreadyRunning { pid: u32 },

    #[error("API server is not running")]
    ApiServerNotRunning,

    #[error(
        "no API key configured; run `awman api start --refresh-key` first, or pass `--dangerously-skip-auth`"
    )]
    ApiServerAuthMissing,

    // ── Squad ─────────────────────────────────────────────────────────────
    /// The squad daemon is reachable but this process holds no bearer key for
    /// it. Reported *before* the request rather than after it, because the
    /// request's own answer is a bare `HTTP 401: API key required`, which
    /// names neither the variable to set nor the fact that the key can no
    /// longer be read back — it was shown once and is stored only as a hash.
    ///
    /// Typed (WI 0113 F-04) so the CLI and TUI stop authoring this text
    /// themselves; the wording is byte-identical to the CLI's former
    /// `missing_squad_key_error`, and tests assert it.
    #[error(
        "squad requires a bearer key and none is set in this shell.\n\n\
         The key is shown only once, when it is minted, and only its hash is \
         stored — so it cannot be read back. Set {} if you saved it, or mint a \
         new one with:\n    awman squad start --refresh-key\n\
         which invalidates the previous key, so any shell still exporting it \
         must be updated too.",
        crate::data::config::env::AWMAN_SQUAD_KEY
    )]
    SquadKeyMissing,
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

    // ── Remote ────────────────────────────────────────────────────────────
    #[error("no remote session id; pass --session <id> or run `awman remote session start`")]
    RemoteSessionMissing,

    #[error("failed to kill remote session '{session_id}': {reason}")]
    RemoteSessionKillFailed { session_id: String, reason: String },

    // ── Work item / spec ──────────────────────────────────────────────────────
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
        CommandError::ApiServerAlreadyRunning { .. }
        | CommandError::ApiServerNotRunning
        | CommandError::ApiServerAuthMissing
        | CommandError::RemoteSessionMissing
        | CommandError::RemoteSessionKillFailed { .. } => 1,
        CommandError::NotImplemented(_) => 1,
        CommandError::NotAvailableForFrontend { .. } => 1,
        CommandError::SquadKeyMissing => 1,
        CommandError::AgentOwnerGone { .. } => 1,
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

/// The post-admission stages a queued command can fail in, as the API
/// reports them in `result.failure.stage` (WI 0137 §1). Engine outcomes and
/// post-command stages the queue worker names itself (`step`, `teardown`,
/// `push`, `setup`) use the same spellings.
pub mod failure_stages {
    /// `/v1/runs` session setup, before the command ran.
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
    /// `result.failure.stage` and the legacy `result.stage` (WI 0137 §1).
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
            | CommandError::ApiServerAlreadyRunning { .. }
            | CommandError::ApiServerNotRunning
            | CommandError::ApiServerAuthMissing
            | CommandError::SquadKeyMissing
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
    /// text. Variants a client can act on get their own code; the rest share
    /// a class code (`invalid_usage`, `not_found`, `internal`). Codes for
    /// outcomes that are not `CommandError`s (`step_failed`,
    /// `teardown_failed`, `push_failed`, `setup_failed`, `cancelled`) live in
    /// `crate::data::fs::api_db::failure_codes`. Exhaustive on purpose.
    pub fn code(&self) -> &'static str {
        match self {
            CommandError::WorktreeExists { .. } => "worktree_exists",
            CommandError::MergeConflict { .. } => "merge_conflict",
            CommandError::MergeTargetDirty { .. } => "merge_target_dirty",
            CommandError::WorkflowPrerequisitesUnresolved { .. } => {
                "workflow_prerequisites_unresolved"
            }
            CommandError::Engine(EngineError::UnknownAgent { .. }) => "unknown_agent",
            CommandError::Engine(EngineError::AgentDefinitionUntrusted { .. }) => "agent_untrusted",
            CommandError::Aborted => "aborted",

            CommandError::UnknownCommand { .. }
            | CommandError::NotAvailableForFrontend { .. }
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
            | CommandError::PrerequisiteWideningRefused { .. }
            | CommandError::PrerequisiteFlagUnsupported { .. }
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
            | CommandError::AgentAmbiguous { .. }
            | CommandError::AgentReferenceEmpty => "invalid_usage",

            CommandError::WorkItemNotFound { .. }
            | CommandError::SpecTemplateMissing { .. }
            | CommandError::WorkflowFileNotFound { .. }
            | CommandError::ApiWorkdirNotFound { .. }
            | CommandError::AgentNotFound { .. }
            | CommandError::AgentNoLongerRunning { .. }
            | CommandError::AgentNotRunning { .. }
            | CommandError::AgentReplaced { .. } => "not_found",

            CommandError::Engine(_)
            | CommandError::Data(_)
            | CommandError::MissingRemoteAddress
            | CommandError::MissingApiKey
            | CommandError::RemoteTimeout
            | CommandError::RemoteConnectionRefused(_)
            | CommandError::RemoteHttpStatus { .. }
            | CommandError::MalformedSseEvent(_)
            | CommandError::RemoteTransport(_)
            | CommandError::ApiServerAlreadyRunning { .. }
            | CommandError::ApiServerNotRunning
            | CommandError::ApiServerAuthMissing
            | CommandError::SquadKeyMissing
            | CommandError::RemoteSessionMissing
            | CommandError::RemoteSessionKillFailed { .. }
            | CommandError::AgentOwnerGone { .. }
            | CommandError::NotImplemented(_)
            | CommandError::Other(_) => "internal",
        }
    }
}

#[cfg(test)]
mod failure_classification_tests {
    use super::*;

    #[test]
    fn every_failure_has_a_stage_and_a_snake_case_code() {
        let cases: Vec<(CommandError, &str, &str)> = vec![
            (
                CommandError::WorktreeExists {
                    path: "/w".into(),
                    branch: "b".into(),
                },
                "preflight",
                "worktree_exists",
            ),
            (
                CommandError::MergeConflict {
                    branch: "b".into(),
                    worktree_path: "/w".into(),
                    git_root: "/r".into(),
                },
                "merge",
                "merge_conflict",
            ),
            (
                CommandError::MergeTargetDirty {
                    target_branch: "main".into(),
                    branch: "b".into(),
                    worktree_path: "/w".into(),
                },
                "merge",
                "merge_target_dirty",
            ),
            (
                CommandError::Engine(EngineError::UnknownAgent {
                    name: "pi".into(),
                    first_party: vec![],
                    custom: vec![],
                    notes: vec![],
                }),
                "preflight",
                "unknown_agent",
            ),
            (
                CommandError::Engine(EngineError::AgentDefinitionUntrusted {
                    name: "pi".into(),
                    path: "/x".into(),
                    sha256: "ab".into(),
                }),
                "preflight",
                "agent_untrusted",
            ),
            (CommandError::Aborted, "step", "aborted"),
            (CommandError::Other("boom".into()), "step", "internal"),
            (
                CommandError::Engine(EngineError::Other("boom".into())),
                "step",
                "internal",
            ),
            (
                CommandError::MissingRequiredArgument {
                    command: vec!["prompt".into()],
                    argument: "prompt".into(),
                },
                "preflight",
                "invalid_usage",
            ),
            (
                CommandError::WorkflowFileNotFound { path: "/w".into() },
                "preflight",
                "not_found",
            ),
        ];
        for (error, stage, code) in cases {
            assert_eq!(error.stage(), stage, "{error}");
            assert_eq!(error.code(), code, "{error}");
            assert!(
                code.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'),
                "{code} is not snake_case"
            );
        }
    }

    #[test]
    fn unresolved_prerequisites_keep_their_stage() {
        let error = CommandError::WorkflowPrerequisitesUnresolved {
            report: Box::new(
                crate::data::workflow_prerequisites::WorkflowPrerequisiteReport {
                    phase: crate::data::workflow_prerequisites::PrerequisitePhase::BeforeTeardown,
                    origin: crate::data::workflow_prerequisites::PrerequisiteOrigin::AssumedAll,
                    checks: vec![],
                    advisories: vec![],
                    notes: vec![],
                },
            ),
        };
        assert_eq!(error.stage(), "workflow_prerequisites");
        assert_eq!(error.code(), "workflow_prerequisites_unresolved");
    }
}

#[cfg(test)]
mod agent_error_class_tests {
    use super::*;

    fn untrusted() -> CommandError {
        CommandError::Engine(EngineError::AgentDefinitionUntrusted {
            name: "pi".into(),
            path: "/x/pi.toml".into(),
            sha256: "ab".into(),
        })
    }
    fn unknown() -> CommandError {
        CommandError::Engine(EngineError::UnknownAgent {
            name: "pi".into(),
            first_party: vec!["claude".into()],
            custom: vec![],
            notes: vec![],
        })
    }

    #[test]
    fn untrusted_is_exit_2_and_http_403() {
        assert_eq!(untrusted().exit_code(), 2);
        assert_eq!(untrusted().http_status(), 403);
    }

    #[test]
    fn unknown_agent_is_exit_4_and_http_404() {
        assert_eq!(unknown().exit_code(), 4);
        assert_eq!(unknown().http_status(), 404);
    }

    #[test]
    fn other_classes_follow_the_exit_code() {
        assert_eq!(CommandError::Other("x".into()).http_status(), 500);
        assert_eq!(CommandError::Aborted.http_status(), 500);
        assert_eq!(
            CommandError::InvalidArgumentValue {
                command: vec![],
                argument: "a".into(),
                reason: "r".into()
            }
            .http_status(),
            400
        );
    }
}
