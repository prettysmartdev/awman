//! Which workflow prerequisites apply to a run's root (WI 0088).
//!
//! A repository-backed run keeps every requirement its workflow declares (or
//! `WorkflowPrerequisites::assume_all()` assumes). A run whose root is a plain
//! directory — by captured decision, never by re-discovering an enclosing
//! repository — has no remote, upstream, branch or pull request to check, so
//! those requirements are *omitted* rather than reported as missing or
//! satisfied. A Git identity stays an optional choice there.
//!
//! The projection is applied by `WorkflowPrerequisiteFlow` right before it
//! hands requirements to the generic Layer 1 preflight, so every checkpoint
//! (interview, leader summary, run start, generated workflow, teardown
//! recheck) sees the same answer, and the probes only ever see what applies.

use crate::data::config::workflows::{CredentialUse, PrerequisitePolicy};
use crate::data::workflow_prerequisites::{
    PrerequisiteRoot, WorkflowPrerequisite, WorkflowPrerequisites,
};

/// How a plain-directory root treats the one Git prerequisite that can still
/// matter to it: the identity a `commit_changes` step would use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirectoryIdentity {
    /// Offer the identity as an optional choice (an interactive interview).
    /// Declining it is a decision, not a gap: no warning follows.
    Offer,
    /// Include the identity only when the resolved policy already allows one
    /// (an unattended run). Unset or declined omits it entirely.
    WhenAllowed,
}

/// The typed applicability of workflow prerequisites to one run's root.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PrerequisiteScope {
    /// Every requirement applies (the behavior of every non-squad caller).
    #[default]
    Repository,
    /// A plain directory: only agent images, non-Git tools and (optionally)
    /// the Git identity apply.
    Directory { identity: DirectoryIdentity },
}

impl PrerequisiteScope {
    /// What the Layer 1 preflight may read about the root.
    pub fn root(self) -> PrerequisiteRoot {
        match self {
            Self::Repository => PrerequisiteRoot::Repository,
            Self::Directory { .. } => PrerequisiteRoot::Directory,
        }
    }

    pub fn is_directory(self) -> bool {
        matches!(self, Self::Directory { .. })
    }

    /// Whether `prerequisite` applies to this root under `policy`.
    pub fn applies(self, prerequisite: &WorkflowPrerequisite, policy: &PrerequisitePolicy) -> bool {
        use WorkflowPrerequisite::*;
        let Self::Directory { identity } = self else {
            return true;
        };
        match prerequisite {
            AgentImage { .. } => true,
            GitIdentity => match identity {
                DirectoryIdentity::Offer => true,
                DirectoryIdentity::WhenAllowed => policy.git_identity.use_ == CredentialUse::Allow,
            },
            ContainerTool { name } => !is_git_or_pr_tool(name),
            GitRemote { .. }
            | PushAuth { .. }
            | PushAuthSsh { .. }
            | PushAuthHttps { .. }
            | GitHubToken { .. }
            | PushedBranch
            | NotDetachedHead => false,
        }
    }

    /// Whether an unsatisfied `prerequisite` is a choice the user may leave
    /// unmade rather than a gap to warn about.
    pub fn is_optional(self, prerequisite: &WorkflowPrerequisite) -> bool {
        self.is_directory() && *prerequisite == WorkflowPrerequisite::GitIdentity
    }

    /// `needs` without the requirements that do not apply. Step references
    /// and advisories are kept; nothing is marked satisfied.
    pub fn project(
        self,
        needs: &WorkflowPrerequisites,
        policy: &PrerequisitePolicy,
    ) -> WorkflowPrerequisites {
        let mut projected = needs.clone();
        projected
            .needs
            .retain(|need| self.applies(&need.prerequisite, policy));
        projected
    }
}

/// The container tools only Git and pull-request steps use.
fn is_git_or_pr_tool(name: &str) -> bool {
    matches!(name, "git" | "gh" | "ssh")
}
