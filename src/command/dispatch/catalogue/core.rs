//! The commands that operate on one repository: `clean`, `init`, `ready`,
//! `chat`, `specs`, `status`, `config`.
//!
//! Split out of `dispatch/catalogue.rs` by WI 0114 F-51. The specs are data;
//! `ROOT` in `mod.rs` is what assembles them into the tree.

use super::*;

// ── clean ─────────────────────────────────────────────────────────────────────

pub(super) const CLEAN: CommandSpec = CommandSpec {
    name: "clean",
    aliases: &[],
    help: "Remove stopped awman containers, completed workflow data, and dangling images.",
    long_help: None,
    arguments: &[],
    flags: &[
        FlagSpec {
            long: "yes",
            short: Some('y'),
            help: "Skip the confirmation prompt (for scripting).",
            kind: FlagKind::Bool,
            default: FlagDefault::Bool(false),
            frontends: FrontendVisibility::All,
            conflicts_with: &[],
            implies: &[],
            optional: true,
        },
        FlagSpec {
            long: "dry-run",
            short: None,
            help: "List what would be removed without deleting anything.",
            kind: FlagKind::Bool,
            default: FlagDefault::Bool(false),
            frontends: FrontendVisibility::All,
            conflicts_with: &[],
            implies: &[],
            optional: true,
        },
    ],
    // Blocked at the catalogue layer for the API frontend; never reaches
    // command dispatch via the API.
    api_allowed: false,
    build: crate::command::dispatch::build::clean,
    gateway_need: GatewayNeed::None,
    requires_container_tier: false,
    requires_runtime: true,
    opens_tui_when_bare: false,
    subcommands: &[],
};

// ── init ─────────────────────────────────────────────────────────────────────

pub(super) const AGENT_VALUES: &[&str] = &[
    "claude",
    "codex",
    "opencode",
    "maki",
    "gemini",
    "copilot",
    "crush",
    "cline",
    "antigravity",
];

pub(super) const INIT: CommandSpec = CommandSpec {
    name: "init",
    aliases: &[],
    help: "Initialize the current Git repo for use with awman.",
    long_help: None,
    arguments: &[],
    flags: &[
        FlagSpec {
            long: "agent",
            short: None,
            help: "Code agent to install in the Dockerfile.dev container.",
            kind: FlagKind::Enum(AGENT_VALUES),
            default: FlagDefault::Str("claude"),
            frontends: FrontendVisibility::All,
            conflicts_with: &[],
            implies: &[],
            optional: true,
        },
        FlagSpec {
            long: "aspec",
            short: None,
            help: "Download aspec templates to the current project.",
            kind: FlagKind::Bool,
            default: FlagDefault::Bool(false),
            frontends: FrontendVisibility::All,
            conflicts_with: &[],
            implies: &[],
            optional: true,
        },
    ],
    api_allowed: false,
    build: crate::command::dispatch::build::init,
    gateway_need: GatewayNeed::None,
    requires_container_tier: false,
    requires_runtime: true,
    opens_tui_when_bare: false,
    subcommands: &[],
};

// ── ready ────────────────────────────────────────────────────────────────────

pub(super) const READY: CommandSpec = CommandSpec {
    name: "ready",
    aliases: &[],
    help: "Check Docker daemon, verify Dockerfile.dev, build image, and report status.",
    long_help: None,
    arguments: &[],
    flags: &[
        FlagSpec {
            long: "refresh",
            short: None,
            help: "Run the Dockerfile agent audit (skipped by default).",
            kind: FlagKind::Bool,
            default: FlagDefault::Bool(false),
            frontends: FrontendVisibility::All,
            conflicts_with: &[],
            implies: &[],
            optional: true,
        },
        FlagSpec {
            long: "build",
            short: None,
            help: "Force rebuild the dev container image from Dockerfile.dev.",
            kind: FlagKind::Bool,
            default: FlagDefault::Bool(false),
            frontends: FrontendVisibility::All,
            conflicts_with: &[],
            implies: &[],
            optional: true,
        },
        FlagSpec {
            long: "no-cache",
            short: None,
            help: "Pass --no-cache to docker build.",
            kind: FlagKind::Bool,
            default: FlagDefault::Bool(false),
            frontends: FrontendVisibility::All,
            conflicts_with: &[],
            implies: &[],
            optional: true,
        },
        FlagSpec {
            long: "non-interactive",
            short: Some('n'),
            help: "Run the agent in non-interactive (print) mode instead of interactive mode.",
            kind: FlagKind::Bool,
            default: FlagDefault::Bool(false),
            frontends: FrontendVisibility::All,
            conflicts_with: &[],
            implies: &[],
            optional: true,
        },
        FlagSpec {
            long: "allow-docker",
            short: None,
            help: "Mount the host Docker daemon socket into the agent container.",
            kind: FlagKind::Bool,
            default: FlagDefault::Bool(false),
            frontends: FrontendVisibility::All,
            conflicts_with: &[],
            implies: &[],
            optional: true,
        },
        FlagSpec {
            long: "json",
            short: None,
            help: "Suppress human output and print structured JSON. Implies --non-interactive.",
            kind: FlagKind::Bool,
            default: FlagDefault::Bool(false),
            frontends: FrontendVisibility::All,
            conflicts_with: &[],
            implies: &["non-interactive"],
            optional: true,
        },
    ],
    api_allowed: false,
    build: crate::command::dispatch::build::ready,
    gateway_need: GatewayNeed::None,
    requires_container_tier: false,
    requires_runtime: true,
    opens_tui_when_bare: false,
    subcommands: &[],
};

// ── chat ─────────────────────────────────────────────────────────────────────

pub(super) const CHAT: CommandSpec = CommandSpec {
    name: "chat",
    aliases: &[],
    help: "Start a freeform chat session with the configured agent in a container.",
    long_help: None,
    arguments: &[],
    flags: &AGENT_RUN_FLAGS_NO_WORKTREE,
    api_allowed: false,
    build: crate::command::dispatch::build::chat,
    gateway_need: GatewayNeed::None,
    requires_container_tier: false,
    requires_runtime: true,
    opens_tui_when_bare: false,
    subcommands: &[],
};

// ── specs ───────────────────────────────────────────────────────────────────

pub(super) const SPECS: CommandSpec = CommandSpec {
    name: "specs",
    aliases: &[],
    help: "Manage work item specs (amend).",
    long_help: None,
    arguments: &[],
    flags: &[],
    api_allowed: false,
    build: crate::command::dispatch::build::unsupported,
    gateway_need: GatewayNeed::None,
    requires_container_tier: false,
    requires_runtime: true,
    opens_tui_when_bare: false,
    subcommands: &[&SPECS_AMEND],
};

pub(super) const SPECS_AMEND: CommandSpec = CommandSpec {
    name: "amend",
    aliases: &[],
    help: "Review and amend a completed work item to match the final implementation.",
    long_help: None,
    arguments: &[ArgumentSpec {
        name: "work_item",
        help: "Work item number (e.g. 0025).",
        kind: ArgumentKind::String,
        optional: false,
    }],
    flags: &[
        FlagSpec {
            long: "non-interactive",
            short: Some('n'),
            help: "Run the agent in non-interactive (print) mode.",
            kind: FlagKind::Bool,
            default: FlagDefault::Bool(false),
            frontends: FrontendVisibility::All,
            conflicts_with: &[],
            implies: &[],
            optional: true,
        },
        FlagSpec {
            long: "allow-docker",
            short: None,
            help: "Mount the host Docker daemon socket into the agent container.",
            kind: FlagKind::Bool,
            default: FlagDefault::Bool(false),
            frontends: FrontendVisibility::All,
            conflicts_with: &[],
            implies: &[],
            optional: true,
        },
    ],
    api_allowed: false,
    build: crate::command::dispatch::build::specs,
    gateway_need: GatewayNeed::None,
    requires_container_tier: false,
    requires_runtime: true,
    opens_tui_when_bare: false,
    subcommands: &[],
};

// ── status ───────────────────────────────────────────────────────────────────

pub(super) const STATUS: CommandSpec = CommandSpec {
    name: "status",
    aliases: &[],
    help: "Show the status of all running code-agent containers.",
    long_help: None,
    arguments: &[],
    flags: &[FlagSpec {
        long: "watch",
        short: None,
        help: "Continuously refresh the output every 3 seconds.",
        kind: FlagKind::Bool,
        default: FlagDefault::Bool(false),
        frontends: FrontendVisibility::All,
        conflicts_with: &[],
        implies: &[],
        optional: true,
    }],
    api_allowed: false,
    build: crate::command::dispatch::build::status,
    gateway_need: GatewayNeed::None,
    requires_container_tier: false,
    requires_runtime: true,
    opens_tui_when_bare: false,
    subcommands: &[],
};

// ── config ───────────────────────────────────────────────────────────────────

pub(super) const CONFIG: CommandSpec = CommandSpec {
    name: "config",
    aliases: &[],
    help: "View and edit global and repo configuration.",
    long_help: None,
    arguments: &[],
    flags: &[],
    api_allowed: false,
    build: crate::command::dispatch::build::unsupported,
    gateway_need: GatewayNeed::None,
    requires_container_tier: false,
    // The recovery path when the configured runtime cannot be constructed
    // on this host: `awman config set runtime …` must stay reachable.
    requires_runtime: false,
    opens_tui_when_bare: false,
    subcommands: &[&CONFIG_SHOW, &CONFIG_GET, &CONFIG_SET],
};

pub(super) const CONFIG_SHOW: CommandSpec = CommandSpec {
    name: "show",
    aliases: &[],
    help: "Display all config fields at both global and repo level.",
    long_help: None,
    arguments: &[],
    flags: &[],
    api_allowed: false,
    build: crate::command::dispatch::build::config,
    gateway_need: GatewayNeed::None,
    requires_container_tier: false,
    // The recovery path when the configured runtime cannot be constructed
    // on this host: `awman config set runtime …` must stay reachable.
    requires_runtime: false,
    opens_tui_when_bare: false,
    subcommands: &[],
};

pub(super) const CONFIG_GET: CommandSpec = CommandSpec {
    name: "get",
    aliases: &[],
    help: "Show a single field's global value, repo value, and effective value.",
    long_help: None,
    arguments: &[ArgumentSpec {
        name: "field",
        help: "Config field name (e.g. terminal_scrollback_lines).",
        kind: ArgumentKind::String,
        optional: false,
    }],
    flags: &[],
    api_allowed: false,
    build: crate::command::dispatch::build::config,
    gateway_need: GatewayNeed::None,
    requires_container_tier: false,
    // The recovery path when the configured runtime cannot be constructed
    // on this host: `awman config set runtime …` must stay reachable.
    requires_runtime: false,
    opens_tui_when_bare: false,
    subcommands: &[],
};

pub(super) const CONFIG_SET: CommandSpec = CommandSpec {
    name: "set",
    aliases: &[],
    help: "Set a config field value (repo scope by default).",
    long_help: None,
    arguments: &[
        ArgumentSpec {
            name: "field",
            help: "Config field name.",
            kind: ArgumentKind::String,
            optional: false,
        },
        ArgumentSpec {
            name: "value",
            help: "New value for the field.",
            kind: ArgumentKind::String,
            optional: false,
        },
    ],
    flags: &[FlagSpec {
        long: "global",
        short: None,
        help: "Write to global config instead of repo config.",
        kind: FlagKind::Bool,
        default: FlagDefault::Bool(false),
        frontends: FrontendVisibility::All,
        conflicts_with: &[],
        implies: &[],
        optional: true,
    }],
    api_allowed: false,
    build: crate::command::dispatch::build::config,
    gateway_need: GatewayNeed::None,
    requires_container_tier: false,
    // The recovery path when the configured runtime cannot be constructed
    // on this host: `awman config set runtime …` must stay reachable.
    requires_runtime: false,
    opens_tui_when_bare: false,
    subcommands: &[],
};
