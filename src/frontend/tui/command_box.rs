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
        CommandError::UnknownCommand { path } => {
            let name = path.join(" ");
            let suggestion = find_suggestion(&name);
            match suggestion {
                Some(s) => format!("did you mean: {s}?"),
                None => format!("unknown command: {name}"),
            }
        }
        CommandError::UnknownFlag { flag, .. } => {
            format!("unknown flag: --{flag}")
        }
        CommandError::CommandBoxParse(msg) => msg.clone(),
        other => format!("{other}"),
    }
}

/// Levenshtein-based suggestion for unknown commands (threshold ≤4).
fn find_suggestion(input: &str) -> Option<String> {
    use crate::command::dispatch::catalogue::CommandCatalogue;
    let cat = CommandCatalogue::get();
    let names: Vec<&str> = cat.root().subcommands.iter().map(|s| s.name).collect();

    let mut best: Option<(&str, usize)> = None;
    for name in &names {
        let dist = strsim::levenshtein(input, name);
        if dist <= 4 && (best.is_none() || dist < best.unwrap().1) {
            best = Some((name, dist));
        }
    }
    best.map(|(name, _)| name.to_string())
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

    #[test]
    fn format_parse_error_unknown_command_close_match_shows_did_you_mean() {
        // "cht" is distance 1 from "chat"
        let err = CommandError::UnknownCommand {
            path: vec!["cht".to_string()],
        };
        let msg = format_parse_error(&err);
        assert!(
            msg.contains("did you mean"),
            "close match must show 'did you mean', got: {msg}"
        );
        assert!(
            msg.contains("chat"),
            "suggestion must include 'chat', got: {msg}"
        );
    }

    #[test]
    fn format_parse_error_unknown_command_no_match_shows_unknown() {
        // "zzzzzzzzz" is far from every command
        let err = CommandError::UnknownCommand {
            path: vec!["zzzzzzzzz".to_string()],
        };
        let msg = format_parse_error(&err);
        assert!(
            msg.contains("unknown command"),
            "no-match must show 'unknown command', got: {msg}"
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
