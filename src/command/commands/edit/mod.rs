//! `EditCommand` — `edit spec|workflow|skill|agent`.

use async_trait::async_trait;

use crate::command::commands::agent_setup::AgentLaunchFrontend;
use crate::command::commands::Command;
use crate::command::dispatch::{BuildContext, Engines};
use crate::command::error::CommandError;
use crate::data::message::{MessageLevel, UserMessage, UserMessageSink};
use crate::data::session::Session;
use crate::engine::agent::AgentRunOptions;

#[derive(Debug, Clone)]
pub struct EditCommonFlags {
    pub reference: String,
    pub interview: bool,
    pub amend: bool,
    pub headless: bool,
    pub prompt: Option<String>,
    pub json: bool,
    pub allow_docker: bool,
    pub yolo: bool,
    pub auto: bool,
    pub agent: Option<String>,
    pub model: Option<String>,
    pub overlays: Vec<String>,
    pub agent_flags: Option<Vec<String>>,
}

#[derive(Debug, Clone)]
pub struct EditSpecFlags {
    pub common: EditCommonFlags,
}

#[derive(Debug, Clone)]
pub struct EditArtifactFlags {
    pub common: EditCommonFlags,
    pub global: bool,
}

#[derive(Debug, Clone)]
pub enum EditSubcommand {
    Spec(EditSpecFlags),
    Workflow(EditArtifactFlags),
    Skill(EditArtifactFlags),
    Agent(EditArtifactFlags),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EditKind {
    Spec,
    Workflow,
    Skill,
    Agent,
}

impl EditKind {
    pub const ALL: [EditKind; 4] = [Self::Spec, Self::Workflow, Self::Skill, Self::Agent];
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Spec => "spec",
            Self::Workflow => "workflow",
            Self::Skill => "skill",
            Self::Agent => "agent",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Spec => "edit spec",
            Self::Workflow => "edit workflow",
            Self::Skill => "edit skill",
            Self::Agent => "edit agent",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "scope", rename_all = "lowercase")]
pub enum EditScope {
    Local,
    Remote { provider: String },
    Repo,
    Global,
    Library { slug: String },
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EditFileRole {
    Primary,
    Companion,
    Supporting,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct EditFile {
    pub host_path: std::path::PathBuf,
    pub container_path: String,
    pub role: EditFileRole,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "strategy", rename_all = "kebab-case")]
pub enum EditPersistence {
    InPlace,
    Staged {
        stage_dir: std::path::PathBuf,
    },
    RemoteDraft {
        draft_path: std::path::PathBuf,
        issue_url: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct EditTarget {
    pub kind: EditKind,
    pub identity: String,
    pub scope: EditScope,
    pub persistence: EditPersistence,
    pub files: Vec<EditFile>,
    pub bundle_dir: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct EditOutcome {
    pub target: EditTarget,
    pub interview: bool,
    pub amend: bool,
    pub headless: bool,
    pub changed: Vec<String>,
    pub notices: Vec<String>,
}

impl EditOutcome {
    /// Human-readable success summary shared by the CLI and TUI.
    pub fn render(&self) -> String {
        let persistence = match &self.target.persistence {
            EditPersistence::InPlace => "in place".to_string(),
            EditPersistence::Staged { .. } => "from a staged draft".to_string(),
            EditPersistence::RemoteDraft { issue_url, .. } => {
                format!("on the tracker ({issue_url})")
            }
        };
        let mut lines = vec![format!(
            "Edited {} '{}' {}.",
            self.target.kind.as_str(),
            self.target.identity,
            persistence
        )];
        if self.changed.is_empty() {
            lines.push("No changes were saved.".to_string());
        } else {
            lines.push("Changed:".to_string());
            lines.extend(self.changed.iter().map(|path| format!("  {path}")));
        }
        lines.extend(self.notices.iter().map(|notice| format!("Note: {notice}")));
        lines.join("\n")
    }
}

#[derive(Debug, Clone, Default)]
pub struct SaveSummary {
    pub changed: Vec<String>,
    pub notices: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SaveFailure {
    pub message: String,
    pub retained: Vec<std::path::PathBuf>,
    pub published: Vec<std::path::PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditSessionFailure {
    pub execution: Option<String>,
    pub save: Option<SaveFailure>,
    pub changed: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum EditError {
    #[error("{0}")]
    Target(String),
    #[error("{}", render_session_failure(.0))]
    Session(Box<EditSessionFailure>),
}

fn render_session_failure(failure: &EditSessionFailure) -> String {
    let mut reasons = Vec::new();
    if let Some(execution) = &failure.execution {
        reasons.push(execution.clone());
    }
    if let Some(save) = &failure.save {
        reasons.push(format!("saving failed: {}", save.message));
    }
    if reasons.is_empty() {
        String::new()
    } else {
        format!(": {}", reasons.join("; "))
    }
}

pub fn conclude(
    label: &str,
    target: EditTarget,
    flags: &EditCommonFlags,
    execution: Result<i32, String>,
    save: Result<SaveSummary, SaveFailure>,
    sink: &mut dyn UserMessageSink,
) -> Result<EditOutcome, CommandError> {
    let execution = match execution {
        Ok(0) => None,
        Ok(code) => Some(format!("agent exited with code {code}")),
        Err(text) => Some(text),
    };
    match save {
        Ok(summary) if execution.is_none() => Ok(EditOutcome {
            target,
            interview: flags.interview,
            amend: flags.amend,
            headless: flags.headless,
            changed: summary.changed,
            notices: summary.notices,
        }),
        Ok(summary) => {
            let execution = execution.expect("guard above checked execution failure");
            sink.write_message(UserMessage {
                level: MessageLevel::Error,
                text: format!("{label}: {execution}"),
            });
            Err(EditError::Session(Box::new(EditSessionFailure {
                execution: Some(execution),
                save: None,
                changed: summary.changed,
            }))
            .into())
        }
        Err(failure) => {
            let changed = failure
                .published
                .iter()
                .map(|path| path.display().to_string())
                .collect();
            if let Some(execution) = &execution {
                sink.write_message(UserMessage {
                    level: MessageLevel::Error,
                    text: format!("{label}: {execution}"),
                });
            }
            sink.write_message(UserMessage {
                level: MessageLevel::Error,
                text: format!("{label}: saving failed: {}", failure.message),
            });
            Err(EditError::Session(Box::new(EditSessionFailure {
                execution,
                save: Some(failure),
                changed,
            }))
            .into())
        }
    }
}

pub trait EditCommandFrontend: UserMessageSink + AgentLaunchFrontend + Send + Sync {
    fn show_edit_outcome(&mut self, _outcome: &EditOutcome) {}
}

pub struct EditCommand {
    sub: EditSubcommand,
    engines: Engines,
    session: Session,
    cancel: tokio_util::sync::CancellationToken,
}

impl EditCommand {
    pub fn new(sub: EditSubcommand, engines: Engines, session: Session) -> Self {
        Self {
            sub,
            engines,
            session,
            cancel: tokio_util::sync::CancellationToken::new(),
        }
    }
    pub fn with_cancellation(mut self, token: tokio_util::sync::CancellationToken) -> Self {
        self.cancel = token;
        self
    }

    pub fn from_input(ctx: &BuildContext) -> Result<Self, CommandError> {
        let reference = ctx.args.require("ref")?;
        let command: Vec<String> = ctx.path().iter().map(|part| (*part).to_string()).collect();
        let prompt = ctx.flags.string("prompt").and_then(|value| {
            let value = value.trim().to_string();
            (!value.is_empty()).then_some(value)
        });
        let common = EditCommonFlags {
            reference,
            interview: ctx.flags.bool("interview"),
            amend: ctx.flags.bool("amend"),
            headless: ctx.flags.bool("headless"),
            prompt,
            json: ctx.flags.bool("json"),
            allow_docker: ctx.flags.bool("allow-docker"),
            yolo: ctx.flags.bool("yolo"),
            auto: ctx.flags.bool("auto"),
            agent: ctx.flags.string("agent"),
            model: ctx.flags.string("model"),
            overlays: ctx.flags.strs("overlay").to_vec(),
            agent_flags: ctx
                .flags
                .supplied("agent-flag")
                .then(|| ctx.flags.strs("agent-flag").to_vec()),
        };
        if common.interview && common.headless {
            return Err(CommandError::InvalidFlagValue { command, flag: "interview".into(), reason: "--interview needs an interactive session; it cannot run headless or with --json".into() });
        }
        if common.headless && !common.amend && common.prompt.is_none() {
            return Err(CommandError::InvalidFlagValue {
                command,
                flag: "prompt".into(),
                reason: "a headless edit needs --prompt <text> (or --amend)".into(),
            });
        }
        let sub = match ctx.caller.leaf() {
            "spec" => EditSubcommand::Spec(EditSpecFlags { common }),
            "workflow" => EditSubcommand::Workflow(EditArtifactFlags {
                common,
                global: ctx.flags.bool("global"),
            }),
            "skill" => EditSubcommand::Skill(EditArtifactFlags {
                common,
                global: ctx.flags.bool("global"),
            }),
            "agent" => EditSubcommand::Agent(EditArtifactFlags {
                common,
                global: ctx.flags.bool("global"),
            }),
            _ => return Err(CommandError::unknown_command(&ctx.path())),
        };
        Ok(Self::new(sub, ctx.engines.clone(), ctx.session.clone())
            .with_cancellation(ctx.cancel.clone()))
    }

    pub fn subcommand(&self) -> &EditSubcommand {
        &self.sub
    }
    pub(crate) fn engines(&self) -> &Engines {
        &self.engines
    }
    pub(crate) fn session(&self) -> &Session {
        &self.session
    }
    pub(crate) fn cancel(&self) -> &tokio_util::sync::CancellationToken {
        &self.cancel
    }
}

#[async_trait]
impl Command for EditCommand {
    type Frontend = Box<dyn EditCommandFrontend>;
    type Outcome = EditOutcome;

    async fn run_with_frontend(
        self,
        mut frontend: Self::Frontend,
    ) -> Result<Self::Outcome, CommandError> {
        let outcome = match self.sub.clone() {
            EditSubcommand::Spec(flags) => spec::run(&self, flags, frontend.as_mut()).await?,
            EditSubcommand::Workflow(flags) => {
                workflow::run(&self, flags, frontend.as_mut()).await?
            }
            EditSubcommand::Skill(flags) => skill::run(&self, flags, frontend.as_mut()).await?,
            EditSubcommand::Agent(flags) => agent::run(&self, flags, frontend.as_mut()).await?,
        };
        frontend.show_edit_outcome(&outcome);
        frontend.replay_queued();
        Ok(outcome)
    }
}

/// Every edit session mounts the whole repository (`session.git_root()`):
/// the target mapping depends on it. Ask for mount-scope consent like other
/// launches and refuse, rather than silently widen, a current-directory-only
/// choice. Must run before anything is staged or launched.
fn require_repository_mount(
    session: &Session,
    frontend: &mut dyn EditCommandFrontend,
    label: &str,
    what: &str,
) -> Result<(), CommandError> {
    let mount = crate::command::commands::mount_scope::MountScope::resolve(
        session.working_dir(),
        session.git_root(),
        frontend,
    )?;
    if mount != crate::data::fs::path_guard::canonicalize_lenient(session.git_root()) {
        let message = format!(
            "editing {what} requires the repository mount; the selected current-directory mount cannot be used"
        );
        frontend.write_message(UserMessage {
            level: MessageLevel::Error,
            text: format!("{label}: {message}"),
        });
        return Err(EditError::Target(message).into());
    }
    Ok(())
}

pub(crate) fn run_options(flags: &EditCommonFlags) -> AgentRunOptions {
    AgentRunOptions {
        yolo: flags
            .yolo
            .then_some(crate::engine::container::options::YoloMode::Enabled),
        auto: flags
            .auto
            .then_some(crate::engine::container::options::AutoMode::Enabled),
        allow_docker: flags.allow_docker,
        non_interactive: flags.headless,
        model: flags.model.clone(),
        agent_flags: flags.agent_flags.clone(),
        ..AgentRunOptions::default()
    }
}

mod agent;
mod artifact;
mod skill;
mod spec;
mod workflow;
