//! Status rules. Sources return metadata; only typed grants carry secrets.

use super::*;
use crate::data::fs::ssh_keys::{SshKeyInfo, SshKeyInventory, SshKeyKind};
use crate::data::workflow_state::PhaseKind;
use crate::engine::auth::credential::SecretString;
use crate::engine::auth::github_token::{GhAuthStatus, TokenVerification};
use crate::engine::container::phase_tools::PhaseToolsStatus;
use crate::engine::git::{GitEngine, GitIdentity};
use crate::engine::workflow::prerequisite_grants::SshGrant;

impl PrerequisitePreflight {
    fn destination(&self, need: &PrerequisiteNeed) -> Option<GitRemoteUrl> {
        let remote = match &need.prerequisite {
            WorkflowPrerequisite::PushAuthSsh { remote, .. }
            | WorkflowPrerequisite::PushAuthHttps { remote, .. } => remote.as_str(),
            _ => "origin",
        };
        self.remote(remote).ok().flatten()
    }
    fn remote(&self, remote: &str) -> Result<Option<GitRemoteUrl>, ()> {
        // URLs in clone_repo/push_branch are already destinations, not remote names.
        if remote.contains("://") || remote.contains(':') || remote.starts_with(['/', '.']) {
            return Ok(Some(GitRemoteUrl::parse(remote)));
        }
        if let Some(SessionType::Remote { repo_url, .. }) = &self.options.session_type {
            return if remote == "origin" {
                Ok(Some(GitRemoteUrl::parse(repo_url)))
            } else {
                Ok(None)
            };
        }
        self.sources
            .git
            .remote_url(self.repo(), remote)
            .map(|url| url.map(|url| GitRemoteUrl::parse(&url)))
            .map_err(|_| ())
    }

    pub(super) fn probe(
        &self,
        need: &PrerequisiteNeed,
        secrets: &RedactionSet,
    ) -> Vec<PrerequisiteCheck> {
        use WorkflowPrerequisite::*;
        let check = match &need.prerequisite {
            GitIdentity => self.identity(need),
            GitRemote { remote } => match self.remote(remote) {
                Ok(Some(url)) => Self::check(
                    need,
                    PrerequisiteStatus::Satisfied,
                    format!("remote: {}", url.display()),
                    vec![],
                ),
                Ok(None) => Self::check(
                    need,
                    PrerequisiteStatus::Missing,
                    "remote is not configured",
                    vec![Remediation::Continue, Remediation::Abort],
                ),
                Err(()) => Self::check(
                    need,
                    PrerequisiteStatus::Unknown,
                    "could not inspect the remote; check the repository",
                    vec![],
                ),
            },
            PushAuth { remote } => match self.remote(remote) {
                Ok(Some(url)) => {
                    let mut classified = need.clone();
                    match url.kind {
                        GitRemoteKind::Ssh { host } => {
                            classified.prerequisite = PushAuthSsh {
                                remote: remote.clone(),
                                host: host.clone(),
                            };
                            return vec![
                                self.ssh(&classified, &host),
                                self.tool(
                                    &PrerequisiteNeed {
                                        prerequisite: ContainerTool { name: "ssh".into() },
                                        needed_by: need.needed_by.clone(),
                                    },
                                    "ssh",
                                ),
                            ];
                        }
                        GitRemoteKind::Https { host } => {
                            classified.prerequisite = PushAuthHttps {
                                remote: remote.clone(),
                                host: host.clone(),
                            };
                            self.https(&classified, &host, secrets)
                        }
                        GitRemoteKind::File => return vec![],
                        GitRemoteKind::Other => Self::check(
                            need,
                            PrerequisiteStatus::Unknown,
                            "remote transport is unsupported; configure an overlay",
                            vec![],
                        ),
                    }
                }
                Ok(None) => Self::check(
                    need,
                    PrerequisiteStatus::Missing,
                    "cannot choose authentication without a configured remote",
                    vec![Remediation::Continue],
                ),
                Err(()) => Self::check(
                    need,
                    PrerequisiteStatus::Unknown,
                    "could not classify the remote; check the repository",
                    vec![],
                ),
            },
            PushAuthSsh { host, .. } => {
                return vec![
                    self.ssh(need, host),
                    self.tool(
                        &PrerequisiteNeed {
                            prerequisite: ContainerTool { name: "ssh".into() },
                            needed_by: need.needed_by.clone(),
                        },
                        "ssh",
                    ),
                ]
            }
            PushAuthHttps { host, .. } => self.https(need, host, secrets),
            GitHubToken { host } => {
                let remote = self.remote("origin").ok().flatten();
                let host = match remote.as_ref().map(|r| &r.kind) {
                    Some(GitRemoteKind::Https { host } | GitRemoteKind::Ssh { host }) => {
                        host.as_str()
                    }
                    _ => host.as_str(),
                };
                let mut classified = need.clone();
                classified.prerequisite = GitHubToken { host: host.into() };
                self.token(&classified, host, secrets)
            }
            ContainerTool { name } => self.tool(need, name),
            PushedBranch => self.pushed_branch(need),
            NotDetachedHead => {
                let attached = self.options.worktree_planned
                    || self.options.worktree.is_some()
                    || matches!(self.options.session_type, Some(SessionType::Remote { ref branch, .. }) if !branch.trim().is_empty())
                    || self.sources.git.current_branch(self.repo()).is_some();
                if attached {
                    Self::check(
                        need,
                        PrerequisiteStatus::Satisfied,
                        "HEAD is attached to a branch",
                        vec![],
                    )
                } else if self.sources.git.is_detached_head(self.repo()) {
                    Self::check(
                        need,
                        PrerequisiteStatus::Missing,
                        "HEAD is detached; select or create a branch before pushing",
                        vec![Remediation::Continue, Remediation::Abort],
                    )
                } else {
                    Self::check(
                        need,
                        PrerequisiteStatus::Unknown,
                        "could not determine the current branch",
                        vec![],
                    )
                }
            }
        };
        vec![check]
    }

    fn identity(&self, need: &PrerequisiteNeed) -> PrerequisiteCheck {
        let complete = |id: &GitIdentity| {
            id.name.as_ref().is_some_and(|s| !s.trim().is_empty())
                && id.email.as_ref().is_some_and(|s| !s.trim().is_empty())
        };
        let local = self.sources.git.local_identity(self.repo());
        let local_unavailable = local.is_err();
        match local {
            Ok(local) if complete(&local) => {
                return Self::check(
                    need,
                    PrerequisiteStatus::Satisfied,
                    "identity from repository-local/worktree common-dir config (container view)",
                    vec![],
                )
            }
            _ => {}
        }
        let policy = &self.options.policy.git_identity;
        if policy.use_ == CredentialUse::Deny {
            return Self::check(need, PrerequisiteStatus::Missing, "identity injection denied by policy; configure repo-local user.name and user.email", vec![Remediation::Continue]);
        }
        if policy.use_ == CredentialUse::Allow && policy.fixed.is_some() {
            return Self::check(
                need,
                PrerequisiteStatus::Satisfied,
                "identity from repo/global config",
                vec![],
            );
        }
        if self
            .options
            .existing_grants
            .as_ref()
            .is_some_and(|grants| grants.identity.is_some())
            || self.options.remembered.identity.is_some()
        {
            return Self::check(
                need,
                PrerequisiteStatus::Satisfied,
                "identity explicitly granted for commit steps",
                vec![],
            );
        }
        if local_unavailable {
            return Self::check(
                need,
                PrerequisiteStatus::Unknown,
                "could not read repository-local identity; check the worktree common directory",
                vec![],
            );
        }
        let host = self.sources.git.host_identity().ok().filter(complete);
        let has_host = host.is_some();
        let mut remediations = vec![];
        if self.options.can_ask {
            if let Some(GitIdentity {
                name: Some(name),
                email: Some(email),
            }) = host
            {
                remediations.push(Remediation::UseHostIdentity {
                    name: name.clone(),
                    email: email.clone(),
                });
                remediations.push(Remediation::EnterIdentity);
                remediations.push(Remediation::SetLocalIdentity {
                    command: GitEngine::set_local_identity_command_display(
                        self.repo(),
                        &name,
                        &email,
                    ),
                    name,
                    email,
                });
            } else {
                remediations.push(Remediation::EnterIdentity);
            }
        }
        remediations.push(Remediation::Continue);
        Self::check(
            need,
            if has_host || self.options.can_ask {
                PrerequisiteStatus::NeedsConsent
            } else {
                PrerequisiteStatus::Missing
            },
            "container has no complete repo-local identity; host-global identity requires consent",
            remediations,
        )
    }

    fn phase_tools(&self) -> PhaseToolsStatus {
        let Some(image) = &self.options.phase_image else {
            return PhaseToolsStatus::Unknown {
                reason: "phase image has not been resolved".into(),
            };
        };
        if self.options.runtime.capabilities().kit_declarative {
            return PhaseToolsStatus::Unknown {
                reason: "runtime has no phase-container probe".into(),
            };
        }
        let mut names = vec!["git", "gh", "ssh"];
        for need in &self.options.prerequisites.needs {
            if let WorkflowPrerequisite::ContainerTool { name } = &need.prerequisite {
                if !names.contains(&name.as_str()) {
                    names.push(name);
                }
            }
        }
        self.sources.tools.probe(image, &names)
    }

    fn tool(&self, need: &PrerequisiteNeed, name: &str) -> PrerequisiteCheck {
        match self.phase_tools() {
            PhaseToolsStatus::Probed(tools) => match tools.tools.get(name) {
                Some(true) => Self::check(need, PrerequisiteStatus::Satisfied, "available in the phase image", vec![]),
                Some(false) => Self::check(need, PrerequisiteStatus::Missing, "missing from the phase image; install it and rebuild the image", vec![
                    Remediation::AddDockerfileLine {
                        file: ".awman/Dockerfile.project".into(),
                        line: "RUN apt-get update && apt-get install -y --no-install-recommends gh openssh-client".into(),
                    }, Remediation::Continue, Remediation::Abort,
                ]),
                None => Self::check(need, PrerequisiteStatus::Unknown, "phase probe did not report this tool; check the image", vec![]),
            },
            PhaseToolsStatus::Unknown { reason } => Self::check(need, PrerequisiteStatus::Unknown, format!("phase image could not be probed: {reason}"), vec![]),
        }
    }

    fn ssh(&self, need: &PrerequisiteNeed, host: &str) -> PrerequisiteCheck {
        let policy = &self.options.policy.ssh;
        if policy.use_ == CredentialUse::Deny {
            return Self::check(
                need,
                PrerequisiteStatus::Missing,
                "SSH keys denied by policy",
                vec![Remediation::Continue],
            );
        }
        let mut inventory = self.sources.ssh.inventory();
        let home = inventory.ssh_dir.parent().unwrap_or(Path::new("/"));
        let granted = self
            .options
            .existing_grants
            .as_ref()
            .filter(|g| {
                self.destination(need)
                    .is_some_and(|u| g.allows_ssh(&u.redacted))
            })
            .and_then(|g| g.ssh.as_ref());
        let selected = match granted {
            Some(SshGrant::Key { key_path, .. }) => Some(key_path),
            _ => self.options.remembered.ssh_key.as_ref().or_else(|| {
                (policy.use_ == CredentialUse::Allow && !policy.key_is_suggestion)
                    .then_some(policy.key.as_ref())
                    .flatten()
            }),
        };
        // The bounded inventory scans id_* names. An explicitly selected key
        // can have another name; use the data layer's containment validation
        // and retain unknown metadata rather than rejecting a legitimate grant.
        // No additional file contents are read by this layer.
        let custom = selected.or(policy.key.as_ref()).and_then(|path| {
            if inventory.find(&expand_key(home, path)).is_some() {
                return None;
            }
            SshKeyInventory::validate_key_path(home, path)
                .ok()
                .map(|path| SshKeyInfo {
                    path,
                    kind: SshKeyKind::Unknown,
                    mode: None,
                    mode_ok: false,
                    passphrase_protected: match granted {
                        Some(SshGrant::Key {
                            passphrase_protected,
                            ..
                        }) => *passphrase_protected,
                        _ => None,
                    },
                    public_key_present: false,
                })
        });
        if let Some(custom) = custom {
            inventory.keys.push(custom);
        }
        let key = selected.and_then(|path| inventory.find(&expand_key(home, path)));
        let whole_dir = matches!(granted, Some(SshGrant::WholeDir { .. }))
            || self.options.remembered.ssh_dir
            || (policy.use_ == CredentialUse::Allow && policy.whole_dir);
        if whole_dir || key.is_some() {
            let mut explanation = if whole_dir {
                "SSH directory granted".to_owned()
            } else {
                "SSH key granted".to_owned()
            };
            if key.is_some_and(|key| key.passphrase_protected == Some(true)) {
                explanation.push_str("; warning: passphrase-protected key may fail without an agent (no agent socket is forwarded)");
            }
            if key.is_some_and(|key| key.mode.is_none()) {
                explanation.push_str("; warning: custom key permissions/ownership were not probed; check mode 0600 and the container uid");
            } else if key.is_some_and(|key| !key.mode_ok) {
                explanation.push_str(
                    "; warning: key permissions or owner are unsuitable; SSH may refuse the key",
                );
            }
            if key.is_some_and(|key| key.passphrase_protected.is_none()) {
                explanation.push_str("; warning: passphrase status is unknown; encrypted keys cannot prompt in the step container");
            }
            if matches!(self.phase_tools(), PhaseToolsStatus::Probed(ref tools) if Self::key_readable_in_image(tools) == Some(false))
            {
                explanation.push_str(
                    "; warning: image uid differs from host key uid; a 0600 key may be unreadable",
                );
            }
            if !inventory.known_hosts_has(host) {
                explanation.push_str("; host is absent from known_hosts (hashed entries cannot be checked); verify its fingerprint");
            }
            return Self::check(need, PrerequisiteStatus::Satisfied, explanation, vec![]);
        }
        let mut remediations = vec![];
        if self.options.can_ask {
            for key in &inventory.keys {
                remediations.push(self.key_remediation(key, home));
            }
            if !inventory.keys.is_empty() {
                remediations.push(Remediation::MountSshDir);
            }
        }
        remediations.push(Remediation::Continue);
        Self::check(
            need,
            if inventory.keys.is_empty() {
                PrerequisiteStatus::Missing
            } else {
                PrerequisiteStatus::NeedsConsent
            },
            if inventory.keys.is_empty() {
                "no usable SSH key found; configure a deploy key or use GitHub HTTPS with a token"
            } else if selected.is_some() {
                "selected SSH key is unavailable; choose a valid key"
            } else {
                "host SSH keys require explicit consent before mounting; encrypted keys may require a passphrase"
            },
            remediations,
        )
    }

    fn key_remediation(&self, key: &SshKeyInfo, home: &Path) -> Remediation {
        Remediation::MountSshKey {
            path: key.path.display().to_string(),
            passphrase_protected: key.passphrase_protected,
            mode_ok: key.mode_ok,
            remembered: self
                .options
                .remembered
                .ssh_key
                .as_ref()
                .is_some_and(|p| expand_key(home, p) == key.path),
            suggested: self
                .options
                .policy
                .ssh
                .key
                .as_ref()
                .is_some_and(|p| expand_key(home, p) == key.path),
        }
    }

    fn https(
        &self,
        need: &PrerequisiteNeed,
        host: &str,
        secrets: &RedactionSet,
    ) -> PrerequisiteCheck {
        if !github_host(host) {
            Self::unsupported(need, "HTTPS authentication for non-GitHub remotes is unsupported; configure a credential overlay")
        } else {
            self.token(need, host, secrets)
        }
    }

    fn unsupported(need: &PrerequisiteNeed, hint: &str) -> PrerequisiteCheck {
        Self::check(
            need,
            PrerequisiteStatus::Missing,
            hint,
            vec![
                Remediation::AddOverlayHint { hint: hint.into() },
                Remediation::Continue,
            ],
        )
    }

    fn token(
        &self,
        need: &PrerequisiteNeed,
        host: &str,
        secrets: &RedactionSet,
    ) -> PrerequisiteCheck {
        let policy = &self.options.policy.github_token;
        if policy.use_ == CredentialUse::Deny {
            return Self::check(
                need,
                PrerequisiteStatus::Missing,
                "GitHub token denied by policy",
                vec![Remediation::Continue],
            );
        }
        if !github_host(host) {
            return Self::unsupported(need, "GitHub Enterprise and non-GitHub API authentication are unsupported in this version");
        }
        if let Some(grant) = self
            .options
            .existing_grants
            .as_ref()
            .and_then(|g| g.token.as_ref())
            .filter(|g| {
                g.host == host
                    && self.options.existing_grants.as_ref().is_some_and(|grants| {
                        self.destination(need)
                            .is_some_and(|u| grants.allows_token(&u.redacted))
                    })
            })
        {
            secrets.add(grant.secret.clone());
            if let TokenGrantSource::Env(var) = &grant.source {
                if self
                    .sources
                    .tokens
                    .env_token_name(std::slice::from_ref(var))
                    .as_deref()
                    != Some(var)
                {
                    return Self::check(
                        need,
                        PrerequisiteStatus::Missing,
                        format!("granted environment variable {var} is no longer set"),
                        vec![Remediation::Continue],
                    );
                }
            }
            return self.verified_token(
                need,
                host,
                &grant.secret,
                "explicitly granted GitHub token",
                secrets,
            );
        }
        // Remembered decisions grant only the configured variable name, never a value.
        let remembered = self
            .options
            .remembered
            .token_env
            .as_ref()
            .filter(|var| policy.env_vars.contains(var));
        let var = remembered
            .and_then(|var| {
                self.sources
                    .tokens
                    .env_token_name(std::slice::from_ref(var))
            })
            .or_else(|| self.sources.tokens.env_token_name(&policy.env_vars));
        if let Some(var) = var {
            let secret = self.sources.tokens.env_token(&var);
            if let Some(secret) = &secret {
                secrets.add(secret.clone());
            }
            let consented = (host == "github.com" && policy.use_ == CredentialUse::Allow)
                || remembered == Some(&var);
            if consented {
                let found = format!("found in {var}");
                return match secret {
                    Some(secret) => self.verified_token(need, host, &secret, &found, secrets),
                    None => Self::check(
                        need,
                        PrerequisiteStatus::Satisfied,
                        format!("{found}; verification unavailable from this source"),
                        vec![],
                    ),
                };
            }
            let mut remediations = if self.options.can_ask {
                vec![Remediation::UseEnvToken {
                    var: var.clone(),
                    remembered: false,
                }]
            } else {
                vec![]
            };
            remediations.extend(self.token_choices(host));
            return Self::check(
                need,
                PrerequisiteStatus::NeedsConsent,
                format!("found in {var}; consent required for workflow use"),
                remediations,
            );
        }
        let poll_only = !need.needed_by.is_empty()
            && need
                .needed_by
                .iter()
                .all(|step| step.kind == PrerequisiteStepKind::PollCi);
        if poll_only
            && host == "github.com"
            && matches!(
                self.sources.tokens.host_gh_status(host),
                GhAuthStatus::LoggedIn { .. }
            )
        {
            return Self::check(
                need,
                PrerequisiteStatus::Satisfied,
                "authenticated host gh (poll_ci only)",
                vec![],
            );
        }
        Self::check(
            need,
            if self.options.can_ask {
                PrerequisiteStatus::NeedsConsent
            } else {
                PrerequisiteStatus::Missing
            },
            "no granted GitHub token is available",
            self.token_choices(host),
        )
    }

    fn verified_token(
        &self,
        need: &PrerequisiteNeed,
        host: &str,
        token: &SecretString,
        found: &str,
        secrets: &RedactionSet,
    ) -> PrerequisiteCheck {
        secrets.add(token.clone());
        if !self.options.policy.github_token.verify {
            return Self::check(
                need,
                PrerequisiteStatus::Satisfied,
                format!("{found}; verification disabled"),
                vec![],
            );
        }
        let (status, note) = match self.sources.tokens.verify(token, host) {
            TokenVerification::Verified { login, repo_scope } => (
                PrerequisiteStatus::Satisfied,
                format!(
                    "verified as {login}{}",
                    if repo_scope == Some(false) {
                        "; warning: classic token lacks repo scope"
                    } else {
                        ""
                    }
                ),
            ),
            TokenVerification::Rejected { .. } => (
                if self.options.can_ask {
                    PrerequisiteStatus::NeedsConsent
                } else {
                    PrerequisiteStatus::Missing
                },
                "token verification rejected the credential; choose another source".into(),
            ),
            TokenVerification::Unknown { reason }
                if reason == "test isolation" || reason == "verification disabled" =>
            {
                (
                    PrerequisiteStatus::Satisfied,
                    "verification disabled/unavailable in test isolation".into(),
                )
            }
            TokenVerification::Unknown { .. } => (
                PrerequisiteStatus::Unknown,
                "verification unavailable; check network/token permissions".into(),
            ),
        };
        Self::check(
            need,
            status,
            format!("{found}; {note}"),
            if status == PrerequisiteStatus::NeedsConsent {
                let mut choices = self.token_choices(host);
                choices.push(Remediation::ReplaceRejectedToken);
                choices
            } else if status == PrerequisiteStatus::Missing {
                vec![Remediation::ReplaceRejectedToken]
            } else {
                vec![]
            },
        )
    }

    fn token_choices(&self, host: &str) -> Vec<Remediation> {
        let mut choices = vec![];
        if self.options.can_ask {
            choices.push(Remediation::PasteToken);
            if let Some(path) = self.sources.tokens.gh_on_path() {
                let logged_in = match if host == "github.com" {
                    self.sources.tokens.host_gh_status(host)
                } else {
                    GhAuthStatus::NotInstalled
                } {
                    GhAuthStatus::LoggedIn { .. } => Some(true),
                    GhAuthStatus::NotLoggedIn { .. } => Some(false),
                    _ => None,
                };
                let resolved_path = path.display().to_string();
                choices.push(Remediation::RunGhAuthToken {
                    argv: vec![
                        resolved_path.clone(),
                        "auth".into(),
                        "token".into(),
                        "--hostname".into(),
                        host.into(),
                    ],
                    resolved_path: resolved_path.clone(),
                    logged_in,
                });
                if logged_in != Some(true) {
                    choices.push(Remediation::RunGhAuthLogin {
                        argv: vec![
                            resolved_path.clone(),
                            "auth".into(),
                            "login".into(),
                            "--hostname".into(),
                            host.into(),
                            "--web".into(),
                            "--git-protocol".into(),
                            "https".into(),
                        ],
                        resolved_path,
                    });
                }
            }
        }
        if let Some(acquire) = &self.options.policy.github_token.acquire {
            let approved =
                acquire.scope == AcquireScope::Global || self.options.repo_acquire_approved;
            if self.options.can_ask
                || (self.options.policy.github_token.use_ == CredentialUse::Allow && approved)
            {
                choices.push(Remediation::RunAcquireCommand {
                    label: "configured token command (ON YOUR HOST)".into(),
                    argv: acquire.argv.clone(),
                    // Layer 2 obtains the hardened HostSecretCommand review
                    // before displaying/approving the actual resolved program.
                    resolved_path: None,
                    scope: acquire.scope,
                    approved,
                    sha256: acquire.sha256.clone(),
                });
            }
        }
        choices.push(Remediation::Continue);
        choices
    }

    fn pushed_branch(&self, need: &PrerequisiteNeed) -> PrerequisiteCheck {
        let prs: Vec<_> = need
            .needed_by
            .iter()
            .filter(|step| step.kind == PrerequisiteStepKind::CreatePullRequest)
            .collect();
        let ordered_push = !prs.is_empty()
            && prs.iter().all(|pr| {
                self.options
                    .prerequisites
                    .needs
                    .iter()
                    .flat_map(|need| &need.needed_by)
                    .any(|step| {
                        step.kind == PrerequisiteStepKind::PushBranch
                            && step.phase == PhaseKind::Teardown
                            && pr.phase == PhaseKind::Teardown
                            && step.index < pr.index
                    })
            });
        let upstream = self
            .sources
            .git
            .current_branch(self.repo())
            .and_then(|branch| self.sources.git.branch_upstream(self.repo(), &branch))
            .is_some();
        if ordered_push || upstream {
            Self::check(
                need,
                PrerequisiteStatus::Satisfied,
                if ordered_push {
                    "push_branch precedes every pull request"
                } else {
                    "current branch has an upstream"
                },
                vec![],
            )
        } else {
            Self::check(
                need,
                PrerequisiteStatus::Missing,
                "branch has no upstream; add push_branch before create_pull_request",
                vec![Remediation::Continue, Remediation::Abort],
            )
        }
    }
}

fn expand_key(home: &Path, path: &Path) -> PathBuf {
    if let Ok(tail) = path.strip_prefix("~/") {
        home.join(tail)
    } else if path.is_relative() {
        home.join(path)
    } else {
        path.to_path_buf()
    }
}
fn github_host(host: &str) -> bool {
    crate::engine::auth::github_token::valid_host(host)
        && !["gitlab.com", "bitbucket.org", "gitea.com", "codeberg.org"].contains(&host)
}
