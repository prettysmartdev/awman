//! The commands that operate on one repository: `clean`, `init`, `ready`,
//! `chat`, `specs`, `config`.
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
    internal: false,
    relay_allowed: false,
    subcommands: &[],
};

// ── init ─────────────────────────────────────────────────────────────────────

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
            help:
                "Code agent to install in the Dockerfile.dev container: claude, codex, opencode, \
                   gemini, copilot, cline, antigravity, or the name of a custom agent defined in \
                   .awman/agents/ or ~/.awman/agents/.",
            kind: FlagKind::String,
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
    internal: false,
    relay_allowed: true,
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
            help: "Skip the build cache when a build is scheduled, and on the builtin runtime \
                   also force a fresh image import.",
            kind: FlagKind::Bool,
            default: FlagDefault::Bool(false),
            frontends: FrontendVisibility::All,
            conflicts_with: &[],
            implies: &[],
            optional: true,
        },
        FlagSpec {
            long: "headless",
            short: Some('n'),
            help: "Run the agent headless (print mode) instead of interactive mode.",
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
            help: "Suppress human output and print structured JSON. Implies --headless.",
            kind: FlagKind::Bool,
            default: FlagDefault::Bool(false),
            frontends: FrontendVisibility::All,
            conflicts_with: &[],
            implies: &["headless"],
            optional: true,
        },
        FlagSpec {
            long: "agent",
            short: None,
            help: "Agent to check and build (overrides .awman/config.json). Reviews and approves \
                   a custom agent definition that needs approval.",
            kind: FlagKind::OptionalString,
            default: FlagDefault::None,
            frontends: FrontendVisibility::All,
            conflicts_with: &[],
            implies: &[],
            optional: true,
        },
    ],
    api_allowed: false,
    build: crate::command::dispatch::build::ready,
    gateway_need: GatewayNeed::None,
    requires_container_tier: false,
    requires_runtime: true,
    opens_tui_when_bare: false,
    internal: false,
    relay_allowed: true,
    subcommands: &[],
};

// ── chat ─────────────────────────────────────────────────────────────────────

pub(super) const CHAT: CommandSpec = CommandSpec {
    name: "chat",
    aliases: &[],
    help: "Start a freeform chat session with the configured agent in a container.",
    long_help: None,
    arguments: &[],
    flags: &CHAT_FLAGS,
    api_allowed: false,
    build: crate::command::dispatch::build::chat,
    gateway_need: GatewayNeed::None,
    requires_container_tier: false,
    requires_runtime: true,
    opens_tui_when_bare: false,
    internal: false,
    relay_allowed: true,
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
    internal: false,
    relay_allowed: false,
    subcommands: &[&SPECS_AMEND],
};

pub(super) const SPECS_AMEND: CommandSpec = CommandSpec {
    name: "amend",
    aliases: &[],
    help: "Review and amend a completed work item to match the final implementation.",
    long_help: None,
    arguments: &[ArgumentSpec {
        name: "work_item",
        help: "Work item to amend: a number in the configured workItems.source, or any \
               explicit --work-item form (local:<N>, a path, github:<N>, owner/repo#<N>, \
               a URL, http:<provider>:<id>).",
        kind: ArgumentKind::String,
        optional: false,
    }],
    flags: &[
        FlagSpec {
            long: "headless",
            short: Some('n'),
            help: "Run the agent headless (print mode).",
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
    internal: false,
    relay_allowed: false,
    subcommands: &[],
};

// ── config ───────────────────────────────────────────────────────────────────

/// `--global` / `--repo` / `--task <name>`: which file `config set` and
/// `config unset` write. Without one the key's own scope picks the file.
const CONFIG_SCOPE_FLAGS: [FlagSpec; 3] = [
    FlagSpec {
        long: "global",
        short: None,
        help: "Write the global config file.",
        kind: FlagKind::Bool,
        default: FlagDefault::Bool(false),
        frontends: FrontendVisibility::All,
        conflicts_with: &["repo", "task"],
        implies: &[],
        optional: true,
    },
    FlagSpec {
        long: "repo",
        short: None,
        help: "Write this repository's config file (.awman/config.json).",
        kind: FlagKind::Bool,
        default: FlagDefault::Bool(false),
        frontends: FrontendVisibility::All,
        conflicts_with: &["global", "task"],
        implies: &[],
        optional: true,
    },
    FlagSpec {
        long: "task",
        short: None,
        help: "Write the config file of the named squad task (task-scope keys only).",
        kind: FlagKind::OptionalString,
        default: FlagDefault::None,
        frontends: FrontendVisibility::All,
        conflicts_with: &["global", "repo"],
        implies: &[],
        optional: true,
    },
];

const CONFIG_JSON_FLAG: FlagSpec = FlagSpec {
    long: "json",
    short: None,
    help: "Print the result as JSON.",
    kind: FlagKind::Bool,
    default: FlagDefault::Bool(false),
    frontends: FrontendVisibility::All,
    conflicts_with: &[],
    implies: &[],
    optional: true,
};

const CONFIG_KEY_ARGUMENT: ArgumentSpec = ArgumentSpec {
    name: "key",
    help: "Config key, as a dotted path (e.g. agents.stuckTimeout).",
    kind: ArgumentKind::String,
    optional: false,
};

/// `awman config` with no subcommand is `awman config show`, so the group is
/// runnable and takes `show`'s flags.
pub(super) const CONFIG: CommandSpec = CommandSpec {
    name: "config",
    aliases: &[],
    help: "Show, get, set and unset configuration keys (bare `config` is `config show`).",
    long_help: None,
    arguments: &[],
    flags: &[CONFIG_JSON_FLAG],
    api_allowed: false,
    build: crate::command::dispatch::build::config,
    gateway_need: GatewayNeed::None,
    requires_container_tier: false,
    // The recovery path when the configured runtime cannot be constructed
    // on this host: `awman config set runtime.kind …` must stay reachable.
    requires_runtime: false,
    opens_tui_when_bare: false,
    internal: false,
    relay_allowed: false,
    subcommands: &[&CONFIG_SHOW, &CONFIG_GET, &CONFIG_SET, &CONFIG_UNSET],
};

pub(super) const CONFIG_SHOW: CommandSpec = CommandSpec {
    name: "show",
    aliases: &[],
    help: "Show every key: its global, repo and effective value, where the effective value \
           comes from, and its type.",
    long_help: None,
    arguments: &[],
    flags: &[CONFIG_JSON_FLAG],
    api_allowed: false,
    build: crate::command::dispatch::build::config,
    gateway_need: GatewayNeed::None,
    requires_container_tier: false,
    requires_runtime: false,
    opens_tui_when_bare: false,
    internal: false,
    relay_allowed: false,
    subcommands: &[],
};

pub(super) const CONFIG_GET: CommandSpec = CommandSpec {
    name: "get",
    aliases: &[],
    help: "Show one key's global, repo and effective value and where it comes from.",
    long_help: None,
    arguments: &[CONFIG_KEY_ARGUMENT],
    flags: &[CONFIG_JSON_FLAG],
    api_allowed: false,
    build: crate::command::dispatch::build::config,
    gateway_need: GatewayNeed::None,
    requires_container_tier: false,
    requires_runtime: false,
    opens_tui_when_bare: false,
    internal: false,
    relay_allowed: false,
    subcommands: &[],
};

pub(super) const CONFIG_SET: CommandSpec = CommandSpec {
    name: "set",
    aliases: &[],
    help: "Set a key in a config file. The key's scope picks the file unless --global, --repo \
           or --task names one; the file written is printed.",
    long_help: Some(
        "Values are parsed by the key's type: lists are comma-separated, maps are \
         k=v,k2=v2, durations look like 30s or 15m, sizes like 4GiB. Global-scope keys go \
         to the global file; repo- and task-scope keys go to the repo file inside a Git \
         repository and to the global file outside one. Use `config unset` to remove a key.",
    ),
    arguments: &[
        CONFIG_KEY_ARGUMENT,
        ArgumentSpec {
            name: "value",
            help: "New value, parsed by the key's type.",
            kind: ArgumentKind::String,
            optional: false,
        },
    ],
    flags: &CONFIG_SCOPE_FLAGS,
    api_allowed: false,
    build: crate::command::dispatch::build::config,
    gateway_need: GatewayNeed::None,
    requires_container_tier: false,
    requires_runtime: false,
    opens_tui_when_bare: false,
    internal: false,
    relay_allowed: false,
    subcommands: &[],
};

pub(super) const CONFIG_UNSET: CommandSpec = CommandSpec {
    name: "unset",
    aliases: &[],
    help: "Remove a key (or a whole named map entry) from a config file. Picks the file like \
           `config set`; the file written is printed.",
    long_help: None,
    arguments: &[CONFIG_KEY_ARGUMENT],
    flags: &CONFIG_SCOPE_FLAGS,
    api_allowed: false,
    build: crate::command::dispatch::build::config,
    gateway_need: GatewayNeed::None,
    requires_container_tier: false,
    requires_runtime: false,
    opens_tui_when_bare: false,
    internal: false,
    relay_allowed: false,
    subcommands: &[],
};
