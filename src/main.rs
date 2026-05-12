#![forbid(unsafe_code)]
//! Layer 4 — the `amux` binary entrypoint.
//!
//! Per `aspec/architecture/2026-grand-architecture.md`, `main.rs`
//! contains no business logic: it builds clap from `CommandCatalogue`,
//! parses argv, constructs the engines + session, and dispatches to either
//! the CLI frontend (when a subcommand is present) or the TUI frontend
//! (bare invocation).

use std::process::ExitCode;
use std::sync::Arc;

use anyhow::{Context, Result};

use amux::command::dispatch::catalogue::CommandCatalogue;
use amux::command::dispatch::Engines;
use amux::data::config::global::GlobalConfig;
use amux::data::session::{Session, SessionOpenOptions};
use amux::engine::agent::AgentEngine;
use amux::engine::auth::AuthEngine;
use amux::engine::container::ContainerRuntime;
use amux::engine::git::GitEngine;
use amux::engine::overlay::OverlayEngine;
use amux::frontend::cli::{self, RuntimeContext};
use amux::frontend::tui;

#[tokio::main]
async fn main() -> Result<ExitCode> {
    let clap_cmd = CommandCatalogue::get().build_clap_command();
    let matches = clap_cmd.get_matches();

    let global_config = GlobalConfig::load().unwrap_or_default();
    let runtime = Arc::new(
        ContainerRuntime::detect(&global_config).context("failed to detect container runtime")?,
    );
    let git_engine = Arc::new(GitEngine::new());

    let working_dir = std::env::current_dir().context("could not read current directory")?;
    let session = Session::open_or_workdir_fallback(
        working_dir.clone(),
        git_engine.as_ref(),
        SessionOpenOptions::default(),
    )
    .context("failed to open session")?;

    let overlay_engine =
        Arc::new(OverlayEngine::new(&session).context("failed to construct overlay engine")?);
    let auth_engine =
        Arc::new(AuthEngine::new(&session).context("failed to construct auth engine")?);
    let agent_engine = Arc::new(AgentEngine::new(overlay_engine.clone(), runtime.clone()));
    let workflow_state_store = Arc::new(amux::data::EngineWorkflowStateStore::at_git_root(
        session.git_root().to_path_buf(),
    ));

    let engines = Engines {
        runtime,
        git_engine,
        overlay_engine,
        auth_engine,
        agent_engine,
        workflow_state_store,
    };

    let ctx = RuntimeContext::new(session, engines);

    if matches.subcommand_name().is_some() {
        Ok(cli::run(matches, ctx).await)
    } else {
        Ok(tui::run(matches, ctx).await)
    }
}

// ─── Layer 4 routing tests ────────────────────────────────────────────────────
//
// `main` is too integrated to call in unit tests (it requires live engines and
// a real session). Instead we test the **routing logic** directly: the condition
// `matches.subcommand_name().is_some()` is what drives the cli-vs-tui branch.
// These tests exercise that predicate with synthetic `ArgMatches`.

#[cfg(test)]
mod tests {
    use amux::command::dispatch::catalogue::CommandCatalogue;
    use amux::frontend::cli::command_path_from_matches;

    /// A subcommand in argv → `subcommand_name().is_some()` → CLI branch.
    #[test]
    fn subcommand_present_signals_cli_branch() {
        let cmd = CommandCatalogue::get().build_clap_command();
        for argv in [
            vec!["amux", "status"],
            vec!["amux", "ready"],
            vec!["amux", "chat"],
            vec!["amux", "init"],
            vec!["amux", "exec", "workflow", "wf.toml"],
            vec!["amux", "headless", "start"],
            vec!["amux", "remote", "session", "start"],
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

    /// Bare `amux` → `subcommand_name().is_none()` → TUI branch.
    #[test]
    fn bare_invocation_signals_tui_branch() {
        let cmd = CommandCatalogue::get().build_clap_command();
        let m = cmd.try_get_matches_from(["amux"]).unwrap();
        assert!(
            m.subcommand_name().is_none(),
            "bare `amux` must have no subcommand — routes to TUI"
        );
        let path = command_path_from_matches(&m);
        assert!(
            path.is_empty(),
            "bare invocation must produce an empty path"
        );
    }

    /// Aliases also route through the CLI branch correctly.
    #[test]
    fn exec_workflow_alias_wf_routes_to_cli() {
        let cmd = CommandCatalogue::get().build_clap_command();
        let m = cmd
            .try_get_matches_from(["amux", "exec", "wf", "wf.toml"])
            .unwrap();
        assert!(m.subcommand_name().is_some());
        let path = command_path_from_matches(&m);
        // Clap resolves the alias to canonical `workflow`.
        assert_eq!(path, vec!["exec", "workflow"]);
    }
}
