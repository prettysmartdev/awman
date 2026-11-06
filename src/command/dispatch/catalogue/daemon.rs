//! `awman daemon`: the one daemon's lifecycle and its resource API client.
use super::*;

const fn bool_flag(long: &'static str, short: Option<char>, help: &'static str) -> FlagSpec {
    FlagSpec {
        long,
        short,
        help,
        kind: FlagKind::Bool,
        default: FlagDefault::Bool(false),
        frontends: FrontendVisibility::All,
        conflicts_with: &[],
        implies: &[],
        optional: true,
    }
}

const fn daemon_leaf(
    name: &'static str,
    help: &'static str,
    long_help: Option<&'static str>,
    flags: &'static [FlagSpec],
) -> CommandSpec {
    CommandSpec {
        name,
        aliases: &[],
        help,
        long_help,
        arguments: &[],
        flags,
        subcommands: &[],
        api_allowed: false,
        build: crate::command::dispatch::build::daemon,
        gateway_need: GatewayNeed::None,
        requires_container_tier: false,
        requires_runtime: false,
        opens_tui_when_bare: false,
    }
}

pub(super) const DAEMON: CommandSpec = CommandSpec {
    name: "daemon",
    aliases: &[],
    help: "Run and control the awman daemon (squad, worker and engine features).",
    long_help: Some(
        "One daemon serves every enabled feature. Turn features on and off with \
         `awman config set daemon.features.<name>.enabled true|false`.",
    ),
    arguments: &[],
    flags: &[],
    subcommands: &[
        &DAEMON_START,
        &DAEMON_STOP,
        &DAEMON_STATUS,
        &DAEMON_LOGS,
        &DAEMON_API,
    ],
    api_allowed: false,
    build: crate::command::dispatch::build::unsupported,
    gateway_need: GatewayNeed::None,
    requires_container_tier: false,
    requires_runtime: false,
    opens_tui_when_bare: false,
};

const DAEMON_START: CommandSpec = daemon_leaf(
    "start",
    "Start the daemon in the foreground (Ctrl-C stops it).",
    Some(
        "Refuses when no feature is enabled. With --background the daemon runs under the OS \
         service manager (systemd user unit awman-daemon, launchd label io.awman.daemon).",
    ),
    &[bool_flag(
        "background",
        None,
        "Run the daemon in the background under the OS service manager.",
    )],
);

const DAEMON_STOP: CommandSpec = daemon_leaf(
    "stop",
    "Stop the daemon; running work drains first.",
    None,
    &[bool_flag(
        "now",
        None,
        "Cancel running jobs of every feature before stopping instead of letting them finish.",
    )],
);

const DAEMON_STATUS: CommandSpec = daemon_leaf(
    "status",
    "Show the daemon's features, listeners and capacity.",
    None,
    &[bool_flag("json", None, "Print GET /v1/status as JSON.")],
);

const DAEMON_LOGS: CommandSpec = daemon_leaf(
    "logs",
    "Print the daemon log.",
    None,
    &[bool_flag(
        "follow",
        Some('f'),
        "Keep printing new lines until Ctrl-C.",
    )],
);

const DAEMON_API: CommandSpec = CommandSpec {
    name: "api",
    aliases: &[],
    help: "Call the daemon API over the owner-only local socket or named pipe.",
    long_help: Some(
        "Use an action with key=value arguments, --list to list actions with their routes, or \
         --raw METHOD /v1/path with optional --data JSON, @file, or - for stdin.",
    ),
    arguments: &[
        ArgumentSpec {
            name: "action",
            help: "Action name, or /v1/path with --raw.",
            kind: ArgumentKind::OptionalString,
            optional: true,
        },
        ArgumentSpec {
            name: "args",
            help: "Action arguments as key=value.",
            kind: ArgumentKind::VarArgs,
            optional: true,
        },
    ],
    flags: &[
        FlagSpec {
            long: "list",
            short: None,
            help: "List every action with its arguments and route.",
            kind: FlagKind::Bool,
            default: FlagDefault::Bool(false),
            frontends: FrontendVisibility::All,
            conflicts_with: &["raw", "data"],
            implies: &[],
            optional: true,
        },
        FlagSpec {
            long: "raw",
            short: None,
            help: "HTTP method for a raw /v1/path request.",
            kind: FlagKind::OptionalString,
            default: FlagDefault::None,
            frontends: FrontendVisibility::All,
            conflicts_with: &["list"],
            implies: &[],
            optional: true,
        },
        FlagSpec {
            long: "data",
            short: None,
            help: "JSON body, @file, or - for stdin (requires --raw).",
            kind: FlagKind::OptionalString,
            default: FlagDefault::None,
            frontends: FrontendVisibility::All,
            conflicts_with: &["list"],
            implies: &[],
            optional: true,
        },
        bool_flag("json", None, "Print the response body as JSON."),
    ],
    subcommands: &[],
    api_allowed: false,
    build: crate::command::dispatch::build::daemon_api,
    gateway_need: GatewayNeed::None,
    requires_container_tier: false,
    requires_runtime: false,
    opens_tui_when_bare: false,
};
