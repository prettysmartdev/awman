//! Command input area — wraps `TextEdit` for the command box.

use crate::command::dispatch::parsed_input::ParsedCommandBoxInput;
use crate::command::dispatch::Dispatch;
use crate::command::error::CommandError;
use crate::frontend::tui::command_frontend::TuiCommandFrontend;

/// Parse the command box input text into a `ParsedCommandBoxInput`.
/// Returns `Ok(parsed)` on success, or `Err` with the error (which may
/// include a "did you mean" suggestion).
pub fn parse_input(text: &str) -> Result<ParsedCommandBoxInput, CommandError> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err(CommandError::CommandBoxParse("empty input".into()));
    }
    Dispatch::<TuiCommandFrontend>::parse_command_box_input(trimmed)
}

/// Given a `CommandError` from parsing, format a user-visible error string.
pub fn format_parse_error(err: &CommandError) -> String {
    match err {
        CommandError::UnknownCommand { path, suggestions } => match suggestions.first() {
            Some(nearest) => format!("did you mean: {nearest}?"),
            None => format!("unknown command: {}", path.join(" ")),
        },
        // `flag` arrives as the catalogue's own name for a long flag
        // (`yolo`) and as the token the user typed for a short one (`-c`) or
        // a short-flag cluster (`-ab`). Prefixing everything with `--` turned
        // the latter into `---ab` (WI 0114 F-26, which routed the command box
        // through the shared parser and so started producing those).
        CommandError::UnknownFlag { flag, .. } => {
            if flag.starts_with('-') {
                format!("unknown flag: {flag}")
            } else {
                format!("unknown flag: --{flag}")
            }
        }
        CommandError::CommandBoxParse(msg) => msg.clone(),
        other => format!("{other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::error::CommandError;

    // ── parse_input ───────────────────────────────────────────────────────────

    #[test]
    fn parse_input_empty_string_returns_error() {
        let err = parse_input("").unwrap_err();
        assert!(
            matches!(err, CommandError::CommandBoxParse(_)),
            "empty input must yield CommandBoxParse error"
        );
    }

    #[test]
    fn parse_input_whitespace_only_returns_error() {
        let err = parse_input("   ").unwrap_err();
        assert!(matches!(err, CommandError::CommandBoxParse(_)));
    }

    #[test]
    fn parse_input_valid_command_returns_ok() {
        let parsed = parse_input("status").unwrap();
        assert_eq!(parsed.path, vec!["status"]);
    }

    #[test]
    fn parse_input_valid_nested_command_returns_ok() {
        let parsed = parse_input("exec workflow my.toml").unwrap();
        assert_eq!(parsed.path, vec!["exec", "workflow"]);
    }

    #[test]
    fn parse_input_unknown_command_returns_error() {
        let err = parse_input("doesnotexist").unwrap_err();
        assert!(matches!(err, CommandError::UnknownCommand { .. }));
    }

    #[test]
    fn parse_input_unknown_flag_returns_error() {
        let err = parse_input("status --bogus-flag").unwrap_err();
        assert!(matches!(err, CommandError::UnknownFlag { .. }));
    }

    // ── format_parse_error ────────────────────────────────────────────────────

    /// Rendering only: *which* commands are near misses is the catalogue's
    /// answer, asserted in `catalogue::tests` (WI 0114 F-43).
    #[test]
    fn format_parse_error_renders_the_first_suggestion_it_is_given() {
        let err = CommandError::UnknownCommand {
            path: vec!["cht".to_string()],
            suggestions: vec!["chat".to_string(), "clean".to_string()],
        };
        let msg = format_parse_error(&err);
        assert!(
            msg.contains("did you mean") && msg.contains("chat"),
            "a suggestion must render as a 'did you mean', got: {msg}"
        );
        assert!(
            !msg.contains("clean"),
            "the command box has one line; only the nearest miss fits: {msg}"
        );
    }

    #[test]
    fn format_parse_error_without_suggestions_shows_unknown_command() {
        let err = CommandError::UnknownCommand {
            path: vec!["zzzzzzzzz".to_string()],
            suggestions: Vec::new(),
        };
        let msg = format_parse_error(&err);
        assert!(
            msg.contains("unknown command") && msg.contains("zzzzzzzzz"),
            "no-match must name what was typed, got: {msg}"
        );
    }

    #[test]
    fn format_parse_error_unknown_flag_shows_flag_name() {
        let err = CommandError::UnknownFlag {
            command: vec!["status".to_string()],
            flag: "bogus".to_string(),
        };
        let msg = format_parse_error(&err);
        assert!(
            msg.contains("bogus"),
            "must mention the unknown flag, got: {msg}"
        );
    }

    /// The hint reads as the user typed it. A long flag gains the `--` the
    /// catalogue name omits; a short flag or a cluster already has its dash
    /// and must not gain two more (WI 0114 F-26).
    #[test]
    fn format_parse_error_renders_a_flag_the_way_it_was_typed() {
        let hint = |flag: &str| {
            format_parse_error(&CommandError::UnknownFlag {
                command: vec!["ready".to_string()],
                flag: flag.to_string(),
            })
        };
        assert_eq!(hint("bogus"), "unknown flag: --bogus");
        assert_eq!(hint("-z"), "unknown flag: -z");
        assert_eq!(hint("-ab"), "unknown flag: -ab");
    }

    /// End to end from the box's own entry point: the cluster the box used to
    /// refuse with its own message now produces the shared parser's
    /// `UnknownFlag`, and the hint still names exactly what was typed.
    #[test]
    fn a_short_flag_cluster_hint_names_the_cluster() {
        let err = parse_input("ready -ab").unwrap_err();
        assert!(
            matches!(err, CommandError::UnknownFlag { ref flag, .. } if flag == "-ab"),
            "got {err:?}"
        );
        assert_eq!(format_parse_error(&err), "unknown flag: -ab");
    }

    #[test]
    fn format_parse_error_command_box_parse_passes_through_message() {
        let err = CommandError::CommandBoxParse("tokenize failed: bad input".to_string());
        let msg = format_parse_error(&err);
        assert!(
            msg.contains("tokenize failed"),
            "must include original message, got: {msg}"
        );
    }
}
