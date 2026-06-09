//! Sandbox naming helpers.
//!
//! Sandboxes are persistent per (worktree, agent) — unlike ephemeral
//! container names, the same inputs must always produce the same name so a
//! later invocation re-attaches to the existing sandbox (WI 0090).

/// Generate a deterministic sandbox name for a (worktree, agent) pair:
/// `awman-<worktree_hash>-<agent>`. Same inputs always produce the same
/// output.
pub fn generate_sandbox_name(worktree_hash: &str, agent: &str) -> String {
    format!("awman-{worktree_hash}-{agent}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_sandbox_name_is_deterministic() {
        let a = generate_sandbox_name("abc123", "claude");
        let b = generate_sandbox_name("abc123", "claude");
        assert_eq!(a, b);
    }

    #[test]
    fn generate_sandbox_name_format() {
        let name = generate_sandbox_name("deadbeef", "gemini");
        assert_eq!(name, "awman-deadbeef-gemini");
        assert!(name.starts_with("awman-"), "name must start with awman-");
    }

    #[test]
    fn generate_sandbox_name_different_inputs_differ() {
        let a = generate_sandbox_name("hash1", "claude");
        let b = generate_sandbox_name("hash2", "claude");
        let c = generate_sandbox_name("hash1", "gemini");
        assert_ne!(
            a, b,
            "different worktree hashes must produce different names"
        );
        assert_ne!(a, c, "different agents must produce different names");
    }
}
