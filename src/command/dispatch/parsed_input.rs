//! Parsed TUI command-box input.
//!
//! The TUI submits a raw user string; Dispatch tokenizes it against the
//! catalogue and returns a typed [`ParsedCommandBoxInput`] the TUI feeds back
//! through a `TuiCommandFrontend`.

use crate::command::dispatch::catalogue::{CommandCatalogue, CommandSpec};
use crate::command::dispatch::projections::raw_args::ParsedArgs;
use crate::command::error::CommandError;

/// Result of `parse_command_box_input`. `path` is the resolved canonical
/// command path the box needs for its own labels and admission; `args` is the
/// typed parse the TUI hands back to Dispatch via a `CommandFrontend`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedCommandBoxInput {
    pub path: Vec<String>,
    pub args: ParsedArgs,
}

/// Tokenize `raw` against the catalogue.
///
/// Three steps, and only the middle one is this module's own: split the string
/// into tokens, resolve the leading tokens to a command path, then hand the
/// rest to the catalogue's raw-argument parser — the same parser the API
/// frontend uses. WI 0114 F-26 deleted the second token loop that used to live
/// here; the TUI now rejects exactly what the API rejects, at the same point.
pub fn parse(
    raw: &str,
    catalogue: &CommandCatalogue,
) -> Result<ParsedCommandBoxInput, CommandError> {
    let tokens = shell_words::split(raw)
        .map_err(|e| CommandError::CommandBoxParse(format!("tokenize failed: {e}")))?;
    if tokens.is_empty() {
        return Err(CommandError::CommandBoxParse("empty input".into()));
    }

    // Walk the catalogue resolving subcommands. The command box submits one
    // string, so unlike argv this has to find where the path ends and the
    // arguments begin; everything after that point is the shared parser's.
    let mut current: &CommandSpec = catalogue.root();
    let mut path: Vec<String> = Vec::new();
    let mut idx = 0;
    while idx < tokens.len() {
        let tok = &tokens[idx];
        if tok.starts_with('-') || tok == "--" {
            break;
        }
        match current.find_subcommand(tok) {
            Some(sub) => {
                path.push(sub.name.to_string());
                current = sub;
                idx += 1;
            }
            None => break,
        }
    }
    if path.is_empty() {
        return Err(CommandError::unknown_command(&[tokens[0].as_str()]));
    }

    let path_refs: Vec<&str> = path.iter().map(String::as_str).collect();
    let args = catalogue.parse_raw_args(&path_refs, &tokens[idx..])?;
    Ok(ParsedCommandBoxInput { path, args })
}
