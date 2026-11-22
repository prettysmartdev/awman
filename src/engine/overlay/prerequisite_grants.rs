//! Container overlays produced from workflow prerequisite grants.

use crate::data::workflow_prerequisites::{GitRemoteUrl, PrerequisiteStepKind};
use crate::data::workflow_state::PhaseKind;
use crate::engine::auth::credential::SecretString;
use crate::engine::container::options::OverlaySpec;
use crate::engine::workflow::step_commands::PushAuth;

pub const GRANT_SSH_KEY_CONTAINER_NAME: &str = "awman_push_key";
pub const GRANT_TOKEN_ENV_GH: &str = "GH_TOKEN";
pub const GRANT_TOKEN_ENV_GIT: &str = "AWMAN_GIT_TOKEN";

/// The commit identity variables for `identity`. Git reads these ahead of
/// any `user.name`/`user.email` config, so an agent's own `git commit`, a
/// `commit_changes` step and a teardown script all author and commit as it.
/// An explicit `git commit --author` still overrides the author (never the
/// committer); awman cannot prevent that, and a work server can check
/// authors when it receives the result.
pub fn identity_env(
    identity: &crate::engine::workflow::prerequisite_grants::GitIdentityGrant,
) -> Vec<(String, String)> {
    vec![
        ("GIT_AUTHOR_NAME".into(), identity.name.clone()),
        ("GIT_AUTHOR_EMAIL".into(), identity.email.clone()),
        ("GIT_COMMITTER_NAME".into(), identity.name.clone()),
        ("GIT_COMMITTER_EMAIL".into(), identity.email.clone()),
    ]
}

/// The grant env every agent container of a run receives (agent steps,
/// parallel groups, the dynamic leader and `on_failure` agents): the run-wide
/// fixed identity, and nothing else. Credentials never reach agents; setup
/// and teardown containers get the same identity through
/// [`OverlayEngine::prerequisite_grant_overlays`](super::OverlayEngine::prerequisite_grant_overlays).
pub fn agent_container_env(
    grants: &crate::engine::workflow::prerequisite_grants::PrerequisiteGrants,
) -> Vec<(String, String)> {
    grants
        .run_identity
        .as_ref()
        .map(identity_env)
        .unwrap_or_default()
}

#[derive(Debug, Clone, Copy)]
pub struct GrantTarget<'a> {
    pub repo: Option<&'a std::path::Path>,
    pub git_source: Option<&'a crate::engine::git::remote_push::WorkflowGitSource>,
    pub step_kind: PrerequisiteStepKind,
    pub phase: PhaseKind,
    pub container_home: Option<&'a str>,
    pub remote: Option<&'a GitRemoteUrl>,
    /// WI 0145: the running command's container credential socket directory
    /// (a `git_auth` push session). Mounted for `push_branch` and
    /// `pull_branch` when no grant supplies credentials for the remote.
    pub session_git_socket: Option<&'a crate::engine::git::credential_socket::CacheSocketListener>,
}

pub struct GrantOverlays {
    pub transport: Option<std::sync::Arc<crate::engine::git::remote_push::RemotePushGit>>,
    pub overlays: Vec<OverlaySpec>,
    pub env_literals: Vec<(String, String)>,
    /// `GIT_AUTHOR_*`/`GIT_COMMITTER_*` for the step, kept apart from
    /// `env_literals`: an identity alone changes no step command (a
    /// `push_branch` or `pull_branch` renders exactly as without grants).
    pub identity_env: Vec<(String, String)>,
    pub secret_env: Vec<(String, SecretString)>,
    /// Seed the phase container's in-memory Git cache through private stdin.
    pub git_cache_seed: Option<SecretString>,
    pub push_auth: PushAuth,
    pub display: Vec<String>,
    pub(crate) scratch: Option<tempfile::TempDir>,
}

impl GrantOverlays {
    pub fn none() -> Self {
        Self {
            transport: None,
            overlays: Vec::new(),
            env_literals: Vec::new(),
            identity_env: Vec::new(),
            secret_env: Vec::new(),
            git_cache_seed: None,
            push_auth: PushAuth::None,
            display: Vec::new(),
            scratch: None,
        }
    }
    pub fn is_empty(&self) -> bool {
        self.overlays.is_empty()
            && self.env_literals.is_empty()
            && self.secret_env.is_empty()
            && self.git_cache_seed.is_none()
    }
    pub fn secret_names(&self) -> Vec<&str> {
        self.secret_env
            .iter()
            .map(|(name, _)| name.as_str())
            .collect()
    }
}

impl std::fmt::Debug for GrantOverlays {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GrantOverlays")
            .field("overlays", &self.overlays)
            .field("env_literals", &self.env_literals)
            .field("identity_env", &self.identity_env)
            .field("secret_env", &self.secret_names())
            .field("push_auth", &self.push_auth)
            .field("display", &self.display)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::workflow::prerequisite_grants::{
        GitIdentityGrant, IdentitySource, PrerequisiteGrants,
    };

    fn identity(name: &str) -> GitIdentityGrant {
        GitIdentityGrant {
            name: name.into(),
            email: format!("{}@example.test", name.to_lowercase()),
            source: IdentitySource::Flag,
        }
    }

    fn grant(kind: PrerequisiteStepKind, grants: &PrerequisiteGrants) -> GrantOverlays {
        let home = tempfile::tempdir().unwrap();
        crate::engine::overlay::OverlayEngine::with_auth_resolver(
            crate::data::fs::auth_paths::AuthPathResolver::at_home(home.path()),
        )
        .prerequisite_grant_overlays(
            &GrantTarget {
                repo: None,
                git_source: None,
                step_kind: kind,
                phase: PhaseKind::Teardown,
                container_home: Some("/root"),
                remote: None,
                session_git_socket: None,
            },
            grants,
        )
        .unwrap()
    }

    #[test]
    fn the_run_identity_reaches_every_phase_step_and_changes_no_command() {
        let grants = PrerequisiteGrants {
            run_identity: Some(identity("Run")),
            ..Default::default()
        };
        assert_eq!(agent_container_env(&grants), identity_env(&identity("Run")));
        for kind in [
            PrerequisiteStepKind::CommitChanges,
            PrerequisiteStepKind::PushBranch,
            PrerequisiteStepKind::PullBranch,
            PrerequisiteStepKind::CloneRepo,
            PrerequisiteStepKind::CheckoutCreateBranch,
            PrerequisiteStepKind::RunShell,
            PrerequisiteStepKind::RunScript,
        ] {
            let out = grant(kind, &grants);
            assert_eq!(out.identity_env, identity_env(&identity("Run")), "{kind:?}");
            // Identity alone is no credential: the step renders as without grants.
            assert!(out.is_empty(), "{kind:?}: {out:?}");
            assert_eq!(out.push_auth, PushAuth::None);
        }
    }

    #[test]
    fn commit_changes_prefers_its_need_identity_and_none_keeps_the_container_identity() {
        let grants = PrerequisiteGrants {
            identity: Some(identity("Need")),
            run_identity: Some(identity("Run")),
            ..Default::default()
        };
        assert_eq!(
            grant(PrerequisiteStepKind::CommitChanges, &grants).identity_env,
            identity_env(&identity("Need"))
        );
        assert_eq!(
            grant(PrerequisiteStepKind::RunShell, &grants).identity_env,
            identity_env(&identity("Run"))
        );
        // A need-only identity (host, remembered, entered) stays in
        // commit_changes containers.
        let need_only = PrerequisiteGrants {
            identity: Some(identity("Need")),
            ..Default::default()
        };
        assert!(grant(PrerequisiteStepKind::RunShell, &need_only)
            .identity_env
            .is_empty());
        assert!(agent_container_env(&need_only).is_empty());
        assert!(grant(PrerequisiteStepKind::RunShell, &Default::default())
            .identity_env
            .is_empty());
    }
}
