//! Typed accessors for API-mode storage paths.
//!
//! Replaces ad-hoc `dirs::data_dir().join("awman/api/...")` calls scattered
//! through `oldsrc/commands/headless/`.

use std::path::{Path, PathBuf};

use crate::data::config::env::{Env, EnvSnapshot};
use crate::data::error::DataError;

/// Filename of the API sqlite database.
pub const API_DB_FILENAME: &str = "awman.db";

/// Subdirectory under the global home that hosts API state.
const API_SUBDIR: &str = "api";

/// Subdirectory holding per-session command logs.
const SESSIONS_SUBDIR: &str = "sessions";

/// Subdirectory holding TLS materials.
const TLS_SUBDIR: &str = "tls";

/// Resolves every path under the API storage root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiPaths {
    root: PathBuf,
}

impl ApiPaths {
    /// Build a `ApiPaths` rooted at an explicit directory.
    pub fn from_root(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Resolve from the current process environment, honouring `AWMAN_API_ROOT`
    /// when set, otherwise falling back to `$HOME/.awman/api`.
    pub fn from_process_env() -> Result<Self, DataError> {
        Self::from_env(&Env::from_process())
    }

    /// Same as [`from_process_env`] but reads from a supplied env snapshot.
    pub fn from_env(env: &EnvSnapshot) -> Result<Self, DataError> {
        if let Some(root) = env.api_root() {
            return Ok(Self::from_root(root));
        }
        let home = dirs::home_dir().ok_or(DataError::HomeNotFound)?;
        Ok(Self::from_root(home.join(".awman").join(API_SUBDIR)))
    }

    /// The API root directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Path to the API sqlite database.
    pub fn db_path(&self) -> PathBuf {
        self.root.join(API_DB_FILENAME)
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

    /// NDJSON `ExecutionEvent` log path for a single command/job.
    pub fn command_events_log_path(&self, session_id: &str, command_id: &str) -> PathBuf {
        self.command_dir(session_id, command_id).join("events.log")
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

    /// Sidecar file recording the SHA-256 fingerprint of the cert's DER
    /// bytes (hex). Cached at cert-generation time so we never need to
    /// re-parse PEM to recompute it on subsequent loads.
    pub fn tls_fingerprint_file(&self) -> PathBuf {
        self.tls_dir().join("fingerprint.sha256")
    }

    /// API server PID file.
    pub fn pid_file(&self) -> PathBuf {
        self.root.join("awman.pid")
    }

    /// Sidecar metadata for the running server (port, scheme). Written next
    /// to the PID file so `api status` can HTTP-probe the right
    /// endpoint without needing CLI flags.
    pub fn server_meta_file(&self) -> PathBuf {
        self.root.join("server.json")
    }

    /// API server log file.
    pub fn log_file(&self) -> PathBuf {
        self.root.join("awman.log")
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

    /// Directory for a remote session's cloned repository.
    pub fn remote_session_repo_path(&self, session_id: &str) -> PathBuf {
        self.session_dir(session_id).join("repo")
    }

    /// Directory for a remote session (parent of repo/).
    pub fn remote_session_dir(&self, session_id: &str) -> PathBuf {
        self.session_dir(session_id)
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
