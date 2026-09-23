//! Parsed TUI command-box input.
//!
//! The TUI submits a raw user string; Dispatch tokenizes it against the
//! catalogue and returns a typed [`ParsedCommandBoxInput`] the TUI feeds back
//! through a `TuiCommandFrontend`.

use std::collections::BTreeMap;

use crate::command::dispatch::catalogue::{CommandCatalogue, CommandSpec};
use crate::command::error::CommandError;

/// Result of `parse_command_box_input`. `path` is the resolved canonical
/// command path; `flags` and `arguments` are typed string maps the TUI hands
/// back to Dispatch via a `CommandFrontend`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedCommandBoxInput {
    pub path: Vec<String>,
    pub flags: BTreeMap<String, FlagValue>,
    pub arguments: BTreeMap<String, ArgValue>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FlagValue {
    Bool(bool),
    String(String),
    Strings(Vec<String>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArgValue {
    Single(String),
    Multi(Vec<String>),
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
    let parsed = catalogue.parse_raw_args(&path_refs, &tokens[idx..])?;
    Ok(parsed.into_command_box_input(current, path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_exec_workflow_with_path_and_yolo() {
        let cat = CommandCatalogue::get();
        let parsed = parse("exec workflow my-workflow.toml --yolo", cat).unwrap();
        assert_eq!(parsed.path, vec!["exec", "workflow"]);
        assert!(matches!(
            parsed.flags.get("yolo"),
            Some(FlagValue::Bool(true))
        ));
        assert!(matches!(
            parsed.arguments.get("workflow"),
            Some(ArgValue::Single(s)) if s == "my-workflow.toml"
        ));
    }

    #[test]
    fn parse_remote_exec_workflow_with_argument() {
        let cat = CommandCatalogue::get();
        let parsed = parse("remote exec workflow my-workflow.toml", cat).unwrap();
        assert_eq!(parsed.path, vec!["remote", "exec", "workflow"]);
        assert!(matches!(
            parsed.arguments.get("workflow"),
            Some(ArgValue::Single(s)) if s == "my-workflow.toml"
        ));
    }

    #[test]
    fn parse_remote_exec_prompt_with_argument() {
        let cat = CommandCatalogue::get();
        let parsed = parse(r#"remote exec prompt "hello world""#, cat).unwrap();
        assert_eq!(parsed.path, vec!["remote", "exec", "prompt"]);
        // `prompt` is a greedy trailing positional (TrailingVarArgs), so a
        // single quoted token collects into a one-element Multi.
        assert!(matches!(
            parsed.arguments.get("prompt"),
            Some(ArgValue::Multi(v)) if v == &["hello world".to_string()]
        ));
    }

    #[test]
    fn parse_unknown_command_errors() {
        let cat = CommandCatalogue::get();
        let err = parse("not-a-command", cat).unwrap_err();
        assert!(matches!(err, CommandError::UnknownCommand { .. }));
    }

    #[test]
    fn parse_unknown_flag_errors() {
        let cat = CommandCatalogue::get();
        let err = parse("status --bogus", cat).unwrap_err();
        assert!(matches!(err, CommandError::UnknownFlag { .. }));
    }

    /// The TUI command box must reject a positional the command never declared,
    /// exactly as clap and the API projection do. `new skill` declares none, so
    /// a stray skill name next to `--pull` is a usage error (WI-0103).
    #[test]
    fn parse_rejects_positional_the_command_never_declared() {
        let cat = CommandCatalogue::get();
        let err = parse("new skill --pull owner/library accidental-name", cat)
            .expect_err("new skill takes no positional argument");
        assert!(
            matches!(
                &err,
                CommandError::UnexpectedArgument { argument, .. } if argument == "accidental-name"
            ),
            "expected UnexpectedArgument naming the stray name, got: {err:?}"
        );
    }

    #[test]
    fn parse_empty_string_returns_command_box_parse_error() {
        let cat = CommandCatalogue::get();
        let err = parse("", cat).unwrap_err();
        assert!(
            matches!(err, CommandError::CommandBoxParse(_)),
            "empty input must return CommandBoxParse, got: {err:?}"
        );
    }

    #[test]
    fn parse_quoted_string_argument_is_handled() {
        let cat = CommandCatalogue::get();
        let parsed = parse(r#"exec prompt "do something complex""#, cat).unwrap();
        assert_eq!(parsed.path, vec!["exec", "prompt"]);
        // `prompt` is a greedy trailing positional: a single quoted token
        // collects into a one-element Multi (the TUI frontend joins it back).
        match parsed.arguments.get("prompt") {
            Some(ArgValue::Multi(v)) => {
                assert_eq!(v, &["do something complex".to_string()]);
            }
            other => panic!("expected Multi prompt argument, got: {other:?}"),
        }
    }

    #[test]
    fn parse_short_flag_maps_to_long_name() {
        let cat = CommandCatalogue::get();
        let parsed = parse("ready -n", cat).unwrap();
        assert_eq!(parsed.path, vec!["ready"]);
        assert!(
            matches!(
                parsed.flags.get("non-interactive"),
                Some(FlagValue::Bool(true))
            ),
            "-n must map to non-interactive flag"
        );
    }
}

#[cfg(test)]
mod shared_parser_tests {
    //! WI 0114 F-26: the command box parses through the same
    //! `CommandCatalogue::parse_raw_args` the API frontend uses, so it rejects
    //! the same input. Each case below was accepted by the box's own parser and
    //! refused by the API's.

    use super::*;
    use crate::command::error::CommandError;

    /// A value outside a flag's declared enum is rejected at parse time, not
    /// carried as a string into the command.
    #[test]
    fn a_bad_enum_value_is_rejected_at_parse_time() {
        let err = parse("chat --launch-mode banana", CommandCatalogue::get()).unwrap_err();
        match err {
            CommandError::InvalidFlagValue { flag, reason, .. } => {
                assert_eq!(flag, "launch-mode");
                assert!(
                    reason.contains("banana") && reason.contains("stdio"),
                    "the reason must name the bad value and the allowed set: {reason}"
                );
            }
            other => panic!("expected InvalidFlagValue, got {other:?}"),
        }
    }

    /// The same error the API gives for the same input.
    #[test]
    fn a_bad_enum_value_gives_the_same_error_as_the_api() {
        let catalogue = CommandCatalogue::get();
        let from_box = parse("chat --launch-mode banana", catalogue).unwrap_err();
        let from_api = catalogue
            .parse_raw_args(&["chat"], &["--launch-mode".into(), "banana".into()])
            .unwrap_err();
        assert_eq!(from_box.to_string(), from_api.to_string());
    }

    /// A non-numeric value for a numeric flag is rejected at parse time.
    #[test]
    fn a_non_numeric_number_is_rejected_at_parse_time() {
        let err = parse("squad start --port abc", CommandCatalogue::get()).unwrap_err();
        match err {
            CommandError::InvalidFlagValue { flag, reason, .. } => {
                assert_eq!(flag, "port");
                assert!(
                    reason.contains("abc"),
                    "the reason must name the bad value: {reason}"
                );
            }
            other => panic!("expected InvalidFlagValue, got {other:?}"),
        }
    }

    #[test]
    fn a_non_numeric_number_gives_the_same_error_as_the_api() {
        let catalogue = CommandCatalogue::get();
        let from_box = parse("squad start --port abc", catalogue).unwrap_err();
        let from_api = catalogue
            .parse_raw_args(&["squad", "start"], &["--port".into(), "abc".into()])
            .unwrap_err();
        assert_eq!(from_box.to_string(), from_api.to_string());
    }

    /// A well-formed numeric value still reaches the frontend as the string
    /// the command box's own parser produced.
    #[test]
    fn a_valid_number_still_arrives_as_a_string() {
        let parsed = parse("squad start --port 9000", CommandCatalogue::get()).unwrap();
        assert!(
            matches!(parsed.flags.get("port"), Some(FlagValue::String(s)) if s == "9000"),
            "got {:?}",
            parsed.flags.get("port")
        );
    }

    /// A short-flag cluster is an unknown flag now, as it is on the CLI and in
    /// the API, rather than a command-box-specific refusal.
    #[test]
    fn a_short_flag_cluster_is_an_unknown_flag() {
        let err = parse("ready -ab", CommandCatalogue::get()).unwrap_err();
        match err {
            CommandError::UnknownFlag { flag, .. } => assert_eq!(flag, "-ab"),
            other => panic!("expected UnknownFlag, got {other:?}"),
        }
    }
}
