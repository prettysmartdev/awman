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
            if !flag_visible_to_tui(FrontendVisibility::All) || sub.internal {
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
