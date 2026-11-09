//! Catalogue spec for the hidden `view` command (WI 0142 Part B §2):
//! `--instance <ins_…>` (required), `--tab <tab_…>`, `--annotation k=v`
//! (repeatable). `internal: true` keeps it out of `--help`, the generated
//! reference and TUI hints; `relay_allowed: false` because it is a frontend
//! entry point, not an engine command.
//!
//! Contract: `/awman/context/workflow/0142-design.md` §4.10.

use super::{CommandSpec, FlagDefault, FlagKind, FlagSpec, FrontendVisibility, GatewayNeed};

pub(super) const VIEW: CommandSpec = CommandSpec {
    name: "view",
    aliases: &[],
    help: "Render one tab of an existing engine instance (internal).",
    long_help: Some(
        "Opens a secondary TUI view of a daemon-hosted engine instance: one tab with --tab, \
         otherwise every tab. Used by host integrations; not a user command.",
    ),
    arguments: &[],
    flags: &[
        FlagSpec {
            long: "instance",
            short: None,
            help: "The engine instance (ins_…) to view.",
            kind: FlagKind::String,
            default: FlagDefault::None,
            frontends: FrontendVisibility::CliOnly,
            conflicts_with: &[],
            implies: &[],
            optional: false,
        },
        FlagSpec {
            long: "tab",
            short: None,
            help: "The tab (tab_…) to render; every tab when omitted.",
            kind: FlagKind::String,
            default: FlagDefault::None,
            frontends: FrontendVisibility::CliOnly,
            conflicts_with: &[],
            implies: &[],
            optional: true,
        },
        FlagSpec {
            long: "annotation",
            short: None,
            help: "Integration metadata on this view, k=v (repeatable).",
            kind: FlagKind::VecString,
            default: FlagDefault::EmptyVec,
            frontends: FrontendVisibility::CliOnly,
            conflicts_with: &[],
            implies: &[],
            optional: true,
        },
    ],
    api_allowed: false,
    build: crate::command::dispatch::build::view,
    gateway_need: GatewayNeed::None,
    requires_container_tier: false,
    requires_runtime: false,
    opens_tui_when_bare: true,
    internal: true,
    relay_allowed: false,
    subcommands: &[],
};
