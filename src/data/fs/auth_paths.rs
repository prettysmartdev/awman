//! Filesystem-resolution for per-agent host-side credential and settings
//! paths.
//!
//! Resolving these paths is a Layer 0 concern; the *passthrough into containers*
//! (copying files, building bind mounts, scrubbing secrets, …) is Layer 1.

use std::path::PathBuf;

use crate::data::error::DataError;

/// Per-agent collection of host-side credential and settings paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentAuthPaths {
    /// Agent name (`"claude"`, `"codex"`, `"opencode"`, …).
    pub agent: String,
    /// Top-level config file (e.g. `~/.claude.json`). May be absent.
    pub config_file: Option<PathBuf>,
    /// Top-level settings directory (e.g. `~/.claude`, `~/.codex`, `~/.gemini`,
    /// `~/.config/opencode`). May be absent.
    pub settings_dir: Option<PathBuf>,
}

/// Resolves host-side credential and settings paths for known agents.
#[derive(Debug, Clone)]
pub struct AuthPathResolver {
    home: PathBuf,
}

impl AuthPathResolver {
    /// Construct a resolver rooted at the supplied home directory.
    pub fn at_home(home: impl Into<PathBuf>) -> Self {
        Self { home: home.into() }
    }

    /// Resolve from the current process's home directory.
    pub fn from_process_env() -> Result<Self, DataError> {
        let home = dirs::home_dir().ok_or(DataError::HomeNotFound)?;
        Ok(Self::at_home(home))
    }

    /// Home directory the resolver was bound to.
    pub fn home(&self) -> &std::path::Path {
        &self.home
    }

    /// Resolve every known auth path for the given agent name.
    ///
    /// Returns `AgentAuthPaths` with `None` fields when the agent has no
    /// known on-host artefacts.
    pub fn resolve(&self, agent: &str) -> AgentAuthPaths {
        match agent {
            "claude" => AgentAuthPaths {
                agent: agent.to_string(),
                config_file: Some(self.home.join(".claude.json")),
                settings_dir: Some(self.home.join(".claude")),
            },
            "codex" => AgentAuthPaths {
                agent: agent.to_string(),
                config_file: None,
                settings_dir: Some(self.home.join(".codex")),
            },
            "gemini" => AgentAuthPaths {
                agent: agent.to_string(),
                config_file: None,
                settings_dir: Some(self.home.join(".gemini")),
            },
            "antigravity" => AgentAuthPaths {
                agent: agent.to_string(),
                config_file: None,
                settings_dir: Some(self.home.join(".gemini")),
            },
            "opencode" => AgentAuthPaths {
                agent: agent.to_string(),
                config_file: None,
                settings_dir: Some(self.home.join(".config").join("opencode")),
            },
            _ => AgentAuthPaths {
                agent: agent.to_string(),
                config_file: None,
                settings_dir: None,
            },
        }
    }
}
