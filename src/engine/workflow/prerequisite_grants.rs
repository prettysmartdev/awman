//! Ephemeral, non-serializable grants used only while executing workflow steps.

use std::path::PathBuf;

use crate::data::config::workflows::AcquirePolicy;
use crate::data::workflow_prerequisites::{WorkflowPrerequisite, WorkflowPrerequisites};
use crate::engine::auth::credential::SecretString;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentitySource {
    Host,
    Entered,
    RepoConfig,
    GlobalConfig,
    Flag,
    Remembered,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitIdentityGrant {
    pub name: String,
    pub email: String,
    pub source: IdentitySource,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SshGrant {
    Key {
        key_path: PathBuf,
        known_hosts: Option<PathBuf>,
        passphrase_protected: Option<bool>,
    },
    WholeDir {
        ssh_dir: PathBuf,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenGrantSource {
    Env(String),
    Pasted,
    HostCommand(String),
    ApiSecret(String),
    GhAuthToken,
}

#[derive(Clone)]
pub struct TokenGrant {
    pub secret: SecretString,
    pub source: TokenGrantSource,
    pub host: String,
    pub acquire: Option<AcquirePolicy>,
}

impl std::fmt::Debug for TokenGrant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenGrant")
            .field("secret", &"<redacted>")
            .field("source", &self.source)
            .field("host", &self.host)
            .field("acquire", &self.acquire)
            .finish()
    }
}

#[derive(Clone, Default)]
pub struct PrerequisiteGrants {
    pub ssh_transports: std::collections::BTreeMap<String, SecretString>,
    /// Credential-stripped destinations approved during this run. Empty fails closed.
    pub ssh_destinations: std::collections::BTreeSet<String>,
    pub token_destinations: std::collections::BTreeSet<String>,
    pub identity: Option<GitIdentityGrant>,
    pub ssh: Option<SshGrant>,
    pub token: Option<TokenGrant>,
}

impl std::fmt::Debug for PrerequisiteGrants {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PrerequisiteGrants")
            .field("identity", &self.identity)
            .field("ssh", &self.ssh)
            .field("token", &self.token)
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DroppedGrant {
    Identity,
    Ssh,
    Token,
}

impl PrerequisiteGrants {
    pub fn token_for_remote(
        &self,
        remote: &crate::data::workflow_prerequisites::GitRemoteUrl,
    ) -> Option<SecretString> {
        let token = self.token.as_ref()?;
        let host = match &remote.kind {
            crate::data::workflow_prerequisites::GitRemoteKind::Https { host }
            | crate::data::workflow_prerequisites::GitRemoteKind::Ssh { host } => host,
            _ => return None,
        };
        (self.allows_token(&remote.redacted) && token.host.eq_ignore_ascii_case(host))
            .then(|| token.secret.clone())
    }
    pub fn allows_ssh(&self, destination: &str) -> bool {
        self.ssh_destinations.contains(destination)
    }
    pub fn allows_token(&self, destination: &str) -> bool {
        self.token_destinations.contains(destination)
    }
    pub fn is_empty(&self) -> bool {
        self.identity.is_none() && self.ssh.is_none() && self.token.is_none()
    }

    pub fn retain_for(&mut self, needs: &WorkflowPrerequisites) -> Vec<DroppedGrant> {
        let identity_needed = needs
            .needs
            .iter()
            .any(|n| matches!(n.prerequisite, WorkflowPrerequisite::GitIdentity));
        let ssh_needed = needs
            .needs
            .iter()
            .any(|n| matches!(n.prerequisite, WorkflowPrerequisite::PushAuthSsh { .. }));
        let token_needed = needs.needs.iter().any(|n| {
            matches!(
                n.prerequisite,
                WorkflowPrerequisite::GitHubToken { .. }
                    | WorkflowPrerequisite::PushAuthHttps { .. }
            )
        });
        let mut dropped = Vec::new();
        if !identity_needed && self.identity.take().is_some() {
            dropped.push(DroppedGrant::Identity);
        }
        if !ssh_needed && self.ssh.take().is_some() {
            dropped.push(DroppedGrant::Ssh);
        }
        if !token_needed && self.token.take().is_some() {
            dropped.push(DroppedGrant::Token);
        }
        dropped
    }

    pub fn redaction_secrets(&self) -> Vec<SecretString> {
        self.token.iter().map(|t| t.secret.clone()).collect()
    }
}
