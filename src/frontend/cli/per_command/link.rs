//! Interactive input for linking. No flag or pipe can approve a browser.
use crate::command::error::CommandError;
use std::io::{IsTerminal, Read};
pub fn confirm() -> Result<bool, CommandError> {
    if !std::io::stdin().is_terminal() {
        return Err(crate::command::commands::link::invalid(
            "run this command in an interactive terminal",
        ));
    }
    Ok(confirm_from_reader(&mut std::io::stdin().lock()))
}

fn confirm_from_reader(reader: &mut impl std::io::BufRead) -> bool {
    let mut line = String::new();
    // EOF and failed reads deny, exactly like every response except y/yes.
    if !matches!(reader.read_line(&mut line), Ok(n) if n > 0) {
        return false;
    }
    matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

pub fn read_token() -> Result<String, CommandError> {
    let mut token = String::new();
    std::io::stdin()
        .read_to_string(&mut token)
        .map_err(|e| CommandError::Other(e.to_string()))?;
    Ok(token.trim().to_string())
}
