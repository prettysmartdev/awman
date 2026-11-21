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
