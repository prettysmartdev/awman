//! `worktree` section: merge-back behaviour for workflow worktree branches.

use serde::{Deserialize, Serialize};

/// The `worktree` block.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorktreeConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_dirty_merge_target: Option<OnDirtyMergeTarget>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge_fix: Option<MergeFixConfig>,
}

/// `worktree.mergeFix`.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MergeFixConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_failure: Option<OnMergeFailure>,
    /// Path relative to the git root of a prompt template replacing the
    /// built-in `merge-fix-prompt.md`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_template: Option<String>,
}

/// `--on-dirty-merge-target` / `worktree.onDirtyMergeTarget`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnDirtyMergeTarget {
    Ask,
    Commit,
    Stash,
    Fail,
}

impl std::str::FromStr for OnDirtyMergeTarget {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "ask" => Ok(Self::Ask),
            "commit" => Ok(Self::Commit),
            "stash" => Ok(Self::Stash),
            "fail" => Ok(Self::Fail),
            other => Err(format!(
                "unknown value '{other}' (expected ask, commit, stash or fail)"
            )),
        }
    }
}

/// `--on-merge-failure` / `worktree.mergeFix.onFailure`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnMergeFailure {
    Ask,
    Agent,
    Keep,
}

impl std::str::FromStr for OnMergeFailure {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "ask" => Ok(Self::Ask),
            "agent" => Ok(Self::Agent),
            "keep" => Ok(Self::Keep),
            other => Err(format!(
                "unknown value '{other}' (expected ask, agent or keep)"
            )),
        }
    }
}
