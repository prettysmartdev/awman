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

/// The deterministic sandbox name awman uses for a workspace + agent pair —
/// the single source of truth for launch, restart, and tests.
pub fn sandbox_name_for(workspace: &std::path::Path, agent: &str) -> String {
    generate_sandbox_name(&worktree_hash(workspace), agent)
}

/// Short deterministic hash of a worktree path — same inputs always produce
/// the same value, so a later invocation finds the existing sandbox.
///
/// FNV-1a rather than `DefaultHasher`: the std hasher's algorithm is not
/// guaranteed stable across Rust releases, and the name must survive awman
/// upgrades or the persistent sandbox (and its one-time install cost) would
/// be silently orphaned.
pub fn worktree_hash(workspace: &std::path::Path) -> String {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = FNV_OFFSET;
    for byte in workspace.to_string_lossy().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    format!("{:08x}", hash & 0xffff_ffff)
}
