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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn resolver() -> AuthPathResolver {
        AuthPathResolver::at_home("/home/testuser")
    }

    #[test]
    fn resolve_claude_has_config_file_and_settings_dir() {
        let r = resolver();
        let paths = r.resolve("claude");
        assert_eq!(paths.agent, "claude");
        assert_eq!(
            paths.config_file,
            Some(Path::new("/home/testuser/.claude.json").to_path_buf())
        );
        assert_eq!(
            paths.settings_dir,
            Some(Path::new("/home/testuser/.claude").to_path_buf())
        );
    }

    #[test]
    fn resolve_codex_has_only_settings_dir() {
        let r = resolver();
        let paths = r.resolve("codex");
        assert_eq!(paths.agent, "codex");
        assert_eq!(paths.config_file, None);
        assert_eq!(
            paths.settings_dir,
            Some(Path::new("/home/testuser/.codex").to_path_buf())
        );
    }

    #[test]
    fn resolve_gemini_has_only_settings_dir() {
        let r = resolver();
        let paths = r.resolve("gemini");
        assert_eq!(paths.agent, "gemini");
        assert_eq!(paths.config_file, None);
        assert_eq!(
            paths.settings_dir,
            Some(Path::new("/home/testuser/.gemini").to_path_buf())
        );
    }

    #[test]
    fn resolve_opencode_has_settings_dir_under_config() {
        let r = resolver();
        let paths = r.resolve("opencode");
        assert_eq!(paths.agent, "opencode");
        assert_eq!(paths.config_file, None);
        assert_eq!(
            paths.settings_dir,
            Some(Path::new("/home/testuser/.config/opencode").to_path_buf())
        );
    }

    #[test]
    fn resolve_unknown_agent_returns_both_none() {
        let r = resolver();
        let paths = r.resolve("completely-unknown-agent");
        assert_eq!(paths.agent, "completely-unknown-agent");
        assert_eq!(paths.config_file, None);
        assert_eq!(paths.settings_dir, None);
    }

    #[test]
    fn at_home_stores_correct_home() {
        let r = AuthPathResolver::at_home("/custom/home");
        assert_eq!(r.home(), Path::new("/custom/home"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn resolve_claude_linux_paths_are_correct() {
        let r = AuthPathResolver::at_home("/home/alice");
        let paths = r.resolve("claude");
        assert_eq!(
            paths.config_file.unwrap(),
            Path::new("/home/alice/.claude.json")
        );
        assert_eq!(
            paths.settings_dir.unwrap(),
            Path::new("/home/alice/.claude")
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn resolve_claude_macos_paths_are_correct() {
        let r = AuthPathResolver::at_home("/Users/alice");
        let paths = r.resolve("claude");
        assert_eq!(
            paths.config_file.unwrap(),
            Path::new("/Users/alice/.claude.json")
        );
        assert_eq!(
            paths.settings_dir.unwrap(),
            Path::new("/Users/alice/.claude")
        );
    }
}
