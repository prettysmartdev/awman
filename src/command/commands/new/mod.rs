//! `NewCommand` — `new spec`, `new workflow`, `new skill`, `new agent`.

use async_trait::async_trait;
use serde::Serialize;

use crate::command::commands::launch_policy::LaunchPolicy;
use crate::command::commands::prompt_templates::{
    render_skill_interview_prompt, render_workflow_interview_prompt,
};
use crate::command::commands::skill_library::{
    pull_all_libraries, pull_library, resolve_pull_target, PullOutcome,
};
use crate::command::commands::Command;
use crate::command::dispatch::{BuildContext, Engines};
use crate::command::error::CommandError;
use crate::data::fs::{SkillDirs, WorkflowDirs, SKILL_INTERVIEW_CONTAINER_DIR};
use crate::data::message::{MessageLevel, UserMessage, UserMessageSink};
use crate::data::session::Session;
use crate::engine::agent::AgentRunOptions;
use crate::engine::container::options::ContainerOption;

#[derive(Debug, Clone)]
pub struct NewSpecFlags {
    pub interview: bool,
    pub non_interactive: bool,
    /// Raw `--work-item` reference that seeds the new spec (WI 0147).
    pub work_item: Option<String>,
}

#[derive(Debug, Clone)]
pub struct NewWorkflowFlags {
    pub interview: bool,
    pub non_interactive: bool,
    pub global: bool,
    pub format: String,
}

#[derive(Debug, Clone)]
pub struct NewSkillFlags {
    pub interview: bool,
    pub non_interactive: bool,
    pub global: bool,
    pub pull: Option<String>,
    pub pull_all: bool,
    pub subdir: Option<String>,
}

#[derive(Debug, Clone)]
pub struct NewAgentFlags {
    /// The positional name; becomes the file stem.
    pub name: String,
    pub global: bool,
    /// `--from <example>`; `None` writes the commented skeleton.
    pub from: Option<String>,
    pub force: bool,
}

#[derive(Debug, Clone)]
pub enum NewSubcommand {
    Spec(NewSpecFlags),
    Workflow(NewWorkflowFlags),
    Skill(NewSkillFlags),
    Agent(NewAgentFlags),
}

#[derive(Debug, Clone, Serialize)]
pub struct NewSpecOutcome {
    pub interview: bool,
    pub path: Option<String>,
    /// The created remote work item (WI 0147); `None` for local files.
    #[serde(default)]
    pub url: Option<String>,
    /// The work item number: the local file number, or the provider's.
    #[serde(default)]
    pub number: Option<u32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct NewWorkflowOutcome {
    pub interview: bool,
    pub global: bool,
    pub format: String,
    pub path: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct PullLibraryOutcome {
    pub slug: String,
    pub dir: String,
    pub updated: bool,
    pub skills_found: Vec<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct NewSkillOutcome {
    pub interview: bool,
    pub global: bool,
    pub path: Option<String>,
    /// `true` when this run was a `--pull`/`--pull-all` library operation
    /// rather than skill creation. `libraries` alone cannot carry that
    /// distinction: `--pull-all` with nothing pulled yet also yields an empty
    /// list, and rendering it as a created skill would be wrong.
    pub pull: bool,
    pub libraries: Vec<PullLibraryOutcome>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct NewAgentOutcome {
    pub name: String,
    /// The definition's TOML path.
    pub path: String,
    pub scope: crate::data::fs::agent_dirs::DefinitionScope,
    /// `true` when the commented skeleton (not an example) was written.
    pub skeleton: bool,
    /// The TOML, plus `maki.Dockerfile` for `--from maki`.
    pub files_written: Vec<String>,
    /// Whether the written content was recorded as approved (repo scope
    /// always; global scope only with `[ping]`; never the unfilled skeleton).
    pub approved: bool,
    /// The review summary's host-command block, when the content has `[ping]`.
    pub host_command_block: Option<String>,
    /// `awman ready --agent <name>`.
    pub next_step: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", content = "payload")]
pub enum NewOutcome {
    Spec(NewSpecOutcome),
    Workflow(NewWorkflowOutcome),
    Skill(NewSkillOutcome),
    Agent(NewAgentOutcome),
}

/// A single step collected from the user during `new workflow`.
#[derive(Debug, Clone)]
pub struct WorkflowStepInput {
    pub name: String,
    pub agent: Option<String>,
    pub model: Option<String>,
    pub prompt: String,
}

/// `NewCommandFrontend` extends `SpecsCommandFrontend` so the `Spec`
/// subcommand can drive the same Q&A (kind / title / summary).
pub trait NewCommandFrontend:
    UserMessageSink + crate::command::commands::specs::SpecsCommandFrontend + Send + Sync
{
    /// Prompt for a workflow name. CLI implementations gate on stdin TTY.
    fn ask_workflow_name(&mut self) -> Result<String, CommandError> {
        Ok("workflow".to_string())
    }
    /// Prompt for a human-readable workflow title.
    fn ask_workflow_title(&mut self) -> Result<String, CommandError> {
        Ok(String::new())
    }
    /// Prompt for a one-line summary for the new workflow (used in interview mode).
    fn ask_workflow_summary(&mut self) -> Result<String, CommandError> {
        Ok(String::new())
    }
    /// Prompt for a step name.
    fn ask_workflow_step_name(&mut self) -> Result<String, CommandError> {
        Ok(String::new())
    }
    /// Prompt for the optional agent override for the current step.
    fn ask_workflow_step_agent(&mut self) -> Result<Option<String>, CommandError> {
        Ok(None)
    }
    /// Prompt for the optional model override for the current step.
    fn ask_workflow_step_model(&mut self) -> Result<Option<String>, CommandError> {
        Ok(None)
    }
    /// Prompt for the step prompt text.
    fn ask_workflow_step_prompt(&mut self) -> Result<String, CommandError> {
        Ok(String::new())
    }
    /// Ask whether to add another workflow step.
    fn ask_add_another_step(&mut self) -> Result<bool, CommandError> {
        Ok(false)
    }
    /// Prompt for a skill name.
    fn ask_skill_name(&mut self) -> Result<String, CommandError> {
        Ok("skill".to_string())
    }
    /// Prompt for a one-line summary for the new skill (used in interview mode).
    fn ask_skill_summary(&mut self) -> Result<String, CommandError> {
        Ok(String::new())
    }
    /// Prompt for the body content of the new skill.
    fn ask_skill_body(&mut self) -> Result<String, CommandError> {
        Ok(String::new())
    }
    /// `new agent` finished, for frontends that do not render outcomes
    /// themselves (the TUI). The CLI renders from the outcome on stdout.
    fn show_new_agent(&mut self, _outcome: &NewAgentOutcome) {}
}

// ─── Serde structs for workflow serialization ────────────────────────────────

#[derive(Debug, Serialize)]
struct WorkflowFileToml<'a> {
    title: &'a str,
    #[serde(rename = "step", skip_serializing_if = "Vec::is_empty")]
    steps_toml: Vec<WorkflowStepSerde<'a>>,
}

#[derive(Debug, Serialize)]
struct WorkflowFileYaml<'a> {
    title: &'a str,
    steps: Vec<WorkflowStepSerde<'a>>,
}

#[derive(Debug, Serialize)]
struct WorkflowStepSerde<'a> {
    name: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    agent: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<&'a str>,
    prompt: &'a str,
}

fn serialize_steps(steps: &[WorkflowStepInput]) -> Vec<WorkflowStepSerde<'_>> {
    steps
        .iter()
        .map(|s| WorkflowStepSerde {
            name: &s.name,
            agent: s.agent.as_deref(),
            model: s.model.as_deref(),
            prompt: &s.prompt,
        })
        .collect()
}

fn serialize_workflow_toml(title: &str, steps: &[WorkflowStepInput]) -> String {
    let file = WorkflowFileToml {
        title,
        steps_toml: serialize_steps(steps),
    };
    toml::to_string_pretty(&file).unwrap_or_else(|_| format!("title = \"{title}\"\n"))
}

fn serialize_workflow_yaml(title: &str, steps: &[WorkflowStepInput]) -> String {
    let file = WorkflowFileYaml {
        title,
        steps: serialize_steps(steps),
    };
    serde_yaml::to_string(&file).unwrap_or_else(|_| format!("title: \"{title}\"\nsteps: []\n"))
}

/// Subdirectory under the git root for user-authored workflow definitions.
const REPO_WORKFLOW_DEFINITIONS_DIR: &str = "aspec/workflows";

pub struct NewCommand {
    sub: NewSubcommand,
    engines: Engines,
    session: Session,
    cancel: tokio_util::sync::CancellationToken,
}

impl NewCommand {
    pub fn new(sub: NewSubcommand, engines: Engines, session: Session) -> Self {
        Self {
            sub,
            engines,
            session,
            cancel: tokio_util::sync::CancellationToken::new(),
        }
    }

    /// Let an external cancel stop an interview agent. A remote work item's
    /// draft is still uploaded once after the agent exits.
    pub fn with_cancellation(mut self, token: tokio_util::sync::CancellationToken) -> Self {
        self.cancel = token;
        self
    }

    /// Construct from the catalogue-resolved input (WI 0113 F-10). The three
    /// `new` leaves share one entry point, selected by the caller's canonical
    /// path; `--format` takes its `"toml"` from the catalogue.
    pub fn from_input(ctx: &BuildContext) -> Result<Self, CommandError> {
        let sub = match ctx.caller.leaf() {
            "spec" => NewSubcommand::Spec(NewSpecFlags {
                interview: ctx.flags.bool("interview"),
                non_interactive: ctx.flags.bool("headless"),
                work_item: ctx.flags.string("work-item"),
            }),
            "workflow" => NewSubcommand::Workflow(NewWorkflowFlags {
                interview: ctx.flags.bool("interview"),
                non_interactive: ctx.flags.bool("headless"),
                global: ctx.flags.bool("global"),
                format: ctx.flags.require_str("format")?,
            }),
            "skill" => {
                let pull = ctx.flags.string("pull");
                let pull_all = ctx.flags.bool("pull-all");
                let subdir = ctx.flags.string("subdir");
                // `--subdir` names a path *inside* a pulled repository, so it
                // is meaningless without one. The catalogue cannot say
                // "requires one of two flags", so the check lives here.
                if subdir.is_some() && pull.is_none() && !pull_all {
                    return Err(CommandError::InvalidFlagValue {
                        command: ctx.path().iter().map(|part| (*part).to_string()).collect(),
                        flag: "subdir".to_string(),
                        reason: "--subdir requires --pull <repo>".to_string(),
                    });
                }
                NewSubcommand::Skill(NewSkillFlags {
                    interview: ctx.flags.bool("interview"),
                    non_interactive: ctx.flags.bool("headless"),
                    global: ctx.flags.bool("global"),
                    pull,
                    pull_all,
                    subdir,
                })
            }
            "agent" => NewSubcommand::Agent(NewAgentFlags {
                name: ctx.args.require("name")?,
                global: ctx.flags.bool("global"),
                from: ctx.flags.string("from"),
                force: ctx.flags.bool("force"),
            }),
            _ => return Err(CommandError::unknown_command(&ctx.path())),
        };
        Ok(Self::new(sub, ctx.engines.clone(), ctx.session.clone())
            .with_cancellation(ctx.cancel.clone()))
    }

    pub fn subcommand(&self) -> &NewSubcommand {
        &self.sub
    }
}

#[async_trait]
impl Command for NewCommand {
    type Frontend = Box<dyn NewCommandFrontend>;
    type Outcome = NewOutcome;

    async fn run_with_frontend(
        self,
        mut frontend: Self::Frontend,
    ) -> Result<Self::Outcome, CommandError> {
        // One clone of the subcommand's flags so `self` can be lent to each
        // runner alongside them (WI 0114 F-51). The flag structs are three
        // `Option<String>`s and a bool apiece.
        let outcome = match self.sub.clone() {
            NewSubcommand::Spec(f) => spec::run(&self, f, frontend.as_mut()).await?,
            NewSubcommand::Workflow(f) => workflow::run(&self, f, frontend.as_mut()).await?,
            NewSubcommand::Skill(f) => skill::run(&self, f, frontend.as_mut()).await?,
            NewSubcommand::Agent(f) => agent::run(&self, f, frontend.as_mut()).await?,
        };
        frontend.replay_queued();
        Ok(outcome)
    }
}

/// Container-side path of the skill file the interview agent is told to edit.
///
/// Pairs with [`skill_interview_overlay`]: the skill's directory is mounted at
/// [`SKILL_INTERVIEW_CONTAINER_DIR`], so the file the host wrote at
/// `<dir>/SKILL.md` is reachable at `/awman/skill/SKILL.md` inside the
/// container. Naming the host path in the prompt instead would send the agent
/// to a path that does not exist there.
fn skill_interview_container_file(host_path: &std::path::Path) -> String {
    let name = host_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("SKILL.md");
    format!("{SKILL_INTERVIEW_CONTAINER_DIR}/{name}")
}

/// Structural mount for `new skill --interview`: the new skill's own
/// directory, read-write, at [`SKILL_INTERVIEW_CONTAINER_DIR`].
///
/// Not a user-supplied overlay — it is how the interview agent reaches the
/// only file it is asked to write, so its container path is fixed and the
/// prompt names it outright.
fn skill_interview_overlay(dir: &std::path::Path) -> crate::engine::overlay::DirectorySpec {
    crate::engine::overlay::DirectorySpec {
        host: dir.to_string_lossy().into_owned(),
        container: SKILL_INTERVIEW_CONTAINER_DIR.to_string(),
        permission: crate::engine::container::options::OverlayPermission::ReadWrite,
    }
}

fn pull_success_message(outcome: &PullOutcome) -> UserMessage {
    UserMessage {
        level: MessageLevel::Info,
        text: format!(
            "Pulled '{}' into {} ({} skill(s) found under {}/): {}",
            outcome.slug,
            outcome.dir.display(),
            outcome.skills_found.len(),
            outcome.subdir,
            outcome.skills_found.join(", ")
        ),
    }
}

fn pull_library_outcome(outcome: PullOutcome) -> PullLibraryOutcome {
    PullLibraryOutcome {
        slug: outcome.slug,
        dir: outcome.dir.display().to_string(),
        updated: outcome.was_update,
        skills_found: outcome.skills_found,
        error: None,
    }
}

pub mod agent;
mod skill;
mod spec;
mod workflow;
