//! `awman attach` (WI 0141, 0139 §7).

use super::{
    ArgumentKind, ArgumentSpec, CommandSpec, FlagDefault, FlagKind, FlagSpec, FrontendVisibility,
    GatewayNeed,
};

pub(super) const ATTACH_TARGET_ARGUMENT: ArgumentSpec = ArgumentSpec {
    name: "id",
    help: "Terminal (term_…), command (cmd_…) or squad task (task_…) id.",
    kind: ArgumentKind::String,
    optional: false,
};

pub(super) const ATTACH: CommandSpec = CommandSpec {
    name: "attach",
    aliases: &[],
    help: "Watch, or step into, a running agent's terminal.",
    long_help: Some(
        "Opens a terminal the daemon serves. `term_…` opens that terminal; `cmd_…` opens the \
         command's current terminal, or with --step that step's; `task_…` opens the squad \
         task's running terminal. Ctrl-C detaches without stopping the agent.",
    ),
    arguments: &[ATTACH_TARGET_ARGUMENT],
    flags: &[
        FlagSpec {
            long: "step",
            short: None,
            help: "With a command id: the step whose terminal to open.",
            kind: FlagKind::String,
            default: FlagDefault::None,
            frontends: FrontendVisibility::CliAndTui,
            conflicts_with: &[],
            implies: &[],
            optional: true,
        },
        FlagSpec {
            long: "read-only",
            short: None,
            help: "Never send input or resize; watch only.",
            kind: FlagKind::Bool,
            default: FlagDefault::Bool(false),
            frontends: FrontendVisibility::CliAndTui,
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
            frontends: FrontendVisibility::CliAndTui,
            conflicts_with: &[],
            implies: &[],
            optional: true,
        },
    ],
    api_allowed: false,
    build: crate::command::dispatch::build::attach,
    gateway_need: GatewayNeed::Running,
    requires_container_tier: false,
    requires_runtime: false,
    opens_tui_when_bare: false,
    internal: false,
    relay_allowed: false,
    subcommands: &[],
};
