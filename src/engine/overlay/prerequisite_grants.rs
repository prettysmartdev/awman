//! Container overlays produced from workflow prerequisite grants.

use crate::data::workflow_prerequisites::{GitRemoteUrl, PrerequisiteStepKind};
use crate::data::workflow_state::PhaseKind;
use crate::engine::auth::credential::SecretString;
use crate::engine::container::options::OverlaySpec;
use crate::engine::workflow::step_commands::PushAuth;

pub const GRANT_SSH_KEY_CONTAINER_NAME: &str = "awman_push_key";
pub const GRANT_TOKEN_ENV_GH: &str = "GH_TOKEN";
pub const GRANT_TOKEN_ENV_GIT: &str = "AWMAN_GIT_TOKEN";

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
    pub secret_env: Vec<(String, SecretString)>,
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
            secret_env: Vec::new(),
            push_auth: PushAuth::None,
            display: Vec::new(),
            scratch: None,
        }
    }
    pub fn is_empty(&self) -> bool {
        self.overlays.is_empty() && self.env_literals.is_empty() && self.secret_env.is_empty()
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
            .field("secret_env", &self.secret_names())
            .field("push_auth", &self.push_auth)
            .field("display", &self.display)
            .finish()
    }
}
