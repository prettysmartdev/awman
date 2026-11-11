//! Markdown user-documentation projection of the catalogue.
//!
//! `markdown_reference()` renders every command, subcommand, alias, flag and
//! argument the catalogue defines into `docs/14-command-reference.md`'s
//! exact text. Nothing here reads a command or flag name back out of a
//! frontend; it only walks `CommandSpec`.

use crate::command::dispatch::catalogue::{
    ArgumentKind, ArgumentSpec, CommandCatalogue, CommandSpec, FlagDefault, FlagKind, FlagSpec,
    FrontendVisibility,
};

impl CommandCatalogue {
    /// Render the full command surface as Markdown, walking the catalogue
    /// depth-first in declaration order. Deterministic: the same catalogue
    /// always renders the same text, so the committed
    /// `docs/14-command-reference.md` can be diffed against it byte-for-byte.
    pub fn markdown_reference(&self) -> String {
        let mut out = String::new();
        out.push_str("# Command Reference\n\n");
        out.push_str(
            "Generated from `CommandCatalogue` (`src/command/dispatch/catalogue/`) — \
             do not hand-edit. Every command, flag, and argument awman accepts is listed \
             here exactly as the catalogue defines it.\n\n",
        );
        out.push_str("---\n\n");
        out.push_str("## Global flags\n\n");
        out.push_str(
            "Accepted by every command, anywhere before `--`, from the CLI, the TUI command \
             box and API `args`.\n\n",
        );
        out.push_str("| Flag | Kind | Default | Implies | Conflicts | Frontends | Help |\n");
        out.push_str("|---|---|---|---|---|---|---|\n");
        for flag in self.global_flags() {
            out.push_str(&render_flag_row(flag));
        }
        out.push_str("\n---\n\n");
        for sub in self.root().subcommands.iter().filter(|sub| !sub.internal) {
            render_command(sub, &["awman"], 2, &mut out);
            out.push_str("---\n\n");
        }
        out.push_str("[← Cleaning Up](13-cleaning-up.md) · [Back to contents](contents.md)\n");
        out
    }
}

fn render_command(
    spec: &'static CommandSpec,
    parent_path: &[&str],
    depth: usize,
    out: &mut String,
) {
    let mut path: Vec<&str> = parent_path.to_vec();
    path.push(spec.name);
    let heading = "#".repeat(depth);
    out.push_str(&format!("{heading} `{}`\n\n", path.join(" ")));

    if !spec.aliases.is_empty() {
        let aliases = spec
            .aliases
            .iter()
            .map(|a| format!("`{a}`"))
            .collect::<Vec<_>>()
            .join(", ");
        out.push_str(&format!("Alias: {aliases}\n\n"));
    }

    out.push_str(&escape_cell(spec.help));
    out.push_str("\n\n");
    if let Some(long) = spec.long_help {
        out.push_str(&escape_cell(long));
        out.push_str("\n\n");
    }

    out.push_str(&format!(
        "Available via: {}\n\n",
        if spec.api_allowed {
            "CLI, TUI, API"
        } else {
            "CLI, TUI"
        }
    ));

    if !spec.requires_runtime {
        out.push_str(
            "Runs without a working agent runtime — reachable even when the \
             configured runtime cannot be started on this host.\n\n",
        );
    }

    if !spec.arguments.is_empty() {
        out.push_str("| Argument | Kind | Required | Help |\n");
        out.push_str("|---|---|---|---|\n");
        for arg in spec.arguments {
            out.push_str(&render_argument_row(arg));
        }
        out.push('\n');
    }

    if !spec.flags.is_empty() {
        out.push_str("| Flag | Kind | Default | Implies | Conflicts | Frontends | Help |\n");
        out.push_str("|---|---|---|---|---|---|---|\n");
        for flag in spec.flags {
            out.push_str(&render_flag_row(flag));
        }
        out.push('\n');
    }

    for sub in spec.subcommands.iter().filter(|sub| !sub.internal) {
        render_command(sub, &path, depth + 1, out);
    }
}

fn render_argument_row(arg: &ArgumentSpec) -> String {
    format!(
        "| `{}` | {} | {} | {} |\n",
        arg.name,
        argument_kind_label(arg.kind),
        if arg.optional { "no" } else { "yes" },
        escape_cell(arg.help),
    )
}

fn render_flag_row(flag: &FlagSpec) -> String {
    format!(
        "| {} | {} | {} | {} | {} | {} | {} |\n",
        flag_label(flag),
        flag_kind_label(flag.kind),
        flag_default_label(flag.default),
        name_list_cell(flag.implies),
        name_list_cell(flag.conflicts_with),
        frontend_visibility_label(flag.frontends),
        escape_cell(flag.help),
    )
}

fn flag_label(flag: &FlagSpec) -> String {
    match flag.short {
        Some(c) => format!("`-{c}, --{}`", flag.long),
        None => format!("`--{}`", flag.long),
    }
}

fn flag_kind_label(kind: FlagKind) -> String {
    match kind {
        FlagKind::Bool => "bool".to_string(),
        FlagKind::String => "string".to_string(),
        FlagKind::OptionalString => "string (optional)".to_string(),
        FlagKind::Enum(values) => format!("enum: {}", values.join(", ")),
        FlagKind::VecString => "string (repeatable)".to_string(),
        FlagKind::Path => "path".to_string(),
        FlagKind::OptionalPath => "path (optional)".to_string(),
        FlagKind::U16 => "u16".to_string(),
        FlagKind::UsizeAtLeastOne => "usize (>= 1)".to_string(),
    }
}

fn flag_default_label(default: FlagDefault) -> String {
    match default {
        FlagDefault::None => "—".to_string(),
        FlagDefault::Bool(b) => b.to_string(),
        FlagDefault::Str(s) => format!("`{s}`"),
        FlagDefault::U16(n) => n.to_string(),
        FlagDefault::EmptyVec => "[]".to_string(),
    }
}

fn name_list_cell(names: &[&str]) -> String {
    if names.is_empty() {
        "—".to_string()
    } else {
        names
            .iter()
            .map(|n| format!("`--{n}`"))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

fn frontend_visibility_label(v: FrontendVisibility) -> &'static str {
    match v {
        FrontendVisibility::All => "CLI, TUI, API",
        FrontendVisibility::CliOnly => "CLI",
        FrontendVisibility::TuiOnly => "TUI",
        FrontendVisibility::CliAndTui => "CLI, TUI",
        FrontendVisibility::Hidden => "hidden",
    }
}

fn argument_kind_label(kind: ArgumentKind) -> &'static str {
    match kind {
        ArgumentKind::String => "string",
        ArgumentKind::OptionalString => "string (optional)",
        ArgumentKind::Path => "path",
        ArgumentKind::OptionalPath => "path (optional)",
        ArgumentKind::TrailingVarArgs | ArgumentKind::VarArgs => "trailing args",
    }
}

/// Escape characters that would otherwise break a Markdown table cell.
fn escape_cell(text: &str) -> String {
    text.replace('|', "\\|")
}
