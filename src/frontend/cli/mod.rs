//! CLI frontend — argv-driven, stdout/stderr/stdin rendering.
//!
//! Per `aspec/architecture/2026-grand-architecture.md` and work item
//! `0069-grand-architecture-layer-3-frontends-and-binary.md` §1.
//!
//! The entry point [`run`] is invoked by `main.rs` whenever clap parsing
//! succeeds with a subcommand. It builds a [`CliFrontend`] over the parsed
//! `clap::ArgMatches`, hands it to [`Dispatch`], and renders the resulting
//! [`CommandOutcome`] (or [`CommandError`]) to stdout/stderr.
//!
//! The CLI frontend contains NO business logic: every behavioral decision
//! lives in Layer 2.

use std::process::ExitCode;
use std::sync::Arc;

use clap::ArgMatches;
use tokio::sync::RwLock;

use crate::command::dispatch::{Dispatch, Engines};
use crate::command::error::CommandError;
use crate::command::CommandOutcome;
use crate::data::session::Session;

mod command_frontend;
mod output;
mod per_command;
mod user_message;

pub use command_frontend::{command_path_from_matches, CliFrontend};

/// Bundle of state that `main.rs` constructs once at startup and hands to
/// either [`run`] (CLI path) or [`crate::frontend::tui::run`] (TUI path).
///
/// The same engines and session are reused regardless of which frontend
/// runs; only the `Dispatch` wrapper differs.
pub struct RuntimeContext {
    pub session: Arc<RwLock<Session>>,
    pub engines: Engines,
}

impl RuntimeContext {
    pub fn new(session: Session, engines: Engines) -> Self {
        Self {
            session: Arc::new(RwLock::new(session)),
            engines,
        }
    }
}

/// Entry point for the CLI frontend.
///
/// Returns a process [`ExitCode`] reflecting the outcome of the dispatched
/// command.
pub async fn run(matches: ArgMatches, ctx: RuntimeContext) -> ExitCode {
    let path = command_path_from_matches(&matches);
    if path.is_empty() {
        // `main.rs` should already have routed bare invocations to the TUI.
        eprintln!("amux: no subcommand supplied; run `amux --help` for usage.");
        return ExitCode::from(2);
    }
    let path_strs: Vec<&str> = path.iter().map(|s| s.as_str()).collect();
    let frontend = CliFrontend::new(matches);
    let dispatch = Dispatch::new(frontend, ctx.session, ctx.engines);
    match dispatch.run_command(&path_strs).await {
        Ok(outcome) => render_outcome(&outcome),
        Err(err) => render_error(&err),
    }
}

/// Format a successful [`CommandOutcome`] to user-facing stdout text.
/// Returns `None` when the outcome carries nothing additional to print
/// beyond what the engine already streamed via `report_*` to stderr.
///
/// The previous implementation fell back to `serde_json::to_string_pretty`
/// for every non-Empty variant, which surfaced raw JSON as the primary
/// user output for `chat`, `status`, `config`, etc. Per-variant rendering
/// now lives in [`per_command::render`].
pub(crate) fn format_outcome(outcome: &CommandOutcome) -> Option<String> {
    per_command::render::render(outcome)
}

/// Format a [`CommandError`] to the user-visible stderr string.
pub(crate) fn format_error(err: &CommandError) -> String {
    format!("amux: {err}")
}

/// Render a successful [`CommandOutcome`] to stdout and return the
/// process exit code.
fn render_outcome(outcome: &CommandOutcome) -> ExitCode {
    if let Some(s) = format_outcome(outcome) {
        println!("{s}");
    }
    ExitCode::from(0)
}

/// Render a [`CommandError`] to stderr and return the corresponding
/// process exit code.
fn render_error(err: &CommandError) -> ExitCode {
    eprintln!("{}", format_error(err));
    ExitCode::from(error_exit_code(err))
}

/// Pure mapping from a [`CommandError`] to a process exit code `u8`.
/// Factored out so the mapping is unit-testable without capturing stderr.
pub(crate) fn error_exit_code(err: &CommandError) -> u8 {
    match err {
        CommandError::Aborted => 130,
        CommandError::UnknownCommand { .. }
        | CommandError::UnknownFlag { .. }
        | CommandError::MissingRequiredFlag { .. }
        | CommandError::MissingRequiredArgument { .. }
        | CommandError::MutuallyExclusive { .. }
        | CommandError::InvalidFlagValue { .. }
        | CommandError::InvalidArgumentValue { .. }
        | CommandError::CommandBoxParse(_) => 2,
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::dispatch::catalogue::CommandCatalogue;
    use crate::command::error::CommandError;

    // ─── error_exit_code — data-table test over every CommandError variant ─────

    fn path(segs: &[&str]) -> Vec<String> {
        segs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn error_exit_code_aborted_is_130() {
        assert_eq!(error_exit_code(&CommandError::Aborted), 130u8);
    }

    #[test]
    fn error_exit_code_usage_errors_are_2() {
        let usage_errors: &[CommandError] = &[
            CommandError::UnknownCommand { path: path(&["bogus"]) },
            CommandError::UnknownFlag { command: path(&["init"]), flag: "bad".into() },
            CommandError::MissingRequiredFlag { command: path(&["init"]), flag: "agent".into() },
            CommandError::MissingRequiredArgument { command: path(&["implement"]), argument: "work_item".into() },
            CommandError::MutuallyExclusive {
                command: path(&["chat"]),
                a: "yolo".into(),
                b: "plan".into(),
            },
            CommandError::InvalidFlagValue {
                command: path(&["init"]),
                flag: "agent".into(),
                reason: "not a valid agent".into(),
            },
            CommandError::InvalidArgumentValue {
                command: path(&["implement"]),
                argument: "work_item".into(),
                reason: "must be 4 digits".into(),
            },
            CommandError::CommandBoxParse("unrecognized".into()),
        ];
        for err in usage_errors {
            assert_eq!(
                error_exit_code(err),
                2u8,
                "expected exit code 2 for {err:?}"
            );
        }
    }

    #[test]
    fn error_exit_code_other_errors_are_1() {
        let other_errors: &[CommandError] = &[
            CommandError::NotImplemented("placeholder"),
            CommandError::Other("something went wrong".into()),
            CommandError::RemoteTimeout,
            CommandError::MissingRemoteAddress,
            CommandError::MissingApiKey,
            CommandError::HeadlessAlreadyRunning { pid: 42 },
        ];
        for err in other_errors {
            assert_eq!(
                error_exit_code(err),
                1u8,
                "expected exit code 1 for {err:?}"
            );
        }
    }

    // ─── command_path_from_matches – frontend selection logic ─────────────────

    #[test]
    fn subcommand_present_routes_to_cli() {
        let cmd = CommandCatalogue::get().build_clap_command();
        let m = cmd.try_get_matches_from(["amux", "status"]).unwrap();
        // main.rs uses `matches.subcommand_name().is_some()` to pick CLI.
        assert!(m.subcommand_name().is_some());
    }

    #[test]
    fn bare_invocation_routes_to_tui() {
        let cmd = CommandCatalogue::get().build_clap_command();
        let m = cmd.try_get_matches_from(["amux"]).unwrap();
        // main.rs uses `matches.subcommand_name().is_none()` to pick TUI.
        assert!(m.subcommand_name().is_none());
    }

    #[test]
    fn render_outcome_empty_is_success() {
        let outcome = crate::command::CommandOutcome::Empty;
        let _code = render_outcome(&outcome);
    }

    // ─── format_outcome — snapshot-style per-variant assertions ──────────────

    #[test]
    fn format_outcome_empty_returns_none() {
        assert!(format_outcome(&crate::command::CommandOutcome::Empty).is_none());
    }

    #[test]
    fn format_outcome_status_renders_dashboard_not_json() {
        use crate::command::commands::status::StatusOutcome;
        use crate::command::CommandOutcome;
        let outcome = CommandOutcome::Status(StatusOutcome {
            containers: vec![],
            watched: false,
        });
        let s = format_outcome(&outcome).expect("status must render text");
        assert!(s.contains("AMUX STATUS DASHBOARD"));
        assert!(!s.contains('{'), "status must not be rendered as JSON");
    }

    #[test]
    fn format_outcome_chat_clean_exit_returns_none() {
        use crate::command::commands::chat::ChatOutcome;
        use crate::command::CommandOutcome;
        let outcome = CommandOutcome::Chat(ChatOutcome {
            agent: Some("claude".into()),
            exit_code: Some(0),
        });
        assert!(format_outcome(&outcome).is_none());
    }

    // ─── format_error — per-variant rendering assertions ─────────────────────

    #[test]
    fn format_error_prefix_is_always_amux() {
        // Every error message must start with "amux: " for consistent UX.
        let errors: &[CommandError] = &[
            CommandError::Aborted,
            CommandError::NotImplemented("x"),
            CommandError::Other("boom".into()),
            CommandError::UnknownCommand { path: vec!["bad".into()] },
        ];
        for err in errors {
            let s = format_error(err);
            assert!(
                s.starts_with("amux: "),
                "error must start with 'amux: ', got: {s:?}"
            );
        }
    }

    #[test]
    fn format_error_aborted_message() {
        let s = format_error(&CommandError::Aborted);
        assert!(s.contains("aborted") || s.contains("Aborted") || s.contains("130"),
            "Aborted error should mention abort: {s:?}");
    }

    #[test]
    fn format_error_unknown_command_includes_path() {
        let err = CommandError::UnknownCommand { path: vec!["foo".into(), "bar".into()] };
        let s = format_error(&err);
        assert!(s.contains("foo") || s.contains("bar"),
            "UnknownCommand error should include the path: {s:?}");
    }

    #[test]
    fn format_error_not_implemented_includes_message() {
        let err = CommandError::NotImplemented("headless");
        let s = format_error(&err);
        assert!(s.contains("headless"), "NotImplemented error must include the message: {s:?}");
    }

    // ─── TTY detection ────────────────────────────────────────────────────────
    // These tests exercise the output.rs TTY-detection functions to confirm
    // they don't panic and return consistent bool values. In CI, both stdin
    // and stderr are non-TTY, so both return false. The behavior is documented
    // rather than asserted to avoid fragility when running locally.

    #[test]
    fn tty_detection_does_not_panic() {
        let _stderr = crate::frontend::cli::output::stderr_is_tty();
        let _stdin = crate::frontend::cli::output::stdin_is_tty();
        // No assertion — just verifying the calls don't panic.
    }

    #[test]
    fn stderr_and_stdin_tty_return_consistent_bools() {
        // Calling twice must return the same value (no side effects, no flicker).
        let a = crate::frontend::cli::output::stderr_is_tty();
        let b = crate::frontend::cli::output::stderr_is_tty();
        assert_eq!(a, b, "stderr_is_tty must be idempotent");

        let c = crate::frontend::cli::output::stdin_is_tty();
        let d = crate::frontend::cli::output::stdin_is_tty();
        assert_eq!(c, d, "stdin_is_tty must be idempotent");
    }
}
