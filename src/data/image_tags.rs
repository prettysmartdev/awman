//! Image tag and repo-hash helpers — Layer 0.
//!
//! Pure functions used by `AgentEngine` and `ContainerRuntime` to derive
//! deterministic image tags from a git-root path. Layer 0 owns this so both
//! engines can share the same algorithm without one calling the other.

use std::path::Path;

use crate::data::fs::workflow_state::sha256_hex;

/// 8-hex-char SHA-256 prefix of the canonicalized git-root path. Used as a
/// stable identifier for per-repo image tags and per-repo state filenames.
pub fn repo_hash(git_root: &Path) -> String {
    let canon = std::fs::canonicalize(git_root)
        .ok()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|| git_root.to_string_lossy().to_string());
    sha256_hex(&canon).chars().take(8).collect()
}

/// Project (base) image tag: `amux-<repo-folder>:latest`.
///
/// Falls back to `amux-repo:latest` when the git-root has no file_name() (root `/`).
pub fn project_image_tag(git_root: &Path) -> String {
    let folder = git_root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("repo");
    format!("amux-{folder}:latest")
}

/// Per-agent image tag: `amux-<repo-folder>-<agent>:latest`.
pub fn agent_image_tag(git_root: &Path, agent: &str) -> String {
    let folder = git_root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("repo");
    format!("amux-{folder}-{agent}:latest")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn project_image_tag_uses_folder_name() {
        let p = PathBuf::from("/tmp/myproj");
        assert_eq!(project_image_tag(&p), "amux-myproj:latest");
    }

    #[test]
    fn agent_image_tag_includes_agent() {
        let p = PathBuf::from("/tmp/myproj");
        assert_eq!(agent_image_tag(&p, "claude"), "amux-myproj-claude:latest");
    }

    #[test]
    fn repo_hash_is_eight_hex_chars() {
        let p = PathBuf::from("/nonexistent/path");
        let h = repo_hash(&p);
        assert_eq!(h.len(), 8);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
