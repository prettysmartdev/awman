//! `agents` — inspect and manage running code agents.

use super::*;

pub(super) const AGENTS: CommandSpec = CommandSpec {
    name: "agents",
    aliases: &[],
    help: "Inspect and manage running code agents.",
    long_help: None,
    arguments: &[],
    flags: &[],
    api_availability: ApiAvailability::Disabled,
    build: crate::command::dispatch::build::unsupported,
    gateway_need: GatewayNeed::None,
    requires_container_tier: false,
    requires_runtime: true,
    opens_tui_when_bare: false,
    internal: false,
    relay_allowed: false,
    subcommands: &[
        &AGENTS_STATUS,
        &AGENTS_LIST,
        &AGENTS_DETAIL,
        &AGENTS_KILL,
        &AGENTS_PROFILE,
    ],
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
    api_availability: ApiAvailability::Standard,
    build: crate::command::dispatch::build::agents,
    gateway_need: GatewayNeed::None,
    requires_container_tier: false,
    requires_runtime: true,
    opens_tui_when_bare: false,
    internal: false,
    relay_allowed: false,
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
    api_availability: ApiAvailability::Standard,
    build: crate::command::dispatch::build::agents,
    gateway_need: GatewayNeed::None,
    requires_container_tier: false,
    requires_runtime: true,
    opens_tui_when_bare: false,
    internal: false,
    relay_allowed: false,
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
    api_availability: ApiAvailability::Standard,
    build: crate::command::dispatch::build::agents,
    gateway_need: GatewayNeed::None,
    requires_container_tier: false,
    requires_runtime: true,
    opens_tui_when_bare: false,
    internal: false,
    relay_allowed: false,
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
    api_availability: ApiAvailability::Standard,
    build: crate::command::dispatch::build::agents,
    gateway_need: GatewayNeed::None,
    requires_container_tier: false,
    requires_runtime: true,
    opens_tui_when_bare: false,
    internal: false,
    relay_allowed: false,
    subcommands: &[],
};

// ── agents profile (WI 0070) ────────────────────────────────────────────────
//
// Named settings profiles of the current repository and owner. `list` and
// `delete` are data operations and `close` reconciles a recorded cleanup
// obligation, so none of them inherits the parent's runtime requirement:
// they must stay reachable when the configured runtime cannot start. They
// are owner-local through the API (`ApiAvailability::LocalOwner`) and are
// never relayed, because a profile lives in the invoking owner's data home.

const PROFILE_AGENT_HELP: &str = "Agent the profile belongs to.";

const PROFILE_JSON_FLAG: FlagSpec = FlagSpec {
    long: "json",
    short: None,
    help: "Emit JSON output.",
    kind: FlagKind::Bool,
    default: FlagDefault::Bool(false),
    frontends: FrontendVisibility::All,
    conflicts_with: &[],
    implies: &[],
    optional: true,
};

/// `--agent NAME`, required by `delete` and `close`: a profile name is only
/// unique per agent.
const PROFILE_REQUIRED_AGENT_FLAG: FlagSpec = FlagSpec {
    long: "agent",
    short: None,
    help: PROFILE_AGENT_HELP,
    kind: FlagKind::String,
    default: FlagDefault::None,
    frontends: FrontendVisibility::All,
    conflicts_with: &[],
    implies: &[],
    optional: false,
};

const PROFILE_NAME_ARGUMENT: ArgumentSpec = ArgumentSpec {
    name: "name",
    help: "Settings profile name ([a-z0-9][a-z0-9_-]{0,62}); a name, not a path.",
    kind: ArgumentKind::String,
    optional: false,
};

pub(super) const AGENTS_PROFILE: CommandSpec = CommandSpec {
    name: "profile",
    aliases: &[],
    help: "Manage named agent settings profiles for this repository.",
    long_help: None,
    arguments: &[],
    flags: &[],
    api_availability: ApiAvailability::Disabled,
    build: crate::command::dispatch::build::unsupported,
    gateway_need: GatewayNeed::None,
    requires_container_tier: false,
    requires_runtime: false,
    opens_tui_when_bare: false,
    internal: false,
    relay_allowed: false,
    subcommands: &[
        &AGENTS_PROFILE_LIST,
        &AGENTS_PROFILE_DELETE,
        &AGENTS_PROFILE_CLOSE,
    ],
};

pub(super) const AGENTS_PROFILE_LIST: CommandSpec = CommandSpec {
    name: "list",
    aliases: &[],
    help: "List settings profiles and their state (active, inactive or closing).",
    long_help: None,
    arguments: &[],
    flags: &[
        FlagSpec {
            long: "agent",
            short: None,
            help: "Only list profiles of this agent.",
            kind: FlagKind::OptionalString,
            default: FlagDefault::None,
            frontends: FrontendVisibility::All,
            conflicts_with: &[],
            implies: &[],
            optional: true,
        },
        PROFILE_JSON_FLAG,
    ],
    api_availability: ApiAvailability::LocalOwner,
    build: crate::command::dispatch::build::agents_profile,
    gateway_need: GatewayNeed::None,
    requires_container_tier: false,
    requires_runtime: false,
    opens_tui_when_bare: false,
    internal: false,
    relay_allowed: false,
    subcommands: &[],
};

pub(super) const AGENTS_PROFILE_DELETE: CommandSpec = CommandSpec {
    name: "delete",
    aliases: &[],
    help: "Delete an inactive settings profile and its retained history.",
    long_help: None,
    arguments: &[PROFILE_NAME_ARGUMENT],
    flags: &[
        PROFILE_REQUIRED_AGENT_FLAG,
        FlagSpec {
            long: "yes",
            short: None,
            help: "Confirm the deletion; required.",
            kind: FlagKind::Bool,
            default: FlagDefault::Bool(false),
            frontends: FrontendVisibility::All,
            conflicts_with: &[],
            implies: &[],
            optional: true,
        },
        PROFILE_JSON_FLAG,
    ],
    api_availability: ApiAvailability::LocalOwner,
    build: crate::command::dispatch::build::agents_profile,
    gateway_need: GatewayNeed::None,
    requires_container_tier: false,
    requires_runtime: false,
    opens_tui_when_bare: false,
    internal: false,
    relay_allowed: false,
    subcommands: &[],
};

pub(super) const AGENTS_PROFILE_CLOSE: CommandSpec = CommandSpec {
    name: "close",
    aliases: &[],
    help:
        "Finish closing a settings profile whose agent has ended, so it can be reused or deleted.",
    long_help: None,
    arguments: &[PROFILE_NAME_ARGUMENT],
    flags: &[PROFILE_REQUIRED_AGENT_FLAG, PROFILE_JSON_FLAG],
    api_availability: ApiAvailability::LocalOwner,
    build: crate::command::dispatch::build::agents_profile,
    gateway_need: GatewayNeed::None,
    requires_container_tier: false,
    requires_runtime: false,
    opens_tui_when_bare: false,
    internal: false,
    relay_allowed: false,
    subcommands: &[],
};
