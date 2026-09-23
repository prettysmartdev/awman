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

use clap::ArgMatches;

use crate::command::commands::Command;
use crate::command::dispatch::catalogue::CommandCatalogue;
use crate::command::dispatch::{BuiltCommand, Dispatch, RuntimeContext};
use crate::command::error::CommandError;
use crate::command::CommandOutcome;

mod command_frontend;
mod output;
mod parallel;
pub(crate) mod per_command;
mod user_message;

pub use command_frontend::{command_path_from_matches, CliFrontend};
pub use parallel::CliParallelFrontend;

/// Entry point for the CLI frontend.
///
/// Returns a process [`ExitCode`] reflecting the outcome of the dispatched
/// command.
pub async fn run(matches: ArgMatches, ctx: RuntimeContext) -> ExitCode {
    let path = command_path_from_matches(&matches);
    if path.is_empty() {
        // `main.rs` should already have routed bare invocations to the TUI.
        eprintln!("awman: no subcommand supplied; run `awman --help` for usage.");
        return ExitCode::from(2);
    }
    let path_strs: Vec<&str> = path.iter().map(|s| s.as_str()).collect();
    if path_strs == ["exec", "workflow"] {
        let build_frontend = CliFrontend::new(matches.clone());
        let json = build_frontend.is_json_mode();
        let dispatch = Dispatch::new(build_frontend, ctx.session.clone(), ctx.engines.clone());
        return match dispatch.build_command(&path_strs) {
            Ok(BuiltCommand::ExecWorkflow(cmd)) => {
                let frontend = CliParallelFrontend::new(CliFrontend::new(matches));
                match cmd.run_with_frontend(Box::new(frontend)).await {
                    Ok(outcome) => render_outcome(&CommandOutcome::ExecWorkflow(outcome), json),
                    Err(err) => render_error(&err),
                }
            }
            Ok(_) => render_error(&CommandError::unknown_command(&path_strs)),
            Err(err) => render_error(&err),
        };
    }

    // The runtime-tier guard and the squad gateway are catalogue-driven and
    // resolved by `Dispatch::run_command` (WI 0113 F-04). The CLI names no
    // squad subcommand of its own.
    let frontend = CliFrontend::new(matches);
    let json = frontend.is_json_mode();
    let dispatch = Dispatch::new(frontend, ctx.session, ctx.engines);
    match dispatch.run_command(&path_strs).await {
        Ok(outcome) => render_outcome(&outcome, json),
        Err(err) => render_error_for_mode(&err, json),
    }
}

fn render_error_for_mode(error: &CommandError, json: bool) -> ExitCode {
    if json {
        println!("{}", serde_json::json!({ "error": format_error(error) }));
        ExitCode::from(error_exit_code(error))
    } else {
        render_error(error)
    }
}

/// True for the TTY form of a bare command that has a TUI form, which main.rs
/// opens in the TUI.
///
/// Which commands those are is the catalogue's fact
/// ([`CommandCatalogue::opens_tui_when_bare`]); this keeps only the two facts
/// about *this invocation* that a frontend is the right place to read — is
/// stdin a terminal, and did the user ask for non-interactive output.
/// `--json` implies `--non-interactive` through the catalogue's own `implies`
/// edge, which `CliFrontend::explicit_non_interactive` resolves, so this no
/// longer spells that rule out a second time.
pub fn is_bare_tui_invocation(matches: &ArgMatches) -> bool {
    let path = command_path_from_matches(matches);
    let path_refs: Vec<&str> = path.iter().map(String::as_str).collect();
    CommandCatalogue::get().opens_tui_when_bare(&path_refs)
        && output::stdin_is_tty()
        && !CliFrontend::explicit_non_interactive(matches, &path)
}

/// Format a successful [`CommandOutcome`] to user-facing stdout text.
/// Returns `None` when the outcome carries nothing additional to print
/// beyond what the engine already streamed via `report_*` to stderr.
///
/// The previous implementation fell back to `serde_json::to_string_pretty`
/// for every non-Empty variant, which surfaced raw JSON as the primary
/// user output for `chat`, `status`, `config`, etc. Per-variant rendering
/// now lives in [`per_command::render`].
pub(crate) fn format_outcome(outcome: &CommandOutcome, json: bool) -> Option<String> {
    per_command::render::render(outcome, json)
}

/// Format a [`CommandError`] to the user-visible stderr string.
///
/// Each variant gets a friendly message + an optional "next step" hint
/// where actionable. The "awman: " prefix is always present so user output
/// is consistent across error types.
pub(crate) fn format_error(err: &CommandError) -> String {
    let body = match err {
        CommandError::Aborted => "command aborted by user".to_string(),
        CommandError::UnknownCommand { path, suggestions } => {
            let mut body = format!("unknown command: {}", path.join(" "));
            if let Some(nearest) = suggestions.first() {
                body.push_str(&format!("\n  did you mean `awman {nearest}`?"));
            }
            body.push_str("\n  try `awman --help` for the full command list");
            body
        }
        CommandError::UnknownFlag { command, flag } => {
            format!(
                "unknown flag '--{flag}' for command `{}`\n  try `awman {} --help`",
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
        CommandError::UnexpectedArgument { command, argument } => {
            format!(
                "unexpected argument '{argument}' for command `{}`\n  try `awman {} --help`",
                command.join(" "),
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
        CommandError::ApiWorkdirNotFound { path } => {
            format!("API workdir not found: {}", path.display())
        }
        CommandError::ApiServerAlreadyRunning { pid } => {
            format!(
                "API server is already running on PID {pid}; run `awman api kill` first"
            )
        }
        CommandError::ApiServerNotRunning => "API server is not running".into(),
        CommandError::ApiServerAuthMissing => {
            "no API key configured. Run `awman api start --refresh-key` first, or pass `--dangerously-skip-auth`.".into()
        }
        CommandError::RemoteSessionMissing => {
            "no remote session id; pass --session <id> or run `awman remote session start` first".into()
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
                "spec template missing at {}; run `awman init --aspec` to create it",
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
            // The five remote-transport variants have Layer 2 twins, and
            // `From<EngineError> for CommandError` maps them onto those, so `?`
            // never lands here. A directly-constructed `Engine(Remote*)` still
            // renders exactly like its twin above.
            crate::engine::error::EngineError::RemoteTimeout => {
                "remote request timed out".into()
            }
            crate::engine::error::EngineError::RemoteConnectionRefused(reason) => {
                format!("remote connection refused: {reason}")
            }
            crate::engine::error::EngineError::RemoteHttpStatus { status, body } => {
                format!("remote returned HTTP {status}: {body}")
            }
            crate::engine::error::EngineError::MalformedSseEvent(msg) => {
                format!("malformed SSE event from remote: {msg}")
            }
            crate::engine::error::EngineError::RemoteTransport(msg) => {
                format!("remote transport error: {msg}")
            }
            crate::engine::error::EngineError::UnknownRuntime { value, valid } => format!(
                "invalid runtime '{value}' in global config ($HOME/.awman/config.json); \
                 valid values: {valid}"
            ),
            crate::engine::error::EngineError::AgentRequiresProjectImage { tag } => format!(
                "agent image build requires the project base image first ({tag}); run `awman ready --build`"
            ),
            crate::engine::error::EngineError::Container(msg) => format!(
                "container backend error: {msg}\n  awman requires Docker; install Docker Desktop / docker-engine and retry"
            ),
            crate::engine::error::EngineError::ContainerImageNotFound { image } => format!(
                "image not found: '{image}'. Run `make build` or build the base image manually."
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
            crate::engine::error::EngineError::Sandbox(msg) => {
                format!("sandbox backend error: {msg}")
            }
            crate::engine::error::EngineError::UnsupportedOnRuntime { runtime, operation } => {
                format!(
                    "{operation} is not supported on the {runtime} runtime; \
                     change `runtime` in the global config to one that provides it"
                )
            }
            crate::engine::error::EngineError::OptionVariantMismatch { runtime, got } => format!(
                "runtime {runtime} was given {got}-paradigm options; this indicates a Layer 2 dispatch bug"
            ),
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
            crate::engine::error::EngineError::SquadRuntimeUnsupported { .. }
            | crate::engine::error::EngineError::SquadDaemonStartup(_)
            | crate::engine::error::EngineError::SquadDaemonConflict(_)
            | crate::engine::error::EngineError::SquadDaemonUnreachable(_) => format!("{e}"),
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
            // ACP (WI 0104). Minimal build-plumbing arms so the exhaustive match
            // compiles after the foundation step added these variants; the
            // cli-frontend step owns the final user-facing wording.
            crate::engine::error::EngineError::AcpUnsupported { agent } => {
                format!("agent '{agent}' does not support ACP (Agent Client Protocol) launch mode")
            }
            crate::engine::error::EngineError::Acp(msg) => {
                format!("ACP protocol error: {msg}")
            }
            crate::engine::error::EngineError::Other(msg) => msg.to_string(),
        },
        CommandError::Data(e) => format!("{e}"),
        CommandError::NotAvailableForFrontend { command, frontend } => {
            format!("command `{command}` is not available via the {frontend} frontend")
        }
        // The squad missing-key answer authors its own full text, including
        // the variable to set and the command to mint a new key.
        CommandError::SquadKeyMissing => err.to_string(),
        // Session-creation validation errors (surfaced by multi-session
        // frontends); their Display text is already user-appropriate.
        CommandError::SessionInvalidType { .. }
        | CommandError::SessionWorkdirRequired
        | CommandError::SessionWorkdirUnresolvable { .. }
        | CommandError::SessionWorkdirNotAllowed { .. }
        | CommandError::SessionRepoUrlRequired
        | CommandError::SessionRepoUrlEmpty
        | CommandError::SessionRepoUrlInvalidScheme { .. } => err.to_string(),
    };
    format!("awman: {body}")
}

/// Render a successful [`CommandOutcome`] to stdout and return the
/// process exit code.
fn render_outcome(outcome: &CommandOutcome, json: bool) -> ExitCode {
    if let Some(s) = format_outcome(outcome, json) {
        println!("{s}");
    }
    ExitCode::from(outcome_exit_code(outcome))
}

/// Pure mapping from a successful [`CommandOutcome`] to a process exit code.
///
/// Some commands deliberately complete without short-circuiting on per-item
/// failures and report those failures inside the outcome instead — today,
/// `new skill --pull-all`, which must still refresh every reachable library
/// when one upstream is gone. Their aggregate status has to reach scripts and
/// CI as a non-zero exit code even though the command itself returned `Ok`.
fn outcome_exit_code(outcome: &CommandOutcome) -> u8 {
    u8::try_from(outcome.exit_code()).unwrap_or(1)
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
        | CommandError::UnexpectedArgument { .. }
        | CommandError::MutuallyExclusive { .. }
        | CommandError::InvalidFlagValue { .. }
        | CommandError::InvalidArgumentValue { .. }
        | CommandError::CommandBoxParse(_)
        | CommandError::InvalidOverlaySpec { .. }
        | CommandError::UnknownConfigField { .. }
        | CommandError::InteractiveInputUnavailable { .. }
        // Session-creation validation failures are invalid-usage class.
        | CommandError::SessionInvalidType { .. }
        | CommandError::SessionWorkdirRequired
        | CommandError::SessionWorkdirUnresolvable { .. }
        | CommandError::SessionWorkdirNotAllowed { .. }
        | CommandError::SessionRepoUrlRequired
        | CommandError::SessionRepoUrlEmpty
        | CommandError::SessionRepoUrlInvalidScheme { .. } => 2,

        // Exit 4 — missing referenced resource
        CommandError::WorkItemNotFound { .. }
        | CommandError::SpecTemplateMissing { .. }
        | CommandError::WorkflowFileNotFound { .. }
        | CommandError::ApiWorkdirNotFound { .. } => 4,

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
        CommandError::ApiServerAlreadyRunning { .. }
        | CommandError::ApiServerNotRunning
        | CommandError::ApiServerAuthMissing
        | CommandError::RemoteSessionMissing
        | CommandError::RemoteSessionKillFailed { .. } => 1,
        CommandError::NotImplemented(_) => 1,
        CommandError::NotAvailableForFrontend { .. } => 1,
        CommandError::SquadKeyMissing => 1,
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
                suggestions: Vec::new(),
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
            CommandError::ApiServerAlreadyRunning { pid: 42 },
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
        let m = cmd.try_get_matches_from(["awman", "status"]).unwrap();
        // main.rs uses `matches.subcommand_name().is_some()` to pick CLI.
        assert!(m.subcommand_name().is_some());
    }

    #[test]
    fn bare_invocation_routes_to_tui() {
        let cmd = CommandCatalogue::get().build_clap_command();
        let m = cmd.try_get_matches_from(["awman"]).unwrap();
        // main.rs uses `matches.subcommand_name().is_none()` to pick TUI.
        assert!(m.subcommand_name().is_none());
    }

    /// The squad daemon serves the squad subtree and nothing else, and the
    /// catalogue is what says so (F-50).
    #[test]
    fn the_squad_daemon_frontend_admits_only_the_squad_subtree() {
        use crate::command::dispatch::catalogue::FrontendKind;
        let catalogue = CommandCatalogue::get();
        assert!(catalogue.is_allowed_for_frontend(FrontendKind::SquadDaemon, &["squad", "list"]));
        for outside in [
            vec!["exec", "workflow"],
            vec!["ready"],
            vec!["chat"],
            vec!["status"],
        ] {
            assert!(
                !catalogue.is_allowed_for_frontend(FrontendKind::SquadDaemon, &outside),
                "{outside:?} is outside the squad subtree"
            );
        }
        // A squad leaf the API itself refuses (interactive/PTY) is refused
        // here too: `SquadDaemon` narrows the API profile, it does not widen
        // it.
        assert!(!catalogue.is_allowed_for_frontend(FrontendKind::SquadDaemon, &["squad", "attach"]));
    }

    /// Which commands have a bare TUI form is the catalogue's fact, so
    /// renaming one cannot leave a stale literal in this module.
    #[test]
    fn the_catalogue_names_the_commands_with_a_bare_tui_form() {
        let catalogue = CommandCatalogue::get();
        assert!(catalogue.opens_tui_when_bare(&["squad"]));
        for other in ["status", "ready", "init", "clean", "config"] {
            assert!(
                !catalogue.opens_tui_when_bare(&[other]),
                "{other} has no bare TUI form"
            );
        }
        // A subcommand of one that does is still a plain CLI command.
        assert!(!catalogue.opens_tui_when_bare(&["squad", "list"]));
        assert!(!catalogue.opens_tui_when_bare(&[]));
    }

    /// `--json` implies `--non-interactive`, so a `--json` invocation never
    /// opens the TUI even though it is otherwise the bare form. This module
    /// used to re-check `--json` itself; it now goes through the one resolver.
    #[test]
    fn json_and_non_interactive_both_keep_bare_squad_in_the_cli() {
        let cmd = CommandCatalogue::get().build_clap_command();
        for argv in [
            vec!["awman", "squad", "--json"],
            vec!["awman", "squad", "--non-interactive"],
        ] {
            let m = cmd.clone().try_get_matches_from(&argv).unwrap();
            let path = command_path_from_matches(&m);
            assert!(
                CliFrontend::explicit_non_interactive(&m, &path),
                "{argv:?} must resolve to non-interactive"
            );
            assert!(
                !is_bare_tui_invocation(&m),
                "{argv:?} must not open the TUI"
            );
        }
    }

    #[test]
    fn render_outcome_empty_is_success() {
        let outcome = crate::command::CommandOutcome::Empty;
        let _code = render_outcome(&outcome, false);
    }

    // ─── format_outcome — snapshot-style per-variant assertions ──────────────

    #[test]
    fn format_outcome_empty_returns_none() {
        assert!(format_outcome(&crate::command::CommandOutcome::Empty, false).is_none());
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
        let s = format_outcome(&outcome, false).expect("status must render text");
        assert!(s.contains("AWMAN STATUS DASHBOARD"));
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
        assert!(format_outcome(&outcome, false).is_none());
    }

    // ─── format_error — per-variant rendering assertions ─────────────────────

    #[test]
    fn format_error_prefix_is_always_awman() {
        // Every error message must start with "awman: " for consistent UX.
        let errors: &[CommandError] = &[
            CommandError::Aborted,
            CommandError::NotImplemented("x"),
            CommandError::Other("boom".into()),
            CommandError::UnknownCommand {
                path: vec!["bad".into()],
                suggestions: Vec::new(),
            },
        ];
        for err in errors {
            let s = format_error(err);
            assert!(
                s.starts_with("awman: "),
                "error must start with 'awman: ', got: {s:?}"
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
            suggestions: Vec::new(),
        };
        let s = format_error(&err);
        assert!(
            s.contains("foo") || s.contains("bar"),
            "UnknownCommand error should include the path: {s:?}"
        );
    }

    /// Rendering only: the catalogue decides what the near misses are.
    #[test]
    fn format_error_unknown_command_renders_a_suggestion_when_given_one() {
        let err = CommandError::UnknownCommand {
            path: vec!["exec".into(), "wrkflow".into()],
            suggestions: vec!["exec workflow".into()],
        };
        let s = format_error(&err);
        assert!(
            s.contains("did you mean `awman exec workflow`?"),
            "a suggestion must render as a runnable command line: {s:?}"
        );
    }

    #[test]
    fn format_error_not_implemented_includes_message() {
        let err = CommandError::NotImplemented("api");
        let s = format_error(&err);
        assert!(
            s.contains("api"),
            "NotImplemented error must include the message: {s:?}"
        );
    }

    // ─── TTY detection ────────────────────────────────────────────────────────
    // These tests exercise the output.rs TTY-detection function to confirm
    // it doesn't panic and returns a consistent bool value. In CI, stdin is
    // non-TTY, so it returns false. The behavior is documented rather than
    // asserted to avoid fragility when running locally.

    #[test]
    fn tty_detection_does_not_panic() {
        let _stdin = crate::frontend::cli::output::stdin_is_tty();
        // No assertion — just verifying the call doesn't panic.
    }

    #[test]
    fn stdin_tty_returns_consistent_bools() {
        // Calling twice must return the same value (no side effects, no flicker).
        let c = crate::frontend::cli::output::stdin_is_tty();
        let d = crate::frontend::cli::output::stdin_is_tty();
        assert_eq!(c, d, "stdin_is_tty must be idempotent");
    }
}
