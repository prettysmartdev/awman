//! Layer 2 startup orchestration for the interactive binary hosts.
//!
//! `Startup` owns the ordered transition from process facts to the session and
//! engine bundle a frontend consumes. Keeping that transition here prevents a
//! new frontend from accidentally opening `RepoConfig` before its legacy path
//! has been migrated.

use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::command::dispatch::catalogue::CommandCatalogue;
use crate::command::dispatch::Engines;
use crate::data::config::env::EnvSnapshot;
use crate::data::error::DataError;
use crate::data::migration;
use crate::data::session::{GitRootResolver, Session, SessionOpenOptions};
use crate::engine::error::EngineError;
use crate::engine::git::GitEngine;

/// The command-aware startup coordinator for one binary invocation.
pub struct Startup {
    command_path: Vec<String>,
}

impl Startup {
    /// Create startup orchestration for the parsed command path.
    pub fn new(command_path: Vec<String>) -> Self {
        Self { command_path }
    }

    /// Return the catalogue-owned hint for a removed flag before clap parses.
    pub fn removed_flag_hint<I>(&self, args: I) -> Option<String>
    where
        I: IntoIterator<Item = String>,
    {
        CommandCatalogue::get().removed_flag_hint(args)
    }

    /// Perform ordered application startup for `working_dir` and `env`.
    pub fn run(&self, working_dir: PathBuf, env: EnvSnapshot) -> Result<StartupOutcome> {
        let mut messages = Vec::new();

        // One-time migration from legacy amux paths and env vars.
        if let Some(msg) = migration::migrate_global_dir() {
            messages.push(msg);
        }
        messages.extend(migration::check_deprecated_env_vars());

        let path_refs: Vec<&str> = self.command_path.iter().map(String::as_str).collect();
        let git_engine = GitEngine::new();

        // Resolve git root first so we can migrate the repo-local `.amux/` → `.awman/`
        // BEFORE `Session::open` reads `RepoConfig` from disk. If we deferred this,
        // a user's first post-rename run would silently fall back to default repo
        // config because the load would miss the legacy `.amux/config.json`.
        let git_root = match git_engine.resolve(&working_dir) {
            Ok(root) => root,
            Err(DataError::GitRootNotFound { .. }) => working_dir.clone(),
            Err(other) => {
                return Err(anyhow::Error::new(other).context("failed to resolve git root"));
            }
        };
        if let Some(msg) = migration::migrate_repo_dir(&git_root) {
            messages.push(msg);
        }

        let session = Session::open_at_git_root(
            working_dir,
            git_root,
            SessionOpenOptions {
                env: Some(env),
                ..Default::default()
            },
        )
        .context("failed to open session")?;
        // Detect after opening the session so repo builtin settings participate
        // in the same effective configuration used by command execution.
        let (detected, fatal_runtime_error) = Engines::detect_effective(
            CommandCatalogue::get(),
            &session.effective_config(),
            &path_refs,
        )
        .map_err(|error| match error {
            EngineError::UnknownRuntime { .. } => anyhow::Error::new(error),
            other => anyhow::Error::new(other).context("failed to detect agent runtime"),
        })?;
        let engines =
            Engines::from_detected(detected, &session).context("failed to construct engines")?;

        Ok(StartupOutcome::new(
            session,
            engines,
            fatal_runtime_error,
            messages,
        ))
    }
}

/// The session, engines, and presentation data produced by [`Startup`].
pub struct StartupOutcome {
    pub session: Session,
    pub engines: Engines,
    pub fatal_runtime_error: Option<String>,
    messages: Vec<String>,
}

impl StartupOutcome {
    fn new(
        session: Session,
        engines: Engines,
        fatal_runtime_error: Option<String>,
        messages: Vec<String>,
    ) -> Self {
        Self {
            session,
            engines,
            fatal_runtime_error,
            messages,
        }
    }

    /// Messages collected during startup in the order they must be presented.
    pub fn messages(&self) -> &[String] {
        &self.messages
    }
}
