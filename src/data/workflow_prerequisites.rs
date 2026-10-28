//! Workflow prerequisite data and policy.

use crate::data::workflow_definition::{SetupStep, TeardownStep, Workflow};
use crate::data::workflow_state::PhaseKind;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrerequisiteStepKind {
    CommitChanges,
    PushBranch,
    CreatePullRequest,
    PollCi,
    PullBranch,
    CloneRepo,
    CheckoutCreateBranch,
    RunShell,
    RunScript,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepRef {
    pub phase: PhaseKind,
    pub kind: PrerequisiteStepKind,
    pub index: usize,
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GitRemoteKind {
    Ssh { host: String },
    Https { host: String },
    File,
    Other,
}

/// A remote URL with any embedded credentials removed at parse time. The raw URL is never stored.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct GitRemoteUrl {
    pub redacted: String,
    pub kind: GitRemoteKind,
    pub owner_repo: Option<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkflowPrerequisite {
    GitIdentity,
    GitRemote {
        remote: String,
    },
    /// Derived before classification. The preflight replaces it (see §2 item 5).
    PushAuth {
        remote: String,
    },
    PushAuthSsh {
        remote: String,
        host: String,
    },
    PushAuthHttps {
        remote: String,
        host: String,
    },
    GitHubToken {
        host: String,
    },
    ContainerTool {
        name: String,
    },
    PushedBranch,
    NotDetachedHead,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrerequisiteOrigin {
    Static,
    AssumedAll,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrerequisiteNeed {
    pub prerequisite: WorkflowPrerequisite,
    pub needed_by: Vec<StepRef>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrerequisiteAdvisory {
    pub step: StepRef,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowPrerequisites {
    pub origin: PrerequisiteOrigin,
    pub needs: Vec<PrerequisiteNeed>,
    pub advisories: Vec<PrerequisiteAdvisory>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrerequisiteStatus {
    Satisfied,
    NeedsConsent,
    Missing,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcquireScope {
    Global,
    Repo,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Remediation {
    ReplaceRejectedToken,
    UseHostIdentity {
        name: String,
        email: String,
    },
    UseRepoConfigIdentity {
        name: String,
        email: String,
    },
    EnterIdentity,
    SetLocalIdentity {
        name: String,
        email: String,
        command: String,
    },
    MountSshKey {
        path: String,
        passphrase_protected: Option<bool>,
        mode_ok: bool,
        remembered: bool,
        suggested: bool,
    },
    MountSshDir,
    UseEnvToken {
        var: String,
        remembered: bool,
    },
    PasteToken,
    RunGhAuthToken {
        argv: Vec<String>,
        resolved_path: String,
        logged_in: Option<bool>,
    },
    RunGhAuthLogin {
        argv: Vec<String>,
        resolved_path: String,
    },
    RunAcquireCommand {
        label: String,
        argv: Vec<String>,
        resolved_path: Option<String>,
        scope: AcquireScope,
        approved: bool,
        sha256: String,
    },
    AddDockerfileLine {
        file: String,
        line: String,
    },
    AddOverlayHint {
        hint: String,
    },
    Continue,
    Abort,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrerequisiteCheck {
    pub prerequisite: WorkflowPrerequisite,
    pub status: PrerequisiteStatus,
    pub explanation: String,
    /// Empty means "any teardown step" (AssumedAll).
    pub needed_by: Vec<StepRef>,
    pub remediations: Vec<Remediation>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrerequisitePhase {
    Start,
    BeforeTeardown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowPrerequisiteReport {
    pub phase: PrerequisitePhase,
    pub origin: PrerequisiteOrigin,
    pub checks: Vec<PrerequisiteCheck>,
    pub advisories: Vec<PrerequisiteAdvisory>,
    /// Free-text notes: policy summary ("identity from repo config"), the global acquire argv shown once,
    /// "repo config mounts ~/.ssh into every step", "granted but unused: SSH key (not mounted)".
    pub notes: Vec<String>,
}

// PhaseKind predates hashable step references; hash its stable label without
// expanding that unrelated type's API.
impl std::hash::Hash for StepRef {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.phase.label().hash(state);
        self.kind.hash(state);
        self.index.hash(state);
        self.description.hash(state);
    }
}

impl PrerequisiteStepKind {
    pub fn of_teardown(step: &TeardownStep) -> Self {
        match step {
            TeardownStep::CommitChanges { .. } => Self::CommitChanges,
            TeardownStep::PushBranch { .. } => Self::PushBranch,
            TeardownStep::CreatePullRequest { .. } => Self::CreatePullRequest,
            TeardownStep::PollCi { .. } => Self::PollCi,
            TeardownStep::RunShell { .. } => Self::RunShell,
            TeardownStep::RunScript { .. } => Self::RunScript,
        }
    }
    pub fn of_setup(step: &SetupStep) -> Self {
        match step {
            SetupStep::PullBranch { .. } => Self::PullBranch,
            SetupStep::CloneRepo { .. } => Self::CloneRepo,
            SetupStep::CheckoutCreateBranch { .. } => Self::CheckoutCreateBranch,
            SetupStep::PollCi { .. } => Self::PollCi,
            SetupStep::RunShell { .. } => Self::RunShell,
            SetupStep::RunScript { .. } => Self::RunScript,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::CommitChanges => "commit_changes",
            Self::PushBranch => "push_branch",
            Self::CreatePullRequest => "create_pull_request",
            Self::PollCi => "poll_ci",
            Self::PullBranch => "pull_branch",
            Self::CloneRepo => "clone_repo",
            Self::CheckoutCreateBranch => "checkout_create_branch",
            Self::RunShell => "run_shell",
            Self::RunScript => "run_script",
        }
    }
    pub fn receives_grants(self) -> bool {
        matches!(
            self,
            Self::CommitChanges
                | Self::PushBranch
                | Self::CreatePullRequest
                | Self::PullBranch
                | Self::CloneRepo
        )
    }
}

impl GitRemoteUrl {
    pub fn parse(url: &str) -> Self {
        let url = url.trim();
        let (redacted, kind, path) = if let Some((scheme, rest)) = url.split_once("://") {
            let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
            let authority = rest[..end].rsplit('@').next().unwrap_or_default();
            let host = if let Some(ipv6) = authority.strip_prefix('[') {
                ipv6.split(']').next().unwrap_or_default()
            } else {
                authority.split(':').next().unwrap_or_default()
            }
            .to_ascii_lowercase();
            // URL query strings and fragments can carry credentials too.
            let path = rest[end..].split(['?', '#']).next().unwrap_or_default();
            let kind = match scheme.to_ascii_lowercase().as_str() {
                "ssh" => GitRemoteKind::Ssh { host },
                "https" => GitRemoteKind::Https { host },
                "file" => GitRemoteKind::File,
                _ => GitRemoteKind::Other,
            };
            (
                format!("{scheme}://{authority}{path}"),
                kind,
                path.to_owned(),
            )
        } else if let Some((authority, path)) = url
            .split_once(':')
            .filter(|(a, _)| !a.contains('/') && !a.contains('\\') && a.len() > 1)
        {
            let host = authority
                .rsplit('@')
                .next()
                .unwrap_or_default()
                .to_ascii_lowercase();
            (
                format!("{host}:{path}"),
                GitRemoteKind::Ssh { host },
                path.to_owned(),
            )
        } else {
            (url.to_owned(), GitRemoteKind::File, String::new())
        };
        let parts: Vec<_> = path.trim_matches('/').split('/').collect();
        let owner_repo = if parts.len() == 2 && parts.iter().all(|p| !p.is_empty()) {
            Some((
                parts[0].to_owned(),
                parts[1].trim_end_matches(".git").to_owned(),
            ))
        } else {
            None
        };
        Self {
            redacted,
            kind,
            owner_repo,
        }
    }
    pub fn host(&self) -> Option<&str> {
        match &self.kind {
            GitRemoteKind::Ssh { host } | GitRemoteKind::Https { host } => Some(host),
            _ => None,
        }
    }
    pub fn is_github_com(&self) -> bool {
        self.host() == Some("github.com")
    }
    pub fn display(&self) -> &str {
        &self.redacted
    }
}

impl WorkflowPrerequisite {
    pub fn label(&self) -> String {
        match self {
            Self::GitIdentity => "Git identity".into(),
            Self::GitRemote { remote } => format!("Git remote {remote}"),
            Self::PushAuth { remote } => format!("Push authentication ({remote})"),
            Self::PushAuthSsh { host, .. } => format!("Push over SSH ({host})"),
            Self::PushAuthHttps { host, .. } => format!("Push over HTTPS ({host})"),
            Self::GitHubToken { host } => format!("GitHub token ({host})"),
            Self::ContainerTool { name } => format!("Container tool: {name}"),
            Self::PushedBranch => "Pushed branch".into(),
            Self::NotDetachedHead => "Attached HEAD".into(),
        }
    }
    pub fn required_for_teardown(step: &TeardownStep) -> Vec<Self> {
        let git = Self::ContainerTool { name: "git".into() };
        match step {
            TeardownStep::CommitChanges { .. } => vec![Self::GitIdentity, git],
            TeardownStep::PushBranch { remote, branch } => {
                let remote = GitRemoteUrl::parse(remote.as_deref().unwrap_or("origin")).redacted;
                let mut needs = vec![
                    Self::GitRemote {
                        remote: remote.clone(),
                    },
                    Self::PushAuth { remote },
                    git,
                ];
                if branch.is_none() {
                    needs.push(Self::NotDetachedHead);
                }
                needs
            }
            TeardownStep::CreatePullRequest { .. } => vec![
                Self::GitRemote {
                    remote: "origin".into(),
                },
                Self::GitHubToken {
                    host: "github.com".into(),
                },
                git,
                Self::ContainerTool { name: "gh".into() },
                Self::PushedBranch,
            ],
            TeardownStep::PollCi { .. } => vec![Self::GitHubToken {
                host: "github.com".into(),
            }],
            TeardownStep::RunShell { .. } | TeardownStep::RunScript { .. } => vec![],
        }
    }
    pub fn required_for_setup(step: &SetupStep) -> Vec<Self> {
        match step {
            SetupStep::PullBranch { remote, .. } => {
                let remote = GitRemoteUrl::parse(remote.as_deref().unwrap_or("origin")).redacted;
                vec![
                    Self::GitRemote {
                        remote: remote.clone(),
                    },
                    Self::PushAuth { remote },
                ]
            }
            SetupStep::CloneRepo { url, .. } => vec![Self::PushAuth {
                remote: GitRemoteUrl::parse(url).redacted,
            }],
            SetupStep::PollCi { .. } => vec![Self::GitHubToken {
                host: "github.com".into(),
            }],
            _ => vec![],
        }
    }
}

impl WorkflowPrerequisites {
    pub fn from_workflow(workflow: &Workflow) -> Self {
        let mut result = Self {
            origin: PrerequisiteOrigin::Static,
            needs: vec![],
            advisories: vec![],
        };
        for (index, entry) in workflow.setup.iter().enumerate() {
            let kind = PrerequisiteStepKind::of_setup(&entry.step);
            let step = StepRef {
                phase: PhaseKind::Setup,
                kind,
                index,
                description: kind.label().into(),
            };
            let text = match &entry.step {
                SetupStep::RunShell { command, .. } => Some(command.as_str()),
                SetupStep::RunScript { path, .. } => Some(path.as_str()),
                _ => None,
            };
            result.add_step(
                step,
                WorkflowPrerequisite::required_for_setup(&entry.step),
                text,
            );
        }
        for (index, entry) in workflow.teardown.iter().enumerate() {
            let kind = PrerequisiteStepKind::of_teardown(&entry.step);
            let step = StepRef {
                phase: PhaseKind::Teardown,
                kind,
                index,
                description: kind.label().into(),
            };
            let text = match &entry.step {
                TeardownStep::RunShell { command, .. } => Some(command.as_str()),
                TeardownStep::RunScript { path } => Some(path.as_str()),
                _ => None,
            };
            result.add_step(
                step,
                WorkflowPrerequisite::required_for_teardown(&entry.step),
                text,
            );
        }
        result
    }
    fn add_step(
        &mut self,
        step: StepRef,
        prerequisites: Vec<WorkflowPrerequisite>,
        text: Option<&str>,
    ) {
        for prerequisite in prerequisites {
            self.merge_needs(PrerequisiteNeed {
                prerequisite,
                needed_by: vec![step.clone()],
            });
        }
        if text.is_some_and(|s| {
            ["git push", "git commit", "gh "]
                .iter()
                .any(|needle| s.contains(needle))
        }) {
            self.advisories.push(PrerequisiteAdvisory { step, message: "This shell/script step may need credentials; use a structured workflow step to request workflow prerequisites.".into() });
        }
    }
    pub fn assume_all() -> Self {
        use WorkflowPrerequisite::*;
        Self {
            origin: PrerequisiteOrigin::AssumedAll,
            needs: vec![
                GitIdentity,
                GitRemote {
                    remote: "origin".into(),
                },
                PushAuth {
                    remote: "origin".into(),
                },
                GitHubToken {
                    host: "github.com".into(),
                },
                ContainerTool { name: "git".into() },
                ContainerTool { name: "gh".into() },
                ContainerTool { name: "ssh".into() },
                NotDetachedHead,
            ]
            .into_iter()
            .map(|prerequisite| PrerequisiteNeed {
                prerequisite,
                needed_by: vec![],
            })
            .collect(),
            advisories: vec![],
        }
    }
    /// Replace dynamic assumptions with the generated workflow's concrete needs.
    pub fn narrow_to(&mut self, workflow: &Workflow) {
        *self = Self::from_workflow(workflow);
    }
    pub fn is_empty(&self) -> bool {
        self.needs.is_empty()
    }
    pub fn contains(&self, p: &WorkflowPrerequisite) -> bool {
        self.needs.iter().any(|n| &n.prerequisite == p)
    }
    pub fn step_kinds(&self) -> BTreeSet<PrerequisiteStepKind> {
        self.needs
            .iter()
            .flat_map(|n| n.needed_by.iter().map(|s| s.kind))
            .collect()
    }
    pub fn merge_needs(&mut self, other: PrerequisiteNeed) {
        if let Some(need) = self
            .needs
            .iter_mut()
            .find(|n| n.prerequisite == other.prerequisite)
        {
            for step in other.needed_by {
                if !need.needed_by.contains(&step) {
                    need.needed_by.push(step);
                }
            }
        } else {
            self.needs.push(other);
        }
    }
}

impl WorkflowPrerequisiteReport {
    pub fn all_satisfied(&self) -> bool {
        self.needs_attention().is_empty()
    }
    pub fn any_missing(&self) -> bool {
        self.checks
            .iter()
            .any(|c| c.status == PrerequisiteStatus::Missing)
    }
    pub fn needs_attention(&self) -> Vec<&PrerequisiteCheck> {
        self.checks
            .iter()
            .filter(|c| {
                matches!(
                    c.status,
                    PrerequisiteStatus::Missing | PrerequisiteStatus::NeedsConsent
                )
            })
            .collect()
    }
    pub fn find(&self, p: &WorkflowPrerequisite) -> Option<&PrerequisiteCheck> {
        self.checks.iter().find(|c| &c.prerequisite == p)
    }
    pub fn find_mut(&mut self, p: &WorkflowPrerequisite) -> Option<&mut PrerequisiteCheck> {
        self.checks.iter_mut().find(|c| &c.prerequisite == p)
    }
    pub fn summary_line(&self) -> String {
        let count = self.needs_attention().len();
        if count == 0 {
            format!(
                "all {} workflow prerequisites are satisfied",
                self.checks.len()
            )
        } else {
            format!(
                "{count} of {} workflow prerequisites need your attention",
                self.checks.len()
            )
        }
    }
    pub fn render_lines(&self) -> Vec<String> {
        let mut lines = vec![format!(
            "{:<32} {:<14} {}",
            "Workflow prerequisite", "Status", "Details"
        )];
        for check in &self.checks {
            let status = match check.status {
                PrerequisiteStatus::Satisfied => "satisfied",
                PrerequisiteStatus::NeedsConsent => "needs consent",
                PrerequisiteStatus::Missing => "missing",
                PrerequisiteStatus::Unknown => "unknown",
            };
            lines.push(format!(
                "{:<32} {:<14} {}",
                check.prerequisite.label(),
                status,
                check.explanation
            ));
        }
        lines.extend(
            self.advisories
                .iter()
                .map(|a| format!("Advisory ({}): {}", a.step.kind.label(), a.message)),
        );
        lines.extend(self.notes.iter().cloned());
        lines.push(self.summary_line());
        lines
    }
    pub fn leader_summary(&self) -> String {
        use PrerequisiteStepKind as Kind;
        let kinds = [
            Kind::CommitChanges,
            Kind::PushBranch,
            Kind::CreatePullRequest,
            Kind::PollCi,
            Kind::PullBranch,
            Kind::CloneRepo,
        ];
        kinds.into_iter().filter_map(|kind| {
            let checks: Vec<_> = self.checks.iter().filter(|check| {
                if !check.needed_by.is_empty() { return check.needed_by.iter().any(|step| step.kind == kind); }
                // AssumedAll has no step references yet. Describe capabilities
                // using the same structured-step prerequisite categories.
                match &check.prerequisite {
                    WorkflowPrerequisite::GitIdentity => kind == Kind::CommitChanges,
                    WorkflowPrerequisite::GitRemote { .. } => matches!(kind, Kind::PushBranch | Kind::CreatePullRequest | Kind::PullBranch),
                    WorkflowPrerequisite::PushAuth { .. } | WorkflowPrerequisite::PushAuthSsh { .. } | WorkflowPrerequisite::PushAuthHttps { .. } => matches!(kind, Kind::PushBranch | Kind::PullBranch | Kind::CloneRepo),
                    WorkflowPrerequisite::GitHubToken { .. } => matches!(kind, Kind::CreatePullRequest | Kind::PollCi),
                    WorkflowPrerequisite::ContainerTool { name } => match name.as_str() {
                        "git" => matches!(kind, Kind::CommitChanges | Kind::PushBranch | Kind::CreatePullRequest),
                        "gh" => kind == Kind::CreatePullRequest,
                        "ssh" => matches!(kind, Kind::PushBranch | Kind::PullBranch | Kind::CloneRepo),
                        _ => false,
                    },
                    WorkflowPrerequisite::NotDetachedHead => kind == Kind::PushBranch,
                    WorkflowPrerequisite::PushedBranch => kind == Kind::CreatePullRequest,
                }
            }).collect();
            if checks.is_empty() { return None; }
            let missing = checks.iter().any(|c| matches!(c.status, PrerequisiteStatus::Missing | PrerequisiteStatus::NeedsConsent));
            let unknown = checks.iter().any(|c| c.status == PrerequisiteStatus::Unknown);
            let mark = if missing { "✗" } else if unknown { "?" } else { "✓" };
            let label = match kind {
                Kind::CommitChanges => "commit",
                Kind::PushBranch if checks.iter().any(|c| matches!(c.prerequisite, WorkflowPrerequisite::PushAuthSsh { .. })) => "push over SSH",
                Kind::PushBranch if checks.iter().any(|c| matches!(c.prerequisite, WorkflowPrerequisite::PushAuthHttps { .. })) => "push over HTTPS",
                _ => kind.label(),
            };
            let token_missing = checks.iter().any(|c| matches!(c.prerequisite, WorkflowPrerequisite::GitHubToken { .. }) && matches!(c.status, PrerequisiteStatus::Missing | PrerequisiteStatus::NeedsConsent));
            let hint = if kind == Kind::CreatePullRequest && token_missing { " (no GitHub token: do not add a create_pull_request step; mention it in your summary)" } else { "" };
            Some(format!("{label} {mark}{hint}"))
        }).collect::<Vec<_>>().join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_classification_strips_userinfo_and_supports_enterprise_hosts() {
        for (input, expected, ssh) in [
            (
                "git@github.com:owner/repo.git",
                "github.com:owner/repo.git",
                true,
            ),
            (
                "ssh://git@ghe.example:2222/owner/repo.git",
                "ssh://ghe.example:2222/owner/repo.git",
                true,
            ),
            (
                "https://user:password@ghe.example/owner/repo.git?access_token=unused#fragment",
                "https://ghe.example/owner/repo.git",
                false,
            ),
        ] {
            let remote = GitRemoteUrl::parse(input);
            assert_eq!(remote.display(), expected);
            assert_eq!(matches!(remote.kind, GitRemoteKind::Ssh { .. }), ssh);
            assert_eq!(remote.owner_repo, Some(("owner".into(), "repo".into())));
            assert!(!serde_json::to_string(&remote).unwrap().contains("password"));
        }
        for path in ["/tmp/repo", "../repo", "file:///tmp/repo", "C:\\repo"] {
            assert_eq!(GitRemoteUrl::parse(path).kind, GitRemoteKind::File);
        }
    }

    #[test]
    fn shell_heuristic_is_advisory_and_dynamic_needs_can_narrow() {
        let workflow: Workflow = serde_json::from_value(serde_json::json!({
            "title": null, "steps": [], "teardown": [
                {"type": "run_shell", "command": "git push; gh pr create"},
                {"type": "commit_changes", "message": "done"}
            ]
        }))
        .unwrap();
        let mut needs = WorkflowPrerequisites::assume_all();
        needs.narrow_to(&workflow);
        assert_eq!(needs.origin, PrerequisiteOrigin::Static);
        assert_eq!(needs.needs.len(), 2);
        assert_eq!(needs.advisories.len(), 1);
        assert!(needs.contains(&WorkflowPrerequisite::GitIdentity));
        let json = serde_json::to_string(&needs).unwrap();
        assert!(!json.contains("gh pr create"));
        assert!(!PrerequisiteStepKind::RunShell.receives_grants());
    }
}

#[cfg(test)]
mod derivation_tests {
    use super::*;
    use WorkflowPrerequisite as P;

    fn workflow(teardown: serde_json::Value) -> Workflow {
        serde_json::from_value(serde_json::json!({
            "title": null, "steps": [], "teardown": teardown
        }))
        .unwrap()
    }

    fn names(needs: &WorkflowPrerequisites) -> Vec<WorkflowPrerequisite> {
        needs.needs.iter().map(|n| n.prerequisite.clone()).collect()
    }

    fn git() -> P {
        P::ContainerTool { name: "git".into() }
    }

    #[test]
    fn required_for_commit_changes_is_identity_and_git() {
        let needs = WorkflowPrerequisites::from_workflow(&workflow(
            serde_json::json!([{"type": "commit_changes", "message": "m"}]),
        ));
        assert_eq!(names(&needs), vec![P::GitIdentity, git()]);
        assert_eq!(needs.origin, PrerequisiteOrigin::Static);
        assert_eq!(
            needs.step_kinds(),
            BTreeSet::from([PrerequisiteStepKind::CommitChanges])
        );
    }

    #[test]
    fn required_for_push_branch_default_branch_adds_not_detached_head() {
        let default = WorkflowPrerequisites::from_workflow(&workflow(
            serde_json::json!([{"type": "push_branch"}]),
        ));
        assert!(default.contains(&P::NotDetachedHead));
        assert!(default.contains(&P::PushAuth {
            remote: "origin".into()
        }));
        assert!(default.contains(&P::GitRemote {
            remote: "origin".into()
        }));
        assert!(default.contains(&git()));
        let explicit = WorkflowPrerequisites::from_workflow(&workflow(
            serde_json::json!([{"type": "push_branch", "branch": "feature"}]),
        ));
        assert!(!explicit.contains(&P::NotDetachedHead));
    }

    #[test]
    fn push_branch_remote_with_embedded_credentials_is_stripped_in_prerequisites() {
        let needs = WorkflowPrerequisites::from_workflow(&workflow(serde_json::json!([
            {"type": "push_branch", "remote": "https://user:hunter2@ghe.example/o/r.git"}
        ])));
        let json = serde_json::to_string(&needs).unwrap();
        assert!(!json.contains("hunter2"));
        assert!(!json.contains("user:"));
        assert!(json.contains("https://ghe.example/o/r.git"));
    }

    #[test]
    fn required_for_create_pull_request_needs_token_gh_and_pushed_branch() {
        let needs = WorkflowPrerequisites::from_workflow(&workflow(
            serde_json::json!([{"type": "create_pull_request"}]),
        ));
        for expected in [
            P::GitHubToken {
                host: "github.com".into(),
            },
            P::ContainerTool { name: "gh".into() },
            git(),
            P::PushedBranch,
        ] {
            assert!(needs.contains(&expected), "missing {expected:?}");
        }
        assert!(!needs.contains(&P::GitIdentity));
    }

    #[test]
    fn poll_ci_needs_only_a_github_token() {
        let needs = WorkflowPrerequisites::from_workflow(&workflow(
            serde_json::json!([{"type": "poll_ci"}]),
        ));
        assert_eq!(
            names(&needs),
            vec![P::GitHubToken {
                host: "github.com".into()
            }]
        );
    }

    #[test]
    fn run_shell_and_run_script_add_only_advisories() {
        let needs = WorkflowPrerequisites::from_workflow(&workflow(serde_json::json!([
            {"type": "run_shell", "command": "git push origin HEAD"},
            {"type": "run_shell", "command": "git commit -am x"},
            {"type": "run_shell", "command": "gh pr create"},
            {"type": "run_script", "path": "scripts/gh-release.sh"},
            {"type": "run_shell", "command": "echo hello"}
        ])));
        assert!(needs.is_empty(), "opaque steps never add hard checks");
        // The run_script step is scanned by path only, which names no git/gh command.
        assert_eq!(needs.advisories.len(), 3);
        assert!(needs
            .advisories
            .iter()
            .all(|a| a.step.phase == PhaseKind::Teardown && !a.message.contains("git push")));
    }

    #[test]
    fn assume_all_covers_every_teardown_kind() {
        let all = WorkflowPrerequisites::assume_all();
        assert_eq!(all.origin, PrerequisiteOrigin::AssumedAll);
        for expected in [
            P::GitIdentity,
            P::GitRemote {
                remote: "origin".into(),
            },
            P::PushAuth {
                remote: "origin".into(),
            },
            P::GitHubToken {
                host: "github.com".into(),
            },
            git(),
            P::ContainerTool { name: "gh".into() },
            P::ContainerTool { name: "ssh".into() },
            P::NotDetachedHead,
        ] {
            assert!(all.contains(&expected), "missing {expected:?}");
        }
        // Every prerequisite any concrete teardown kind needs is covered by assume_all.
        let concrete = WorkflowPrerequisites::from_workflow(&workflow(serde_json::json!([
            {"type": "commit_changes", "message": "m"},
            {"type": "push_branch"},
            {"type": "create_pull_request"},
            {"type": "poll_ci"}
        ])));
        for need in &concrete.needs {
            if need.prerequisite == P::PushedBranch {
                continue; // order-dependent, not assumable
            }
            assert!(all.contains(&need.prerequisite), "{:?}", need.prerequisite);
        }
        assert!(all.advisories.is_empty());
    }

    #[test]
    fn narrowing_drops_prerequisites_of_unused_step_kinds() {
        let mut needs = WorkflowPrerequisites::assume_all();
        needs.narrow_to(&workflow(
            serde_json::json!([{"type": "commit_changes", "message": "m"}]),
        ));
        assert!(needs.contains(&P::GitIdentity));
        assert!(!needs.contains(&P::ContainerTool { name: "ssh".into() }));
        assert!(!needs.contains(&P::GitHubToken {
            host: "github.com".into()
        }));
        assert!(!needs.contains(&P::PushAuth {
            remote: "origin".into()
        }));
        needs.narrow_to(&workflow(serde_json::json!([])));
        assert!(needs.is_empty());
    }

    #[test]
    fn setup_steps_reuse_push_auth_and_record_phase() {
        let workflow: Workflow = serde_json::from_value(serde_json::json!({
            "title": null, "steps": [],
            "setup": [
                {"type": "clone_repo", "url": "https://tok@github.com/o/r.git"},
                {"type": "pull_branch"}
            ]
        }))
        .unwrap();
        let needs = WorkflowPrerequisites::from_workflow(&workflow);
        assert!(needs.contains(&P::PushAuth {
            remote: "https://github.com/o/r.git".into()
        }));
        assert!(needs.contains(&P::PushAuth {
            remote: "origin".into()
        }));
        assert!(needs
            .needs
            .iter()
            .flat_map(|n| &n.needed_by)
            .all(|s| s.phase == PhaseKind::Setup));
        assert!(PrerequisiteStepKind::CloneRepo.receives_grants());
        assert!(!PrerequisiteStepKind::RunScript.receives_grants());
    }

    #[test]
    fn same_prerequisite_from_two_steps_is_merged_with_both_step_refs() {
        let needs = WorkflowPrerequisites::from_workflow(&workflow(serde_json::json!([
            {"type": "commit_changes", "message": "a"},
            {"type": "commit_changes", "message": "b"}
        ])));
        let identity = needs
            .needs
            .iter()
            .find(|n| n.prerequisite == P::GitIdentity)
            .unwrap();
        assert_eq!(identity.needed_by.len(), 2);
        assert_eq!(needs.needs.len(), 2);
    }

    #[test]
    fn remote_url_classification_matrix() {
        let ssh = |h: &str| GitRemoteKind::Ssh { host: h.into() };
        let https = |h: &str| GitRemoteKind::Https { host: h.into() };
        for (input, kind, display) in [
            (
                "git@github.com:org/repo.git",
                ssh("github.com"),
                "github.com:org/repo.git",
            ),
            (
                "ssh://git@github.com:2222/o/r",
                ssh("github.com"),
                "ssh://github.com:2222/o/r",
            ),
            (
                "https://github.com/o/r",
                https("github.com"),
                "https://github.com/o/r",
            ),
            (
                "https://x-access-token:ghp_abc@github.com/o/r.git",
                https("github.com"),
                "https://github.com/o/r.git",
            ),
            (
                "https://ghe.corp.example/o/r.git",
                https("ghe.corp.example"),
                "https://ghe.corp.example/o/r.git",
            ),
            (
                "HTTPS://GitHub.COM/o/r",
                https("github.com"),
                "HTTPS://GitHub.COM/o/r",
            ),
            (
                "file:///srv/git/repo",
                GitRemoteKind::File,
                "file:///srv/git/repo",
            ),
            ("/srv/git/repo", GitRemoteKind::File, "/srv/git/repo"),
            (
                "ftp://example/o/r",
                GitRemoteKind::Other,
                "ftp://example/o/r",
            ),
        ] {
            let parsed = GitRemoteUrl::parse(input);
            assert_eq!(parsed.kind, kind, "{input}");
            assert_eq!(parsed.display(), display, "{input}");
            assert!(!parsed.display().contains("ghp_abc"));
        }
        let ipv6 = GitRemoteUrl::parse("ssh://git@[::1]:22/o/r");
        assert_eq!(ipv6.host(), Some("::1"));
        assert!(GitRemoteUrl::parse("git@github.com:o/r").is_github_com());
        assert!(!GitRemoteUrl::parse("https://ghe.example/o/r").is_github_com());
        assert_eq!(GitRemoteUrl::parse("/local").host(), None);
        assert_eq!(
            GitRemoteUrl::parse("https://github.com/org/repo.git").owner_repo,
            Some(("org".into(), "repo".into()))
        );
        assert_eq!(
            GitRemoteUrl::parse("https://github.com/a/b/c").owner_repo,
            None
        );
    }

    fn check(p: P, status: PrerequisiteStatus, needed_by: Vec<StepRef>) -> PrerequisiteCheck {
        PrerequisiteCheck {
            prerequisite: p,
            status,
            explanation: "found in GH_TOKEN".into(),
            needed_by,
            remediations: vec![Remediation::UseEnvToken {
                var: "GH_TOKEN".into(),
                remembered: false,
            }],
        }
    }

    fn report(checks: Vec<PrerequisiteCheck>) -> WorkflowPrerequisiteReport {
        WorkflowPrerequisiteReport {
            phase: PrerequisitePhase::Start,
            origin: PrerequisiteOrigin::Static,
            checks,
            advisories: vec![],
            notes: vec![],
        }
    }

    #[test]
    fn report_json_never_contains_a_token_value() {
        let fake = "ghp_FAKE_TOKEN_VALUE_123";
        let r = report(vec![check(
            P::GitHubToken {
                host: "github.com".into(),
            },
            PrerequisiteStatus::NeedsConsent,
            vec![],
        )]);
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains("GH_TOKEN"));
        assert!(!json.contains(fake));
        assert!(!r.render_lines().join("\n").contains(fake));
        let back: WorkflowPrerequisiteReport = serde_json::from_str(&json).unwrap();
        assert_eq!(back, r);
    }

    #[test]
    fn report_summary_line_counts_needs_attention() {
        let ok = check(P::GitIdentity, PrerequisiteStatus::Satisfied, vec![]);
        let consent = check(git(), PrerequisiteStatus::NeedsConsent, vec![]);
        let missing = check(P::PushedBranch, PrerequisiteStatus::Missing, vec![]);
        let unknown = check(P::NotDetachedHead, PrerequisiteStatus::Unknown, vec![]);
        let r = report(vec![ok.clone(), consent, missing, unknown]);
        assert_eq!(
            r.summary_line(),
            "2 of 4 workflow prerequisites need your attention"
        );
        assert!(!r.all_satisfied());
        assert!(r.any_missing());
        assert_eq!(r.needs_attention().len(), 2);
        let all_ok = report(vec![ok]);
        assert!(all_ok.all_satisfied());
        assert!(!all_ok.any_missing());
        assert_eq!(
            all_ok.summary_line(),
            "all 1 workflow prerequisites are satisfied"
        );
        // `unknown` alone never blocks --check-prerequisites.
        assert!(report(vec![check(
            P::PushedBranch,
            PrerequisiteStatus::Unknown,
            vec![]
        )])
        .all_satisfied());
    }

    #[test]
    fn leader_summary_marks_token_gap_and_ssh_push() {
        let step = |kind| StepRef {
            phase: PhaseKind::Teardown,
            kind,
            index: 0,
            description: kind.label().into(),
        };
        let r = report(vec![
            check(
                P::GitIdentity,
                PrerequisiteStatus::Satisfied,
                vec![step(PrerequisiteStepKind::CommitChanges)],
            ),
            check(
                P::PushAuthSsh {
                    remote: "origin".into(),
                    host: "github.com".into(),
                },
                PrerequisiteStatus::Satisfied,
                vec![step(PrerequisiteStepKind::PushBranch)],
            ),
            check(
                P::GitHubToken {
                    host: "github.com".into(),
                },
                PrerequisiteStatus::Missing,
                vec![step(PrerequisiteStepKind::CreatePullRequest)],
            ),
        ]);
        let summary = r.leader_summary();
        assert!(summary.contains("commit ✓"), "{summary}");
        assert!(summary.contains("push over SSH ✓"), "{summary}");
        assert!(
            summary.contains(
                "create_pull_request ✗ (no GitHub token: do not add a create_pull_request step"
            ),
            "{summary}"
        );
    }

    #[test]
    fn remediation_and_check_serialization_carries_no_secret_fields() {
        let r = Remediation::RunAcquireCommand {
            label: "configured".into(),
            argv: vec!["op".into(), "read".into()],
            resolved_path: Some("/usr/bin/op".into()),
            scope: AcquireScope::Repo,
            approved: false,
            sha256: "abc".into(),
        };
        let json = serde_json::to_value(&r).unwrap();
        assert_eq!(json["type"], "run_acquire_command");
        assert_eq!(json["scope"], "repo");
    }
}

#[cfg(test)]
mod layering_tests {
    use std::path::Path;

    fn rust_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                rust_files(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }

    #[test]
    fn no_layer_zero_type_holds_a_secret_string() {
        // Built from parts so this test does not match itself.
        let needle = format!("{}{}", "Secret", "String");
        let mut files = vec![];
        rust_files(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("src/data"),
            &mut files,
        );
        assert!(files.len() > 20);
        let offenders: Vec<_> = files
            .iter()
            .filter(|f| std::fs::read_to_string(f).unwrap().contains(&needle))
            .collect();
        assert!(
            offenders.is_empty(),
            "Layer 0 must not hold secrets: {offenders:?}"
        );
    }

    #[test]
    fn layer_zero_prerequisite_types_derive_serialize_but_hold_no_value_fields() {
        // The only free-text fields in a report are explanations/notes; the
        // serialized shape of every remediation names a choice, not a value.
        let remediation = serde_json::to_string(&super::Remediation::PasteToken).unwrap();
        assert_eq!(remediation, r#"{"type":"paste_token"}"#);
    }
}
