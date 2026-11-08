//! `agents` — inspect and manage running code agents.

use super::*;

pub(super) const AGENTS: CommandSpec = CommandSpec {
    name: "agents",
    aliases: &[],
    help: "Inspect and manage running code agents.",
    long_help: None,
    arguments: &[],
    flags: &[],
    api_allowed: false,
    build: crate::command::dispatch::build::unsupported,
    gateway_need: GatewayNeed::None,
    requires_container_tier: false,
    requires_runtime: true,
    opens_tui_when_bare: false,
    subcommands: &[&AGENTS_STATUS, &AGENTS_LIST, &AGENTS_DETAIL, &AGENTS_KILL],
};

pub(super) const AGENTS_STATUS: CommandSpec = CommandSpec {
    name: "status",
    aliases: &[],
    help: "Show the status of running code agents.",
    long_help: None,
    arguments: &[],
    flags: &[
        FlagSpec {
            long: "watch",
            short: None,
            help: "Keep refreshing the dashboard until interrupted.",
            kind: FlagKind::Bool,
            default: FlagDefault::Bool(false),
            frontends: FrontendVisibility::CliAndTui,
            conflicts_with: &["json"],
            implies: &[],
            optional: true,
        },
        FlagSpec {
            long: "json",
            short: None,
            help: "Emit JSON output.",
            kind: FlagKind::Bool,
            default: FlagDefault::Bool(false),
            frontends: FrontendVisibility::All,
            conflicts_with: &["watch"],
            implies: &[],
            optional: true,
        },
    ],
    api_allowed: true,
    build: crate::command::dispatch::build::agents,
    gateway_need: GatewayNeed::None,
    requires_container_tier: false,
    requires_runtime: true,
    opens_tui_when_bare: false,
    subcommands: &[],
};

pub(super) const AGENTS_LIST: CommandSpec = CommandSpec {
    name: "list",
    aliases: &["ps", "ls"],
    help: "List running code agents.",
    long_help: None,
    arguments: &[],
    flags: &[
        FlagSpec {
            long: "all",
            short: Some('a'),
            help: "Include stopped agents that are still eligible for cleanup.",
            kind: FlagKind::Bool,
            default: FlagDefault::Bool(false),
            frontends: FrontendVisibility::All,
            conflicts_with: &[],
            implies: &[],
            optional: true,
        },
        FlagSpec {
            long: "quiet",
            short: Some('q'),
            help: "Print only agent IDs.",
            kind: FlagKind::Bool,
            default: FlagDefault::Bool(false),
            frontends: FrontendVisibility::All,
            conflicts_with: &["json"],
            implies: &[],
            optional: true,
        },
        FlagSpec {
            long: "json",
            short: None,
            help: "Emit JSON output.",
            kind: FlagKind::Bool,
            default: FlagDefault::Bool(false),
            frontends: FrontendVisibility::All,
            conflicts_with: &["quiet"],
            implies: &[],
            optional: true,
        },
        FlagSpec {
            long: "no-trunc",
            short: None,
            help: "Do not truncate IDs, names or image names.",
            kind: FlagKind::Bool,
            default: FlagDefault::Bool(false),
            frontends: FrontendVisibility::All,
            conflicts_with: &[],
            implies: &[],
            optional: true,
        },
    ],
    api_allowed: true,
    build: crate::command::dispatch::build::agents,
    gateway_need: GatewayNeed::None,
    requires_container_tier: false,
    requires_runtime: true,
    opens_tui_when_bare: false,
    subcommands: &[],
};

pub(super) const AGENT_ID_ARGUMENT: ArgumentSpec = ArgumentSpec {
    name: "id",
    help: "Agent ID, unique ID prefix, or name.",
    kind: ArgumentKind::String,
    optional: false,
};

impl CommandCatalogue {
    /// Kept for old command-box transcripts; the `agents attach` catalogue
    /// entry itself has been removed in favour of root `attach`.
    pub fn is_agent_attach<S: AsRef<str>>(&self, words: &[S]) -> bool {
        let words: Vec<_> = words.iter().map(AsRef::as_ref).collect();
        words.first() == Some(&"agents") && words.get(1) == Some(&"attach")
    }

    /// Attach commands own Ctrl-C as a detach signal, rather than forwarding
    /// it to the agent process.
    pub fn is_terminal_attach<S: AsRef<str>>(&self, words: &[S]) -> bool {
        let words: Vec<_> = words.iter().map(AsRef::as_ref).collect();
        words.first() == Some(&"attach")
            || (words.first() == Some(&"squad") && words.get(1) == Some(&"attach"))
            || self.is_agent_attach(words.as_slice())
    }
}

pub(super) const AGENTS_DETAIL: CommandSpec = CommandSpec {
    name: "detail",
    aliases: &["inspect"],
    help: "Show details for an agent.",
    long_help: None,
    arguments: &[AGENT_ID_ARGUMENT],
    flags: &[FlagSpec {
        long: "json",
        short: None,
        help: "Emit JSON output.",
        kind: FlagKind::Bool,
        default: FlagDefault::Bool(false),
        frontends: FrontendVisibility::All,
        conflicts_with: &[],
        implies: &[],
        optional: true,
    }],
    api_allowed: true,
    build: crate::command::dispatch::build::agents,
    gateway_need: GatewayNeed::None,
    requires_container_tier: false,
    requires_runtime: true,
    opens_tui_when_bare: false,
    subcommands: &[],
};

pub(super) const AGENTS_KILL: CommandSpec = CommandSpec {
    name: "kill",
    aliases: &["rm"],
    help: "Stop and remove a running agent.",
    long_help: None,
    arguments: &[AGENT_ID_ARGUMENT],
    flags: &[
        FlagSpec {
            long: "json",
            short: None,
            help: "Emit JSON output.",
            kind: FlagKind::Bool,
            default: FlagDefault::Bool(false),
            frontends: FrontendVisibility::All,
            conflicts_with: &[],
            implies: &[],
            optional: true,
        },
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
    ],
    api_allowed: true,
    build: crate::command::dispatch::build::agents,
    gateway_need: GatewayNeed::None,
    requires_container_tier: false,
    requires_runtime: true,
    opens_tui_when_bare: false,
    subcommands: &[],
};
