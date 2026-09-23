//! `remote` — running against someone else's awman server.
//!
//! Split out of `dispatch/catalogue.rs` by WI 0114 F-51. The specs are data;
//! `ROOT` in `mod.rs` is what assembles them into the tree.

use super::*;

// ── remote ──────────────────────────────────────────────────────────────────

pub(super) const REMOTE: CommandSpec = CommandSpec {
    name: "remote",
    aliases: &[],
    help: "Connect to a remote awman API instance and execute commands.",
    long_help: None,
    arguments: &[],
    flags: &[],
    api_allowed: false,
    build: crate::command::dispatch::build::unsupported,
    gateway_need: GatewayNeed::None,
    requires_container_tier: false,
    requires_runtime: true,
    opens_tui_when_bare: false,
    subcommands: &[&REMOTE_SESSION, &REMOTE_EXEC],
};

// ── remote exec ─────────────────────────────────────────────────────────────

pub(super) const REMOTE_EXEC: CommandSpec = CommandSpec {
    name: "exec",
    aliases: &[],
    help: "Execute a command on the remote awman API host.",
    long_help: None,
    arguments: &[],
    flags: &[],
    api_allowed: false,
    build: crate::command::dispatch::build::unsupported,
    gateway_need: GatewayNeed::None,
    requires_container_tier: false,
    requires_runtime: true,
    opens_tui_when_bare: false,
    subcommands: &[&REMOTE_EXEC_WORKFLOW, &REMOTE_EXEC_PROMPT],
};

pub(super) const REMOTE_EXEC_WORKFLOW: CommandSpec = CommandSpec {
    name: "workflow",
    aliases: &["wf"],
    help: "Submit a workflow for execution on the remote host.",
    long_help: None,
    arguments: &[ArgumentSpec {
        name: "workflow",
        help: "Path to the workflow file.",
        kind: ArgumentKind::Path,
        optional: false,
    }],
    flags: &REMOTE_EXEC_WORKFLOW_FLAGS,
    api_allowed: false,
    build: crate::command::dispatch::build::remote,
    gateway_need: GatewayNeed::None,
    requires_container_tier: false,
    requires_runtime: true,
    opens_tui_when_bare: false,
    subcommands: &[],
};

pub(super) const REMOTE_EXEC_PROMPT: CommandSpec = CommandSpec {
    name: "prompt",
    aliases: &[],
    help: "Send a one-shot prompt to the remote host.",
    long_help: None,
    arguments: &[ArgumentSpec {
        name: "prompt",
        // Greedy trailing positional (see EXEC_PROMPT): joins remaining tokens
        // into one prompt string, spec-driven for every frontend.
        help: "The prompt text to send to the agent.",
        kind: ArgumentKind::TrailingVarArgs,
        optional: false,
    }],
    flags: &REMOTE_EXEC_PROMPT_FLAGS,
    api_allowed: false,
    build: crate::command::dispatch::build::remote,
    gateway_need: GatewayNeed::None,
    requires_container_tier: false,
    requires_runtime: true,
    opens_tui_when_bare: false,
    subcommands: &[],
};

// ── remote session ──────────────────────────────────────────────────────────

pub(super) const REMOTE_SESSION: CommandSpec = CommandSpec {
    name: "session",
    aliases: &[],
    help: "Manage sessions on the remote awman API host.",
    long_help: None,
    arguments: &[],
    flags: &[],
    api_allowed: false,
    build: crate::command::dispatch::build::unsupported,
    gateway_need: GatewayNeed::None,
    requires_container_tier: false,
    requires_runtime: true,
    opens_tui_when_bare: false,
    subcommands: &[&REMOTE_SESSION_START, &REMOTE_SESSION_KILL],
};

pub(super) const REMOTE_SESSION_START: CommandSpec = CommandSpec {
    name: "start",
    aliases: &[],
    help: "Start a new session on the remote host.",
    long_help: None,
    arguments: &[],
    flags: &REMOTE_SESSION_START_FLAGS,
    api_allowed: false,
    build: crate::command::dispatch::build::remote,
    gateway_need: GatewayNeed::None,
    requires_container_tier: false,
    requires_runtime: true,
    opens_tui_when_bare: false,
    subcommands: &[],
};

pub(super) const REMOTE_SESSION_START_FLAGS: [FlagSpec; 6] = [
    FlagSpec {
        long: "remote-addr",
        short: None,
        help: "Address of the remote awman API host.",
        kind: FlagKind::OptionalString,
        default: FlagDefault::None,
        frontends: FrontendVisibility::All,
        conflicts_with: &[],
        implies: &[],
        optional: true,
    },
    FlagSpec {
        long: "api-key",
        short: None,
        help: "API key for the remote awman API host.",
        kind: FlagKind::OptionalString,
        default: FlagDefault::None,
        frontends: FrontendVisibility::All,
        conflicts_with: &[],
        implies: &[],
        optional: true,
    },
    FlagSpec {
        long: "type",
        short: None,
        help: "Session type: 'local' or 'remote'.",
        // The values are `SessionKind`'s, not a second list (WI 0114 F-48):
        // `session_kind_flag_values_match_the_enum` fails if they drift.
        kind: FlagKind::Enum(SESSION_KIND_FLAG_VALUES),
        default: FlagDefault::Str("local"),
        frontends: FrontendVisibility::All,
        conflicts_with: &[],
        implies: &[],
        optional: true,
    },
    FlagSpec {
        long: "workdir",
        short: None,
        help: "Working directory (required for --type local).",
        kind: FlagKind::OptionalString,
        default: FlagDefault::None,
        frontends: FrontendVisibility::All,
        conflicts_with: &[],
        implies: &[],
        optional: true,
    },
    FlagSpec {
        long: "repo-url",
        short: None,
        help: "Repository URL (required for --type remote).",
        kind: FlagKind::OptionalString,
        default: FlagDefault::None,
        frontends: FrontendVisibility::All,
        conflicts_with: &[],
        implies: &[],
        optional: true,
    },
    FlagSpec {
        long: "branch",
        short: None,
        help: "Branch name (optional, for --type remote).",
        kind: FlagKind::OptionalString,
        default: FlagDefault::None,
        frontends: FrontendVisibility::All,
        conflicts_with: &[],
        implies: &[],
        optional: true,
    },
];

pub(super) const REMOTE_SESSION_KILL: CommandSpec = CommandSpec {
    name: "kill",
    aliases: &[],
    help: "Kill a session on the remote host.",
    long_help: None,
    arguments: &[ArgumentSpec {
        name: "session_id",
        help: "Session ID to kill.",
        kind: ArgumentKind::OptionalString,
        optional: true,
    }],
    flags: &REMOTE_SESSION_KILL_FLAGS,
    api_allowed: false,
    build: crate::command::dispatch::build::remote,
    gateway_need: GatewayNeed::None,
    requires_container_tier: false,
    requires_runtime: true,
    opens_tui_when_bare: false,
    subcommands: &[],
};

pub(super) const REMOTE_SESSION_KILL_FLAGS: [FlagSpec; 2] = [
    FlagSpec {
        long: "remote-addr",
        short: None,
        help: "Address of the remote awman API host.",
        kind: FlagKind::OptionalString,
        default: FlagDefault::None,
        frontends: FrontendVisibility::All,
        conflicts_with: &[],
        implies: &[],
        optional: true,
    },
    FlagSpec {
        long: "api-key",
        short: None,
        help: "API key for the remote awman API host.",
        kind: FlagKind::OptionalString,
        default: FlagDefault::None,
        frontends: FrontendVisibility::All,
        conflicts_with: &[],
        implies: &[],
        optional: true,
    },
];
