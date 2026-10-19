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
    pub fatal_runtime_error: Option<RuntimeStartupFailure>,
    messages: Vec<String>,
}

impl StartupOutcome {
    fn new(
        session: Session,
        engines: Engines,
        fatal_runtime_error: Option<RuntimeStartupFailure>,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::error::CommandError;
    use crate::data::config::env::{EnvSnapshot, AMBIENT_MSB_OVERRIDES, AWMAN_CONFIG_HOME};

    #[test]
    fn tui_startup_preserves_override_failure_for_the_fatal_frontend() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(
            temp.path().join("config.json"),
            r#"{"runtime":"builtin-experimental"}"#,
        )
        .unwrap();
        for variable in AMBIENT_MSB_OVERRIDES {
            let env = EnvSnapshot::with_overrides([
                (AWMAN_CONFIG_HOME, temp.path().to_str().unwrap()),
                (*variable, ""),
            ]);
            let outcome = Startup::new(Vec::new())
                .run(temp.path().into(), env)
                .unwrap();
            let failure = outcome
                .fatal_runtime_error
                .expect("builtin open error must reach the modal");
            assert_eq!(failure.error.exit_code(), 1);
            assert_eq!(
                failure.error.to_string(),
                EngineError::AmbientRuntimeOverride {
                    variable: variable.to_string()
                }
                .to_string()
            );
            assert!(matches!(
                failure.error,
                CommandError::Engine(EngineError::AmbientRuntimeOverride { .. })
            ));
        }
    }

    #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
    #[test]
    fn tui_startup_preserves_the_unsupported_target_error() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(
            temp.path().join("config.json"),
            r#"{"runtime":"builtin-experimental"}"#,
        )
        .unwrap();
        let env = EnvSnapshot::with_overrides([(AWMAN_CONFIG_HOME, temp.path().to_str().unwrap())]);
        let outcome = Startup::new(Vec::new())
            .run(temp.path().into(), env)
            .unwrap();
        assert!(matches!(
            outcome.fatal_runtime_error.unwrap().error,
            CommandError::Engine(EngineError::BuiltinTargetUnreleased { .. })
        ));
    }
}
