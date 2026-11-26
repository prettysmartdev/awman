//! Squad tasks store prerequisite **decisions** (WI 0149 §6), never values.
//!
//! `squad add` / `squad edit` run from an interactive client walk the same
//! questions as `exec workflow --dynamic` (assume all workflow requirements), then
//! turn the answers into the `PrerequisitePolicy` kept on the `Task`. The
//! daemon resolves the credentials again at every run.
//!
//! Which requirements apply is the task's captured scope, carried by
//! [`TaskPrerequisitePlan`] (WI 0088): a repository-backed task keeps every
//! assumed check; a directory task is offered only the optional Git identity
//! and its leader summary, run preflight and generated workflow omit the
//! Git/PR-only checks.

use std::path::{Path, PathBuf};

use crate::command::dispatch::catalogue::FrontendKind;
use crate::command::dispatch::Engines;
use crate::command::error::CommandError;
use crate::data::config::env::EnvSnapshot;
use crate::data::config::flags::PrerequisiteFlags;
use crate::data::config::workflows::{
    CredentialUse, FixedIdentity, PolicySource, PrerequisitePolicy,
};
use crate::data::fs::task_store::{MountScope, Task};
use crate::data::message::{MessageLevel, UserMessage};
use crate::data::session::{Session, SessionOpenOptions};
use crate::data::workflow_prerequisites::{
    AcquireScope, PrerequisiteOrigin, WorkflowPrerequisites,
};
use crate::engine::workflow::prerequisite_grants::{SshGrant, TokenGrantSource};

use super::flow::{PrerequisiteOutcome, WorkflowPrerequisiteFlow, WorkflowPrerequisiteFlowOptions};
use super::frontend::WorkflowPrerequisiteFrontend;
use super::scope::{DirectoryIdentity, PrerequisiteScope};

/// The captured scope a squad task's prerequisite decisions are made for
/// (WI 0088): the task's effective root and the [`MountScope`] recorded for it
/// at creation. Built from a stored [`Task`] (edit, leader summary, run) or
/// from the very pair the gateway is about to persist (add) — never by
/// re-discovering a repository around the root or from the caller's current
/// directory.
///
/// Every squad prerequisite checkpoint projects through this one plan: the
/// add/edit interview ([`interview_scope`](Self::interview_scope)), and the
/// leader-prompt summary, run preflight and generated-workflow narrowing
/// ([`run_scope`](Self::run_scope)).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskPrerequisitePlan {
    root: PathBuf,
    mount_scope: MountScope,
}

impl TaskPrerequisitePlan {
    /// The plan of a stored task, from its captured scope.
    pub fn for_task(task: &Task) -> Self {
        Self::captured(task.repo_scope.clone(), task.mount_scope)
    }

    /// The plan for a root and the scope captured (or about to be captured)
    /// for it.
    pub fn captured(root: PathBuf, mount_scope: MountScope) -> Self {
        Self { root, mount_scope }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn mount_scope(&self) -> MountScope {
        self.mount_scope
    }

    /// Whether the task is a plain-directory task (no repository applies).
    pub fn is_directory(&self) -> bool {
        !self.mount_scope.is_git_repo()
    }

    /// The applicability the add/edit interview asks under: a directory task
    /// is offered only the optional Git identity.
    pub fn interview_scope(&self) -> PrerequisiteScope {
        if self.is_directory() {
            PrerequisiteScope::Directory {
                identity: DirectoryIdentity::Offer,
            }
        } else {
            PrerequisiteScope::Repository
        }
    }

    /// The applicability every unattended checkpoint uses: the leader-prompt
    /// summary, the run preflight and the generated workflow. A directory
    /// task's identity applies only when an approved policy allows one.
    pub fn run_scope(&self) -> PrerequisiteScope {
        if self.is_directory() {
            PrerequisiteScope::Directory {
                identity: DirectoryIdentity::WhenAllowed,
            }
        } else {
            PrerequisiteScope::Repository
        }
    }

    /// The prerequisite summary a squad leader is given for this task
    /// (`commit ✓, push_branch ✗, …`): an unattended, ask-nothing preflight of
    /// every assumed requirement that applies under [`run_scope`](Self::run_scope),
    /// against the task's stored `policy`. Empty when nothing applies — a
    /// directory task with no identity chosen.
    pub fn leader_summary(
        &self,
        engines: &Engines,
        session: &Session,
        policy: Option<&PrerequisitePolicy>,
    ) -> String {
        let mut options = WorkflowPrerequisiteFlowOptions::for_session(
            session,
            FrontendKind::SQUAD_LOCAL,
            &PrerequisiteFlags::default(),
            policy,
            PrerequisiteOrigin::AssumedAll,
            None,
        );
        options.scope = self.run_scope();
        WorkflowPrerequisiteFlow::new(engines, session, options)
            .preflight(&WorkflowPrerequisites::assume_all(), false)
            .leader_summary()
    }

    /// Resolve a captured repository without authorizing a newly discovered
    /// enclosing checkout. Legacy Cwd records without a separate repository
    /// authority fail closed when their captured directory is not the root.
    pub fn repository_root(
        &self,
        git: &crate::engine::git::GitEngine,
    ) -> Result<PathBuf, CommandError> {
        let git_root = git.resolve_root(&self.root).map_err(|error| {
            CommandError::Other(format!(
                "squad task repository {} cannot be resolved: {error}",
                self.root.display()
            ))
        })?;
        // Discovery may succeed in an enclosing checkout after this
        // repository is removed. That checkout was never authorized.
        let captured = std::fs::canonicalize(&self.root)
            .map_err(|error| crate::data::error::DataError::io(&self.root, error))?;
        let discovered = std::fs::canonicalize(&git_root)
            .map_err(|error| crate::data::error::DataError::io(&git_root, error))?;
        if discovered != captured {
            return Err(CommandError::Other(format!(
                "squad task repository boundary changed at {}; recreate the task with the intended workspace",
                self.root.display()
            )));
        }
        Ok(git_root)
    }

    /// A session rooted where the task's runs are rooted. A directory task is
    /// opened at its own root (which need not exist yet: the default
    /// workspace is created with the task). A repository-backed task resolves
    /// its repository and fails explicitly when it is gone.
    pub fn open_session(
        &self,
        engines: &Engines,
        env: Option<EnvSnapshot>,
    ) -> Result<Session, CommandError> {
        let opts = SessionOpenOptions {
            env,
            ..Default::default()
        };
        let session = match self.mount_scope {
            MountScope::Directory => Session::open_at_directory_root(self.root.clone(), opts),
            MountScope::GitRoot | MountScope::Cwd => {
                let git_root = self.repository_root(engines.git_engine.as_ref())?;
                let working_dir = match self.mount_scope {
                    MountScope::Cwd => self.root.clone(),
                    _ => git_root.clone(),
                };
                Session::open_at_git_root(working_dir, git_root, opts)
            }
        };
        session.map_err(CommandError::from)
    }
}

/// Ask the prerequisite questions for a squad task under `plan` and return
/// the decisions as a task policy. `None` when nobody can be asked (the task
/// keeps "not set", which the daemon resolves to deny).
///
/// Nothing is persisted here: the policy travels back to the caller, which
/// stores it with the rest of the task, so an abandoned interview leaves
/// nothing behind.
pub fn collect_task_prerequisites(
    engines: &Engines,
    frontend_kind: FrontendKind,
    plan: &TaskPrerequisitePlan,
    env: Option<EnvSnapshot>,
    frontend: &mut dyn WorkflowPrerequisiteFrontend,
) -> Result<Option<PrerequisitePolicy>, CommandError> {
    if !frontend.can_ask_prerequisites() {
        return Ok(None);
    }
    let session = plan.open_session(engines, env)?;
    let mut options = WorkflowPrerequisiteFlowOptions::for_session(
        &session,
        frontend_kind,
        &PrerequisiteFlags::default(),
        None,
        PrerequisiteOrigin::AssumedAll,
        None,
    );
    options.scope = plan.interview_scope();
    let mut flow = WorkflowPrerequisiteFlow::new(engines, &session, options);
    let needs = WorkflowPrerequisites::assume_all();
    let report = flow.preflight(&needs, true);
    let redaction = crate::engine::auth::redaction::RedactionSet::new();
    let outcome = flow.resolve(&needs, report, &redaction, frontend)?;
    Ok(Some(task_policy_from_outcome(&outcome, frontend)))
}

/// The decisions in `outcome`, as a task policy. A granted secret value is
/// dropped here; only where it came from (an env var name, the global token
/// command) is kept. A pasted or `gh auth token` token cannot be stored, so
/// that choice is reported and left as "ask".
pub fn task_policy_from_outcome(
    outcome: &PrerequisiteOutcome,
    sink: &mut dyn WorkflowPrerequisiteFrontend,
) -> PrerequisitePolicy {
    let mut policy = PrerequisitePolicy::default();
    policy.git_identity.source = PolicySource::Task;
    policy.ssh.source = PolicySource::Task;
    policy.github_token.source = PolicySource::Task;
    if let Some(identity) = &outcome.grants.identity {
        policy.git_identity.use_ = CredentialUse::Allow;
        policy.git_identity.scope = crate::data::config::workflows::IdentityGrantScope::CommitSteps;
        policy.git_identity.fixed = Some(FixedIdentity {
            name: identity.name.clone(),
            email: identity.email.clone(),
        });
    }
    match &outcome.grants.ssh {
        Some(SshGrant::Key { key_path, .. }) => {
            policy.ssh.use_ = CredentialUse::Allow;
            policy.ssh.key = Some(key_path.clone());
        }
        Some(SshGrant::WholeDir { .. }) => {
            policy.ssh.use_ = CredentialUse::Allow;
            policy.ssh.whole_dir = true;
        }
        None => {}
    }
    if let Some(token) = &outcome.grants.token {
        match &token.source {
            TokenGrantSource::Env(var) => {
                policy.github_token.use_ = CredentialUse::Allow;
                policy.github_token.env_vars = vec![var.clone()];
            }
            TokenGrantSource::HostCommand(_)
                if token
                    .acquire
                    .as_ref()
                    .is_some_and(|a| a.scope == AcquireScope::Global) =>
            {
                policy.github_token.use_ = CredentialUse::Allow;
                policy.github_token.env_vars.clear();
                policy.github_token.acquire = token.acquire.clone();
            }
            _ => sink.write_message(UserMessage {
                level: MessageLevel::Warning,
                text: "a pasted, gh or repository-command token cannot be stored on a squad task; \
                       push it to the daemon (`awman squad env --push` with GH_TOKEN set) or \
                       configure workflows.prerequisites.githubToken.acquire globally"
                    .into(),
            }),
        }
    }
    policy
}

/// `squad edit --prerequisite-*`: `edit` is the policy the flags describe
/// (`PrerequisitePolicy::from_flags`). Only the credentials a flag named
/// (source `Flag`) replace the task's stored decision; the rest are kept.
pub fn edit_task_policy(
    current: &PrerequisitePolicy,
    edit: &PrerequisitePolicy,
) -> PrerequisitePolicy {
    current.edited_task(edit)
}
