//! Filesystem-resolution for host-global Docker Sandbox (`sbx`) kit
//! directories.
//!
//! sbx kits are host-global (one per agent under `$HOME/.awman/kits/<agent>/`),
//! unlike the per-repo `.awman/Dockerfile.<agent>` files used by the Docker and
//! Apple runtimes. Resolving these paths is a Layer 0 concern; emitting the kit
//! contents (rendering `spec.yaml`, copying the startup script) is Layer 1.

use std::path::{Path, PathBuf};

use crate::data::error::DataError;

/// Resolves host-global kit directory paths for the sbx runtime.
#[derive(Debug, Clone)]
pub struct SandboxKitPaths {
    /// The resolved `$HOME/.awman` (or `$AWMAN_HOME`) directory.
    home: PathBuf,
}

impl SandboxKitPaths {
    /// Construct a resolver rooted at the supplied awman home directory.
    pub fn at_home(home: impl Into<PathBuf>) -> Self {
        Self { home: home.into() }
    }

    /// Resolve from the current process environment, honouring
    /// `AWMAN_HOME` / `XDG_CONFIG_HOME` exactly as the global config does.
    pub fn from_process_env() -> Result<Self, DataError> {
        Ok(Self::at_home(crate::data::fs::data_paths::config_home(
            &crate::data::config::env::Env::from_process(),
        )?))
    }

    /// The awman home directory this resolver is bound to.
    pub fn home(&self) -> &Path {
        &self.home
    }

    /// Root directory holding every emitted kit: `<home>/kits`.
    pub fn kits_root(&self) -> PathBuf {
        self.home.join("kits")
    }

    /// Directory for a single agent's kit: `<home>/kits/<agent>`.
    pub fn kit_dir(&self, agent: &str) -> PathBuf {
        self.kits_root().join(agent)
    }

    /// The rendered kit manifest: `<home>/kits/<agent>/spec.yaml`.
    pub fn spec_file(&self, agent: &str) -> PathBuf {
        self.kit_dir(agent).join("spec.yaml")
    }

    /// The bundled-assets root copied into the VM's `/home/agent/`:
    /// `<home>/kits/<agent>/files/home`.
    pub fn files_home_dir(&self, agent: &str) -> PathBuf {
        self.kit_dir(agent).join("files").join("home")
    }

    /// The per-launch startup script:
    /// `<home>/kits/<agent>/files/home/.awman/apply-session-config.sh`.
    pub fn apply_script_file(&self, agent: &str) -> PathBuf {
        self.files_home_dir(agent)
            .join(".awman")
            .join("apply-session-config.sh")
    }
}
