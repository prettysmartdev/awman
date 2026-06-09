//! Per-repo Dockerfile path resolution — Layer 0.
//!
//! Resolves project and per-agent Dockerfile paths relative to `<git_root>`.
//! Pure path computation — no I/O beyond `Path::join`.

use std::path::{Path, PathBuf};

/// Resolves Dockerfile paths beneath `<git_root>/.awman/`.
#[derive(Debug, Clone)]
pub struct RepoDockerfilePaths {
    git_root: PathBuf,
    project_dockerfile_override: Option<PathBuf>,
}

impl RepoDockerfilePaths {
    pub fn new(git_root: impl Into<PathBuf>) -> Self {
        Self {
            git_root: git_root.into(),
            project_dockerfile_override: None,
        }
    }

    /// Construct with an optional override path for the project Dockerfile.
    /// When provided, `project_dockerfile()` returns the override instead of
    /// the default `<git_root>/Dockerfile.dev`.
    pub fn with_project_dockerfile(git_root: impl Into<PathBuf>, path: Option<PathBuf>) -> Self {
        Self {
            git_root: git_root.into(),
            project_dockerfile_override: path,
        }
    }

    /// Project base image Dockerfile. Returns the configured override when
    /// provided, otherwise `<git_root>/Dockerfile.dev`.
    pub fn project_dockerfile(&self) -> PathBuf {
        if let Some(ref p) = self.project_dockerfile_override {
            return p.clone();
        }
        self.git_root.join("Dockerfile.dev")
    }

    /// `<git_root>/.awman/Dockerfile.<agent>` — per-agent layered Dockerfile.
    pub fn agent_dockerfile(&self, agent: &str) -> PathBuf {
        self.git_root
            .join(".awman")
            .join(format!("Dockerfile.{agent}"))
    }

    /// `<git_root>/aspec/` — spec and work-items directory.
    pub fn aspec_root(&self) -> PathBuf {
        self.git_root.join("aspec")
    }

    /// `<git_root>/.awman/` — directory holding agent dockerfiles and engine state.
    pub fn awman_dir(&self) -> PathBuf {
        self.git_root.join(".awman")
    }

    pub fn git_root(&self) -> &Path {
        &self.git_root
    }

    /// Discover all per-agent Dockerfiles in `.awman/`.
    /// Returns `(agent_name, path)` for each `Dockerfile.<agent>` found.
    pub fn discover_agent_dockerfiles(&self) -> Vec<(String, PathBuf)> {
        let awman_dir = self.awman_dir();
        let mut result = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&awman_dir) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name_str = name.to_string_lossy().to_string();
                if let Some(agent) = name_str.strip_prefix("Dockerfile.") {
                    if !agent.is_empty() {
                        result.push((agent.to_string(), entry.path()));
                    }
                }
            }
        }
        result.sort_by(|a, b| a.0.cmp(&b.0));
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_dockerfile_at_repo_root() {
        let p = RepoDockerfilePaths::new("/r");
        assert_eq!(p.project_dockerfile(), Path::new("/r/Dockerfile.dev"));
    }

    #[test]
    fn project_dockerfile_returns_override_when_supplied() {
        let p = RepoDockerfilePaths::with_project_dockerfile(
            "/r",
            Some(PathBuf::from("/custom/Dockerfile")),
        );
        assert_eq!(p.project_dockerfile(), Path::new("/custom/Dockerfile"));
    }

    #[test]
    fn project_dockerfile_returns_default_when_no_override() {
        let p = RepoDockerfilePaths::with_project_dockerfile("/r", None);
        assert_eq!(p.project_dockerfile(), Path::new("/r/Dockerfile.dev"));
    }

    #[test]
    fn agent_dockerfile_under_dot_awman() {
        let p = RepoDockerfilePaths::new("/r");
        assert_eq!(
            p.agent_dockerfile("claude"),
            Path::new("/r/.awman/Dockerfile.claude")
        );
    }
}
