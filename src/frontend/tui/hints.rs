//! TUI hint text — all hints pulled from the catalogue, never hardcoded.

use crate::command::dispatch::catalogue::CommandCatalogue;

/// Build a trailing hint for the current command-box input.
///
/// Returns only the *flags* portion of the hint — the command path the user
/// already typed is not repeated.  Returns `None` when there are no flags to
/// show (or the input is unknown).
pub fn hint_for_input(input: &str) -> Option<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    let parts: Vec<&str> = trimmed.split_whitespace().collect();
    let cat = CommandCatalogue::get();
    let hint = cat.tui_hint_for(&parts)?;
    if hint.flags.is_empty() {
        None
    } else {
        Some(hint.flags.join(" "))
    }
}

/// Build the suggestion row string from completions.
pub fn format_suggestion_row(suggestions: &[String]) -> String {
    if suggestions.is_empty() {
        return String::new();
    }
    format!("> {}", suggestions.join(" · "))
}
