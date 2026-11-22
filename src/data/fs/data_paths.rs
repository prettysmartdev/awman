//! `DataPaths` — the awman home layout and the shared data root.
//!
//! Every awman path derives from two homes:
//!
//! | Need | `AWMAN_HOME` set | unset, XDG set | unset, no XDG |
//! |---|---|---|---|
//! | config home (`config.json`) | `$AWMAN_HOME` | `$XDG_CONFIG_HOME/awman` | `~/.awman` |
//! | data home (everything else) | `$AWMAN_HOME` | `$XDG_DATA_HOME/awman` | `~/.awman` |
//!
//! Under the data home: `data/awman.db`, `data/repos/<hash>/state.json`,
//! `api/`, `squad/`, `attach/`, `builtin/`, and the other data dirs.
use std::path::{Path, PathBuf};

use crate::data::config::env::{Env, EnvSnapshot};
use crate::data::error::DataError;

/// The one process variable that relocates every awman path.
pub const AWMAN_HOME: &str = "AWMAN_HOME";

/// Subdirectory of `$HOME` used when neither `AWMAN_HOME` nor XDG is set.
pub const HOME_SUBDIR: &str = ".awman";

/// `AWMAN_HOME`, when set and non-empty.
pub fn awman_home(env: &EnvSnapshot) -> Option<PathBuf> {
    env.get(AWMAN_HOME)
        .filter(|v| !v.trim().is_empty())
        .map(PathBuf::from)
}

/// Where `config.json` lives: `$AWMAN_HOME` → `$XDG_CONFIG_HOME/awman` → `~/.awman`.
pub fn config_home(env: &EnvSnapshot) -> Result<PathBuf, DataError> {
    if let Some(home) = awman_home(env) {
        return Ok(home);
    }
    if let Some(xdg) = env.xdg_config_home() {
        return Ok(xdg.join("awman"));
    }
    user_home(env)
}

/// Where everything else lives: `$AWMAN_HOME` → `$XDG_DATA_HOME/awman` → `~/.awman`.
pub fn data_home(env: &EnvSnapshot) -> Result<PathBuf, DataError> {
    if let Some(home) = awman_home(env) {
        return Ok(home);
    }
    if let Some(xdg) = env.xdg_data_home() {
        return Ok(xdg.join("awman"));
    }
    user_home(env)
}

fn user_home(env: &EnvSnapshot) -> Result<PathBuf, DataError> {
    let home = env
        .home_dir()
        .or_else(dirs::home_dir)
        .ok_or(DataError::HomeNotFound)?;
    Ok(home.join(HOME_SUBDIR))
}

/// Subdirectory under the data home that hosts the shared database.
pub const DAEMON_SUBDIR: &str = "daemon";

pub const DATA_SUBDIR: &str = "data";

/// Filename of the shared sqlite database. Identical to
/// `api_paths::API_DB_FILENAME`.
pub const DB_FILENAME: &str = "awman.db";

mod migration;
pub use migration::{
    MigrationFault, MigrationFileRecord, MigrationJournal, MigrationPhase, MigrationStep,
    StoreOpenOptions, MIGRATION_JOURNAL_FILENAME, MIGRATION_JOURNAL_SCHEMA,
    MIGRATION_LOCK_FILENAME, MIGRATION_LOCK_WAIT, MIGRATION_STAGING_PREFIX,
};

/// Result of relocating the legacy API-owned database into the shared data
/// directory. Presentation layers decide whether and how to report it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MigrationOutcome {
    FreshInstall,
    AlreadyMigrated,
    Migrated {
        copied_bytes: u64,
        sidecars: Vec<PathBuf>,
        backup_kept: bool,
    },
    /// A journaled migration interrupted earlier was completed, or redone
    /// from the untouched legacy files.
    RecoveredFromInterrupted {
        copied_bytes: u64,
    },
}

/// Resolves paths under the shared data root (`<data_home>/data`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataPaths {
    root: PathBuf,
}

impl DataPaths {
    /// Build `DataPaths` rooted at an explicit `<data_home>/data` directory.
    pub fn at_root(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Resolve from the current process environment.
    pub fn from_process_env() -> Result<Self, DataError> {
        Self::from_env(&Env::from_process())
    }

    /// Resolve from a supplied env snapshot: `<data_home>/data`.
    pub fn from_env(env: &EnvSnapshot) -> Result<Self, DataError> {
        let data_home = data_home(env)?;
        Ok(Self::at_root(data_home.join(DATA_SUBDIR)))
    }

    /// Where `config.json` lives for `env` (see [`config_home`]).
    pub fn config_home(env: &EnvSnapshot) -> Result<PathBuf, DataError> {
        config_home(env)
    }

    /// The shared data root (`<data_home>/data`).
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The single daemon identity directory.
    pub fn daemon(&self) -> crate::data::fs::daemon_paths::DaemonPaths {
        crate::data::fs::daemon_paths::DaemonPaths::new(self.root.join(DAEMON_SUBDIR))
    }

    /// The data home this root sits in (`<data_home>`).
    pub fn home(&self) -> &Path {
        self.root.parent().unwrap_or(&self.root)
    }

    /// Owner-private provider profiles for the invoking repository.
    pub fn agent_profiles_dir(&self, repo_root: &Path) -> PathBuf {
        self.repo_state_dir(repo_root)
            .join(crate::data::settings_profile::AGENT_PROFILES_DIR)
    }

    /// Per-repo state directory: `<root>/repos/<first 16 hex of sha256(git_root)>`.
    pub fn repo_state_dir(&self, git_root: &Path) -> PathBuf {
        let hash = crate::data::fs::hash::sha256_hex(git_root.to_string_lossy().as_bytes());
        self.root.join("repos").join(&hash[..16])
    }

    /// Path to the shared sqlite database (`<root>/awman.db`).
    pub fn db_path(&self) -> PathBuf {
        self.root.join(DB_FILENAME)
    }

    /// Create the data root (and parents) on disk.
    pub fn ensure_root(&self) -> Result<(), DataError> {
        // The data home is also the ancestor of builtin state. A permissive
        // user umask must not make a fresh installation group-writable.
        // Existing directories retain their permissions and are checked by
        // the consumer that requires a private/trusted path.
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        builder
            .create(&self.root)
            .map_err(|e| DataError::io(&self.root, e))
    }
}
