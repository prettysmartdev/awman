//! TUI command-box hint and completion projection.

use crate::command::dispatch::catalogue::{CommandCatalogue, CommandSpec, FrontendVisibility};

/// Hint shown above the TUI command box for a given command path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TuiHint {
    pub path: Vec<String>,
    pub help: String,
    pub flags: Vec<String>,
}

/// One completion entry returned to the TUI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TuiCompletion {
    pub completion: String,
    pub help: String,
}

impl CommandCatalogue {
    /// Hint string shown above the TUI command box for a given path.
    pub fn tui_hint_for(&self, path: &[&str]) -> Option<TuiHint> {
        let spec = self.lookup(path)?;
        let flags = spec
            .flags
            .iter()
            .filter(|f| flag_visible_to_tui(f.frontends))
            .map(|f| format!("--{}", f.long))
            .collect();
        Some(TuiHint {
            path: path.iter().map(|s| s.to_string()).collect(),
            help: spec.help.to_string(),
            flags,
        })
    }

    /// Compute completions for a partial input (e.g. `"ex"` or `"exec wo"`).
    /// Returns matching command path tails and flag names.
    pub fn tui_completions(&self, partial: &str) -> Vec<TuiCompletion> {
        let trimmed = partial.trim();
        let parts: Vec<&str> = if trimmed.is_empty() {
            Vec::new()
        } else {
            trimmed.split_whitespace().collect()
        };
        let mut current: &CommandSpec = self.root();
        let mut consumed = 0;
        for (idx, part) in parts.iter().enumerate() {
            // If this is the last token AND a non-empty prefix, treat it as a
            // partial and complete against current.subcommands.
            let is_last = idx + 1 == parts.len();
            let is_empty_partial = part.is_empty();
            if is_last && (is_empty_partial || current.find_subcommand(part).is_none()) {
                break;
            }
            match current.find_subcommand(part) {
                Some(sub) => {
                    current = sub;
                    consumed = idx + 1;
                }
                None => break,
            }
        }
        let prefix = if consumed < parts.len() {
            parts[consumed]
        } else {
            ""
        };
        let mut out = Vec::new();
        for sub in current.subcommands {
            if !flag_visible_to_tui(FrontendVisibility::All) {
                continue;
            }
            if sub.name.starts_with(prefix) {
                out.push(TuiCompletion {
                    completion: sub.name.to_string(),
                    help: sub.help.to_string(),
                });
            }
        }
        for flag in current.flags {
            if !flag_visible_to_tui(flag.frontends) {
                continue;
            }
            let candidate = format!("--{}", flag.long);
            if candidate.starts_with(prefix) {
                out.push(TuiCompletion {
                    completion: candidate,
                    help: flag.help.to_string(),
                });
            }
        }
        out
    }
}

fn flag_visible_to_tui(v: FrontendVisibility) -> bool {
    matches!(
        v,
        FrontendVisibility::All | FrontendVisibility::TuiOnly | FrontendVisibility::CliAndTui
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hint_for_chat_returns_help_and_flags() {
        let cat = CommandCatalogue::get();
        let hint = cat.tui_hint_for(&["chat"]).unwrap();
        assert!(hint.flags.iter().any(|f| f == "--yolo"));
        assert!(!hint.help.is_empty());
    }

    #[test]
    fn completions_for_partial_top_level() {
        let cat = CommandCatalogue::get();
        let comps = cat.tui_completions("ex");
        assert!(comps.iter().any(|c| c.completion == "exec"));
    }

    #[test]
    fn completions_for_partial_subcommand() {
        let cat = CommandCatalogue::get();
        let comps = cat.tui_completions("exec wo");
        assert!(comps.iter().any(|c| c.completion == "workflow"));
    }

    #[test]
    fn hint_for_nested_commands_returns_some() {
        let cat = CommandCatalogue::get();
        for path in &[
            vec!["exec", "workflow"],
            vec!["exec", "prompt"],
            vec!["config", "show"],
            vec!["config", "get"],
            vec!["config", "set"],
            vec!["api", "start"],
            vec!["remote", "exec", "workflow"],
            vec!["new", "spec"],
        ] {
            let hint = cat.tui_hint_for(path);
            assert!(hint.is_some(), "tui_hint_for({path:?}) must return Some");
            assert!(
                !hint.unwrap().help.is_empty(),
                "help must not be empty for {path:?}"
            );
        }
    }

    // Walk every catalogue command path and assert tui_hint_for returns Some.
    fn walk_and_check_tui_hints(
        cat: &CommandCatalogue,
        spec: &'static CommandSpec,
        path: Vec<String>,
    ) {
        if !path.is_empty() {
            let path_strs: Vec<&str> = path.iter().map(|s| s.as_str()).collect();
            assert!(
                cat.tui_hint_for(&path_strs).is_some(),
                "tui_hint_for({path:?}) returned None"
            );
        }
        for sub in spec.subcommands {
            let mut new_path = path.clone();
            new_path.push(sub.name.to_string());
            walk_and_check_tui_hints(cat, sub, new_path);
        }
    }

    #[test]
    fn catalogue_tui_consistency_every_command_has_a_hint() {
        let cat = CommandCatalogue::get();
        walk_and_check_tui_hints(cat, cat.root(), vec![]);
    }

    #[test]
    fn completions_include_all_flag_flags_for_chat() {
        let cat = CommandCatalogue::get();
        // typing "chat " (with trailing space) should yield flags for chat
        let comps = cat.tui_completions("chat ");
        let flag_completions: Vec<&str> = comps.iter().map(|c| c.completion.as_str()).collect();
        for expected_flag in &[
            "--yolo",
            "--plan",
            "--non-interactive",
            "--auto",
            "--overlay",
        ] {
            assert!(
                flag_completions.iter().any(|c| c == expected_flag),
                "completion '{expected_flag}' missing from: {flag_completions:?}"
            );
        }
    }

    #[test]
    fn completions_do_not_include_cli_only_flags() {
        let cat = CommandCatalogue::get();
        // api start's CliOnly flags must not appear in TUI completions.
        let comps = cat.tui_completions("api start ");
        let flag_completions: Vec<&str> = comps.iter().map(|c| c.completion.as_str()).collect();
        for cli_only_flag in &[
            "--port",
            "--workdirs",
            "--background",
            "--refresh-key",
            "--dangerously-skip-tls",
        ] {
            assert!(
                !flag_completions.contains(cli_only_flag),
                "CLI-only flag '{cli_only_flag}' must not appear in TUI completions; got: {flag_completions:?}"
            );
        }
    }

    #[test]
    fn completions_empty_prefix_returns_all_top_level_commands() {
        let cat = CommandCatalogue::get();
        let comps = cat.tui_completions("");
        let names: Vec<&str> = comps.iter().map(|c| c.completion.as_str()).collect();
        for expected in &["chat", "exec", "status", "ready", "config"] {
            assert!(
                names.contains(expected),
                "empty prefix must return all top-level commands; missing '{expected}'"
            );
        }
    }

    #[test]
    fn completions_no_match_returns_empty() {
        let cat = CommandCatalogue::get();
        let comps = cat.tui_completions("zzzzz");
        assert!(
            comps.is_empty(),
            "non-matching prefix must return empty; got: {comps:?}"
        );
    }

    #[test]
    fn hint_for_unknown_path_returns_none() {
        let cat = CommandCatalogue::get();
        let hint = cat.tui_hint_for(&["notacommand"]);
        assert!(hint.is_none(), "unknown path must return None");
    }

    #[test]
    fn hint_for_unknown_nested_path_returns_none() {
        let cat = CommandCatalogue::get();
        let hint = cat.tui_hint_for(&["exec", "notasubcommand"]);
        assert!(hint.is_none(), "unknown nested path must return None");
    }
}
