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

#[cfg(test)]
mod tests {
    use super::*;

    // ── format_suggestion_row ─────────────────────────────────────────────────

    #[test]
    fn format_suggestion_row_empty_returns_empty_string() {
        assert_eq!(format_suggestion_row(&[]), "");
    }

    #[test]
    fn format_suggestion_row_single_suggestion() {
        let result = format_suggestion_row(&["chat".to_string()]);
        assert_eq!(result, "> chat");
    }

    #[test]
    fn format_suggestion_row_multiple_suggestions_separated_by_middots() {
        let result =
            format_suggestion_row(&["chat".to_string(), "exec".to_string(), "status".to_string()]);
        assert_eq!(result, "> chat · exec · status");
    }

    // ── hint_for_input ────────────────────────────────────────────────────────

    #[test]
    fn hint_for_input_empty_returns_none() {
        assert!(hint_for_input("").is_none());
    }

    #[test]
    fn hint_for_input_whitespace_returns_none() {
        assert!(hint_for_input("   ").is_none());
    }

    #[test]
    fn hint_for_input_known_command_with_flags_returns_some() {
        // chat has flags (e.g. --yolo), so a hint should be returned
        let hint = hint_for_input("chat");
        assert!(
            hint.is_some(),
            "known command 'chat' must yield a hint when it has flags"
        );
    }

    #[test]
    fn hint_for_input_unknown_command_returns_none() {
        assert!(hint_for_input("notacommand").is_none());
    }

    #[test]
    fn hint_for_input_does_not_repeat_command_name() {
        // The hint should only show flags, not the typed command path.
        let hint = hint_for_input("chat").unwrap();
        assert!(
            !hint.starts_with("chat"),
            "hint must not repeat the command name; got: {hint}"
        );
        assert!(
            hint.contains("--yolo"),
            "hint for 'chat' must include --yolo flag"
        );
    }
}
