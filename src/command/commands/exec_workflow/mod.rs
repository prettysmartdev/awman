//! `ExecWorkflowCommand` — run a workflow file.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde::Serialize;

use crate::command::commands::launch_policy::LaunchPolicy;
use crate::command::commands::mount_scope::MountScope;
use crate::command::commands::worktree_lifecycle::{WorktreeLifecycle, WorktreeLifecycleFrontend};
use crate::command::commands::Command;
use crate::command::commands::{parse_overlay_list, TypedOverlay};
use crate::command::dispatch::{BuildContext, Engines};
use crate::command::error::CommandError;
use crate::data::message::{MessageLevel, UserMessage, UserMessageSink};
use crate::data::session::Session;
use crate::data::workflow_definition::{Workflow, WorkflowStep};
use crate::data::workflow_prompt_template::{substitute_prompt, WorkItemContext};
use crate::data::workflow_state::PhaseKind;
use crate::engine::agent::AgentRunOptions;
use crate::engine::agent_runtime::frontend::AgentFrontend;
use crate::engine::auth::keychain::refreshable_spec_for;
use crate::engine::container::options::{AutoMode, PlanMode, YoloMode};
use crate::engine::credential_refresh::RefreshOutcome;
use crate::engine::error::EngineError;
use crate::engine::workflow::actions::{
    AvailableActions, CountdownKind, NextAction, WorkflowOutcome,
};
use crate::engine::workflow::factory::{AgentExecutionFactory, WorkflowRuntimeContext};
use crate::engine::workflow::frontend::WorkflowFrontend;
use crate::engine::workflow::{EngineRequest, WorkflowEngine};

use super::dynamic_repair::{RepairDecision, WorkflowRepairLoop};

#[derive(Debug, Clone)]
pub struct ExecWorkflowCommandFlags {
    /// The positional workflow path. `None` is only valid with `--dynamic`,
    /// where the leader agent generates the workflow file. Non-dynamic
    /// invocations with `None` produce the existing missing-required-argument
    /// error.
    pub workflow: Option<PathBuf>,
    pub work_item: Option<String>,
    pub non_interactive: bool,
    pub plan: bool,
    pub allow_docker: bool,
    pub worktree: bool,
    pub yolo: bool,
    pub auto: bool,
    pub agent: Option<String>,
    pub model: Option<String>,
    pub launch_mode: Option<crate::data::config::repo::LaunchMode>,
    pub overlay: Vec<String>,
    pub max_concurrent: Option<usize>,
    pub issue_source: crate::engine::issue::IssueSourceFlags,
    /// When true, a leader agent designs and runs a workflow for the work item.
    /// Implies `--yolo`, `--worktree`, and `context(workflow)`.
    pub dynamic: bool,
    /// Raw `agent::model` string for the dynamic leader agent. Only valid with
    /// `--dynamic`.
    pub leader: Option<String>,
}

pub use crate::data::config::leader_spec::LeaderSpec;

/// Parse a `--leader` value of the form `agent::model`, with the flag's own
/// wording for a malformed one.
///
/// The shape rule is [`LeaderSpec::parse`]'s, in Layer 0 (WI 0114 F-51); this
/// is the flag's error message and nothing else.
pub(crate) fn parse_leader_flag(raw: &str) -> Result<LeaderSpec, CommandError> {
    LeaderSpec::parse(raw).map_err(|_| {
        CommandError::Other(format!(
            "invalid --leader value {raw:?}; expected agent::model \
             (e.g. claude::claude-opus-4-8)"
        ))
    })
}

#[derive(Debug, Clone, Serialize)]
pub struct ExecWorkflowOutcome {
    pub workflow: String,
    pub exit_code: Option<i32>,
    pub worktree_used: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkflowSummary {
    pub steps_completed: usize,
    pub steps_failed: usize,
}

/// Per-command frontend trait: supertrait composition of every Layer 1 and
/// Layer 2 trait that `ExecWorkflowCommand` calls during its lifecycle.
#[async_trait]
pub trait ExecWorkflowCommandFrontend:
    UserMessageSink
    + AgentFrontend
    + crate::command::commands::agent_setup::AgentLaunchFrontend
    + WorkflowFrontend
    + WorktreeLifecycleFrontend
    + Send
    + Sync
{
    fn report_workflow_summary(&mut self, summary: &WorkflowSummary);

    /// A previous run of this workflow left resumable state on disk. Offer to
    /// pick it back up from one of the named steps, to discard that state and
    /// start over, or to cancel the command (WI-0115 §2).
    ///
    /// Both `exec workflow` and `exec workflow --dynamic` ask this same
    /// question, with the same offered start points, so the two modes behave
    /// identically on a resume. Both ask it before anything is created on
    /// disk, so [`WorkflowResumeDecision::Cancel`] can back out of the whole
    /// command leaving the previous run exactly as it was.
    fn ask_workflow_resume(
        &mut self,
        prompt: &WorkflowResumePrompt,
    ) -> Result<WorkflowResumeDecision, CommandError>;

    /// The previous run's worktree is on disk but the run cannot be resumed —
    /// `reason` says what is missing or why. Frontends that can block should
    /// state it and wait for the user to acknowledge before a fresh dynamic
    /// workflow starts; the rest return immediately.
    fn notify_dynamic_workflow_resume_unavailable(
        &mut self,
        work_item: u32,
        reason: &str,
    ) -> Result<(), CommandError>;
}

/// One start point offered by the workflow resume prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowResumeStep {
    /// The step's name, verbatim from the saved workflow.
    pub name: String,
    /// Why it is on offer — "the step that failed", "the step before it",
    /// "the step after it".
    pub role: String,
}

/// Everything a frontend needs to render the workflow resume prompt.
///
/// All copy lives here — frontends render these strings rather than composing
/// their own, so the prompt reads identically in the TUI, the CLI, and the API,
/// and in dynamic and non-dynamic mode alike. (Same contract as
/// [`PostWorkflowWorktreePrompt`](super::worktree_lifecycle::PostWorkflowWorktreePrompt).)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowResumePrompt {
    /// Title of the saved workflow.
    pub workflow_name: String,
    /// The work item this run is bound to, when there is one.
    pub work_item: Option<u32>,
    /// The worktree the previous run left behind, when the run used one.
    pub worktree_path: Option<PathBuf>,
    /// True when this is a `--dynamic` resume, meaning accepting it also skips
    /// a leader-design pass.
    pub dynamic: bool,
    pub completed_steps: usize,
    pub total_steps: usize,
    /// Offered start points, in order: the step the run stopped on, the step
    /// before it, the step after it. Never empty, and never longer than three.
    pub start_points: Vec<WorkflowResumeStep>,
    /// Title shown at the top of the dialog.
    pub title: String,
    /// Body text rendered above the choices.
    pub body: String,
    /// Label for the "do not resume" choice.
    pub fresh_label: String,
}

impl WorkflowResumePrompt {
    /// Build the prompt, composing its user-facing copy.
    pub fn new(
        workflow_name: String,
        work_item: Option<u32>,
        worktree_path: Option<PathBuf>,
        dynamic: bool,
        completed_steps: usize,
        total_steps: usize,
        start_points: Vec<WorkflowResumeStep>,
    ) -> Self {
        let what = if dynamic {
            "dynamic run".to_string()
        } else {
            format!("run of '{workflow_name}'")
        };
        let mut body = format!("A previous {what} left resumable state on disk.\n\n");
        if !dynamic {
            body.push_str(&format!("Workflow: {workflow_name}\n"));
        }
        if let Some(wi) = work_item {
            body.push_str(&format!("Work item: {wi:04}\n"));
        }
        if let Some(path) = &worktree_path {
            body.push_str(&format!("Worktree: {}\n", path.display()));
        }
        body.push_str(&format!(
            "Progress: {completed_steps}/{total_steps} step(s) completed.\n\n\
             Resume it from one of these steps, or start over?"
        ));
        let fresh_label = if dynamic {
            "Start a fresh dynamic workflow".to_string()
        } else {
            "Discard the saved state and start over".to_string()
        };
        Self {
            title: if dynamic {
                "Resume previous dynamic workflow?".to_string()
            } else {
                "Resume previous workflow?".to_string()
            },
            body,
            fresh_label,
            workflow_name,
            work_item,
            worktree_path,
            dynamic,
            completed_steps,
            total_steps,
            start_points,
        }
    }

    /// The unattended answer: pick up at the step the previous run stopped on,
    /// which is always the first offered start point. Frontends with nobody to
    /// ask use this rather than discarding the saved work. Falls back to
    /// starting over only if nothing was offered — which the command layer
    /// never does, since an empty list is retired before the prompt is raised.
    pub fn resume_from_stop_point(&self) -> WorkflowResumeDecision {
        self.start_points
            .first()
            .map(|p| WorkflowResumeDecision::ResumeFrom(p.name.clone()))
            .unwrap_or(WorkflowResumeDecision::Fresh)
    }

    /// The choice labels, in the order a frontend should number them: one per
    /// start point, then the "start over" option last.
    pub fn choice_labels(&self) -> Vec<String> {
        self.start_points
            .iter()
            .map(|p| format!("Resume from '{}' ({})", p.name, p.role))
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkflowResumeDecision {
    /// Resume the saved run, starting from this step.
    ResumeFrom(String),
    /// Discard the saved state and start over. In dynamic mode that also means
    /// a leader designs a new workflow.
    Fresh,
    /// Cancel the command outright: run nothing and change nothing. The saved
    /// state and (in dynamic mode) the saved `workflow.toml` are left exactly
    /// as they were, so the same choice is on offer next time.
    ///
    /// This is what Esc means on the prompt. Discarding a previous run's
    /// progress — and, in dynamic mode, a paid-for leader design — is not
    /// something to do by dismissing a dialog, so the destructive answer has
    /// to be chosen deliberately.
    Cancel,
}

/// Offered resume points, named: the step the previous run stopped on, the step
/// before it, and the step after it. Empty when that run has nothing left to do
/// — every step succeeded or was skipped.
pub struct ExecWorkflowCommand {
    flags: ExecWorkflowCommandFlags,
    engines: Engines,
    session: Session,
    /// When set (only for squad-generated workflows), every container this
    /// command launches is stamped with the task's squad name + labels so
    /// prefix discovery finds the workflow's step containers, not just the
    /// evaluation leader. `None` for an ordinary `awman exec workflow`.
    squad_identity: Option<crate::engine::squad::launcher::SquadContainerIdentity>,
    /// When set (only for squad-generated workflows), the task's durable
    /// workspace directory, mounted into every step container at the
    /// `context(workflow)` path so a task's persistent data is reachable from
    /// its workflow as well as from its evaluation leader. `None` for an
    /// ordinary `awman exec workflow`, which keeps its own per-invocation
    /// workflow context directory.
    task_workspace: Option<PathBuf>,
    /// When set (only for squad-generated workflows whose session root must
    /// stay untouched between runs), the directory the engine's workflow-state
    /// file lives under, instead of the session's git root. `None` for an
    /// ordinary `awman exec workflow`.
    workflow_state_root: Option<PathBuf>,
    /// The shared session to mirror the run's summary into, so a frontend
    /// viewing this session sees the workflow's progress (decision Q3,
    /// WI 0114 F-22). `None` for a squad-generated run, whose session is the
    /// daemon's and has no viewer, and for tests built with
    /// [`ExecWorkflowCommand::new`].
    managed_session: Option<Arc<tokio::sync::RwLock<Session>>>,
}

impl ExecWorkflowCommand {
    pub fn new(flags: ExecWorkflowCommandFlags, engines: Engines, session: Session) -> Self {
        Self {
            flags,
            engines,
            session,
            squad_identity: None,
            task_workspace: None,
            workflow_state_root: None,
            managed_session: None,
        }
    }

    /// Mirror this run's summary into the shared session it belongs to.
    pub fn mirroring_into(mut self, session: Arc<tokio::sync::RwLock<Session>>) -> Self {
        self.managed_session = Some(session);
        self
    }

    /// Construct from the catalogue-resolved input (WI 0113 F-10).
    ///
    /// `--yolo` and `--auto` declare `implies: ["worktree"]`, so `worktree`
    /// arrives already set and no implication is re-derived here. The two
    /// checks that remain are genuine command-layer policy: the WI-0092
    /// dynamic/leader relationships, and the positional path that only a
    /// non-dynamic run requires (the catalogue marks it optional so
    /// `--dynamic` may omit it).
    pub fn from_input(ctx: &BuildContext) -> Result<Self, CommandError> {
        let flags = ExecWorkflowCommandFlags {
            workflow: ctx.args.get("workflow").map(PathBuf::from),
            work_item: ctx.flags.string("work-item"),
            non_interactive: ctx.flags.bool("non-interactive"),
            plan: ctx.flags.bool("plan"),
            allow_docker: ctx.flags.bool("allow-docker"),
            worktree: ctx.flags.bool("worktree"),
            yolo: ctx.flags.bool("yolo"),
            auto: ctx.flags.bool("auto"),
            agent: ctx.flags.string("agent"),
            model: ctx.flags.string("model"),
            launch_mode: crate::command::dispatch::parse_launch_mode(
                ctx.flags.string("launch-mode"),
                &ctx.path(),
            )?,
            overlay: ctx.flags.strs("overlay").to_vec(),
            max_concurrent: ctx.flags.usize("max-concurrent"),
            issue_source: crate::engine::issue::IssueSourceFlags {
                issue: ctx.flags.string("issue"),
            },
            dynamic: ctx.flags.bool("dynamic"),
            leader: ctx.flags.string("leader"),
        };
        validate_dynamic_flags(&flags)?;
        if !flags.dynamic && flags.workflow.is_none() {
            return Err(CommandError::missing_required_argument(
                &ctx.path(),
                "workflow",
            ));
        }
        Ok(Self::new(flags, ctx.engines.clone(), ctx.session.clone())
            .mirroring_into(Arc::clone(&ctx.managed_session)))
    }

    /// Carry a squad container identity so every generated-workflow step
    /// container is stamped exactly as the evaluation leader is. A non-squad
    /// `exec workflow` never calls this and is unaffected.
    pub fn with_squad_identity(
        mut self,
        identity: crate::engine::squad::launcher::SquadContainerIdentity,
    ) -> Self {
        self.squad_identity = Some(identity);
        self
    }

    /// Override the `context(workflow)` host directory for every step with the
    /// squad task's durable workspace. This is a structural, always-on mount
    /// for a squad run — not a user-specified overlay — so the same stable
    /// container path serves the leader and every step.
    pub fn with_task_workspace(mut self, workspace: PathBuf) -> Self {
        self.task_workspace = Some(workspace);
        self
    }

    /// Root the engine's workflow-state file outside the session's working
    /// tree.
    ///
    /// A squad task bound to a plain directory (its durable workspace, or a
    /// custom folder that is not a repository) has no worktree to absorb
    /// awman's own bookkeeping, and that directory must survive every run
    /// untouched (WI 0106 §6a). Pointing the state file at the run-scoped
    /// `runs/<run-id>/` directory keeps the create/rewrite/delete cycle out of
    /// it entirely. A non-squad `exec workflow` never calls this.
    pub fn with_workflow_state_root(mut self, root: PathBuf) -> Self {
        self.workflow_state_root = Some(root);
        self
    }

    pub fn flags(&self) -> &ExecWorkflowCommandFlags {
        &self.flags
    }
}

// ─── Module layout (WI 0114 F-51) ────────────────────────────────────────────
//
// This file keeps the types and the command's constructors; the rest of a
// 7,000-line file lives in five child modules and a test module. They are
// private: `ExecWorkflowCommand` and every public type are declared here, so
// the module's external surface is unchanged.
mod dynamic;
mod execute;
mod factory;
mod issue;
mod prepare;

#[cfg(test)]
mod tests;

// The child modules' items are `pub(super)`; re-exporting them here is what
// lets each one say `use super::*;` and see the others, exactly as they all
// saw each other when this was one file.
pub(super) use factory::*;
pub(super) use issue::*;
pub(super) use prepare::*;

// Q13: the preflight helpers live in `commands::workflow_preflight` so the
// squad evaluator can reach them without importing from this command. They are
// re-exported here because this module's own code uses them throughout.
pub(super) use crate::command::commands::workflow_preflight::*;
