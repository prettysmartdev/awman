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
        build_clap_for_spec(root, true)
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
        cmd = cmd.subcommand(build_clap_for_spec(sub, false));
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
    }
    for c in spec.conflicts_with {
        arg = arg.conflicts_with(*c);
    }
    arg
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_root_succeeds_and_includes_top_level_commands() {
        let cat = CommandCatalogue::get();
        let cmd = cat.build_clap_command();
        let names: Vec<_> = cmd
            .get_subcommands()
            .map(|c| c.get_name().to_string())
            .collect();
        for n in [
            "init", "ready", "chat", "specs", "status", "config", "exec", "api", "remote", "new",
        ] {
            assert!(
                names.iter().any(|x| x == n),
                "missing subcommand {n} in clap projection"
            );
        }
    }

    #[test]
    fn exec_workflow_alias_wf_is_present() {
        let cat = CommandCatalogue::get();
        let cmd = cat.build_clap_command();
        let exec = cmd
            .get_subcommands()
            .find(|c| c.get_name() == "exec")
            .unwrap();
        let workflow = exec
            .get_subcommands()
            .find(|c| c.get_name() == "workflow")
            .unwrap();
        let aliases: Vec<_> = workflow.get_all_aliases().map(|s| s.to_string()).collect();
        assert!(aliases.iter().any(|a| a == "wf"));
    }

    // Recursively verify that every long flag in the clap projection matches a
    // flag or argument in the catalogue. Built-in clap flags (help, version) are
    // excluded. TUI-only flags must NOT appear in the clap projection.
    fn verify_clap_args_against_catalogue(
        cat: &CommandCatalogue,
        cmd: &clap::Command,
        path: Vec<String>,
    ) {
        if !path.is_empty() {
            let path_strs: Vec<&str> = path.iter().map(|s| s.as_str()).collect();
            if let Some(spec) = cat.lookup(&path_strs) {
                for arg in cmd.get_arguments() {
                    let id = arg.get_id().as_str();
                    if matches!(id, "help" | "version") {
                        continue;
                    }
                    let in_flags = spec.find_flag(id).is_some();
                    let in_args = spec.arguments.iter().any(|a| a.name == id);
                    assert!(
                        in_flags || in_args,
                        "clap arg '{id}' at {path:?} not found in catalogue"
                    );
                    // TUI-only flags must NOT appear in CLI projection.
                    if let Some(flag) = spec.find_flag(id) {
                        assert!(
                            !matches!(
                                flag.frontends,
                                crate::command::dispatch::catalogue::FrontendVisibility::TuiOnly
                            ),
                            "TUI-only flag '{id}' at {path:?} must not be in clap projection"
                        );
                    }
                }
            }
        }
        for sub in cmd.get_subcommands() {
            let mut new_path = path.clone();
            new_path.push(sub.get_name().to_string());
            verify_clap_args_against_catalogue(cat, sub, new_path);
        }
    }

    #[test]
    fn catalogue_clap_consistency() {
        let cat = CommandCatalogue::get();
        let clap_cmd = cat.build_clap_command();
        verify_clap_args_against_catalogue(cat, &clap_cmd, vec![]);
    }

    #[test]
    fn clap_tui_only_flags_absent_from_cli_projection() {
        // There are currently no TUI-only flags in the catalogue, but if one
        // is ever added the clap projection must exclude it.  The consistency
        // walk in catalogue_clap_consistency already asserts this; this test
        // adds a targeted lookup to make the intent explicit.
        let cat = CommandCatalogue::get();
        let clap_cmd = cat.build_clap_command();
        // Walk the full tree and collect every long flag present in the clap
        // projection.
        let mut all_clap_longs: Vec<String> = Vec::new();
        collect_clap_longs(&clap_cmd, &mut all_clap_longs);
        // None of those should be from a TUI-only flag in the catalogue.
        let root = cat.root();
        check_no_tui_only_in_longs(root, &all_clap_longs, &[]);
    }

    fn collect_clap_longs(cmd: &clap::Command, out: &mut Vec<String>) {
        for arg in cmd.get_arguments() {
            if let Some(long) = arg.get_long() {
                out.push(long.to_string());
            }
        }
        for sub in cmd.get_subcommands() {
            collect_clap_longs(sub, out);
        }
    }

    fn check_no_tui_only_in_longs(
        spec: &'static crate::command::dispatch::catalogue::CommandSpec,
        clap_longs: &[String],
        path: &[&str],
    ) {
        for flag in spec.flags {
            if matches!(
                flag.frontends,
                crate::command::dispatch::catalogue::FrontendVisibility::TuiOnly
            ) {
                assert!(
                    !clap_longs.contains(&flag.long.to_string()),
                    "TUI-only flag '{}' at {:?} must not appear in clap projection",
                    flag.long,
                    path
                );
            }
        }
        for sub in spec.subcommands {
            let mut new_path = path.to_vec();
            new_path.push(sub.name);
            check_no_tui_only_in_longs(sub, clap_longs, &new_path);
        }
    }
}
