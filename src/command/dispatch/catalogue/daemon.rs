//! Daemon resource API client.
use super::*;

pub(super) const DAEMON: CommandSpec = CommandSpec {
    name: "daemon",
    aliases: &[],
    help: "Control daemon resources.",
    long_help: None,
    arguments: &[],
    flags: &[],
    subcommands: &[&DAEMON_API],
    api_allowed: false,
    build: crate::command::dispatch::build::unsupported,
    gateway_need: GatewayNeed::None,
    requires_container_tier: false,
    requires_runtime: false,
    opens_tui_when_bare: false,
};
const DAEMON_API: CommandSpec = CommandSpec {
    name: "api", aliases: &[], help: "Call the daemon resource API over the owner-authenticated local socket, or a named remote target.",
    long_help: Some("Use an action with key=value arguments, --list to list actions, or --raw METHOD /v1/path with optional --data JSON, @file, or - for stdin."),
    arguments: &[
        ArgumentSpec { name: "action", help: "Action name, or /v1/path with --raw.", kind: ArgumentKind::OptionalString, optional: true },
        ArgumentSpec { name: "args", help: "Action arguments as key=value.", kind: ArgumentKind::VarArgs, optional: true },
    ],
    flags: &[
        FlagSpec { long: "list", short: None, help: "List every action and its method/path.", kind: FlagKind::Bool, default: FlagDefault::Bool(false), frontends: FrontendVisibility::All, conflicts_with: &["raw", "data"], implies: &[], optional: true },
        FlagSpec { long: "raw", short: None, help: "HTTP method for a raw /v1/path request.", kind: FlagKind::OptionalString, default: FlagDefault::None, frontends: FrontendVisibility::All, conflicts_with: &["list"], implies: &[], optional: true },
        FlagSpec { long: "data", short: None, help: "JSON body, @file, or - for stdin (requires --raw).", kind: FlagKind::OptionalString, default: FlagDefault::None, frontends: FrontendVisibility::All, conflicts_with: &["list"], implies: &[], optional: true },
        FlagSpec { long: "json", short: None, help: "Print the response body as JSON.", kind: FlagKind::Bool, default: FlagDefault::Bool(false), frontends: FrontendVisibility::All, conflicts_with: &[], implies: &[], optional: true },
    ],
    subcommands: &[], api_allowed: false, build: crate::command::dispatch::build::daemon_api,
    gateway_need: GatewayNeed::None, requires_container_tier: false, requires_runtime: false, opens_tui_when_bare: false,
};
