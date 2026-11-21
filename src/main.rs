#![deny(unsafe_code)]
//! Layer 4 — the `awman` binary entrypoint.
//!
//! Per `aspec/architecture/2026-grand-architecture.md`, `main.rs`
//! contains no business logic: it builds clap from `CommandCatalogue`,
//! parses argv, delegates startup to Layer 2, and selects the CLI frontend
//! (when a subcommand is present) or TUI frontend (bare invocation).

use std::process::ExitCode;

use anyhow::{Context, Result};

use awman::command::commands::view::{
    ViewArgs, ViewCommand, FLAG_ANNOTATION, FLAG_INSTANCE, FLAG_TAB, VIEW_COMMAND,
};
use awman::command::dispatch::catalogue::{CommandCatalogue, SET_FLAG};
use awman::command::dispatch::RuntimeContext;
use awman::command::startup::Startup;
use awman::data::config::env::Env;
use awman::data::config::fields::KeyError;
use awman::data::config::overrides::ConfigOverrides;
use awman::engine::error::EngineError;
use awman::frontend::cli;
use awman::frontend::tui;

fn main() -> Result<ExitCode> {
    #[cfg(windows)]
    awman_host_shims::install_metadata_publisher();
    #[cfg(windows)]
    awman::engine::git::transport::install_windows_spawn(awman_host_shims::spawn_git);
    if let Some(success) = awman::command::git_credential_helper::dispatch(
        &Env::from_process(),
        &mut std::io::stdin().lock(),
        &mut std::io::stdout().lock(),
    ) {
        return Ok(if success {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        });
    }
    #[cfg(windows)]
    awman::engine::remote::local::install_owner_pipe(
        awman_host_shims::owner_pipe,
        awman_host_shims::verify_client,
    );
    if awman_host_shims::dispatch_worker(std::env::vars_os(), std::env::args_os()) {
        return Ok(ExitCode::SUCCESS);
    }
    #[cfg(target_os = "macos")]
    awman::engine::oci::apple_store::install_transport(std::sync::Arc::new(awman_cli::NativeXpc));
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(frontend_main())
}

async fn frontend_main() -> Result<ExitCode> {
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

    // `--set key=value` (global, repeatable): resolved against the key table
    // and parsed by the key's type before anything reads config. A bad key or
    // value is a usage error, like any other bad flag.
    let overrides = match set_overrides(&matches) {
        Ok(overrides) => overrides,
        Err(error) => {
            eprintln!("error: invalid value for '--set <KEY=VALUE>': {error}");
            return Ok(ExitCode::from(2));
        }
    };

    init_tracing();

    let path = cli::command_path_from_matches(&matches);
    let launch = LaunchMode::from_matches(&matches);
    // The bare-squad TUI launch (`awman squad` from a TTY, no --json/
    // --headless) has a non-empty command path but still lands in the
    // TUI, not the CLI. `Engines::detect`'s unknown-runtime policy branches
    // on "is this path empty" as its CLI/TUI proxy (a fatal exit for CLI, a
    // startup-modal message for TUI) — an unrelated frontend-choice decision
    // must not defeat that proxy, so the path used for detection reflects
    // the launch mode this invocation actually resolves to, not the raw
    // parsed subcommand path.
    let detect_path: Vec<String> = if launch.is_tui() { Vec::new() } else { path };
    let working_dir = std::env::current_dir().context("could not read current directory")?;
    let startup = Startup::new(detect_path);
    let outcome = match startup.run_with_overrides(working_dir, Env::from_process(), overrides) {
        Ok(outcome) => outcome,
        Err(error) => match error.downcast::<EngineError>() {
            Ok(error) => return Ok(cli::render_startup_error(matches, error.into())),
            Err(error) => {
                if error
                    .downcast_ref::<awman::data::error::DataError>()
                    .is_some_and(|e| {
                        matches!(
                            e,
                            awman::data::error::DataError::ConfigParse { .. }
                                | awman::data::error::DataError::Other(_)
                        )
                    })
                {
                    eprintln!("error: {error:#}");
                    return Ok(ExitCode::from(2));
                }
                return Err(error);
            }
        },
    };
    // Each frontend's `run` prints these: stderr in command mode, and the
    // TUI shows notices in its status area where they stay visible.
    let messages = outcome.messages().to_vec();
    let notices = outcome.notices().to_vec();

    let fatal_runtime_error = outcome.fatal_runtime_error;
    let ctx = RuntimeContext::new(outcome.session, outcome.engines);

    let initial_tab = match launch {
        LaunchMode::Cli => None,
        LaunchMode::TuiNormal => Some(tui::InitialTab::Normal),
        LaunchMode::TuiSquad => Some(tui::InitialTab::Squad),
        LaunchMode::TuiView => {
            // The hidden `awman view` secondary frontend (WI 0142): Layer 2
            // parses its flags and runs it through the TUI's entry point.
            let args = match view_args(&matches) {
                Ok(args) => args,
                Err(error) => {
                    eprintln!("awman: {error}");
                    return Ok(ExitCode::from(2));
                }
            };
            let entry = tui::ViewEntryPoint::new(ctx, messages, notices);
            return Ok(match ViewCommand::new(args).run(&entry) {
                Ok(code) => ExitCode::from(u8::try_from(code).unwrap_or(1)),
                Err(error) => {
                    eprintln!("awman: {error}");
                    ExitCode::from(1)
                }
            });
        }
    };

    match initial_tab {
        Some(tab) => Ok(tui::run(matches, ctx, fatal_runtime_error, tab, messages, notices).await),
        None => Ok(cli::run(matches, ctx, messages, notices).await),
    }
}

/// `awman view`'s flags, parsed by Layer 2.
fn view_args(matches: &clap::ArgMatches) -> Result<ViewArgs, awman::command::error::CommandError> {
    let view = matches
        .subcommand_matches(VIEW_COMMAND)
        .expect("TuiView is only chosen for the view subcommand");
    let annotations: Vec<String> = view
        .get_many::<String>(FLAG_ANNOTATION)
        .map(|values| values.cloned().collect())
        .unwrap_or_default();
    ViewArgs::from_flags(
        view.get_one::<String>(FLAG_INSTANCE)
            .map(String::as_str)
            .unwrap_or_default(),
        view.get_one::<String>(FLAG_TAB).map(String::as_str),
        &annotations,
    )
}

/// The global `--set` values, wherever on the command line they appeared,
/// parsed against the key table.
fn set_overrides(matches: &clap::ArgMatches) -> Result<ConfigOverrides, KeyError> {
    ConfigOverrides::parse_set_args(&set_args(matches))
}

/// Raw `--set` values. clap propagates a global arg's values to every level,
/// so the deepest subcommand's matches hold them all.
fn set_args(matches: &clap::ArgMatches) -> Vec<String> {
    let mut current = matches;
    while let Some((_, sub)) = current.subcommand() {
        current = sub;
    }
    current
        .get_many::<String>(SET_FLAG)
        .map(|values| values.cloned().collect())
        .unwrap_or_default()
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
    /// The hidden `awman view` (always a TUI, whatever stdin is).
    TuiView,
}

impl LaunchMode {
    fn from_matches(matches: &clap::ArgMatches) -> Self {
        if matches.subcommand_name().is_none() {
            Self::TuiNormal
        } else if matches.subcommand_name() == Some(VIEW_COMMAND) {
            Self::TuiView
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
/// is silently dropped, which made `awman daemon start` look like it was
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
