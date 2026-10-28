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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::workflow_prerequisites::{PrerequisiteNeed, PrerequisiteOrigin};

    const TOKEN: &str = "ghp_VERY_SECRET_VALUE_42";

    fn token_grant() -> TokenGrant {
        TokenGrant {
            secret: SecretString::new(TOKEN),
            source: TokenGrantSource::Env("GH_TOKEN".into()),
            host: "github.com".into(),
            acquire: Some(AcquirePolicy {
                argv: vec!["op".into(), "read".into()],
                timeout_secs: 5,
                refresh: Default::default(),
                scope: crate::data::workflow_prerequisites::AcquireScope::Global,
                sha256: "h".into(),
            }),
        }
    }

    fn full() -> PrerequisiteGrants {
        PrerequisiteGrants {
            ssh_transports: Default::default(),
            ssh_destinations: Default::default(),
            token_destinations: Default::default(),
            identity: Some(GitIdentityGrant {
                name: "Ada".into(),
                email: "ada@x.test".into(),
                source: IdentitySource::Host,
            }),
            ssh: Some(SshGrant::Key {
                key_path: "/h/.ssh/id_ed25519".into(),
                known_hosts: None,
                passphrase_protected: Some(false),
            }),
            token: Some(token_grant()),
        }
    }

    fn needs(items: Vec<WorkflowPrerequisite>) -> WorkflowPrerequisites {
        WorkflowPrerequisites {
            origin: PrerequisiteOrigin::Static,
            needs: items
                .into_iter()
                .map(|prerequisite| PrerequisiteNeed {
                    prerequisite,
                    needed_by: vec![],
                })
                .collect(),
            advisories: vec![],
        }
    }

    #[test]
    fn debug_never_contains_token() {
        let grants = full();
        for text in [
            format!("{grants:?}"),
            format!("{:?}", grants.token),
            format!("{:#?}", grants),
            format!("{:?}", grants.token.as_ref().unwrap().secret),
        ] {
            assert!(!text.contains(TOKEN), "{text}");
            assert!(!text.contains("VERY_SECRET"), "{text}");
        }
        assert!(format!("{grants:?}").contains("<redacted>"));
        // Non-secret context is still visible for diagnostics.
        assert!(format!("{grants:?}").contains("GH_TOKEN"));
    }

    #[test]
    fn secret_debug_hides_value_prefix_and_length() {
        let text = format!("{:?}", SecretString::new("abcdef0123456789"));
        assert_eq!(text, "SecretString(<redacted>)");
        assert_eq!(
            format!("{:?}", SecretString::new("x")),
            format!("{:?}", SecretString::new("a-much-longer-secret"))
        );
    }

    #[test]
    fn retain_for_drops_unused_and_reports_them() {
        let mut grants = full();
        let dropped = grants.retain_for(&needs(vec![WorkflowPrerequisite::GitIdentity]));
        assert_eq!(dropped, vec![DroppedGrant::Ssh, DroppedGrant::Token]);
        assert!(grants.identity.is_some());
        assert!(grants.ssh.is_none());
        assert!(grants.token.is_none());
        assert!(grants.redaction_secrets().is_empty());

        let mut grants = full();
        let dropped = grants.retain_for(&needs(vec![]));
        assert_eq!(
            dropped,
            vec![
                DroppedGrant::Identity,
                DroppedGrant::Ssh,
                DroppedGrant::Token
            ]
        );
        assert!(grants.is_empty());
    }

    #[test]
    fn retain_for_keeps_what_each_prerequisite_needs() {
        for (prerequisite, keeps) in [
            (
                WorkflowPrerequisite::PushAuthSsh {
                    remote: "origin".into(),
                    host: "github.com".into(),
                },
                "ssh",
            ),
            (
                WorkflowPrerequisite::GitHubToken {
                    host: "github.com".into(),
                },
                "token",
            ),
            (
                WorkflowPrerequisite::PushAuthHttps {
                    remote: "origin".into(),
                    host: "github.com".into(),
                },
                "token",
            ),
            (WorkflowPrerequisite::GitIdentity, "identity"),
        ] {
            let mut grants = full();
            grants.retain_for(&needs(vec![prerequisite.clone()]));
            assert_eq!(grants.ssh.is_some(), keeps == "ssh", "{prerequisite:?}");
            assert_eq!(grants.token.is_some(), keeps == "token", "{prerequisite:?}");
            assert_eq!(
                grants.identity.is_some(),
                keeps == "identity",
                "{prerequisite:?}"
            );
        }
        // The unclassified PushAuth keeps neither SSH nor token: it must be resolved first.
        let mut grants = full();
        grants.retain_for(&needs(vec![WorkflowPrerequisite::PushAuth {
            remote: "origin".into(),
        }]));
        assert!(grants.ssh.is_none() && grants.token.is_none());
    }

    #[test]
    fn redaction_secrets_exposes_exactly_the_token() {
        let secrets = full().redaction_secrets();
        assert_eq!(secrets.len(), 1);
        assert_eq!(secrets[0].expose(), TOKEN);
        assert!(PrerequisiteGrants::default().redaction_secrets().is_empty());
        assert!(PrerequisiteGrants::default().is_empty());
        assert!(!full().is_empty());
    }

    #[test]
    fn grants_are_not_serializable() {
        // The grant types derive no serde traits, so no JSON path can leak them.
        let source = include_str!("prerequisite_grants.rs");
        let production = source.split("#[cfg(test)]").next().unwrap();
        assert!(!production.contains("Serialize"));
        assert!(!production.contains("Deserialize"));
    }
}

#[cfg(test)]
mod ci_scope_review_tests {
    use super::*;
    use crate::data::workflow_prerequisites::GitRemoteUrl;
    #[test]
    fn ci_does_not_reuse_a_token_approved_only_for_another_remote() {
        let mut grants = PrerequisiteGrants {
            token: Some(TokenGrant {
                secret: SecretString::new("fixture"),
                source: TokenGrantSource::Pasted,
                host: "github.com".into(),
                acquire: None,
            }),
            ..Default::default()
        };
        grants
            .token_destinations
            .insert("https://github.com/backup/repo".into());
        assert!(grants
            .token_for_remote(&GitRemoteUrl::parse("https://github.com/origin/repo"))
            .is_none());
        assert!(grants
            .token_for_remote(&GitRemoteUrl::parse("https://github.com/backup/repo"))
            .is_some());
        assert!(grants
            .token_for_remote(&GitRemoteUrl::parse("https://ghe.corp.test/backup/repo"))
            .is_none());
    }
}
