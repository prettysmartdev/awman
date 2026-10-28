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

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::data::fs::auth_paths::AuthPathResolver;
    use crate::engine::overlay::OverlayEngine;
    use crate::engine::workflow::prerequisite_grants::{
        GitIdentityGrant, IdentitySource, PrerequisiteGrants, SshGrant, TokenGrant,
        TokenGrantSource,
    };
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    const TOKEN: &str = "ghp_GRANT_OVERLAY_TOKEN";

    fn engine() -> OverlayEngine {
        let home = tempfile::tempdir().unwrap();
        OverlayEngine::with_auth_resolver(AuthPathResolver::at_home(home.path()))
    }

    /// The overlay engine validates SSH paths against `dirs::home_dir()`. Only
    /// touch it when the test harness has isolated HOME.
    fn isolated() -> bool {
        crate::data::config::env::Env::from_process().test_isolation()
    }

    struct Key {
        path: PathBuf,
        known_hosts: PathBuf,
        ssh_dir: PathBuf,
    }
    impl Key {
        fn new(tag: &str) -> Self {
            let ssh_dir = dirs::home_dir().unwrap().join(".ssh");
            std::fs::create_dir_all(&ssh_dir).unwrap();
            let path = ssh_dir.join(format!("id_wi0149_{tag}_{}", std::process::id()));
            std::fs::write(&path, "-----BEGIN PRIVATE KEY-----\nFAKE\n").unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
            let known_hosts =
                ssh_dir.join(format!("known_hosts_wi0149_{tag}_{}", std::process::id()));
            std::fs::write(&known_hosts, "github.com ssh-ed25519 AAAA\n").unwrap();
            Self {
                path,
                known_hosts,
                ssh_dir,
            }
        }
    }
    impl Drop for Key {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
            let _ = std::fs::remove_file(&self.known_hosts);
        }
    }

    fn identity() -> GitIdentityGrant {
        GitIdentityGrant {
            name: "Ada Lovelace".into(),
            email: "ada@x.test".into(),
            source: IdentitySource::Host,
        }
    }
    fn token(host: &str) -> TokenGrant {
        TokenGrant {
            secret: SecretString::new(TOKEN),
            source: TokenGrantSource::Env("GH_TOKEN".into()),
            host: host.into(),
            acquire: None,
        }
    }
    fn target<'a>(kind: PrerequisiteStepKind, remote: Option<&'a GitRemoteUrl>) -> GrantTarget<'a> {
        GrantTarget {
            repo: None,
            git_source: None,
            step_kind: kind,
            phase: PhaseKind::Teardown,
            container_home: Some("/home/dev"),
            remote,
        }
    }
    fn everything(key: Option<&Key>) -> PrerequisiteGrants {
        PrerequisiteGrants {
            ssh_transports: Default::default(),
            ssh_destinations: [GitRemoteUrl::parse("git@github.com:o/r.git").redacted].into(),
            token_destinations: ["https://github.com/o/r".into()].into(),
            identity: Some(identity()),
            ssh: key.map(|k| SshGrant::Key {
                key_path: k.path.clone(),
                known_hosts: Some(k.known_hosts.clone()),
                passphrase_protected: Some(false),
            }),
            token: Some(token("github.com")),
        }
    }
    fn overlays(
        kind: PrerequisiteStepKind,
        remote: Option<&GitRemoteUrl>,
        grants: &PrerequisiteGrants,
    ) -> GrantOverlays {
        engine()
            .prerequisite_grant_overlays(&target(kind, remote), grants)
            .unwrap()
    }
    fn env_names(o: &GrantOverlays) -> Vec<String> {
        o.env_literals
            .iter()
            .map(|(k, _)| k.clone())
            .chain(o.secret_env.iter().map(|(k, _)| k.clone()))
            .collect()
    }

    #[test]
    fn commit_gets_identity_env_only() {
        let ssh = GitRemoteUrl::parse("git@github.com:o/r.git");
        let o = overlays(
            PrerequisiteStepKind::CommitChanges,
            Some(&ssh),
            &everything(None),
        );
        let mut names = env_names(&o);
        names.sort();
        assert_eq!(
            names,
            vec![
                "GIT_AUTHOR_EMAIL",
                "GIT_AUTHOR_NAME",
                "GIT_COMMITTER_EMAIL",
                "GIT_COMMITTER_NAME"
            ]
        );
        assert!(o.overlays.is_empty());
        assert!(o.secret_env.is_empty(), "identity is not a secret");
        assert_eq!(o.push_auth, PushAuth::None);
        let value = |n: &str| {
            o.env_literals
                .iter()
                .find(|(k, _)| k == n)
                .unwrap()
                .1
                .clone()
        };
        assert_eq!(value("GIT_AUTHOR_NAME"), "Ada Lovelace");
        assert_eq!(value("GIT_COMMITTER_EMAIL"), "ada@x.test");
        assert!(!format!("{o:?}").contains(TOKEN));
    }

    #[test]
    fn pr_gets_gh_token_only() {
        let https = GitRemoteUrl::parse("https://github.com/o/r");
        let o = overlays(
            PrerequisiteStepKind::CreatePullRequest,
            Some(&https),
            &everything(None),
        );
        assert_eq!(o.secret_names(), vec!["GH_TOKEN"]);
        assert_eq!(o.secret_env[0].1.expose(), TOKEN);
        assert!(o
            .env_literals
            .contains(&("GH_PROMPT_DISABLED".into(), "1".into())));
        assert!(!env_names(&o).iter().any(|n| n.starts_with("GIT_")));
        assert!(o.overlays.is_empty());
        assert_eq!(o.display, vec!["GH_TOKEN=***"]);
        assert!(!format!("{o:?}").contains(TOKEN));
        assert!(!o.display.join(" ").contains(TOKEN));
    }

    #[test]
    fn pr_for_enterprise_host_sets_gh_host() {
        let mut grants = everything(None);
        grants.token = Some(token("ghe.corp.example"));
        let remote = GitRemoteUrl::parse("https://ghe.corp.example/o/r");
        grants.token_destinations.insert(remote.redacted.clone());
        let o = overlays(
            PrerequisiteStepKind::CreatePullRequest,
            Some(&remote),
            &grants,
        );
        assert!(o
            .env_literals
            .contains(&("GH_HOST".into(), "ghe.corp.example".into())));
    }

    #[test]
    fn push_gets_key_mounts_only() {
        if !isolated() {
            return;
        }
        let key = Key::new("push");
        let ssh = GitRemoteUrl::parse("git@github.com:o/r.git");
        let o = overlays(
            PrerequisiteStepKind::PushBranch,
            Some(&ssh),
            &everything(Some(&key)),
        );
        assert_eq!(o.overlays.len(), 2);
        let key_mount = &o.overlays[0];
        assert_eq!(
            key_mount.host_path,
            std::fs::canonicalize(&key.path).unwrap()
        );
        assert_eq!(
            key_mount.container_path,
            PathBuf::from("/home/dev/.ssh").join(GRANT_SSH_KEY_CONTAINER_NAME)
        );
        assert_eq!(
            key_mount.permission,
            crate::data::config::overlays::OverlayPermission::ReadOnly
        );
        // known_hosts is a writable *copy*, never the host file.
        let kh = &o.overlays[1];
        assert_eq!(
            kh.container_path,
            PathBuf::from("/home/dev/.ssh/known_hosts")
        );
        assert_ne!(
            kh.host_path,
            std::fs::canonicalize(&key.known_hosts).unwrap()
        );
        assert_eq!(
            std::fs::read_to_string(&kh.host_path).unwrap(),
            "github.com ssh-ed25519 AAAA\n"
        );
        assert!(
            matches!(&o.push_auth, PushAuth::Ssh { key_path, known_hosts: Some(_) }
            if key_path == "/home/dev/.ssh/awman_push_key")
        );
        // No env at all for an SSH push: neither the token nor the identity.
        assert!(env_names(&o).is_empty(), "{:?}", env_names(&o));
        // The whole ssh dir is never mounted for a single-key grant.
        assert!(o.overlays.iter().all(|m| m.host_path != key.ssh_dir));
    }

    #[test]
    fn push_over_https_gets_token_by_env_name_and_no_mounts() {
        let https = GitRemoteUrl::parse("https://github.com/o/r");
        let o = overlays(
            PrerequisiteStepKind::PushBranch,
            Some(&https),
            &everything(None),
        );
        assert_eq!(o.secret_names(), vec!["AWMAN_GIT_TOKEN"]);
        assert!(o.overlays.is_empty());
        assert_eq!(
            o.push_auth,
            PushAuth::HttpsToken {
                env_var: "AWMAN_GIT_TOKEN".into()
            }
        );
        assert!(o
            .env_literals
            .contains(&("GIT_TERMINAL_PROMPT".into(), "0".into())));
        assert_eq!(o.display, vec!["AWMAN_GIT_TOKEN=***"]);
        // Identity never rides on a push container.
        assert!(!env_names(&o).iter().any(|n| n.starts_with("GIT_AUTHOR")));
    }

    #[test]
    fn push_over_https_to_other_host_gets_nothing() {
        let gitlab = GitRemoteUrl::parse("https://gitlab.com/o/r");
        let o = overlays(
            PrerequisiteStepKind::PushBranch,
            Some(&gitlab),
            &everything(None),
        );
        assert!(o.is_empty());
        // A token granted for a different host is not sent to github.com either.
        let mut grants = everything(None);
        grants.token = Some(token("ghe.corp.example"));
        let https = GitRemoteUrl::parse("https://github.com/o/r");
        assert!(overlays(PrerequisiteStepKind::PushBranch, Some(&https), &grants).is_empty());
    }

    #[test]
    fn ssh_grant_is_not_applied_to_https_remotes_and_whole_dir_is_explicit() {
        if !isolated() {
            return;
        }
        let key = Key::new("https");
        let https = GitRemoteUrl::parse("https://github.com/o/r");
        let mut grants = everything(Some(&key));
        grants.token = None;
        assert!(overlays(PrerequisiteStepKind::PushBranch, Some(&https), &grants).is_empty());
        // Whole-dir grant: one read-only directory mount for an SSH remote, nothing else.
        let ssh = GitRemoteUrl::parse("git@github.com:o/r.git");
        grants.ssh = Some(SshGrant::WholeDir {
            ssh_dir: key.ssh_dir.clone(),
        });
        let o = overlays(PrerequisiteStepKind::PushBranch, Some(&ssh), &grants);
        assert_eq!(o.overlays.len(), 1);
        assert_eq!(
            o.overlays[0].container_path,
            PathBuf::from("/home/dev/.ssh")
        );
        assert_eq!(
            o.overlays[0].permission,
            crate::data::config::overlays::OverlayPermission::ReadOnly
        );
        assert_eq!(o.push_auth, PushAuth::None);
    }

    #[test]
    fn run_shell_and_run_script_get_nothing() {
        if !isolated() {
            return;
        }
        let key = Key::new("shell");
        let ssh = GitRemoteUrl::parse("git@github.com:o/r.git");
        let https = GitRemoteUrl::parse("https://github.com/o/r");
        for kind in [
            PrerequisiteStepKind::RunShell,
            PrerequisiteStepKind::RunScript,
        ] {
            for remote in [Some(&ssh), Some(&https), None] {
                let o = overlays(kind, remote, &everything(Some(&key)));
                assert!(o.is_empty(), "{kind:?} received grants: {o:?}");
                assert_eq!(o.push_auth, PushAuth::None);
                assert!(o.display.is_empty());
            }
        }
    }

    #[test]
    fn poll_ci_and_checkout_get_nothing_from_the_overlay_engine() {
        for kind in [
            PrerequisiteStepKind::PollCi,
            PrerequisiteStepKind::CheckoutCreateBranch,
        ] {
            let o = overlays(kind, None, &everything(None));
            assert!(o.is_empty(), "{kind:?}");
        }
    }

    #[test]
    fn no_grants_means_no_overlays_for_any_step_kind() {
        let ssh = GitRemoteUrl::parse("git@github.com:o/r.git");
        for kind in [
            PrerequisiteStepKind::CommitChanges,
            PrerequisiteStepKind::PushBranch,
            PrerequisiteStepKind::CreatePullRequest,
            PrerequisiteStepKind::PullBranch,
            PrerequisiteStepKind::CloneRepo,
            PrerequisiteStepKind::RunShell,
        ] {
            let result = overlays(kind, Some(&ssh), &PrerequisiteGrants::default());
            assert!(result.overlays.is_empty());
            assert!(result.secret_env.is_empty());
            if kind == PrerequisiteStepKind::CreatePullRequest {
                assert_eq!(
                    result.env_literals,
                    [("GH_PROMPT_DISABLED".into(), "1".into())]
                );
            } else {
                assert!(result.env_literals.is_empty());
            }
        }
    }

    #[test]
    fn key_path_validated_again_at_the_mount_boundary() {
        if !isolated() {
            return;
        }
        let ssh = GitRemoteUrl::parse("git@github.com:o/r.git");
        let outside = tempfile::NamedTempFile::new().unwrap();
        let grants = PrerequisiteGrants {
            ssh_transports: Default::default(),
            ssh_destinations: [GitRemoteUrl::parse("git@github.com:o/r.git").redacted].into(),
            token_destinations: ["https://github.com/o/r".into()].into(),
            ssh: Some(SshGrant::Key {
                key_path: outside.path().to_path_buf(),
                known_hosts: None,
                passphrase_protected: None,
            }),
            ..Default::default()
        };
        let result = engine().prerequisite_grant_overlays(
            &target(PrerequisiteStepKind::PushBranch, Some(&ssh)),
            &grants,
        );
        assert!(
            result.is_err(),
            "a key outside ~/.ssh must never be mounted"
        );
        // A symlink inside ~/.ssh pointing outside is refused as well.
        let key = Key::new("escape");
        let link = key
            .ssh_dir
            .join(format!("id_wi0149_link_{}", std::process::id()));
        std::os::unix::fs::symlink(outside.path(), &link).unwrap();
        let grants = PrerequisiteGrants {
            ssh_transports: Default::default(),
            ssh_destinations: [GitRemoteUrl::parse("git@github.com:o/r.git").redacted].into(),
            token_destinations: ["https://github.com/o/r".into()].into(),
            ssh: Some(SshGrant::Key {
                key_path: link.clone(),
                known_hosts: None,
                passphrase_protected: None,
            }),
            ..Default::default()
        };
        let result = engine().prerequisite_grant_overlays(
            &target(PrerequisiteStepKind::PushBranch, Some(&ssh)),
            &grants,
        );
        let _ = std::fs::remove_file(&link);
        assert!(result.is_err());
    }

    #[test]
    fn display_masks_values_and_debug_lists_secret_names_only() {
        let https = GitRemoteUrl::parse("https://github.com/o/r");
        let pr = overlays(
            PrerequisiteStepKind::CreatePullRequest,
            Some(&https),
            &everything(None),
        );
        let push = overlays(
            PrerequisiteStepKind::PushBranch,
            Some(&https),
            &everything(None),
        );
        for o in [&pr, &push] {
            let shown = format!("{o:?} {}", o.display.join("\n"));
            assert!(!shown.contains(TOKEN));
            assert!(shown.contains("***") || shown.contains("secret_env"));
        }
        assert!(GrantOverlays::none().is_empty());
        assert!(GrantOverlays::none().secret_names().is_empty());
    }
}

#[cfg(test)]
mod destination_review_tests {
    use super::*;
    use crate::data::fs::auth_paths::AuthPathResolver;
    use crate::engine::overlay::OverlayEngine;
    use crate::engine::workflow::prerequisite_grants::{
        PrerequisiteGrants, TokenGrant, TokenGrantSource,
    };
    #[test]
    fn token_applies_only_to_the_exact_approved_destination_and_host() {
        let home = tempfile::tempdir().unwrap();
        let engine = OverlayEngine::with_auth_resolver(AuthPathResolver::at_home(home.path()));
        let mut grants = PrerequisiteGrants {
            token: Some(TokenGrant {
                secret: SecretString::new("fixture_token"),
                source: TokenGrantSource::Pasted,
                host: "ghe.corp.test".into(),
                acquire: None,
            }),
            ..Default::default()
        };
        grants
            .token_destinations
            .insert("https://ghe.corp.test/o/r".into());
        for (url, allowed) in [
            ("https://ghe.corp.test/o/r", true),
            ("https://ghe.corp.test/other/r", false),
            ("https://evil.test/o/r", false),
        ] {
            let remote = GitRemoteUrl::parse(url);
            let target = GrantTarget {
                repo: None,
                git_source: None,
                step_kind: PrerequisiteStepKind::CreatePullRequest,
                phase: PhaseKind::Teardown,
                container_home: None,
                remote: Some(&remote),
            };
            let output = engine
                .prerequisite_grant_overlays(&target, &grants)
                .unwrap();
            assert_eq!(!output.secret_env.is_empty(), allowed);
            assert!(output
                .env_literals
                .contains(&("GH_PROMPT_DISABLED".into(), "1".into())));
        }
    }
}
