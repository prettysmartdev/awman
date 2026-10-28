#![allow(clippy::field_reassign_with_default)]
//! Status-table tests for [`PrerequisitePreflight`] with injected fake sources.
//! Nothing here touches git, the network, `gh`, Docker or the host's keys.

use super::*;
use crate::data::config::workflows::AcquirePolicy;
use crate::data::fs::ssh_keys::{SshKeyInfo, SshKeyInventory, SshKeyKind};
use crate::data::workflow_state::PhaseKind;
use crate::engine::auth::credential::SecretString;
use crate::engine::auth::github_token::{GhAuthStatus, TokenVerification};
use crate::engine::auth::host_secret_command::{HostSecretCommandError, HostSecretOutput};
use crate::engine::container::phase_tools::PhaseToolsStatus;
use crate::engine::error::EngineError;
use crate::engine::git::GitIdentity;
use crate::engine::workflow::prerequisite_grants::{SshGrant, TokenGrant};
use sources::*;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

const TOKEN: &str = "ghp_PREFLIGHT_FAKE_TOKEN_9";

struct FakeGit {
    remote: Option<String>,
    local: GitIdentity,
    host: GitIdentity,
    detached: bool,
    branch: Option<String>,
    upstream: Option<String>,
    delay: Duration,
}

impl Default for FakeGit {
    fn default() -> Self {
        Self {
            remote: Some("git@github.com:org/repo.git".into()),
            local: GitIdentity::default(),
            host: GitIdentity::default(),
            detached: false,
            branch: Some("feature".into()),
            upstream: None,
            delay: Duration::ZERO,
        }
    }
}

impl GitProbeSource for FakeGit {
    fn local_identity(&self, _: &Path) -> Result<GitIdentity, EngineError> {
        std::thread::sleep(self.delay);
        Ok(self.local.clone())
    }
    fn host_identity(&self) -> Result<GitIdentity, EngineError> {
        Ok(self.host.clone())
    }
    fn remote_url(&self, _: &Path, _: &str) -> Result<Option<String>, EngineError> {
        Ok(self.remote.clone())
    }
    fn is_detached_head(&self, _: &Path) -> bool {
        self.detached
    }
    fn current_branch(&self, _: &Path) -> Option<String> {
        std::thread::sleep(self.delay);
        self.branch.clone()
    }
    fn branch_upstream(&self, _: &Path, _: &str) -> Option<String> {
        self.upstream.clone()
    }
}

struct FakeSsh {
    keys: Vec<SshKeyInfo>,
    known_hosts: bool,
}
impl SshKeySource for FakeSsh {
    fn inventory(&self) -> SshKeyInventory {
        let mut inventory = SshKeyInventory::empty(Path::new("/fake-home"));
        inventory.keys = self.keys.clone();
        let _ = self.known_hosts;
        inventory
    }
}
fn key(name: &str, passphrase: Option<bool>) -> SshKeyInfo {
    SshKeyInfo {
        path: PathBuf::from("/fake-home/.ssh").join(name),
        kind: SshKeyKind::Ed25519,
        passphrase_protected: passphrase,
        mode: Some(0o600),
        mode_ok: true,
        public_key_present: true,
    }
}

struct FakeTools(PhaseToolsStatus);
impl PhaseToolSource for FakeTools {
    fn probe(&self, _: &str, _: &[&str]) -> PhaseToolsStatus {
        self.0.clone()
    }
}
fn all_tools() -> PhaseToolsStatus {
    tools_with(&[("git", true), ("gh", true), ("ssh", true)], Some(0))
}
fn tools_with(tools: &[(&str, bool)], uid: Option<u32>) -> PhaseToolsStatus {
    PhaseToolsStatus::Probed(crate::data::fs::phase_tools_cache::PhaseTools {
        tools: tools.iter().map(|(n, v)| (n.to_string(), *v)).collect(),
        user: None,
        uid,
        probed_at: "0".into(),
    })
}

struct FakeTokens {
    reject: bool,
    env: Option<(&'static str, &'static str)>,
    present: AtomicBool,
    gh: GhAuthStatus,
    acquired: AtomicUsize,
    verified: AtomicUsize,
}
impl FakeTokens {
    fn new(env: Option<(&'static str, &'static str)>) -> Arc<Self> {
        Arc::new(Self {
            env,
            reject: false,
            present: AtomicBool::new(true),
            gh: GhAuthStatus::NotInstalled,
            acquired: AtomicUsize::new(0),
            verified: AtomicUsize::new(0),
        })
    }
    fn var(&self, vars: &[String]) -> Option<(&'static str, &'static str)> {
        let (name, value) = self.env?;
        (self.present.load(Ordering::SeqCst) && vars.iter().any(|v| v == name))
            .then_some((name, value))
    }
}
impl TokenProbeSource for FakeTokens {
    fn env_token_name(&self, vars: &[String]) -> Option<String> {
        self.var(vars).map(|(n, _)| n.to_string())
    }
    fn env_token(&self, var: &str) -> Option<SecretString> {
        self.var(&[var.to_string()])
            .map(|(_, v)| SecretString::new(v))
    }
    fn gh_on_path(&self) -> Option<PathBuf> {
        None
    }
    fn host_gh_status(&self, _: &str) -> GhAuthStatus {
        self.gh.clone()
    }
    fn verify(&self, _: &SecretString, _: &str) -> TokenVerification {
        self.verified.fetch_add(1, Ordering::SeqCst);
        if self.reject {
            return TokenVerification::Rejected { status: 401 };
        }
        TokenVerification::Verified {
            login: "fixture".into(),
            repo_scope: Some(true),
        }
    }
    fn acquire_token(
        &self,
        _: &AcquirePolicy,
        _: &[String],
    ) -> Result<HostSecretOutput, HostSecretCommandError> {
        self.acquired.fetch_add(1, Ordering::SeqCst);
        Ok(HostSecretOutput {
            secret: Some(SecretString::new("ghp_REFRESHED_FAKE_TOKEN_9")),
            stderr: String::new(),
            extra_lines: 0,
        })
    }
}

fn step(kind: PrerequisiteStepKind, index: usize) -> StepRef {
    StepRef {
        phase: PhaseKind::Teardown,
        kind,
        index,
        description: kind.label().into(),
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

fn options(items: Vec<WorkflowPrerequisite>) -> PrerequisitePreflightOptions {
    let mut o = PrerequisitePreflightOptions::new(
        needs(items),
        PathBuf::from("/fake/repo"),
        Arc::new(ContainerRuntime::docker()),
        PrerequisitePolicy::default(),
    );
    o.phase_image = Some("fake:local".into());
    o
}

struct Fixture {
    git: FakeGit,
    ssh: FakeSsh,
    tools: PhaseToolsStatus,
    tokens: Arc<FakeTokens>,
}
impl Default for Fixture {
    fn default() -> Self {
        Self {
            git: FakeGit::default(),
            ssh: FakeSsh {
                keys: vec![key("id_ed25519", Some(false))],
                known_hosts: true,
            },
            tools: all_tools(),
            tokens: FakeTokens::new(None),
        }
    }
}
impl Fixture {
    fn preflight(self, o: PrerequisitePreflightOptions) -> PrerequisitePreflight {
        PrerequisitePreflight::new(
            o,
            PreflightSources {
                git: Arc::new(self.git),
                ssh: Arc::new(self.ssh),
                tokens: self.tokens,
                tools: Arc::new(FakeTools(self.tools)),
            },
        )
    }
}

fn status_of(report: &WorkflowPrerequisiteReport, p: &WorkflowPrerequisite) -> PrerequisiteStatus {
    report
        .find(p)
        .unwrap_or_else(|| panic!("no check for {p:?} in {:?}", report.checks))
        .status
}

fn token_need() -> WorkflowPrerequisite {
    WorkflowPrerequisite::GitHubToken {
        host: "github.com".into(),
    }
}
fn push_need() -> WorkflowPrerequisite {
    WorkflowPrerequisite::PushAuth {
        remote: "origin".into(),
    }
}

// ── identity ──────────────────────────────────────────────────────────────

#[test]
fn only_global_identity_is_needs_consent() {
    let mut f = Fixture::default();
    f.git.host = GitIdentity {
        name: Some("Ada".into()),
        email: Some("ada@x.test".into()),
    };
    let mut o = options(vec![WorkflowPrerequisite::GitIdentity]);
    o.can_ask = true;
    let report = f.preflight(o).run();
    let check = &report.checks[0];
    assert_eq!(check.status, PrerequisiteStatus::NeedsConsent);
    assert!(check
        .explanation
        .contains("host-global identity requires consent"));
    assert!(matches!(
        &check.remediations[0],
        Remediation::UseHostIdentity { name, email } if name == "Ada" && email == "ada@x.test"
    ));
    assert!(check.remediations.contains(&Remediation::EnterIdentity));
    assert!(check
        .remediations
        .iter()
        .any(|r| matches!(r, Remediation::SetLocalIdentity { .. })));
    assert_eq!(check.remediations.last(), Some(&Remediation::Continue));
    // The persistent remediation is listed after the transient ones.
    let positions: Vec<_> = check
        .remediations
        .iter()
        .map(|r| format!("{r:?}"))
        .collect();
    assert!(
        positions
            .iter()
            .position(|r| r.starts_with("EnterIdentity"))
            .unwrap()
            < positions
                .iter()
                .position(|r| r.starts_with("SetLocalIdentity"))
                .unwrap()
    );
}

#[test]
fn local_identity_satisfies_and_global_alone_never_does_when_headless() {
    let mut f = Fixture::default();
    f.git.local = GitIdentity {
        name: Some("L".into()),
        email: Some("l@x.test".into()),
    };
    let report = f
        .preflight(options(vec![WorkflowPrerequisite::GitIdentity]))
        .run();
    assert_eq!(report.checks[0].status, PrerequisiteStatus::Satisfied);
    // Headless (can_ask = false) with only a global identity: no prompts offered.
    let mut f = Fixture::default();
    f.git.host = GitIdentity {
        name: Some("G".into()),
        email: Some("g@x.test".into()),
    };
    let report = f
        .preflight(options(vec![WorkflowPrerequisite::GitIdentity]))
        .run();
    assert_eq!(report.checks[0].status, PrerequisiteStatus::NeedsConsent);
    assert_eq!(report.checks[0].remediations, vec![Remediation::Continue]);
    // No identity anywhere and nobody to ask: Missing.
    let report = Fixture::default()
        .preflight(options(vec![WorkflowPrerequisite::GitIdentity]))
        .run();
    assert_eq!(report.checks[0].status, PrerequisiteStatus::Missing);
    // Half an identity is not an identity.
    let mut f = Fixture::default();
    f.git.local = GitIdentity {
        name: Some("only name".into()),
        email: None,
    };
    let report = f
        .preflight(options(vec![WorkflowPrerequisite::GitIdentity]))
        .run();
    assert_ne!(report.checks[0].status, PrerequisiteStatus::Satisfied);
}

#[test]
fn identity_policy_deny_and_allow_fixed_and_existing_grant() {
    let mut o = options(vec![WorkflowPrerequisite::GitIdentity]);
    o.policy.git_identity.use_ = CredentialUse::Deny;
    let report = Fixture::default().preflight(o).run();
    assert_eq!(report.checks[0].status, PrerequisiteStatus::Missing);
    assert!(report.checks[0].explanation.contains("denied by policy"));

    let mut o = options(vec![WorkflowPrerequisite::GitIdentity]);
    o.policy.git_identity.use_ = CredentialUse::Allow;
    o.policy.git_identity.fixed = Some(crate::data::config::workflows::FixedIdentity {
        name: "Bot".into(),
        email: "bot@x.test".into(),
    });
    let report = Fixture::default().preflight(o).run();
    assert_eq!(report.checks[0].status, PrerequisiteStatus::Satisfied);

    let mut o = options(vec![WorkflowPrerequisite::GitIdentity]);
    o.existing_grants = Some(PrerequisiteGrants {
        ssh_transports: Default::default(),
        ssh_destinations: Default::default(),
        token_destinations: [GitRemoteUrl::parse("git@github.com:org/repo.git").redacted].into(),
        identity: Some(
            crate::engine::workflow::prerequisite_grants::GitIdentityGrant {
                name: "N".into(),
                email: "n@x.test".into(),
                source: crate::engine::workflow::prerequisite_grants::IdentitySource::Entered,
            },
        ),
        ..Default::default()
    });
    let report = Fixture::default().preflight(o).run();
    assert_eq!(report.checks[0].status, PrerequisiteStatus::Satisfied);
}

// ── remote / push auth classification ─────────────────────────────────────

#[test]
fn push_auth_resolves_to_ssh_https_or_is_removed() {
    // SSH remote: PushAuthSsh + the ssh container tool replace the unclassified need.
    let report = Fixture::default()
        .preflight(options(vec![push_need()]))
        .run();
    assert!(report
        .checks
        .iter()
        .any(|c| matches!(&c.prerequisite, WorkflowPrerequisite::PushAuthSsh { host, .. } if host == "github.com")));
    assert!(report.find(&push_need()).is_none());
    assert_eq!(
        status_of(
            &report,
            &WorkflowPrerequisite::ContainerTool { name: "ssh".into() }
        ),
        PrerequisiteStatus::Satisfied
    );

    // HTTPS github remote: PushAuthHttps, which follows the token rules.
    let mut f = Fixture::default();
    f.git.remote = Some("https://github.com/org/repo".into());
    let report = f.preflight(options(vec![push_need()])).run();
    assert!(report
        .checks
        .iter()
        .any(|c| matches!(c.prerequisite, WorkflowPrerequisite::PushAuthHttps { .. })));

    // file:// and local path remotes need no authentication at all.
    for remote in ["file:///srv/repo.git", "/srv/repo.git"] {
        let mut f = Fixture::default();
        f.git.remote = Some(remote.into());
        let report = f.preflight(options(vec![push_need()])).run();
        assert!(report.checks.is_empty(), "{remote}: {:?}", report.checks);
    }
}

#[test]
fn missing_remote_is_missing_and_gitlab_https_is_unsupported() {
    let mut f = Fixture::default();
    f.git.remote = None;
    let report = f
        .preflight(options(vec![
            WorkflowPrerequisite::GitRemote {
                remote: "origin".into(),
            },
            push_need(),
        ]))
        .run();
    assert_eq!(
        status_of(
            &report,
            &WorkflowPrerequisite::GitRemote {
                remote: "origin".into()
            }
        ),
        PrerequisiteStatus::Missing
    );
    assert_eq!(
        status_of(&report, &push_need()),
        PrerequisiteStatus::Missing
    );

    let mut f = Fixture::default();
    f.git.remote = Some("https://user:hunter2@gitlab.com/org/repo".into());
    let report = f.preflight(options(vec![push_need()])).run();
    let check = &report.checks[0];
    assert_eq!(check.status, PrerequisiteStatus::Missing);
    assert!(matches!(
        check.prerequisite,
        WorkflowPrerequisite::PushAuthHttps { .. }
    ));
    let rendered = serde_json::to_string(&report).unwrap() + &report.render_lines().join("\n");
    assert!(
        !rendered.contains("hunter2"),
        "credentials must be stripped"
    );
}

#[test]
fn credentials_in_derived_remote_names_are_stripped_by_the_preflight() {
    let o = options(vec![WorkflowPrerequisite::GitRemote {
        remote: "https://user:hunter2@github.com/o/r".into(),
    }]);
    let report = Fixture::default().preflight(o).run();
    assert!(!serde_json::to_string(&report).unwrap().contains("hunter2"));
}

// ── ssh ───────────────────────────────────────────────────────────────────

#[test]
fn ssh_keys_need_consent_unless_policy_allows_a_key() {
    let mut o = options(vec![push_need()]);
    o.can_ask = true;
    let report = Fixture::default().preflight(o).run();
    let ssh = report
        .checks
        .iter()
        .find(|c| matches!(c.prerequisite, WorkflowPrerequisite::PushAuthSsh { .. }))
        .unwrap();
    assert_eq!(ssh.status, PrerequisiteStatus::NeedsConsent);
    assert!(
        matches!(&ssh.remediations[0], Remediation::MountSshKey { path, .. } if path.ends_with("id_ed25519"))
    );
    let pos = |pred: fn(&Remediation) -> bool| ssh.remediations.iter().position(pred).unwrap();
    assert!(
        pos(|r| matches!(r, Remediation::MountSshKey { .. }))
            < pos(|r| matches!(r, Remediation::MountSshDir))
    );
    assert!(
        pos(|r| matches!(r, Remediation::MountSshDir))
            < pos(|r| matches!(r, Remediation::Continue))
    );

    let mut o = options(vec![push_need()]);
    o.policy.ssh.use_ = CredentialUse::Allow;
    o.policy.ssh.key = Some("/fake-home/.ssh/id_ed25519".into());
    o.policy.ssh.source = PolicySource::Global;
    let report = Fixture::default().preflight(o).run();
    let ssh = report
        .checks
        .iter()
        .find(|c| matches!(c.prerequisite, WorkflowPrerequisite::PushAuthSsh { .. }))
        .unwrap();
    assert_eq!(ssh.status, PrerequisiteStatus::Satisfied);
}

#[test]
fn ssh_denied_by_policy_or_without_keys_is_missing() {
    let mut o = options(vec![push_need()]);
    o.policy.ssh.use_ = CredentialUse::Deny;
    let report = Fixture::default().preflight(o).run();
    let ssh = &report.checks[0];
    assert_eq!(ssh.status, PrerequisiteStatus::Missing);
    assert!(ssh.explanation.contains("denied by policy"));

    let mut f = Fixture::default();
    f.ssh.keys.clear();
    let report = f.preflight(options(vec![push_need()])).run();
    assert_eq!(report.checks[0].status, PrerequisiteStatus::Missing);
    assert!(report.checks[0].explanation.contains("no usable SSH key"));
}

#[test]
fn passphrase_protected_and_uid_mismatch_keys_carry_warnings() {
    let mut f = Fixture::default();
    f.ssh.keys = vec![key("id_ed25519", Some(true))];
    f.tools = tools_with(
        &[("git", true), ("gh", true), ("ssh", true)],
        Some(4_000_000_000),
    );
    let mut o = options(vec![push_need()]);
    o.policy.ssh.use_ = CredentialUse::Allow;
    o.policy.ssh.key = Some("/fake-home/.ssh/id_ed25519".into());
    o.policy.ssh.source = PolicySource::Global;
    let report = f.preflight(o).run();
    let text = &report.checks[0].explanation;
    assert!(text.contains("passphrase-protected"), "{text}");
    assert!(text.contains("uid"), "{text}");
}

#[test]
fn repo_suggested_key_is_never_a_grant() {
    let mut o = options(vec![push_need()]);
    o.policy.ssh.use_ = CredentialUse::Ask;
    o.policy.ssh.key = Some("/fake-home/.ssh/id_ed25519".into());
    o.policy.ssh.key_is_suggestion = true;
    o.policy.ssh.source = PolicySource::Repo;
    o.can_ask = true;
    let report = Fixture::default().preflight(o).run();
    let ssh = report
        .checks
        .iter()
        .find(|c| matches!(c.prerequisite, WorkflowPrerequisite::PushAuthSsh { .. }))
        .unwrap();
    assert_eq!(ssh.status, PrerequisiteStatus::NeedsConsent);
    assert!(matches!(
        &ssh.remediations[0],
        Remediation::MountSshKey {
            suggested: true,
            ..
        }
    ));
}

#[test]
fn remote_sessions_deny_host_keys_by_default() {
    let mut o = options(vec![push_need()]);
    o.session_type = Some(crate::data::session::SessionType::Remote {
        repo_url: "ssh://git@github.com/org/remote".into(),
        branch: "main".into(),
        cloned_path: "/uncloned".into(),
    });
    let mut f = Fixture::default();
    f.git.remote = Some("https://gitlab.com/wrong/host".into());
    let report = f.preflight(o).run();
    let ssh = &report.checks[0];
    assert!(
        matches!(&ssh.prerequisite, WorkflowPrerequisite::PushAuthSsh { host, .. } if host == "github.com")
    );
    assert_eq!(ssh.status, PrerequisiteStatus::Missing);
    assert!(ssh.explanation.contains("denied"));
}

// ── token ─────────────────────────────────────────────────────────────────

#[test]
fn static_commit_push_pr_without_token_reports_missing() {
    let mut f = Fixture::default();
    f.git.remote = Some("https://github.com/org/repo".into());
    let mut o = options(vec![
        WorkflowPrerequisite::GitIdentity,
        push_need(),
        token_need(),
        WorkflowPrerequisite::ContainerTool { name: "gh".into() },
    ]);
    o.prerequisites.needs[2].needed_by = vec![step(PrerequisiteStepKind::CreatePullRequest, 2)];
    let report = f.preflight(o).run();
    assert_eq!(
        status_of(&report, &token_need()),
        PrerequisiteStatus::Missing
    );
    assert!(report.any_missing());
    assert!(!report.all_satisfied());
}

#[test]
fn token_in_env_needs_consent_then_is_satisfied_by_policy_or_remembered_decision() {
    let tokens = || FakeTokens::new(Some(("GH_TOKEN", TOKEN)));
    let mut f = Fixture::default();
    f.tokens = tokens();
    let mut o = options(vec![token_need()]);
    o.can_ask = true;
    let report = f.preflight(o).run();
    let check = &report.checks[0];
    assert_eq!(check.status, PrerequisiteStatus::NeedsConsent);
    assert!(check.explanation.contains("found in GH_TOKEN"));
    assert!(
        matches!(&check.remediations[0], Remediation::UseEnvToken { var, .. } if var == "GH_TOKEN")
    );
    assert!(check.remediations.contains(&Remediation::PasteToken));

    let mut f = Fixture::default();
    f.tokens = tokens();
    let mut o = options(vec![token_need()]);
    o.policy.github_token.use_ = CredentialUse::Allow;
    let report = f.preflight(o).run();
    assert_eq!(report.checks[0].status, PrerequisiteStatus::Satisfied);
    assert!(report.checks[0].explanation.contains("verified as fixture"));

    let mut f = Fixture::default();
    f.tokens = tokens();
    let mut o = options(vec![token_need()]);
    o.remembered.token_env = Some("GH_TOKEN".into());
    let report = f.preflight(o).run();
    assert_eq!(report.checks[0].status, PrerequisiteStatus::Satisfied);
}

#[test]
fn token_denied_by_policy_is_missing_and_never_reads_the_env_value() {
    let mut f = Fixture::default();
    f.tokens = FakeTokens::new(Some(("GH_TOKEN", TOKEN)));
    let tokens = f.tokens.clone();
    let mut o = options(vec![token_need()]);
    o.policy.github_token.use_ = CredentialUse::Deny;
    let report = f.preflight(o).run();
    assert_eq!(report.checks[0].status, PrerequisiteStatus::Missing);
    assert_eq!(tokens.verified.load(Ordering::SeqCst), 0);
}

#[test]
fn report_never_contains_the_token_value() {
    let mut f = Fixture::default();
    f.tokens = FakeTokens::new(Some(("GH_TOKEN", TOKEN)));
    let mut o = options(vec![token_need()]);
    o.policy.github_token.use_ = CredentialUse::Allow;
    o.notes.push(format!("operator note mentioning {TOKEN}"));
    let report = f.preflight(o).run();
    let text = serde_json::to_string(&report).unwrap() + &report.render_lines().join("\n");
    assert!(!text.contains(TOKEN), "{text}");
    assert!(text.contains("***"));
}

#[test]
fn poll_ci_alone_accepts_an_authenticated_host_gh() {
    let mut f = Fixture::default();
    let mut tokens = FakeTokens::new(None);
    Arc::get_mut(&mut tokens).unwrap().gh = GhAuthStatus::LoggedIn {
        path: "/usr/bin/gh".into(),
    };
    f.tokens = tokens;
    let mut o = options(vec![token_need()]);
    o.prerequisites.needs[0].needed_by = vec![step(PrerequisiteStepKind::PollCi, 0)];
    let report = f.preflight(o).run();
    assert_eq!(report.checks[0].status, PrerequisiteStatus::Satisfied);
    // The same gh login does not satisfy a create_pull_request container.
    let mut f = Fixture::default();
    let mut tokens = FakeTokens::new(None);
    Arc::get_mut(&mut tokens).unwrap().gh = GhAuthStatus::LoggedIn {
        path: "/usr/bin/gh".into(),
    };
    f.tokens = tokens;
    let mut o = options(vec![token_need()]);
    o.prerequisites.needs[0].needed_by = vec![step(PrerequisiteStepKind::CreatePullRequest, 0)];
    let report = f.preflight(o).run();
    assert_eq!(report.checks[0].status, PrerequisiteStatus::Missing);
}

#[test]
fn pull_requests_to_non_github_remotes_are_flagged_unsupported() {
    let mut f = Fixture::default();
    f.git.remote = Some("https://gitlab.com/org/repo".into());
    let mut o = options(vec![token_need()]);
    o.prerequisites.needs[0].needed_by = vec![step(PrerequisiteStepKind::CreatePullRequest, 0)];
    let report = f.preflight(o).run();
    assert_eq!(report.checks[0].status, PrerequisiteStatus::Missing);
    assert!(report.checks[0].explanation.contains("non-GitHub"));
}

#[test]
fn acquire_choice_is_offered_with_scope_and_approval_state() {
    let acquire = |scope| AcquirePolicy {
        argv: vec!["op".into(), "read".into()],
        timeout_secs: 30,
        refresh: Default::default(),
        scope,
        sha256: "abc".into(),
    };
    for (scope, approved_flag, expect_approved) in [
        (AcquireScope::Global, false, true),
        (AcquireScope::Repo, false, false),
        (AcquireScope::Repo, true, true),
    ] {
        let mut o = options(vec![token_need()]);
        o.can_ask = true;
        o.policy.github_token.acquire = Some(acquire(scope));
        o.repo_acquire_approved = approved_flag;
        let report = Fixture::default().preflight(o).run();
        let found = report.checks[0].remediations.iter().find_map(|r| match r {
            Remediation::RunAcquireCommand {
                approved,
                scope: s,
                argv,
                ..
            } => Some((*approved, *s, argv.clone())),
            _ => None,
        });
        let (approved, s, argv) = found.expect("acquire remediation offered when asking");
        assert_eq!(approved, expect_approved, "{scope:?}/{approved_flag}");
        assert_eq!(s, scope);
        assert_eq!(argv, vec!["op", "read"]);
    }
    // Headless + ask: not offered at all (no prompt can approve it).
    let mut o = options(vec![token_need()]);
    o.policy.github_token.acquire = Some(acquire(AcquireScope::Repo));
    let report = Fixture::default().preflight(o).run();
    assert!(!report.checks[0]
        .remediations
        .iter()
        .any(|r| matches!(r, Remediation::RunAcquireCommand { .. })));
    // Headless + allow + unapproved repo acquire: still not offered.
    let mut o = options(vec![token_need()]);
    o.policy.github_token.use_ = CredentialUse::Allow;
    o.policy.github_token.acquire = Some(acquire(AcquireScope::Repo));
    let report = Fixture::default().preflight(o).run();
    assert!(!report.checks[0]
        .remediations
        .iter()
        .any(|r| matches!(r, Remediation::RunAcquireCommand { .. })));
}

// ── tools, branch, HEAD ───────────────────────────────────────────────────

#[test]
fn container_tool_status_table() {
    let tool = |n: &str| WorkflowPrerequisite::ContainerTool { name: n.into() };
    let mut f = Fixture::default();
    f.tools = tools_with(&[("git", true), ("gh", false), ("ssh", true)], Some(0));
    let report = f.preflight(options(vec![tool("git"), tool("gh")])).run();
    assert_eq!(
        status_of(&report, &tool("git")),
        PrerequisiteStatus::Satisfied
    );
    let gh = report.find(&tool("gh")).unwrap();
    assert_eq!(gh.status, PrerequisiteStatus::Missing);
    assert!(gh
        .remediations
        .iter()
        .any(|r| matches!(r, Remediation::AddDockerfileLine { line, .. } if line.contains("gh"))));
    assert!(gh.remediations.contains(&Remediation::Abort));

    // An unprobeable image (isolation, no runtime, not imported) is Unknown, never Missing.
    let mut f = Fixture::default();
    f.tools = PhaseToolsStatus::Unknown {
        reason: "test isolation".into(),
    };
    let report = f
        .preflight(options(vec![tool("git"), tool("gh"), tool("ssh")]))
        .run();
    assert!(report
        .checks
        .iter()
        .all(|c| c.status == PrerequisiteStatus::Unknown));

    // No resolved image: also Unknown.
    let mut o = options(vec![tool("git")]);
    o.phase_image = None;
    let report = Fixture::default().preflight(o).run();
    assert_eq!(report.checks[0].status, PrerequisiteStatus::Unknown);
}

#[test]
fn pushed_branch_is_satisfied_by_an_earlier_push_or_an_upstream() {
    let pr = step(PrerequisiteStepKind::CreatePullRequest, 1);
    let push = step(PrerequisiteStepKind::PushBranch, 0);
    let build = |push_index: usize| {
        let mut o = options(vec![WorkflowPrerequisite::PushedBranch, push_need()]);
        o.prerequisites.needs[0].needed_by = vec![pr.clone()];
        o.prerequisites.needs[1].needed_by = vec![StepRef {
            index: push_index,
            ..push.clone()
        }];
        o
    };
    let report = Fixture::default().preflight(build(0)).run();
    assert_eq!(
        status_of(&report, &WorkflowPrerequisite::PushedBranch),
        PrerequisiteStatus::Satisfied
    );
    // The push comes after the PR: not satisfied, unless the branch already has an upstream.
    let report = Fixture::default().preflight(build(5)).run();
    assert_eq!(
        status_of(&report, &WorkflowPrerequisite::PushedBranch),
        PrerequisiteStatus::Missing
    );
    let mut f = Fixture::default();
    f.git.upstream = Some("origin/feature".into());
    let report = f.preflight(build(5)).run();
    assert_eq!(
        status_of(&report, &WorkflowPrerequisite::PushedBranch),
        PrerequisiteStatus::Satisfied
    );
}

#[test]
fn detached_head_is_missing_but_a_worktree_is_always_attached() {
    let mut f = Fixture::default();
    f.git.branch = None;
    f.git.detached = true;
    let report = f
        .preflight(options(vec![WorkflowPrerequisite::NotDetachedHead]))
        .run();
    assert_eq!(report.checks[0].status, PrerequisiteStatus::Missing);
    assert!(report.checks[0].remediations.contains(&Remediation::Abort));

    let mut f = Fixture::default();
    f.git.branch = None;
    f.git.detached = true;
    let mut o = options(vec![WorkflowPrerequisite::NotDetachedHead]);
    o.worktree = Some(PathBuf::from("/fake/worktree"));
    let report = f.preflight(o).run();
    assert_eq!(report.checks[0].status, PrerequisiteStatus::Satisfied);

    let mut f = Fixture::default();
    f.git.branch = None;
    f.git.detached = false;
    let report = f
        .preflight(options(vec![WorkflowPrerequisite::NotDetachedHead]))
        .run();
    assert_eq!(report.checks[0].status, PrerequisiteStatus::Unknown);
}

// ── off, budgets ──────────────────────────────────────────────────────────

#[test]
fn preflight_off_makes_every_prerequisite_unknown_and_probes_nothing() {
    let mut f = Fixture::default();
    f.git.delay = Duration::from_secs(5);
    f.tokens = FakeTokens::new(Some(("GH_TOKEN", TOKEN)));
    let tokens = f.tokens.clone();
    let mut o = options(vec![
        WorkflowPrerequisite::GitIdentity,
        token_need(),
        push_need(),
        WorkflowPrerequisite::PushedBranch,
    ]);
    o.policy.preflight = PreflightMode::Off;
    let started = Instant::now();
    let report = f.preflight(o).run();
    assert!(started.elapsed() < Duration::from_secs(1));
    assert_eq!(report.checks.len(), 4);
    assert!(report
        .checks
        .iter()
        .all(|c| c.status == PrerequisiteStatus::Unknown));
    assert!(report
        .checks
        .iter()
        .all(|c| c.explanation == "preflight disabled"));
    assert_eq!(tokens.verified.load(Ordering::SeqCst), 0);
}

#[test]
fn probe_timeout_yields_unknown_without_stalling_the_run() {
    let mut f = Fixture::default();
    f.git.delay = Duration::from_millis(400);
    let mut o = options(vec![
        WorkflowPrerequisite::GitIdentity,
        WorkflowPrerequisite::NotDetachedHead,
    ]);
    o.probe_budget = Duration::from_millis(30);
    let started = Instant::now();
    let report = f.preflight(o).run();
    assert!(
        started.elapsed() < Duration::from_millis(300),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(report.checks.len(), 2);
    assert!(report
        .checks
        .iter()
        .all(|c| c.status == PrerequisiteStatus::Unknown));
    assert!(report.checks[0].explanation.contains("timed out"));
}

#[test]
fn concurrent_probes_respect_one_budget_not_the_sum() {
    let mut f = Fixture::default();
    f.git.delay = Duration::from_millis(300);
    let mut o = options(vec![
        WorkflowPrerequisite::GitIdentity,
        WorkflowPrerequisite::NotDetachedHead,
        WorkflowPrerequisite::PushedBranch,
        WorkflowPrerequisite::GitIdentity,
    ]);
    o.probe_budget = Duration::from_millis(50);
    let started = Instant::now();
    let report = f.preflight(o).run();
    assert!(
        started.elapsed() < Duration::from_millis(250),
        "{:?}",
        started.elapsed()
    );
    assert!(!report.checks.is_empty());
    assert!(PROBE_BUDGET <= Duration::from_secs(10));
}

#[test]
fn duplicate_tool_checks_are_merged_keeping_the_worst_status_and_all_steps() {
    let ssh_tool = WorkflowPrerequisite::ContainerTool { name: "ssh".into() };
    let mut o = options(vec![
        WorkflowPrerequisite::PushAuthSsh {
            remote: "origin".into(),
            host: "github.com".into(),
        },
        ssh_tool.clone(),
    ]);
    o.prerequisites.needs[0].needed_by = vec![step(PrerequisiteStepKind::PushBranch, 0)];
    o.prerequisites.needs[1].needed_by = vec![step(PrerequisiteStepKind::PushBranch, 3)];
    let mut f = Fixture::default();
    f.tools = tools_with(&[("git", true), ("gh", true), ("ssh", false)], Some(0));
    let report = f.preflight(o).run();
    let ssh_checks: Vec<_> = report
        .checks
        .iter()
        .filter(|c| c.prerequisite == ssh_tool)
        .collect();
    assert_eq!(ssh_checks.len(), 1);
    assert_eq!(ssh_checks[0].status, PrerequisiteStatus::Missing);
    assert_eq!(ssh_checks[0].needed_by.len(), 2);
}

// ── before teardown ───────────────────────────────────────────────────────

fn grants_with_token(
    source: TokenGrantSource,
    acquire: Option<AcquirePolicy>,
) -> PrerequisiteGrants {
    PrerequisiteGrants {
        ssh_transports: Default::default(),
        ssh_destinations: Default::default(),
        token_destinations: [GitRemoteUrl::parse("git@github.com:org/repo.git").redacted].into(),
        token: Some(TokenGrant {
            secret: SecretString::new(TOKEN),
            source,
            host: "github.com".into(),
            acquire,
        }),
        ..Default::default()
    }
}

#[test]
fn before_teardown_env_var_unset_is_missing() {
    let mut f = Fixture::default();
    f.tokens = FakeTokens::new(Some(("GH_TOKEN", TOKEN)));
    let tokens = f.tokens.clone();
    let mut o = options(vec![token_need()]);
    o.policy.github_token.use_ = CredentialUse::Allow;
    let pre = f.preflight(o);
    assert_eq!(pre.run().checks[0].status, PrerequisiteStatus::Satisfied);
    tokens.present.store(false, Ordering::SeqCst);
    let mut grants = grants_with_token(TokenGrantSource::Env("GH_TOKEN".into()), None);
    let report = pre.recheck_before_teardown(&mut grants, &RedactionSet::new(), &[]);
    assert_eq!(report.phase, PrerequisitePhase::BeforeTeardown);
    assert_eq!(report.checks[0].status, PrerequisiteStatus::Missing);
    assert!(
        grants.token.is_none(),
        "unsetting the env revokes the old token"
    );
    assert!(!serde_json::to_string(&report).unwrap().contains(TOKEN));
}

#[test]
fn before_teardown_refreshes_only_before_teardown_host_commands() {
    let acquire = |refresh| AcquirePolicy {
        argv: vec!["fake-secret-store".into()],
        timeout_secs: 1,
        refresh,
        scope: AcquireScope::Global,
        sha256: "h".into(),
    };
    let mut f = Fixture::default();
    f.tokens = FakeTokens::new(None);
    let tokens = f.tokens.clone();
    let mut o = options(vec![token_need()]);
    o.policy.github_token.use_ = CredentialUse::Allow;
    let pre = f.preflight(o);
    let redaction = RedactionSet::new();

    let mut grants = grants_with_token(
        TokenGrantSource::HostCommand("cmd".into()),
        Some(acquire(AcquireRefresh::Start)),
    );
    pre.recheck_before_teardown(&mut grants, &redaction, &[]);
    assert_eq!(
        tokens.acquired.load(Ordering::SeqCst),
        0,
        "refresh=start never re-runs"
    );

    let mut grants = grants_with_token(
        TokenGrantSource::HostCommand("cmd".into()),
        Some(acquire(AcquireRefresh::BeforeTeardown)),
    );
    let report = pre.recheck_before_teardown(&mut grants, &redaction, &[]);
    assert_eq!(tokens.acquired.load(Ordering::SeqCst), 1);
    assert_eq!(
        grants.token.as_ref().unwrap().secret.expose(),
        "ghp_REFRESHED_FAKE_TOKEN_9"
    );
    // Both the old and the new value are redacted and neither appears in the report.
    assert_eq!(
        redaction.redact(&format!("{TOKEN} ghp_REFRESHED_FAKE_TOKEN_9")),
        "*** ***"
    );
    assert!(!serde_json::to_string(&report)
        .unwrap()
        .contains("REFRESHED"));

    // An env-sourced token is never refreshed by an unrelated acquire command.
    let mut grants = grants_with_token(
        TokenGrantSource::Env("GH_TOKEN".into()),
        Some(acquire(AcquireRefresh::BeforeTeardown)),
    );
    pre.recheck_before_teardown(&mut grants, &redaction, &[]);
    assert_eq!(tokens.acquired.load(Ordering::SeqCst), 1);
}

#[test]
fn before_teardown_repo_acquire_without_approval_is_not_run_and_marks_unknown() {
    let mut f = Fixture::default();
    f.tokens = FakeTokens::new(None);
    let tokens = f.tokens.clone();
    let mut o = options(vec![token_need()]);
    o.policy.github_token.use_ = CredentialUse::Allow;
    o.repo_acquire_approved = false;
    let pre = f.preflight(o);
    let mut grants = grants_with_token(
        TokenGrantSource::HostCommand("cmd".into()),
        Some(AcquirePolicy {
            argv: vec!["repo-cmd".into()],
            timeout_secs: 1,
            refresh: AcquireRefresh::BeforeTeardown,
            scope: AcquireScope::Repo,
            sha256: "h".into(),
        }),
    );
    let report = pre.recheck_before_teardown(&mut grants, &RedactionSet::new(), &[]);
    assert_eq!(
        tokens.acquired.load(Ordering::SeqCst),
        0,
        "no spawn without approval"
    );
    assert_eq!(report.checks[0].status, PrerequisiteStatus::Unknown);
    assert!(report.checks[0].explanation.contains("refresh failed"));
}

#[test]
fn before_teardown_ignores_setup_phase_needs_and_never_asks() {
    let mut o = options(vec![WorkflowPrerequisite::GitIdentity, push_need()]);
    o.prerequisites.needs[0].needed_by = vec![StepRef {
        phase: PhaseKind::Setup,
        kind: PrerequisiteStepKind::PullBranch,
        index: 0,
        description: "pull".into(),
    }];
    o.prerequisites.needs[1].needed_by = vec![step(PrerequisiteStepKind::PushBranch, 0)];
    o.can_ask = true;
    let pre = Fixture::default().preflight(o);
    let report = pre.recheck_before_teardown(
        &mut PrerequisiteGrants::default(),
        &RedactionSet::new(),
        &[],
    );
    assert!(report.find(&WorkflowPrerequisite::GitIdentity).is_none());
    // can_ask is forced off: no interactive remediations.
    assert!(report
        .checks
        .iter()
        .all(|c| !c.remediations.iter().any(|r| matches!(
            r,
            Remediation::MountSshKey { .. } | Remediation::PasteToken | Remediation::EnterIdentity
        ))));
}

#[test]
fn resolved_prerequisites_reflects_the_classified_checks() {
    let pre = Fixture::default().preflight(options(vec![push_need()]));
    let report = pre.run();
    let resolved = pre.resolved_prerequisites(&report);
    assert!(resolved.contains(&WorkflowPrerequisite::PushAuthSsh {
        remote: "origin".into(),
        host: "github.com".into()
    }));
    assert!(!resolved.contains(&push_need()));
    // Narrowing grants to it keeps an SSH grant for an SSH push.
    let mut grants = PrerequisiteGrants {
        ssh_transports: Default::default(),
        ssh_destinations: Default::default(),
        token_destinations: [GitRemoteUrl::parse("git@github.com:org/repo.git").redacted].into(),
        ssh: Some(SshGrant::WholeDir {
            ssh_dir: "/fake-home/.ssh".into(),
        }),
        ..Default::default()
    };
    assert!(grants.retain_for(&resolved).is_empty());
}

#[test]
fn key_readable_in_image_requires_root_or_matching_uid() {
    use crate::data::fs::phase_tools_cache::PhaseTools;
    let with_uid = |uid| PhaseTools {
        tools: Default::default(),
        user: None,
        uid,
        probed_at: "0".into(),
    };
    assert_eq!(
        PrerequisitePreflight::key_readable_in_image(&with_uid(Some(0))),
        Some(true)
    );
    assert_eq!(
        PrerequisitePreflight::key_readable_in_image(&with_uid(None)),
        None
    );
    assert_eq!(
        PrerequisitePreflight::key_readable_in_image(&with_uid(Some(4_000_000_000))),
        Some(false)
    );
}

// ── security regressions (engine level) ───────────────────────────────────

#[test]
fn a_full_preflight_never_runs_a_host_command() {
    // run() only probes. Even with allow + an approved global acquire and no env
    // token, nothing is acquired: acquisition is a Layer 2 decision.
    let mut f = Fixture::default();
    f.tokens = FakeTokens::new(None);
    let tokens = f.tokens.clone();
    let mut o = options(vec![
        token_need(),
        push_need(),
        WorkflowPrerequisite::GitIdentity,
    ]);
    o.policy.github_token.use_ = CredentialUse::Allow;
    o.policy.ssh.use_ = CredentialUse::Allow;
    o.policy.github_token.acquire = Some(AcquirePolicy {
        argv: vec!["touch".into(), "/tmp/awman-must-not-run".into()],
        timeout_secs: 1,
        refresh: AcquireRefresh::Start,
        scope: AcquireScope::Repo,
        sha256: "h".into(),
    });
    o.repo_acquire_approved = false;
    let report = f.preflight(o).run();
    assert_eq!(tokens.acquired.load(Ordering::SeqCst), 0);
    assert!(!Path::new("/tmp/awman-must-not-run").exists());
    assert_eq!(
        status_of(&report, &token_need()),
        PrerequisiteStatus::Missing
    );
}

#[test]
fn repo_policy_with_allow_and_acquire_grants_nothing_in_a_headless_report() {
    use crate::data::config::flags::PrerequisiteFlags;
    use crate::data::config::workflows::{
        PrerequisitePolicyLayers, PrerequisiteResolveContext, PrerequisitesConfig,
    };
    let repo: PrerequisitesConfig = serde_json::from_value(serde_json::json!({
        "ssh": {"use": "allow", "key": "~/.ssh/id_ed25519"},
        "githubToken": {"use": "allow", "acquire": {"argv": ["touch", "/tmp/awman-repo-marker"]}}
    }))
    .unwrap();
    let policy = PrerequisitePolicy::resolve(PrerequisitePolicyLayers {
        global: None,
        squad_global: None,
        repo: Some(&repo),
        task: None,
        flags: &PrerequisiteFlags::default(),
        context: PrerequisiteResolveContext::default(),
    });
    let mut f = Fixture::default();
    f.tokens = FakeTokens::new(None);
    let tokens = f.tokens.clone();
    let mut o = options(vec![token_need(), push_need()]);
    o.policy = policy;
    o.can_ask = false;
    let report = f.preflight(o).run();
    // SSH: still needs consent (the repo suggestion is not a grant); token: missing.
    let ssh = report
        .checks
        .iter()
        .find(|c| matches!(c.prerequisite, WorkflowPrerequisite::PushAuthSsh { .. }))
        .unwrap();
    assert_eq!(ssh.status, PrerequisiteStatus::NeedsConsent);
    assert_eq!(
        status_of(&report, &token_need()),
        PrerequisiteStatus::Missing
    );
    assert!(!report
        .checks
        .iter()
        .flat_map(|c| &c.remediations)
        .any(|r| matches!(r, Remediation::RunAcquireCommand { approved: true, .. })));
    assert_eq!(tokens.acquired.load(Ordering::SeqCst), 0);
    assert!(!Path::new("/tmp/awman-repo-marker").exists());
}

#[test]
fn token_never_reaches_the_report_through_notes_remediations_or_json() {
    let mut f = Fixture::default();
    f.tokens = FakeTokens::new(Some(("GH_TOKEN", TOKEN)));
    let mut o = options(vec![token_need()]);
    o.can_ask = true;
    o.existing_grants = Some(grants_with_token(
        TokenGrantSource::Env("GH_TOKEN".into()),
        None,
    ));
    o.notes.push(format!("token is {TOKEN}"));
    let report = f.preflight(o).run();
    assert!(!format!("{report:?}").contains(TOKEN));
    assert!(!serde_json::to_string(&report).unwrap().contains(TOKEN));
}

#[test]
fn enterprise_host_is_classified_and_global_allow_never_leaks_to_a_new_host() {
    let mut f = Fixture::default();
    f.git.remote = Some("https://ghe.corp.test/org/repo".into());
    f.tokens = FakeTokens::new(Some(("GH_TOKEN", TOKEN)));
    let tokens = f.tokens.clone();
    let mut options = options(vec![token_need()]);
    options.policy.github_token.use_ = CredentialUse::Allow;
    let report = f.preflight(options).run();
    assert_eq!(
        report.checks[0].prerequisite,
        WorkflowPrerequisite::GitHubToken {
            host: "ghe.corp.test".into()
        }
    );
    assert_eq!(report.checks[0].status, PrerequisiteStatus::NeedsConsent);
    assert_eq!(
        tokens.verified.load(Ordering::SeqCst),
        0,
        "ambient token must not be sent to a new host before consent"
    );
}

#[test]
fn rejected_token_is_never_satisfied_and_offers_replacement() {
    let mut f = Fixture::default();
    let mut tokens = FakeTokens::new(Some(("GH_TOKEN", TOKEN)));
    Arc::get_mut(&mut tokens).unwrap().reject = true;
    f.tokens = tokens;
    let mut options = options(vec![token_need()]);
    options.policy.github_token.use_ = CredentialUse::Allow;
    options.can_ask = true;
    let report = f.preflight(options).run();
    assert_eq!(report.checks[0].status, PrerequisiteStatus::NeedsConsent);
    assert!(report.checks[0]
        .remediations
        .contains(&Remediation::ReplaceRejectedToken));
}

#[test]
fn planned_worktree_is_attached_before_the_worktree_exists() {
    let mut fixture = Fixture::default();
    fixture.git.detached = true;
    fixture.git.branch = None;
    let mut options = options(vec![WorkflowPrerequisite::NotDetachedHead]);
    options.worktree_planned = true;
    let report = fixture.preflight(options).run();
    assert_eq!(report.checks[0].status, PrerequisiteStatus::Satisfied);
}
