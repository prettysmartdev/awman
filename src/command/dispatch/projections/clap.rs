//! Build a `clap::Command` from the canonical catalogue.

use clap::{Arg, ArgAction, Command};

use crate::command::dispatch::catalogue::{
    ArgumentKind, ArgumentSpec, CommandCatalogue, CommandSpec, FlagDefault, FlagKind, FlagSpec,
    FrontendVisibility,
};

impl CommandCatalogue {
    /// Build the top-level `clap::Command` from the catalogue. Any flag whose
    /// visibility is `TuiOnly` or `Hidden` is omitted from the CLI projection.
    pub fn build_clap_command(&self) -> Command {
        let root = self.root();
        let mut cmd = build_clap_for_spec(root, true);
        for flag in self.global_flags() {
            if flag_visible_to_cli(flag) {
                cmd = cmd.arg(build_clap_flag(flag).global(true));
            }
        }
        cmd
    }
}

fn build_clap_for_spec(spec: &'static CommandSpec, is_root: bool) -> Command {
    let mut cmd = Command::new(spec.name).about(spec.help);
    if is_root {
        cmd = cmd.version(env!("CARGO_PKG_VERSION"));
    }
    if let Some(long) = spec.long_help {
        cmd = cmd.long_about(long);
    }
    for alias in spec.aliases {
        cmd = cmd.alias(*alias);
    }
    for arg in spec.arguments {
        cmd = cmd.arg(build_clap_argument(arg));
    }
    for flag in spec.flags {
        if !flag_visible_to_cli(flag) {
            continue;
        }
        cmd = cmd.arg(build_clap_flag(flag));
    }
    for sub in spec.subcommands {
        // An internal entry point parses like any command but is not listed.
        cmd = cmd.subcommand(build_clap_for_spec(sub, false).hide(sub.internal));
    }
    if !is_root && !spec.subcommands.is_empty() {
        cmd = cmd.subcommand_required(false);
    }
    cmd
}

fn flag_visible_to_cli(flag: &FlagSpec) -> bool {
    matches!(
        flag.frontends,
        FrontendVisibility::All | FrontendVisibility::CliOnly | FrontendVisibility::CliAndTui
    )
}

fn build_clap_argument(spec: &ArgumentSpec) -> Arg {
    let mut arg = Arg::new(spec.name).help(spec.help);
    match spec.kind {
        ArgumentKind::String | ArgumentKind::Path => {
            arg = arg.required(!spec.optional);
        }
        ArgumentKind::OptionalString | ArgumentKind::OptionalPath => {
            arg = arg.required(false);
        }
        ArgumentKind::VarArgs => {
            arg = arg.required(!spec.optional).num_args(1..);
        }
        ArgumentKind::TrailingVarArgs => {
            arg = arg
                .required(!spec.optional)
                .num_args(1..)
                .trailing_var_arg(true)
                .allow_hyphen_values(true);
        }
    }
    arg
}

fn build_clap_flag(spec: &FlagSpec) -> Arg {
    let mut arg = Arg::new(spec.long).long(spec.long).help(spec.help);
    if let Some(c) = spec.short {
        arg = arg.short(c);
    }
    match spec.kind {
        FlagKind::Bool => {
            arg = arg.action(ArgAction::SetTrue);
            if let FlagDefault::Bool(true) = spec.default {
                arg = arg.default_value("true");
            }
        }
        FlagKind::String | FlagKind::OptionalString => {
            arg = arg.action(ArgAction::Set);
            if let FlagDefault::Str(s) = spec.default {
                arg = arg.default_value(s);
            }
        }
        FlagKind::Enum(values) => {
            arg = arg.action(ArgAction::Set).value_parser(values.to_vec());
            if let FlagDefault::Str(s) = spec.default {
                arg = arg.default_value(s);
            }
        }
        FlagKind::VecString => {
            arg = arg.action(ArgAction::Append);
        }
        FlagKind::Path | FlagKind::OptionalPath => {
            arg = arg.action(ArgAction::Set);
        }
        FlagKind::U16 => {
            arg = arg
                .action(ArgAction::Set)
                .value_parser(clap::value_parser!(u16));
            if let FlagDefault::U16(n) = spec.default {
                let s: &'static str = Box::leak(n.to_string().into_boxed_str());
                arg = arg.default_value(s);
            }
        }
        FlagKind::UsizeAtLeastOne => {
            arg = arg
                .action(ArgAction::Set)
                .value_parser(clap::builder::RangedU64ValueParser::<usize>::new().range(1..));
        }
    }
    for c in spec.conflicts_with {
        arg = arg.conflicts_with(*c);
    }
    arg
}
