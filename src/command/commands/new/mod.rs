//! `NewCommand` — `new spec`, `new workflow`, `new skill`, `new agent`.

use async_trait::async_trait;
use serde::Serialize;

use crate::command::commands::skill_library::{
    pull_all_libraries, pull_library, resolve_pull_target, PullOutcome,
};
use crate::command::commands::Command;
use crate::command::dispatch::{BuildContext, Engines};
use crate::command::error::CommandError;
use crate::data::fs::{SkillDirs, WorkflowDirs};
use crate::data::message::{MessageLevel, UserMessage, UserMessageSink};
use crate::data::session::Session;

/// Supplied answers to the simple creation questions, including headless runs.
#[derive(Debug, Clone, Default)]
pub struct NewInput {
    pub title: Option<String>,
    pub summary: Option<String>,
}

impl NewInput {
    fn from_context(ctx: &BuildContext) -> Self {
        Self {
            title: ctx.flags.string("title"),
            summary: ctx.flags.string("prompt"),
        }
    }

    pub(crate) fn title(
        &self,
        headless: bool,
        ask: impl FnOnce() -> Result<String, CommandError>,
    ) -> Result<String, CommandError> {
        answer(self.title.as_deref(), headless, "title", ask)
    }

    pub(crate) fn summary(
        &self,
        headless: bool,
        ask: impl FnOnce() -> Result<String, CommandError>,
    ) -> Result<String, CommandError> {
        answer(self.summary.as_deref(), headless, "prompt", ask)
    }
}

fn answer(
    supplied: Option<&str>,
    headless: bool,
    flag: &str,
    ask: impl FnOnce() -> Result<String, CommandError>,
) -> Result<String, CommandError> {
    if let Some(value) = supplied.filter(|value| !value.trim().is_empty()) {
        return Ok(value.trim().to_owned());
    }
    if headless {
        return Err(CommandError::InteractiveInputUnavailable {
            prompt: format!("supply --{flag} for headless creation"),
        });
    }
    let value = ask()?;
    if value.trim().is_empty() {
        return Err(CommandError::Aborted);
    }
    Ok(value.trim().to_owned())
}

struct NewSession {
    headless: bool,
    yolo: bool,
    summary: String,
}

/// Use the same staging, prompt, validation and cancellation path as editing.
async fn author_artifact(
    command: &NewCommand,
    kind: crate::data::fs::edit_target::ArtifactKind,
    path: &std::path::Path,
    global: bool,
    authoring: NewSession,
    frontend: &mut dyn NewCommandFrontend,
) -> Result<(), CommandError> {
    use crate::command::commands::edit::{
        EditArtifactFlags, EditCommand, EditCommonFlags, EditSubcommand,
    };
    let flags = EditArtifactFlags {
        common: EditCommonFlags {
            reference: path.display().to_string(),
            headless: authoring.headless,
            yolo: authoring.yolo,
            prompt: Some(authoring.summary),
            ..Default::default()
        },
        global,
    };
    let subcommand = match kind {
        crate::data::fs::edit_target::ArtifactKind::Workflow => {
            EditSubcommand::Workflow(flags.clone())
        }
        crate::data::fs::edit_target::ArtifactKind::Skill => EditSubcommand::Skill(flags.clone()),
        crate::data::fs::edit_target::ArtifactKind::Agent => EditSubcommand::Agent(flags.clone()),
    };
    let mut edit = EditCommand::new(subcommand, command.engines.clone(), command.session.clone())
        .with_cancellation(command.cancel.clone());
    edit.creating = true;
    crate::command::commands::edit::artifact::run(&edit, kind, flags, frontend).await?;
    Ok(())
}

/// Create only a seed file, refusing existing files and symlinked ancestors.
fn write_seed(
    root: &std::path::Path,
    path: &std::path::Path,
    body: &str,
) -> Result<(), CommandError> {
    use crate::data::error::DataError;
    use std::io::Write;
    crate::data::fs::path_guard::validate_under_root(
        root,
        path,
        "creation target escapes its root",
    )?;
    for parent in path.ancestors().take_while(|parent| *parent != root) {
        crate::data::fs::path_guard::ensure_not_symlinked(parent)?;
    }
    let parent = path
        .parent()
        .ok_or_else(|| CommandError::Other("creation target has no parent".into()))?;
    std::fs::create_dir_all(parent).map_err(|e| DataError::io(parent, e))?;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|e| DataError::io(path, e))?;
    file.write_all(body.as_bytes())
        .map_err(|e| DataError::io(path, e))?;
    Ok(())
}

#[derive(Debug, Clone, Default)]
pub struct NewSpecFlags {
    pub yolo: bool,
    pub input: NewInput,
    pub kind: Option<crate::command::work_item::spec_service::WorkItemKind>,
    pub non_interactive: bool,
    /// Raw `--work-item` reference that seeds the new spec (WI 0147).
    pub work_item: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct NewWorkflowFlags {
    pub yolo: bool,
    pub input: NewInput,
    pub non_interactive: bool,
    pub global: bool,
    pub format: String,
}

#[derive(Debug, Clone, Default)]
pub struct NewSkillFlags {
    pub yolo: bool,
    pub input: NewInput,
    pub non_interactive: bool,
    pub global: bool,
    pub pull: Option<String>,
    pub pull_all: bool,
    pub subdir: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct NewAgentFlags {
    pub yolo: bool,
    pub input: NewInput,
    pub non_interactive: bool,
    /// The positional name; becomes the file stem.
    pub name: String,
    pub global: bool,
    /// `--from <example>`; `None` writes the commented skeleton.
    pub from: Option<String>,
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
    /// The TOML, plus `maki.Dockerfile` for `--from maki`.
    pub files_written: Vec<String>,
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

/// `NewCommandFrontend` extends `NewSpecFrontend` so the `Spec`
/// subcommand can drive the same Q&A (kind / title / summary).
pub trait NewCommandFrontend:
    UserMessageSink + crate::command::work_item::spec_service::NewSpecFrontend + Send + Sync
{
    /// Prompt for a human-readable workflow title.
    fn ask_workflow_title(&mut self) -> Result<String, CommandError> {
        Err(CommandError::InteractiveInputUnavailable {
            prompt: "workflow title".into(),
        })
    }
    /// Prompt for a one-line summary for the new workflow.
    fn ask_workflow_summary(&mut self) -> Result<String, CommandError> {
        Err(CommandError::InteractiveInputUnavailable {
            prompt: "workflow summary".into(),
        })
    }
    /// Prompt for a skill name.
    fn ask_skill_name(&mut self) -> Result<String, CommandError> {
        Err(CommandError::InteractiveInputUnavailable {
            prompt: "skill name".into(),
        })
    }
    /// Prompt for a one-line summary for the new skill.
    fn ask_skill_summary(&mut self) -> Result<String, CommandError> {
        Err(CommandError::InteractiveInputUnavailable {
            prompt: "skill summary".into(),
        })
    }
    /// The short brief for the agent definition to author.
    fn ask_agent_summary(&mut self) -> Result<String, CommandError> {
        Err(CommandError::InteractiveInputUnavailable {
            prompt: "agent summary".into(),
        })
    }

    /// `new agent` finished, for frontends that do not render outcomes
    /// themselves (the TUI). The CLI renders from the outcome on stdout.
    fn show_new_agent(&mut self, _outcome: &NewAgentOutcome) {}
}

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

    /// Let an external cancel stop the authoring agent. A remote work item's
    /// draft is still uploaded once after the agent exits.
    pub fn with_cancellation(mut self, token: tokio_util::sync::CancellationToken) -> Self {
        self.cancel = token;
        self
    }

    /// Construct from the catalogue-resolved input (WI 0113 F-10). The four
    /// `new` leaves share one entry point, selected by the caller's canonical
    /// path; `--format` takes its `"toml"` from the catalogue.
    pub fn from_input(ctx: &BuildContext) -> Result<Self, CommandError> {
        let sub = match ctx.caller.leaf() {
            "spec" => NewSubcommand::Spec(NewSpecFlags {
                input: NewInput::from_context(ctx),
                yolo: ctx.flags.bool("yolo"),
                kind: ctx.flags.string("kind").map(|kind| match kind.as_str() {
                    "feature" => crate::command::work_item::spec_service::WorkItemKind::Feature,
                    "bug" => crate::command::work_item::spec_service::WorkItemKind::Bug,
                    "enhancement" => {
                        crate::command::work_item::spec_service::WorkItemKind::Enhancement
                    }
                    _ => crate::command::work_item::spec_service::WorkItemKind::Task,
                }),
                non_interactive: ctx.flags.bool("headless"),
                work_item: ctx.flags.string("work-item"),
            }),
            "workflow" => NewSubcommand::Workflow(NewWorkflowFlags {
                input: NewInput::from_context(ctx),
                yolo: ctx.flags.bool("yolo"),
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
                    input: NewInput::from_context(ctx),
                    yolo: ctx.flags.bool("yolo"),
                    non_interactive: ctx.flags.bool("headless"),
                    global: ctx.flags.bool("global"),
                    pull,
                    pull_all,
                    subdir,
                })
            }
            "agent" => NewSubcommand::Agent(NewAgentFlags {
                input: NewInput::from_context(ctx),
                yolo: ctx.flags.bool("yolo"),
                non_interactive: ctx.flags.bool("headless"),
                name: ctx.args.require("name")?,
                global: ctx.flags.bool("global"),
                from: ctx.flags.string("from"),
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
        // runner alongside them (WI 0114 F-51).
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
