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

use crate::command::dispatch::catalogue::CommandCatalogue;
use crate::command::dispatch::{Dispatch, RuntimeContext};
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
///
/// `startup_messages` and `notices` come from Layer 2's startup and are
/// printed to stderr, one per line and in that order, before the command
/// runs. A builtin backend whose first open was deferred opens during the
/// command, so the notices it raises are printed after the command returns.
pub async fn run(
    matches: ArgMatches,
    ctx: RuntimeContext,
    startup_messages: Vec<String>,
    notices: Vec<String>,
) -> ExitCode {
    for line in startup_messages.iter().chain(&notices) {
        eprintln!("{line}");
    }
    let engines = ctx.engines.clone();
    let code = run_command(matches, ctx).await;
    for notice in engines.take_runtime_notices() {
        eprintln!("{notice}");
    }
    code
}

async fn run_command(matches: ArgMatches, ctx: RuntimeContext) -> ExitCode {
    let path = command_path_from_matches(&matches);
    if path.is_empty() {
        // `main.rs` should already have routed bare invocations to the TUI.
        eprintln!("awman: no subcommand supplied; run `awman --help` for usage.");
        return ExitCode::from(2);
    }
    let path_strs: Vec<&str> = path.iter().map(|s| s.as_str()).collect();

    // The runtime-tier guard and the squad gateway are catalogue-driven and
    // resolved by `Dispatch::run_command` (WI 0113 F-04). The CLI names no
    // squad subcommand of its own.
    let argv = ctx
        .session
        .read()
        .await
        .env()
        .argv()
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let frontend = CliFrontend::new(matches);
    let json = frontend.is_json_mode();
    let dispatch = Dispatch::new(frontend, ctx.session, ctx.engines);
    match dispatch.run_relayed(&path_strs, argv).await {
        Ok(crate::command::dispatch::CliCompletion::Local(result)) => match *result {
            Ok(outcome) => render_outcome(&outcome, json),
            Err(err) => render_error_for_mode(&err, json),
        },
        Ok(crate::command::dispatch::CliCompletion::Relayed(result)) => {
            CliFrontend::render_relay_result(result, json)
        }
        Err(error) => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({"error":format!("awman: {}",error.message)})
                );
            } else {
                eprintln!("awman: {}", error.message);
            }
            ExitCode::from(error.exit_code)
        }
    }
}

/// Present a typed startup failure through the same renderer as dispatch,
/// including JSON output for API-daemon startup commands invoked by the CLI.
pub fn render_startup_error(matches: ArgMatches, error: CommandError) -> ExitCode {
    render_error_for_mode(&error, CliFrontend::new(matches).is_json_mode())
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
/// stdin a terminal, and did the user ask for headless output.
/// `--json` implies `--headless` through the catalogue's own `implies`
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
            git_root,
        } => format!(
            "merge of branch {branch} is still unresolved in {}; resolve it there (worktree kept at {})",
            git_root.display(),
            worktree_path.display()
        ),
        CommandError::MergeTargetDirty {
            target_branch,
            branch,
            worktree_path,
        } => format!(
            "merge target '{target_branch}' has uncommitted changes; commit or stash them, then merge '{branch}' from the worktree at {}",
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
        CommandError::DaemonNotRunning
        | CommandError::DaemonAlreadyRunning { .. }
        | CommandError::DaemonNoFeatures
        | CommandError::DaemonHttpStatus { .. } => err.to_string(),
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
        CommandError::WorkflowPrerequisitesUnresolved { report } => format!(
            "workflow prerequisites unresolved\n{}\n\n\
             Grant them with --prerequisite-identity / --prerequisite-ssh / --prerequisite-token, \
             or pass --prerequisite-unresolved warn to continue anyway.",
            report.render_lines().join("\n")
        ),
        CommandError::PrerequisiteWideningRefused { .. }
        | CommandError::PrerequisiteFlagUnsupported { .. } => err.to_string(),
        // The engine owns its user-facing text, guidance included (WI 0126
        // F1), so the CLI, the TUI and the API print the same words. The
        // five remote-transport variants are mapped onto their Layer 2 twins
        // by `From<EngineError> for CommandError` and never land here.
        CommandError::Engine(e) => e.to_string(),
        CommandError::Data(e) => format!("{e}"),
        CommandError::NotAvailableForFrontend { command, frontend } => {
            format!("command `{command}` is not available via the {frontend} frontend")
        }
        // Session-creation validation errors (surfaced by multi-session
        // frontends); their Display text is already user-appropriate.
        CommandError::SessionInvalidType { .. }
        | CommandError::ApiWorkdirRootInvalid { .. }
        | CommandError::WorktreeExists { .. }
        | CommandError::SessionWorkdirRequired
        | CommandError::SessionWorkdirUnresolvable { .. }
        | CommandError::SessionWorkdirNotAllowed { .. }
        | CommandError::SessionRepoUrlRequired
        | CommandError::SessionRepoUrlEmpty
        | CommandError::SessionRepoUrlInvalidScheme { .. }
        | CommandError::SessionFieldRemoteOnly { .. }
        | CommandError::SessionFieldRequiresBranch { .. }
        | CommandError::SessionRefInvalid { .. }
        | CommandError::AgentNotFound { .. }
        | CommandError::AgentNoLongerRunning { .. }
        | CommandError::AgentNotRunning { .. }
        | CommandError::AgentReplaced { .. }
        | CommandError::AgentAmbiguous { .. }
        | CommandError::AgentReferenceEmpty
        | CommandError::AgentOwnerGone { .. } => err.to_string(),
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
    err.exit_code()
}
