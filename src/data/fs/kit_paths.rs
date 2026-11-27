//! Filesystem-resolution for host-global Docker Sandbox (`sbx`) kit
//! directories.
//!
//! v3 kits are host-global (`<home>/kits/v3/<handle>/`),
//! unlike the per-repo `.awman/Dockerfile.<agent>` files used by the Docker and
//! Apple runtimes. Resolving these paths is a Layer 0 concern; emitting the kit
//! contents (descriptor, recipe and build context) is Layer 1. The v3
//! directory of one agent is selected by the engine's SBX matrix
//! (`engine::agent::sbx_contract::kit_dir_v3`) under [`SandboxKitPaths::kits_root`].

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
}
