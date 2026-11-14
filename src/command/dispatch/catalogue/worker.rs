//! Work server linking is the worker's sole direct command.
use super::*;

pub(super) const WORKER: CommandSpec = CommandSpec {
    name: "worker",
    aliases: &[],
    help: "Link awman to a work server.",
    long_help: None,
    arguments: &[],
    flags: &[],
    subcommands: &[&WORKER_LINK],
    api_allowed: false,
    build: crate::command::dispatch::build::unsupported,
    gateway_need: GatewayNeed::None,
    requires_container_tier: false,
    requires_runtime: false,
    opens_tui_when_bare: false,
    internal: false,
    relay_allowed: false,
};

const WORKER_LINK: CommandSpec = CommandSpec {
    name: "link",
    aliases: &[],
    help: "Approve a device link or store a server-issued token.",
    long_help: Some("Links this machine to a work server. The refresh token is stored as owner-only state, never in config."),
    arguments: &[ArgumentSpec {
        name: "server-url",
        help: "HTTPS base URL of the work server.",
        kind: ArgumentKind::String,
        optional: false,
    }],
    flags: &[
        FlagSpec { long: "name", short: None, help: "Config name for this server (defaults to its host name).", kind: FlagKind::String, default: FlagDefault::None, frontends: FrontendVisibility::All, conflicts_with: &[], implies: &[], optional: true },
        FlagSpec { long: "label", short: None, help: "Owner-configured label k=v (repeatable).", kind: FlagKind::VecString, default: FlagDefault::EmptyVec, frontends: FrontendVisibility::All, conflicts_with: &[], implies: &[], optional: true },
        FlagSpec { long: "token", short: None, help: "Static token, or - to read from stdin; omit for device flow.", kind: FlagKind::String, default: FlagDefault::None, frontends: FrontendVisibility::All, conflicts_with: &[], implies: &[], optional: true },
    ],
    subcommands: &[],
    api_allowed: false,
    build: crate::command::dispatch::build::worker_link,
    gateway_need: GatewayNeed::None,
    requires_container_tier: false,
    requires_runtime: false,
    opens_tui_when_bare: false,
    internal: false,
    relay_allowed: false,
};
