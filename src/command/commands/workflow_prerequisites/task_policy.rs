//! Squad tasks store prerequisite **decisions** (WI 0149 §6), never values.
//!
//! `squad add` / `squad edit` run from an interactive client walk the same
//! questions as `exec workflow --dynamic` (assume every teardown kind), then
//! turn the answers into the `PrerequisitePolicy` kept on the `Task`. The
//! daemon resolves the credentials again at every run.

use std::path::Path;

use crate::command::dispatch::catalogue::FrontendKind;
use crate::command::dispatch::Engines;
use crate::command::error::CommandError;
use crate::data::config::flags::PrerequisiteFlags;
use crate::data::config::workflows::{
    CredentialUse, FixedIdentity, PolicySource, PrerequisitePolicy,
};
use crate::data::message::{MessageLevel, UserMessage};
use crate::data::session::{Session, SessionOpenOptions};
use crate::data::workflow_prerequisites::{
    AcquireScope, PrerequisiteOrigin, WorkflowPrerequisites,
};
use crate::engine::workflow::prerequisite_grants::{SshGrant, TokenGrantSource};

use super::flow::{PrerequisiteOutcome, WorkflowPrerequisiteFlow, WorkflowPrerequisiteFlowOptions};
use super::frontend::WorkflowPrerequisiteFrontend;

/// Ask the prerequisite questions for a squad task bound to `workspace` and
/// return the decisions as a task policy. `None` when nobody can be asked
/// (the task keeps "not set", which the daemon resolves to deny).
pub fn collect_task_prerequisites(
    engines: &Engines,
    frontend_kind: FrontendKind,
    workspace: &Path,
    frontend: &mut dyn WorkflowPrerequisiteFrontend,
) -> Result<Option<PrerequisitePolicy>, CommandError> {
    if !frontend.can_ask_prerequisites() {
        return Ok(None);
    }
    let git_root = engines
        .git_engine
        .resolve_root(workspace)
        .unwrap_or_else(|_| workspace.to_path_buf());
    let session = Session::open_at_git_root(
        workspace.to_path_buf(),
        git_root,
        SessionOpenOptions::default(),
    )
    .map_err(CommandError::from)?;
    let options = WorkflowPrerequisiteFlowOptions::for_session(
        &session,
        frontend_kind,
        &PrerequisiteFlags::default(),
        None,
        PrerequisiteOrigin::AssumedAll,
        None,
    );
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
