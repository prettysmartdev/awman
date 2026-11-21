//! Flags and commands that are declared but not wired yet (WI 0070).
//!
//! WI 0070 lands in several steps that share one public surface. The
//! catalogue declares that whole surface up front so every projection (clap,
//! raw args, TUI hints, the generated reference) agrees on names and syntax
//! from the first step. A declared option whose behavior is not implemented
//! must not be accepted and then ignored: a caller who asked for
//! `--read-only-workspace` and got a writable workspace has been lied to.
//!
//! So each unwired option has a row here, and the shared parser and
//! [`ResolvedFlags`](crate::command::dispatch::ResolvedFlags) refuse a
//! supplied pending flag — and [`build::pending`] a pending command — with
//! [`CommandError::OptionPending`] (usage, exit 2 / HTTP 400) before any
//! image preparation, profile creation or agent launch.
//!
//! The step that implements an option deletes its row (and, for a command,
//! replaces `build::pending` with the real builder) in the same change. When
//! the table is empty this module and the error variant can be removed.
//!
//! [`build::pending`]: crate::command::dispatch::build::pending

use super::CommandCatalogue;
use crate::command::error::CommandError;

/// One declared-but-unwired flag or command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PendingOption {
    /// The canonical command path the option belongs to.
    pub path: &'static [&'static str],
    /// The flag's long name, or `None` when the command itself is pending.
    pub flag: Option<&'static str>,
    /// The workflow step that wires it and removes this row.
    pub owner: &'static str,
}

#[allow(dead_code)] // Rows are added ahead of an unwired option and removed with it.
const fn flag(
    path: &'static [&'static str],
    flag: &'static str,
    owner: &'static str,
) -> PendingOption {
    PendingOption {
        path,
        flag: Some(flag),
        owner,
    }
}

#[cfg(test)]
const CHAT: &[&str] = &["chat"];
#[cfg(test)]
const EXEC_PROMPT: &[&str] = &["exec", "prompt"];
#[cfg(test)]
const EXEC_WORKFLOW: &[&str] = &["exec", "workflow"];

/// Every option awaiting its implementing step. One row per line so parallel
/// steps delete disjoint lines.
pub const PENDING_OPTIONS: &[PendingOption] = &[];

impl CommandCatalogue {
    /// Whether `flag` on the canonical command `path` is declared but not
    /// wired yet.
    pub fn is_pending_flag(&self, path: &[&str], flag: &str) -> bool {
        PENDING_OPTIONS
            .iter()
            .any(|pending| pending.path == path && pending.flag == Some(flag))
    }

    /// Whether the command at canonical `path` is itself declared but not
    /// wired yet.
    pub fn is_pending_command(&self, path: &[&str]) -> bool {
        PENDING_OPTIONS
            .iter()
            .any(|pending| pending.path == path && pending.flag.is_none())
    }

    /// Refuse the first supplied flag that is still pending on `path`.
    ///
    /// `supplied` is the flags the caller actually asked for (an explicit
    /// `--flag=false` asks for nothing). Called by the shared raw-args parser
    /// and by flag resolution, so the CLI, the TUI command box, API `args`
    /// and relayed argv all refuse the same input the same way.
    pub fn refuse_pending_flags<'a>(
        &self,
        path: &[&str],
        supplied: impl IntoIterator<Item = &'a str>,
    ) -> Result<(), CommandError> {
        for flag in supplied {
            if self.is_pending_flag(path, flag) {
                return Err(CommandError::option_pending(path, Some(flag)));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::dispatch::catalogue::FrontendKind;

    fn args(tokens: &[&str]) -> Vec<String> {
        tokens.iter().map(|token| (*token).to_string()).collect()
    }

    #[test]
    fn every_pending_row_names_a_declared_flag_or_leaf_exactly_once() {
        let catalogue = CommandCatalogue::get();
        let mut seen = std::collections::BTreeSet::new();
        for pending in PENDING_OPTIONS {
            assert!(seen.insert((pending.path, pending.flag)), "{pending:?}");
            let spec = catalogue
                .lookup(pending.path)
                .unwrap_or_else(|| panic!("{pending:?}: unknown command"));
            assert_eq!(
                catalogue.canonical_path(pending.path),
                pending.path,
                "{pending:?}: path must be canonical"
            );
            match pending.flag {
                Some(flag) => assert!(
                    spec.find_flag(flag).is_some(),
                    "{pending:?}: flag is not declared"
                ),
                None => assert!(
                    spec.subcommands.is_empty(),
                    "{pending:?}: a pending command must be a leaf"
                ),
            }
            assert!(!pending.owner.is_empty());
        }
    }

    /// The argv that supplies one pending flag, with a plausible value.
    fn supplying(flag: &str) -> Vec<String> {
        match flag {
            "settings-profile" => args(&["--settings-profile", "review"]),
            "agent-session" => args(&["--agent-session", "thread-1"]),
            other => vec![format!("--{other}")],
        }
    }

    #[test]
    fn a_supplied_pending_flag_is_a_usage_error_in_the_shared_parser() {
        let catalogue = CommandCatalogue::get();
        for pending in PENDING_OPTIONS {
            let Some(flag) = pending.flag else { continue };
            for frontend in [
                FrontendKind::Cli,
                FrontendKind::Tui,
                FrontendKind::WORKER_LOCAL,
            ] {
                let error = catalogue
                    .parse_raw_args_with_profile(pending.path, &supplying(flag), frontend)
                    .unwrap_err();
                assert!(
                    matches!(
                        &error,
                        CommandError::OptionPending { command, option }
                            if command == pending.path && option.as_deref() == Some(flag)
                    ),
                    "{pending:?} via {frontend:?}: {error:?}"
                );
                assert_eq!(
                    (error.exit_code(), error.http_status(), error.code()),
                    (2, 400, "invalid_request"),
                    "{pending:?}"
                );
                assert!(error.to_string().contains(&format!("--{flag}")));
                assert!(error.clone_parse_stage().is_some());
            }
        }
    }

    #[test]
    fn an_absent_or_false_pending_flag_changes_nothing() {
        let catalogue = CommandCatalogue::get();
        // Omitted: every command still parses as before.
        for path in [CHAT, EXEC_PROMPT, EXEC_WORKFLOW, &["ready"][..]] {
            catalogue
                .parse_raw_args(path, &args(&[]))
                .unwrap_or_else(|error| panic!("{path:?}: {error:?}"));
        }
        // An explicit `--flag=false` asks for nothing.
        catalogue
            .parse_raw_args(&["ready"], &args(&["--prepare-only=false"]))
            .unwrap();
    }

    #[test]
    fn a_pending_flag_spelled_inside_prompt_text_is_prompt_text() {
        let catalogue = CommandCatalogue::get();
        let parsed = catalogue
            .parse_raw_args(EXEC_PROMPT, &args(&["--", "--automation", "explained"]))
            .unwrap();
        assert_eq!(
            parsed.argument("prompt").as_deref(),
            Some("--automation explained")
        );
        assert!(!parsed.has_flag("automation"));
    }

    #[test]
    fn pending_lookups_are_exact() {
        let catalogue = CommandCatalogue::get();
        // automation-admission wired `--automation` (WI 0070): nothing pends.
        assert!(!catalogue.is_pending_flag(CHAT, "automation"));
        assert!(!catalogue.is_pending_flag(EXEC_PROMPT, "automation"));
        assert!(PENDING_OPTIONS.is_empty());
        assert!(!catalogue.is_pending_flag(CHAT, "plan"));
        // `exec prompt` and `chat` implement `--read-only-workspace`.
        assert!(!catalogue.is_pending_flag(EXEC_PROMPT, "read-only-workspace"));
        assert!(!catalogue.is_pending_flag(CHAT, "read-only-workspace"));
        assert!(!catalogue.is_pending_flag(EXEC_WORKFLOW, "automation"));
        assert!(!catalogue.is_pending_command(&["agents", "profile", "list"]));
        assert!(!catalogue.is_pending_command(&["agents", "list"]));
        assert!(!catalogue.is_pending_command(&["agents", "profile"]));
    }
}
