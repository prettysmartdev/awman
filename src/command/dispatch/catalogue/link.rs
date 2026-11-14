//! Top-level linking; authority remains specific to each target.
use super::*;

pub(super) const LINK: CommandSpec = CommandSpec {
    name: "link",
    aliases: &[],
    help:
        "Link this machine to a work server, or approve a browser link requested on this machine.",
    long_help: Some(
        "Use awman link https://server.example or awman link web-XXXX-XXXX. \
         Browser approval requires an interactive terminal.",
    ),
    arguments: &[ArgumentSpec {
        name: "target",
        help: "A work server URL, or a browser code shown by the web app (web-XXXX-XXXX).",
        kind: ArgumentKind::String,
        optional: false,
    }],
    flags: &[
        FlagSpec {
            long: "name",
            short: None,
            help: "Server entry name.",
            kind: FlagKind::String,
            default: FlagDefault::None,
            frontends: FrontendVisibility::CliOnly,
            conflicts_with: &[],
            implies: &[],
            optional: true,
        },
        FlagSpec {
            long: "label",
            short: None,
            help: "Worker label k=v (repeatable).",
            kind: FlagKind::VecString,
            default: FlagDefault::EmptyVec,
            frontends: FrontendVisibility::CliOnly,
            conflicts_with: &[],
            implies: &[],
            optional: true,
        },
        FlagSpec {
            long: "token",
            short: None,
            help: "Static server token; - reads stdin.",
            kind: FlagKind::String,
            default: FlagDefault::None,
            frontends: FrontendVisibility::CliOnly,
            conflicts_with: &[],
            implies: &[],
            optional: true,
        },
    ],
    subcommands: &[],
    api_allowed: false,
    build: crate::command::dispatch::build::link,
    gateway_need: GatewayNeed::None,
    requires_container_tier: false,
    requires_runtime: false,
    opens_tui_when_bare: false,
    internal: false,
    relay_allowed: false,
};
