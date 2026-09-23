#![forbid(unsafe_code)]
//! Layer 4 — the `awman` binary entrypoint.
//!
//! Per `aspec/architecture/2026-grand-architecture.md`, `main.rs`
//! contains no business logic: it builds clap from `CommandCatalogue`,
//! parses argv, delegates startup to Layer 2, and selects the CLI frontend
//! (when a subcommand is present) or TUI frontend (bare invocation).

use std::process::ExitCode;

use anyhow::{Context, Result};

use awman::command::dispatch::catalogue::CommandCatalogue;
use awman::command::dispatch::RuntimeContext;
use awman::command::startup::Startup;
use awman::data::config::env::Env;
use awman::engine::error::EngineError;
use awman::frontend::cli;
use awman::frontend::tui;

#[tokio::main]
async fn main() -> Result<ExitCode> {
    let removed_flag_startup = Startup::new(Vec::new());
    // Retired flags (e.g. `--mount-ssh`) are intercepted before clap renders
    // its generic "unexpected argument" message, so the user sees a migration
    // hint instead. The removed-flag list and matching live in the catalogue;
    // adding a future removal needs no `main.rs` change.
    if let Some(hint) = removed_flag_startup.removed_flag_hint(std::env::args()) {
        eprintln!("error: {hint}");
        return Ok(ExitCode::from(2));
    }

    let clap_cmd = CommandCatalogue::get().build_clap_command();
    let matches = clap_cmd.get_matches();

    init_tracing();

    let path = cli::command_path_from_matches(&matches);
    let launch = LaunchMode::from_matches(&matches);
    // The bare-squad TUI launch (`awman squad` from a TTY, no --json/
    // --non-interactive) has a non-empty command path but still lands in the
    // TUI, not the CLI. `Engines::detect`'s unknown-runtime policy branches
    // on "is this path empty" as its CLI/TUI proxy (a fatal exit for CLI, a
    // startup-modal message for TUI) — an unrelated launch-mode decision
    // must not defeat that proxy, so the path used for detection reflects
    // the launch mode this invocation actually resolves to, not the raw
    // parsed subcommand path.
    let detect_path: Vec<String> = if launch.is_tui() { Vec::new() } else { path };
    let working_dir = std::env::current_dir().context("could not read current directory")?;
    let startup = Startup::new(detect_path);
    let outcome = match startup.run(working_dir, Env::from_process()) {
        Ok(outcome) => outcome,
        Err(error)
            if matches!(
                error.downcast_ref::<EngineError>(),
                Some(EngineError::UnknownRuntime { .. })
            ) =>
        {
            eprintln!("awman: {error}");
            return Ok(ExitCode::from(2));
        }
        Err(error) => return Err(error),
    };
    for message in outcome.messages() {
        eprintln!("{message}");
    }

    let fatal_runtime_error = outcome.fatal_runtime_error;
    let ctx = RuntimeContext::new(outcome.session, outcome.engines);

    let initial_tab = match launch {
        LaunchMode::Cli => None,
        LaunchMode::TuiNormal => Some(tui::InitialTab::Normal),
        LaunchMode::TuiSquad => Some(tui::InitialTab::Squad),
    };

    match initial_tab {
        Some(tab) => Ok(tui::run(matches, ctx, fatal_runtime_error, tab).await),
        None => Ok(cli::run(matches, ctx).await),
    }
}

/// Which frontend this invocation resolves to, and — for the TUI arms —
/// which tab it opens on. This is also the single CLI/TUI classification
/// `Engines::detect`'s unknown-runtime policy is keyed on: every `TuiSquad`
/// launch must be treated as TUI for that policy even though its parsed
/// command path (`["squad"]`) is non-empty.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LaunchMode {
    Cli,
    TuiNormal,
    TuiSquad,
}

impl LaunchMode {
    fn from_matches(matches: &clap::ArgMatches) -> Self {
        if matches.subcommand_name().is_none() {
            Self::TuiNormal
        } else if cli::is_bare_tui_invocation(matches) {
            Self::TuiSquad
        } else {
            Self::Cli
        }
    }

    fn is_tui(self) -> bool {
        !matches!(self, Self::Cli)
    }
}

/// Initialize the global tracing subscriber once at process start.
///
/// Without this, every `tracing::info!`/`warn!`/`error!` call in the
/// codebase (notably the API-server startup messages in `frontend::api`)
/// is silently dropped, which made `awman api start` look like it was
/// hanging until Ctrl-C. We write to stderr (so stdout stays clean for
/// `--json` callers), default to `info`-level for awman code, and honor
/// `RUST_LOG` for overrides. ANSI colors are auto-enabled by the `fmt`
/// layer when stderr is a TTY.
///
/// When the TUI is active, stderr writes would paint raw text over the
/// alternate-screen rendering. The writer gates on `is_tui_active()` and
/// redirects to `io::sink()` while the TUI owns the terminal.
fn init_tracing() {
    use tracing_subscriber::fmt::{format::Writer, time::FormatTime, MakeWriter};
    use tracing_subscriber::{fmt, EnvFilter};

    struct ShortLocalTime;
    impl FormatTime for ShortLocalTime {
        fn format_time(&self, w: &mut Writer<'_>) -> std::fmt::Result {
            write!(w, "{}", chrono::Local::now().format("%H:%M:%S%.3f"))
        }
    }

    enum MaybeStderr {
        Stderr(std::io::Stderr),
        Sink(std::io::Sink),
    }
    impl std::io::Write for MaybeStderr {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            match self {
                Self::Stderr(w) => w.write(buf),
                Self::Sink(w) => w.write(buf),
            }
        }
        fn flush(&mut self) -> std::io::Result<()> {
            match self {
                Self::Stderr(w) => w.flush(),
                Self::Sink(w) => w.flush(),
            }
        }
    }

    struct TuiAwareWriter;
    impl<'a> MakeWriter<'a> for TuiAwareWriter {
        type Writer = MaybeStderr;
        fn make_writer(&'a self) -> Self::Writer {
            if tui::is_tui_active() {
                MaybeStderr::Sink(std::io::sink())
            } else {
                MaybeStderr::Stderr(std::io::stderr())
            }
        }
    }

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = fmt()
        .with_env_filter(filter)
        .with_writer(TuiAwareWriter)
        .with_target(false)
        .with_timer(ShortLocalTime)
        .compact()
        .try_init();
}

// ─── Layer 4 routing tests ────────────────────────────────────────────────────
//
// `main` is too integrated to call in unit tests (it requires live engines and
// a real session). Instead we test the **routing logic** directly: the task
// `matches.subcommand_name().is_some()` is what drives the cli-vs-tui branch.
// These tests exercise that predicate with synthetic `ArgMatches`.

#[cfg(test)]
mod tests {
    use awman::command::dispatch::catalogue::CommandCatalogue;
    use awman::frontend::cli::command_path_from_matches;

    /// A subcommand in argv → `subcommand_name().is_some()` → CLI branch.
    #[test]
    fn subcommand_present_signals_cli_branch() {
        let cmd = CommandCatalogue::get().build_clap_command();
        for argv in [
            vec!["awman", "status"],
            vec!["awman", "ready"],
            vec!["awman", "chat"],
            vec!["awman", "init"],
            vec!["awman", "exec", "workflow", "wf.toml"],
            vec!["awman", "api", "start"],
            vec!["awman", "remote", "session", "start"],
        ] {
            let m = cmd
                .clone()
                .try_get_matches_from(&argv)
                .unwrap_or_else(|e| panic!("failed to parse {argv:?}: {e}"));
            assert!(
                m.subcommand_name().is_some(),
                "{argv:?} must have a subcommand — routes to CLI"
            );
            // command_path_from_matches must also return a non-empty path.
            let path = command_path_from_matches(&m);
            assert!(!path.is_empty(), "{argv:?} must produce a non-empty path");
        }
    }

    /// Bare `awman` → `subcommand_name().is_none()` → TUI branch.
    #[test]
    fn bare_invocation_signals_tui_branch() {
        let cmd = CommandCatalogue::get().build_clap_command();
        let m = cmd.try_get_matches_from(["awman"]).unwrap();
        assert!(
            m.subcommand_name().is_none(),
            "bare `awman` must have no subcommand — routes to TUI"
        );
        let path = command_path_from_matches(&m);
        assert!(
            path.is_empty(),
            "bare invocation must produce an empty path"
        );
    }

    /// Every `LaunchMode` variant reports `is_tui()` correctly — this is the
    /// mapping `main` uses to decide the path fed to `Engines::detect`, so a
    /// wrong answer here would defeat the CLI/TUI unknown-runtime proxy for
    /// whichever variant is wrong.
    #[test]
    fn launch_mode_is_tui_matches_each_variant() {
        assert!(!super::LaunchMode::Cli.is_tui());
        assert!(super::LaunchMode::TuiNormal.is_tui());
        assert!(super::LaunchMode::TuiSquad.is_tui());
    }

    /// Regression for the bug where a bare `awman squad` TUI launch under an
    /// unknown `runtime:` config lost its fatal modal and exited like a CLI
    /// invocation instead. `awman squad`'s parsed command path is non-empty
    /// (`["squad"]`) — same as any ordinary CLI subcommand — even though the
    /// invocation can resolve to the Squad TUI. `main` must not feed that raw
    /// path to `Engines::detect`'s CLI/TUI proxy for a `TuiSquad` launch: it
    /// has to collapse to the same empty path a bare `awman` uses, exactly
    /// like `LaunchMode::TuiSquad.is_tui()` (asserted above) requires.
    #[test]
    fn bare_squad_has_a_non_empty_path_despite_resolving_to_the_tui() {
        let cmd = CommandCatalogue::get().build_clap_command();
        let m = cmd.try_get_matches_from(["awman", "squad"]).unwrap();
        assert!(
            m.subcommand_name().is_some(),
            "`awman squad` has a subcommand, same as any CLI invocation"
        );
        let path = command_path_from_matches(&m);
        assert_eq!(
            path,
            vec!["squad"],
            "the raw parsed path must not be used directly as the CLI/TUI \
             detection proxy for this invocation"
        );
    }

    /// Aliases also route through the CLI branch correctly.
    #[test]
    fn exec_workflow_alias_wf_routes_to_cli() {
        let cmd = CommandCatalogue::get().build_clap_command();
        let m = cmd
            .try_get_matches_from(["awman", "exec", "wf", "wf.toml"])
            .unwrap();
        assert!(m.subcommand_name().is_some());
        let path = command_path_from_matches(&m);
        // Clap resolves the alias to canonical `workflow`.
        assert_eq!(path, vec!["exec", "workflow"]);
    }
}
