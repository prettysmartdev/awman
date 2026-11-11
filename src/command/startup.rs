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
use crate::data::config::ConfigOverrides;
use crate::data::error::DataError;
use crate::data::session::{GitRootResolver, Session, SessionOpenOptions};
use crate::engine::error::EngineError;
use crate::engine::git::GitEngine;

/// A fatal startup failure selected for frontend presentation. The inert
/// engine bundle only allows the frontend to open its quit-only modal.
pub struct RuntimeStartupFailure {
    pub error: crate::command::error::CommandError,
}
impl RuntimeStartupFailure {
    pub(crate) fn new(error: EngineError) -> Self {
        Self {
            error: error.into(),
        }
    }
}

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
        self.run_with_overrides(working_dir, env, ConfigOverrides::default())
    }

    /// [`Startup::run`] with `--set` overrides (already parsed against the key
    /// table) applied to the session's config.
    pub fn run_with_overrides(
        &self,
        working_dir: PathBuf,
        env: EnvSnapshot,
        overrides: ConfigOverrides,
    ) -> Result<StartupOutcome> {
        let messages = Vec::new();

        let path_refs: Vec<&str> = self.command_path.iter().map(String::as_str).collect();
        let git_engine = GitEngine::new();

        // Outside a Git repo the working directory stands in for the root.
        let (git_root, in_repo) = match git_engine.resolve(&working_dir) {
            Ok(root) => (root, true),
            Err(DataError::GitRootNotFound { .. }) => (working_dir.clone(), false),
            Err(other) => {
                return Err(anyhow::Error::new(other).context("failed to resolve git root"));
            }
        };

        let opts = SessionOpenOptions {
            env: Some(env),
            overrides,
            ..Default::default()
        };
        // Outside a Git repo the directory is not a repository: config scope
        // and `--repo` must not treat it as one.
        let session = if in_repo {
            Session::open_at_git_root(working_dir, git_root, opts)
        } else {
            Session::open_at_directory_root(git_root, opts)
        }
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
        // Opening the runtime may have upgraded the builtin state directory in
        // place; its notice is presented by the frontend that runs next.
        let notices = detected.take_notices();
        let engines =
            Engines::from_detected(detected, &session).context("failed to construct engines")?;

        Ok(StartupOutcome::new(
            session,
            engines,
            fatal_runtime_error,
            messages,
            notices,
        ))
    }
}

/// The session, engines, and presentation data produced by [`Startup`].
pub struct StartupOutcome {
    pub session: Session,
    pub engines: Engines,
    pub fatal_runtime_error: Option<RuntimeStartupFailure>,
    messages: Vec<String>,
    notices: Vec<String>,
}

impl StartupOutcome {
    fn new(
        session: Session,
        engines: Engines,
        fatal_runtime_error: Option<RuntimeStartupFailure>,
        messages: Vec<String>,
        notices: Vec<String>,
    ) -> Self {
        Self {
            session,
            engines,
            fatal_runtime_error,
            messages,
            notices,
        }
    }

    /// Messages collected during startup in the order they must be presented.
    pub fn messages(&self) -> &[String] {
        &self.messages
    }

    /// One-line runtime notices (an in-place builtin state upgrade) for the
    /// frontend that runs next: stderr in command mode, the status area in
    /// the TUI. Never a prompt.
    pub fn notices(&self) -> &[String] {
        &self.notices
    }
}
