//! Workflow prerequisite derivation and report policy.

use std::collections::BTreeSet;

use crate::data::workflow_definition::{SetupStep, TeardownStep, Workflow};
use crate::data::workflow_prerequisites::*;
use crate::data::workflow_state::PhaseKind;

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

impl PrerequisiteCheck {
    /// The step kinds that need this check, each named once, in order
    /// ("any teardown step" when the workflow is not known yet).
    pub fn needed_by_label(&self) -> String {
        if self.needed_by.is_empty() {
            return "any teardown step".to_string();
        }
        let mut kinds: Vec<&str> = Vec::new();
        for step in &self.needed_by {
            if !kinds.contains(&step.kind.label()) {
                kinds.push(step.kind.label());
            }
        }
        kinds.join(", ")
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
    pub fn render_lines(&self) -> Vec<String> {
        let mut lines = vec![format!(
            "{:<32} {:<14} {}",
            "Workflow prerequisite", "Status", "Details"
        )];
        for check in &self.checks {
            lines.push(format!(
                "{:<32} {:<14} {}",
                check.prerequisite.label(),
                check.status.label(),
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

impl PrerequisiteStatus {
    /// The words every frontend shows for this status.
    pub fn label(self) -> &'static str {
        match self {
            Self::Satisfied => "satisfied",
            Self::NeedsConsent => "needs consent",
            Self::Missing => "missing",
            Self::Unknown => "unknown",
        }
    }
}
