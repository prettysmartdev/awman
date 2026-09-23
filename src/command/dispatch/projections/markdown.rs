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
            "Generated from `CommandCatalogue` (`src/command/dispatch/catalogue.rs`) — \
             do not hand-edit. Every command, flag, and argument awman accepts is listed \
             here exactly as the catalogue defines it.\n\n",
        );
        out.push_str("---\n\n");
        for sub in self.root().subcommands {
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

    for sub in spec.subcommands {
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
        ArgumentKind::TrailingVarArgs => "trailing args",
    }
}

/// Escape characters that would otherwise break a Markdown table cell.
fn escape_cell(text: &str) -> String {
    text.replace('|', "\\|")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The committed reference doc this projection must match exactly.
    /// Regenerate it with `make docs-reference`.
    const COMMITTED_REFERENCE: &str = include_str!("../../../../docs/14-command-reference.md");

    #[test]
    fn markdown_reference_matches_committed_docs() {
        let generated = CommandCatalogue::get().markdown_reference();
        assert_eq!(
            generated, COMMITTED_REFERENCE,
            "docs/14-command-reference.md is stale relative to CommandCatalogue; \
             regenerate with `make docs-reference`"
        );
    }

    /// Not a check: writes the current catalogue's rendering over
    /// `docs/14-command-reference.md`. `make docs-reference` is the wrapper;
    /// run it after a catalogue change makes
    /// `markdown_reference_matches_committed_docs` fail.
    #[test]
    #[ignore = "regenerates docs/14-command-reference.md from the catalogue; run explicitly"]
    fn regenerate_command_reference() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/docs/14-command-reference.md");
        std::fs::write(path, CommandCatalogue::get().markdown_reference())
            .expect("failed to write docs/14-command-reference.md");
    }

    #[test]
    fn reference_contains_every_top_level_command() {
        let generated = CommandCatalogue::get().markdown_reference();
        for name in &[
            "init", "ready", "chat", "specs", "status", "config", "exec", "api", "squad", "remote",
            "new", "clean",
        ] {
            assert!(
                generated.contains(&format!("`awman {name}`")),
                "markdown_reference missing top-level command '{name}'"
            );
        }
    }

    // Walk every catalogue command path and assert the rendered heading and
    // every flag/argument name for that command appear in the output.
    fn walk_and_check(spec: &'static CommandSpec, path: Vec<&'static str>, generated: &str) {
        let full_path = path.join(" ");
        assert!(
            generated.contains(&format!("`{full_path}`")),
            "markdown_reference missing heading for '{full_path}'"
        );
        for flag in spec.flags {
            assert!(
                generated.contains(&format!("--{}", flag.long)),
                "markdown_reference missing flag '--{}' under '{full_path}'",
                flag.long
            );
        }
        for arg in spec.arguments {
            assert!(
                generated.contains(&format!("`{}`", arg.name)),
                "markdown_reference missing argument '{}' under '{full_path}'",
                arg.name
            );
        }
        for alias in spec.aliases {
            assert!(
                generated.contains(&format!("`{alias}`")),
                "markdown_reference missing alias '{alias}' under '{full_path}'"
            );
        }
        for sub in spec.subcommands {
            let mut child_path = path.clone();
            child_path.push(sub.name);
            walk_and_check(sub, child_path, generated);
        }
    }

    #[test]
    fn catalogue_markdown_consistency_every_command_is_rendered() {
        let cat = CommandCatalogue::get();
        let generated = cat.markdown_reference();
        for sub in cat.root().subcommands {
            walk_and_check(sub, vec!["awman", sub.name], &generated);
        }
    }

    #[test]
    fn reference_notes_api_availability() {
        let generated = CommandCatalogue::get().markdown_reference();
        assert!(generated.contains("Available via: CLI, TUI, API"));
        assert!(generated.contains("Available via: CLI, TUI"));
    }

    // Every command/alias name and flag long-name the catalogue defines,
    // deduplicated. Root ("awman") is excluded: naming the binary itself is
    // not "documenting a specific command".
    fn collect_names(
        spec: &'static CommandSpec,
        commands: &mut std::collections::HashSet<&'static str>,
        flags: &mut std::collections::HashSet<&'static str>,
    ) {
        commands.insert(spec.name);
        commands.extend(spec.aliases.iter().copied());
        flags.extend(spec.flags.iter().map(|f| f.long));
        for sub in spec.subcommands {
            collect_names(sub, commands, flags);
        }
    }

    /// `aspec/uxui/cli.md` (WI 0114 F-25) documents UX standards, not the
    /// command surface — that surface is `markdown_reference()`'s job. The
    /// only flags it names are the cross-cutting pair it exists to explain
    /// (`--json` implies `--non-interactive`, per decision Q8); every other
    /// real command, alias, or flag name — and the removed `--format md`
    /// value (decision Q9) — must never reappear in it.
    #[test]
    fn cli_md_documents_ux_standards_not_specific_commands_or_flags() {
        const ALLOWED_FLAG_MENTIONS: &[&str] = &["json", "non-interactive"];

        let cli_md =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/aspec/uxui/cli.md"))
                .expect("aspec/uxui/cli.md must exist");

        let cat = CommandCatalogue::get();
        let mut commands = std::collections::HashSet::new();
        let mut flags = std::collections::HashSet::new();
        for sub in cat.root().subcommands {
            collect_names(sub, &mut commands, &mut flags);
        }

        for name in &commands {
            let pattern = format!("awman {name}");
            assert!(
                !cli_md.contains(&pattern),
                "aspec/uxui/cli.md names a specific command ('{pattern}'); it must stay \
                 general UX standards — the per-command reference belongs in \
                 docs/14-command-reference.md"
            );
        }

        for flag in &flags {
            if ALLOWED_FLAG_MENTIONS.contains(flag) {
                continue;
            }
            let pattern = format!("--{flag}");
            assert!(
                !cli_md.contains(&pattern),
                "aspec/uxui/cli.md names a specific flag ('{pattern}'); it must stay \
                 general UX standards — the per-command reference belongs in \
                 docs/14-command-reference.md"
            );
        }

        assert!(
            !cli_md.contains("--format"),
            "the removed --format flag (decision Q9) must not reappear in aspec/uxui/cli.md"
        );
    }
}
