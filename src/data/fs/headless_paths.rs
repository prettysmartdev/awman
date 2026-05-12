//! Typed accessors for headless-mode storage paths.
//!
//! Replaces ad-hoc `dirs::data_dir().join("amux/headless/...")` calls scattered
//! through `oldsrc/commands/headless/`.

use std::path::{Path, PathBuf};

use crate::data::config::env::{Env, EnvSnapshot};
use crate::data::error::DataError;

/// Filename of the headless sqlite database.
pub const HEADLESS_DB_FILENAME: &str = "amux.db";

/// Subdirectory under the global home that hosts headless state.
const HEADLESS_SUBDIR: &str = "headless";

/// Subdirectory holding per-session command logs.
const SESSIONS_SUBDIR: &str = "sessions";

/// Subdirectory holding TLS materials.
const TLS_SUBDIR: &str = "tls";

/// Resolves every path under the headless storage root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadlessPaths {
    root: PathBuf,
}

impl HeadlessPaths {
    /// Build a `HeadlessPaths` rooted at an explicit directory.
    pub fn from_root(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Resolve from the current process environment, honouring `AMUX_HEADLESS_ROOT`
    /// when set, otherwise falling back to `$HOME/.amux/headless`.
    pub fn from_process_env() -> Result<Self, DataError> {
        Self::from_env(&Env::from_process())
    }

    /// Same as [`from_process_env`] but reads from a supplied env snapshot.
    pub fn from_env(env: &EnvSnapshot) -> Result<Self, DataError> {
        if let Some(root) = env.headless_root() {
            return Ok(Self::from_root(root));
        }
        let home = dirs::home_dir().ok_or(DataError::HomeNotFound)?;
        Ok(Self::from_root(home.join(".amux").join(HEADLESS_SUBDIR)))
    }

    /// The headless root directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Path to the headless sqlite database.
    pub fn db_path(&self) -> PathBuf {
        self.root.join(HEADLESS_DB_FILENAME)
    }

    /// Directory holding per-session subdirectories.
    pub fn sessions_dir(&self) -> PathBuf {
        self.root.join(SESSIONS_SUBDIR)
    }

    /// Directory for a single session's command output.
    pub fn session_dir(&self, session_id: &str) -> PathBuf {
        self.sessions_dir().join(session_id)
    }

    /// Directory for command logs within a session.
    pub fn session_commands_dir(&self, session_id: &str) -> PathBuf {
        self.session_dir(session_id).join("commands")
    }

    /// Directory for one command's logs.
    pub fn command_dir(&self, session_id: &str, command_id: &str) -> PathBuf {
        self.session_commands_dir(session_id).join(command_id)
    }

    /// Default log path for a single command run.
    pub fn command_log_path(&self, session_id: &str, command_id: &str) -> PathBuf {
        self.command_dir(session_id, command_id).join("output.log")
    }

    /// TLS material directory.
    pub fn tls_dir(&self) -> PathBuf {
        self.root.join(TLS_SUBDIR)
    }

    /// PEM-encoded TLS certificate.
    pub fn tls_cert_file(&self) -> PathBuf {
        self.tls_dir().join("cert.pem")
    }

    /// PEM-encoded TLS private key (mode 0o600 on Unix).
    pub fn tls_key_file(&self) -> PathBuf {
        self.tls_dir().join("key.pem")
    }

    /// Sidecar file recording the bind IP that the cert was generated for.
    /// Used to detect SAN-mismatch and trigger regeneration safely without
    /// having to parse DER.
    pub fn tls_bind_ip_file(&self) -> PathBuf {
        self.tls_dir().join("bind_ip")
    }

    /// Headless server PID file.
    pub fn pid_file(&self) -> PathBuf {
        self.root.join("amux.pid")
    }

    /// Sidecar metadata for the running server (port, scheme). Written next
    /// to the PID file so `headless status` can HTTP-probe the right
    /// endpoint without needing CLI flags.
    pub fn server_meta_file(&self) -> PathBuf {
        self.root.join("server.json")
    }

    /// Headless server log file.
    pub fn log_file(&self) -> PathBuf {
        self.root.join("amux.log")
    }

    /// API key hash file (mode 0o600 on Unix).
    pub fn api_key_hash_file(&self) -> PathBuf {
        self.root.join("api_key.hash")
    }

    /// Workflow state file for a single command run.
    pub fn command_workflow_state_path(&self, session_id: &str, command_id: &str) -> PathBuf {
        self.command_dir(session_id, command_id)
            .join("workflow.state.json")
    }

    /// Metadata file for a single command run.
    pub fn command_metadata_path(&self, session_id: &str, command_id: &str) -> PathBuf {
        self.command_dir(session_id, command_id)
            .join("metadata.json")
    }

    /// Per-session worktree directory.
    pub fn session_worktree_dir(&self, session_id: &str) -> PathBuf {
        self.session_dir(session_id).join("worktree")
    }

    /// Per-session agent settings directory.
    pub fn session_agent_settings_dir(&self, session_id: &str) -> PathBuf {
        self.session_dir(session_id).join("agent-settings")
    }

    /// Alias for `from_root` to match the legacy `at_root` naming.
    pub fn at_root(root: impl Into<PathBuf>) -> Self {
        Self::from_root(root)
    }

    /// Create the root directory (and parents) on disk.
    pub fn ensure_root(&self) -> Result<(), DataError> {
        std::fs::create_dir_all(&self.root).map_err(|e| DataError::io(&self.root, e))
    }
}
