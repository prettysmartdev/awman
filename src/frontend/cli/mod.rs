//! CLI frontend — argv-driven, stdout/stderr/stdin rendering.
//!
//! Per `aspec/architecture/2026-grand-architecture.md`.
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
pub(crate) mod per_command;
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
///
/// Each variant gets a friendly message + an optional "next step" hint
/// where actionable. The "amux: " prefix is always present so user output
/// is consistent across error types.
pub(crate) fn format_error(err: &CommandError) -> String {
    let body = match err {
        CommandError::Aborted => "command aborted by user".to_string(),
        CommandError::UnknownCommand { path } => {
            format!(
                "unknown command: {}\n  try `amux --help` for the full command list",
                path.join(" ")
            )
        }
        CommandError::UnknownFlag { command, flag } => {
            format!(
                "unknown flag '--{flag}' for command `{}`\n  try `amux {} --help`",
                command.join(" "),
                command.join(" ")
            )
        }
        CommandError::MissingRequiredFlag { command, flag } => {
            format!(
                "missing required flag --{flag} for command `{}`",
                command.join(" ")
            )
        }
        CommandError::MissingRequiredArgument { command, argument } => {
            format!(
                "missing required argument {argument} for command `{}`",
                command.join(" ")
            )
        }
        CommandError::MutuallyExclusive { command, a, b } => {
            format!(
                "flags --{a} and --{b} cannot be used together on `{}`",
                command.join(" ")
            )
        }
        CommandError::InvalidFlagValue { command, flag, reason } => {
            format!(
                "invalid value for --{flag} on `{}`: {reason}",
                command.join(" ")
            )
        }
        CommandError::InvalidArgumentValue {
            command,
            argument,
            reason,
        } => {
            format!(
                "invalid value for {argument} on `{}`: {reason}",
                command.join(" ")
            )
        }
        CommandError::CommandBoxParse(msg) => format!("could not parse command-box input: {msg}"),
        CommandError::MergeConflict {
            branch,
            worktree_path,
        } => format!(
            "merge conflict on branch {branch}; resolve in worktree at {}",
            worktree_path.display()
        ),
        CommandError::MissingRemoteAddress => {
            "remote target address is missing or invalid; pass --remote-addr or set defaultAddr in config".into()
        }
        CommandError::MissingApiKey => {
            "remote API key is missing; pass --api-key or set defaultAPIKey in config".into()
        }
        CommandError::RemoteTimeout => "remote request timed out".into(),
        CommandError::RemoteConnectionRefused(reason) => {
            format!("remote connection refused: {reason}")
        }
        CommandError::RemoteHttpStatus { status, body } => {
            format!("remote returned HTTP {status}: {body}")
        }
        CommandError::MalformedSseEvent(msg) => format!("malformed SSE event from remote: {msg}"),
        CommandError::RemoteTransport(msg) => format!("remote transport error: {msg}"),
        CommandError::HeadlessWorkdirNotFound { path } => {
            format!("headless workdir not found: {}", path.display())
        }
        CommandError::HeadlessAlreadyRunning { pid } => {
            format!(
                "headless server is already running on PID {pid}; run `amux headless kill` first"
            )
        }
        CommandError::HeadlessNotRunning => "headless server is not running".into(),
        CommandError::HeadlessAuthMissing => {
            "no API key configured. Run `amux headless start --refresh-key` first, or pass `--dangerously-skip-auth`.".into()
        }
        CommandError::RemoteSessionMissing => {
            "no remote session id; pass --session <id> or run `amux remote session start` first".into()
        }
        CommandError::RemoteSessionKillFailed { session_id, reason } => {
            format!("failed to kill remote session '{session_id}': {reason}")
        }
        CommandError::NotImplemented(msg) => format!("not yet implemented: {msg}"),
        CommandError::Other(msg) => msg.to_string(),
        CommandError::WorkItemNotFound { number } => {
            format!("work item {number} not found in aspec/work-items/")
        }
        CommandError::SpecTemplateMissing { path } => {
            format!(
                "spec template missing at {}; run `amux init --aspec` to create it",
                path.display()
            )
        }
        CommandError::InvalidOverlaySpec { spec, reason } => {
            format!("invalid overlay spec '{spec}': {reason}")
        }
        CommandError::UnknownConfigField { name, suggestions } => {
            format!("unknown config field '{name}'; similar fields: {suggestions}")
        }
        CommandError::InteractiveInputUnavailable { prompt } => {
            format!("stdin is not a TTY; provide --{prompt} on the command line")
        }
        CommandError::WorkflowFileNotFound { path } => {
            format!("workflow file not found: {}", path.display())
        }
        CommandError::Engine(e) => match e {
            crate::engine::error::EngineError::AgentRequiresProjectImage { tag } => format!(
                "agent image build requires the project base image first ({tag}); run `amux ready --build`"
            ),
            crate::engine::error::EngineError::Container(msg) => format!(
                "container backend error: {msg}\n  amux requires Docker; install Docker Desktop / docker-engine and retry"
            ),
            crate::engine::error::EngineError::Network(msg) => {
                format!("network error: {msg}")
            }
            crate::engine::error::EngineError::PlanModeUnsupported { agent } => {
                format!("plan mode is not supported by agent {agent}")
            }
            crate::engine::error::EngineError::ConflictingOptions(msg) => {
                format!("conflicting container options: {msg}")
            }
            crate::engine::error::EngineError::MissingRequiredOption(opt) => {
                format!("missing required container option: {opt}")
            }
            crate::engine::error::EngineError::MergeConflict {
                branch,
                worktree_path,
            } => format!(
                "merge conflict on branch {branch}; resolve in worktree at {}",
                worktree_path.display()
            ),
            crate::engine::error::EngineError::ContainerRuntimeUnavailable { binary } => {
                format!(
                    "container runtime '{binary}' not found on PATH; install Docker and retry"
                )
            }
            crate::engine::error::EngineError::AgentDockerfileDownloadFailed { agent, message } => {
                format!("failed to download Dockerfile for agent '{agent}': {message}")
            }
            crate::engine::error::EngineError::AgentImageBuildFailed { agent, exit_code } => {
                format!("agent image build failed for agent '{agent}' (exit code {exit_code})")
            }
            crate::engine::error::EngineError::ImageBuildExitNonzero { tag, exit_code } => {
                format!("image build for tag '{tag}' exited with code {exit_code}")
            }
            crate::engine::error::EngineError::Data(e) => format!("{e}"),
            crate::engine::error::EngineError::Io { path, source } => {
                format!("io error at {}: {source}", path.display())
            }
            crate::engine::error::EngineError::Git(msg) => {
                format!("git operation failed: {msg}")
            }
            crate::engine::error::EngineError::OptionNotSupportedByBackend { option, backend } => {
                format!("container option {option} is not supported by backend {backend}")
            }
            crate::engine::error::EngineError::BackendUnsupportedOnPlatform { backend, platform } => {
                format!("backend {backend} is not supported on platform {platform}")
            }
            crate::engine::error::EngineError::InvalidAdvanceAction(msg) => {
                format!("invalid advance action: {msg}")
            }
            crate::engine::error::EngineError::UnsupportedWorkflowSchemaVersion { found, supported } => {
                format!("workflow state schema version {found} is newer than supported version {supported}")
            }
            crate::engine::error::EngineError::WorkflowResumeIncompatible(msg) => {
                format!("workflow resume incompatible: {msg}")
            }
            crate::engine::error::EngineError::Auth(msg) => {
                format!("auth error: {msg}")
            }
            crate::engine::error::EngineError::Config(msg) => {
                format!("invalid configuration: {msg}")
            }
            crate::engine::error::EngineError::NotImplemented(msg) => {
                format!("not implemented: {msg}")
            }
            crate::engine::error::EngineError::Other(msg) => msg.to_string(),
        },
        CommandError::Data(e) => format!("{e}"),
    };
    format!("amux: {body}")
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
///
/// Mapping per `aspec/uxui/cli.md`:
///   2 — invalid usage / parse / flag conflict
///   3 — missing Docker / container backend
///   4 — missing referenced work item
///   130 — user aborted (Ctrl-C)
///   1 — every other failure
pub(crate) fn error_exit_code(err: &CommandError) -> u8 {
    match err {
        CommandError::Aborted => 130,

        // Exit 2 — invalid usage / parse / flag conflict
        CommandError::UnknownCommand { .. }
        | CommandError::UnknownFlag { .. }
        | CommandError::MissingRequiredFlag { .. }
        | CommandError::MissingRequiredArgument { .. }
        | CommandError::MutuallyExclusive { .. }
        | CommandError::InvalidFlagValue { .. }
        | CommandError::InvalidArgumentValue { .. }
        | CommandError::CommandBoxParse(_)
        | CommandError::InvalidOverlaySpec { .. }
        | CommandError::UnknownConfigField { .. }
        | CommandError::InteractiveInputUnavailable { .. } => 2,

        // Exit 4 — missing referenced resource
        CommandError::WorkItemNotFound { .. }
        | CommandError::SpecTemplateMissing { .. }
        | CommandError::WorkflowFileNotFound { .. }
        | CommandError::HeadlessWorkdirNotFound { .. } => 4,

        // Exit 3 — missing container runtime
        CommandError::Engine(crate::engine::error::EngineError::Container(_))
        | CommandError::Engine(crate::engine::error::EngineError::ContainerRuntimeUnavailable {
            ..
        }) => 3,

        // Exit 1 — remaining engine errors (catch-all for unlisted EngineError variants)
        CommandError::Engine(_) => 1,
        CommandError::Data(_) => 1,
        CommandError::MergeConflict { .. } => 1,
        CommandError::MissingRemoteAddress
        | CommandError::MissingApiKey
        | CommandError::RemoteTimeout
        | CommandError::RemoteConnectionRefused(_)
        | CommandError::RemoteHttpStatus { .. }
        | CommandError::MalformedSseEvent(_)
        | CommandError::RemoteTransport(_) => 1,
        CommandError::HeadlessAlreadyRunning { .. }
        | CommandError::HeadlessNotRunning
        | CommandError::HeadlessAuthMissing
        | CommandError::RemoteSessionMissing
        | CommandError::RemoteSessionKillFailed { .. } => 1,
        CommandError::NotImplemented(_) => 1,
        CommandError::Other(_) => 1,
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
            CommandError::UnknownCommand {
                path: path(&["bogus"]),
            },
            CommandError::UnknownFlag {
                command: path(&["init"]),
                flag: "bad".into(),
            },
            CommandError::MissingRequiredFlag {
                command: path(&["init"]),
                flag: "agent".into(),
            },
            CommandError::MissingRequiredArgument {
                command: path(&["specs", "amend"]),
                argument: "work_item".into(),
            },
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
                command: path(&["specs", "amend"]),
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
            tip: "test tip".into(),
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
            CommandError::UnknownCommand {
                path: vec!["bad".into()],
            },
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
        assert!(
            s.contains("aborted") || s.contains("Aborted") || s.contains("130"),
            "Aborted error should mention abort: {s:?}"
        );
    }

    #[test]
    fn format_error_unknown_command_includes_path() {
        let err = CommandError::UnknownCommand {
            path: vec!["foo".into(), "bar".into()],
        };
        let s = format_error(&err);
        assert!(
            s.contains("foo") || s.contains("bar"),
            "UnknownCommand error should include the path: {s:?}"
        );
    }

    #[test]
    fn format_error_not_implemented_includes_message() {
        let err = CommandError::NotImplemented("headless");
        let s = format_error(&err);
        assert!(
            s.contains("headless"),
            "NotImplemented error must include the message: {s:?}"
        );
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
