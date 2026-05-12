//! Per-repo Dockerfile path resolution — Layer 0.
//!
//! Resolves `<git_root>/.amux/Dockerfile.dev` and `<git_root>/.amux/Dockerfile.<agent>`.
//! Pure path computation — no I/O beyond `Path::join`.

use std::path::{Path, PathBuf};

/// Resolves Dockerfile paths beneath `<git_root>/.amux/`.
#[derive(Debug, Clone)]
pub struct RepoDockerfilePaths {
    git_root: PathBuf,
}

impl RepoDockerfilePaths {
    pub fn new(git_root: impl Into<PathBuf>) -> Self {
        Self {
            git_root: git_root.into(),
        }
    }

    /// `<git_root>/Dockerfile.dev` — the project base image's Dockerfile.
    /// Lives at the repo root (NOT under `.amux/`) because the user is expected
    /// to author and version-control it.
    pub fn project_dockerfile(&self) -> PathBuf {
        self.git_root.join("Dockerfile.dev")
    }

    /// `<git_root>/.amux/Dockerfile.<agent>` — per-agent layered Dockerfile.
    pub fn agent_dockerfile(&self, agent: &str) -> PathBuf {
        self.git_root
            .join(".amux")
            .join(format!("Dockerfile.{agent}"))
    }

    /// `<git_root>/aspec/` — spec and work-items directory.
    pub fn aspec_root(&self) -> PathBuf {
        self.git_root.join("aspec")
    }

    /// `<git_root>/.amux/` — directory holding agent dockerfiles and engine state.
    pub fn amux_dir(&self) -> PathBuf {
        self.git_root.join(".amux")
    }

    pub fn git_root(&self) -> &Path {
        &self.git_root
    }

    /// Discover all per-agent Dockerfiles in `.amux/`.
    /// Returns `(agent_name, path)` for each `Dockerfile.<agent>` found.
    pub fn discover_agent_dockerfiles(&self) -> Vec<(String, PathBuf)> {
        let amux_dir = self.amux_dir();
        let mut result = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&amux_dir) {
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
    fn agent_dockerfile_under_dot_amux() {
        let p = RepoDockerfilePaths::new("/r");
        assert_eq!(
            p.agent_dockerfile("claude"),
            Path::new("/r/.amux/Dockerfile.claude")
        );
    }
}
