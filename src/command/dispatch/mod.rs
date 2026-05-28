//! `Dispatch` — Layer 2's gateway from frontends into typed `*Command` values.
//!
//! Frontends construct a `Dispatch` with a frontend-specific
//! [`CommandFrontend`] implementation (CLI, TUI, API). Dispatch reads
//! flag values from the frontend, applies catalogue-driven validation
//! (mutually-exclusive flags, type errors, implications), and returns a typed
//! [`BuiltCommand`] enum containing the constructed `*Command` struct.

use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::RwLock;

use crate::command::commands::api_server::{
    ApiServerCommand, ApiServerCommandFrontend, ApiServerKillFlags, ApiServerLogsFlags,
    ApiServerStartFlags, ApiServerStatusFlags, ApiServerSubcommand,
};
use crate::command::commands::auth::{AuthCommand, AuthCommandFrontend};
use crate::command::commands::chat::{ChatCommand, ChatCommandFlags, ChatCommandFrontend};
use crate::command::commands::config::{
    ConfigCommand, ConfigCommandFrontend, ConfigGetFlags, ConfigSetFlags, ConfigShowFlags,
    ConfigSubcommand,
};
use crate::command::commands::download::{DownloadCommand, DownloadCommandFrontend};
use crate::command::commands::exec_prompt::{
    ExecPromptCommand, ExecPromptCommandFlags, ExecPromptCommandFrontend,
};
use crate::command::commands::exec_workflow::{
    ExecWorkflowCommand, ExecWorkflowCommandFlags, ExecWorkflowCommandFrontend,
};
use crate::command::commands::init::{InitCommand, InitCommandFlags, InitCommandFrontend};
use crate::command::commands::new::{
    NewCommand, NewCommandFrontend, NewSkillFlags, NewSpecFlags, NewSubcommand, NewWorkflowFlags,
};
use crate::command::commands::ready::{ReadyCommand, ReadyCommandFlags, ReadyCommandFrontend};
use crate::command::commands::remote::{
    RemoteCommand, RemoteCommandFrontend, RemoteExecPromptFlags, RemoteExecWorkflowFlags,
    RemoteSessionKillFlags, RemoteSessionStartFlags, RemoteSubcommand,
};
use crate::command::commands::specs::{
    SpecsAmendFlags, SpecsCommand, SpecsCommandFrontend, SpecsSubcommand,
};
use crate::command::commands::status::{StatusCommand, StatusCommandFlags, StatusCommandFrontend};
use crate::command::commands::Command;
use crate::command::dispatch::catalogue::{CommandCatalogue, FlagKind, FlagSpec};
use crate::command::error::CommandError;
use crate::data::session::Session;
use crate::engine::agent::AgentEngine;
use crate::engine::auth::AuthEngine;
use crate::engine::container::ContainerRuntime;
use crate::engine::git::GitEngine;
use crate::engine::message::UserMessageSink;
use crate::engine::overlay::OverlayEngine;

pub mod catalogue;
pub mod parsed_input;
pub mod projections;

pub use parsed_input::ParsedCommandBoxInput;

// ─── Pre-wired engines bundle ───────────────────────────────────────────────

/// All Layer 1 engine handles a `Dispatch` needs to construct a `*Command`.
/// `ReadyEngine` and `InitEngine` are NOT pre-constructed here — those
/// engines accept per-invocation flag values.
#[derive(Clone)]
pub struct Engines {
    pub runtime: Arc<ContainerRuntime>,
    pub git_engine: Arc<GitEngine>,
    pub overlay_engine: Arc<OverlayEngine>,
    pub auth_engine: Arc<AuthEngine>,
    pub agent_engine: Arc<AgentEngine>,
    pub workflow_state_store: Arc<crate::data::EngineWorkflowStateStore>,
}

// ─── CommandFrontend trait ──────────────────────────────────────────────────

/// Frontend trait that supplies flag values to Dispatch. Extended by per-
/// command frontend traits (e.g. [`crate::command::commands::exec_workflow::ExecWorkflowCommandFrontend`])
/// for command-specific Q&A and reporting.
pub trait CommandFrontend: UserMessageSink + Send + Sync {
    fn flag_bool(&self, command_path: &[&str], flag: &str) -> Result<Option<bool>, CommandError>;

    fn flag_string(
        &self,
        command_path: &[&str],
        flag: &str,
    ) -> Result<Option<String>, CommandError>;

    fn flag_strings(&self, command_path: &[&str], flag: &str) -> Result<Vec<String>, CommandError>;

    fn flag_path(&self, command_path: &[&str], flag: &str)
        -> Result<Option<PathBuf>, CommandError>;

    fn flag_enum(&self, command_path: &[&str], flag: &str) -> Result<Option<String>, CommandError>;

    fn flag_u16(&self, command_path: &[&str], flag: &str) -> Result<Option<u16>, CommandError>;

    fn argument(&self, command_path: &[&str], name: &str) -> Result<Option<String>, CommandError>;

    fn arguments(&self, command_path: &[&str], name: &str) -> Result<Vec<String>, CommandError>;
}

// ─── Frontend supertrait ────────────────────────────────────────────────────

/// Frontend type accepted by [`Dispatch::run_command`]. A single concrete
/// frontend (CLI, TUI, or API) implements every per-command frontend
/// trait via this supertrait so that dispatch can move the frontend value
/// into the matching `Box<dyn *CommandFrontend>` for whichever variant
/// `build_command` returned. Layer 3 frontends typically derive this
/// automatically via a blanket impl over a single struct that implements
/// each trait.
pub trait DispatchFrontend:
    CommandFrontend
    + InitCommandFrontend
    + ReadyCommandFrontend
    + ChatCommandFrontend
    + StatusCommandFrontend
    + ConfigCommandFrontend
    + ExecPromptCommandFrontend
    + ExecWorkflowCommandFrontend
    + ApiServerCommandFrontend
    + RemoteCommandFrontend
    + NewCommandFrontend
    + AuthCommandFrontend
    + DownloadCommandFrontend
    + SpecsCommandFrontend
    + 'static
{
}

impl<T> DispatchFrontend for T where
    T: CommandFrontend
        + InitCommandFrontend
        + ReadyCommandFrontend
        + ChatCommandFrontend
        + StatusCommandFrontend
        + ConfigCommandFrontend
        + ExecPromptCommandFrontend
        + ExecWorkflowCommandFrontend
        + ApiServerCommandFrontend
        + RemoteCommandFrontend
        + NewCommandFrontend
        + AuthCommandFrontend
        + DownloadCommandFrontend
        + SpecsCommandFrontend
        + 'static
{
}

// ─── Outcome / error wrappers ───────────────────────────────────────────────

/// Catch-all outcome enum returned by `Dispatch::run_command`. Layer 3
/// inspects the variant to choose an appropriate rendering.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "kind", content = "payload")]
pub enum CommandOutcome {
    Init(crate::command::commands::init::InitOutcome),
    Ready(crate::command::commands::ready::ReadyOutcome),
    Chat(crate::command::commands::chat::ChatOutcome),
    Status(crate::command::commands::status::StatusOutcome),
    Config(crate::command::commands::config::ConfigOutcome),
    ExecPrompt(crate::command::commands::exec_prompt::ExecPromptOutcome),
    ExecWorkflow(crate::command::commands::exec_workflow::ExecWorkflowOutcome),
    ApiServer(crate::command::commands::api_server::ApiServerOutcome),
    Remote(crate::command::commands::remote::RemoteOutcome),
    New(crate::command::commands::new::NewOutcome),
    Specs(crate::command::commands::specs::SpecsOutcome),
    Auth(crate::command::commands::auth::AuthOutcome),
    Download(crate::command::commands::download::DownloadOutcome),
    /// Trivial wrapper used by no-op leaf commands during the refactor.
    Empty,
}

/// One per `*Command` struct in `src/command/commands/`. Constructed by
/// [`Dispatch::build_command`] and consumed by [`Dispatch::run_command`].
pub enum BuiltCommand {
    Init(InitCommand),
    Ready(ReadyCommand),
    Chat(ChatCommand),
    Specs(SpecsCommand),
    Status(StatusCommand),
    Config(ConfigCommand),
    ExecPrompt(ExecPromptCommand),
    ExecWorkflow(ExecWorkflowCommand),
    ApiServer(ApiServerCommand),
    Remote(RemoteCommand),
    New(NewCommand),
    Auth(AuthCommand),
    Download(DownloadCommand),
}

// ─── Dispatch ───────────────────────────────────────────────────────────────

pub struct Dispatch<F: CommandFrontend> {
    catalogue: &'static CommandCatalogue,
    frontend: F,
    session: Arc<RwLock<Session>>,
    engines: Engines,
}

impl<F: CommandFrontend> Dispatch<F> {
    pub fn new(frontend: F, session: Arc<RwLock<Session>>, engines: Engines) -> Self {
        Self {
            catalogue: CommandCatalogue::get(),
            frontend,
            session,
            engines,
        }
    }

    pub fn catalogue(&self) -> &'static CommandCatalogue {
        self.catalogue
    }

    pub fn frontend(&self) -> &F {
        &self.frontend
    }

    pub fn frontend_mut(&mut self) -> &mut F {
        &mut self.frontend
    }

    pub fn session(&self) -> Arc<RwLock<Session>> {
        Arc::clone(&self.session)
    }

    pub fn engines(&self) -> &Engines {
        &self.engines
    }

    /// Read flags from the frontend and construct the typed `*Command`. No
    /// engine work happens at this point — the command is "ready to run".
    pub fn build_command(&self, path: &[&str]) -> Result<BuiltCommand, CommandError> {
        let canonical: Vec<&str> = self.catalogue.canonical_path(path).into_iter().collect();
        let canonical_refs: Vec<&str> = canonical.to_vec();
        let spec = self
            .catalogue
            .lookup(&canonical_refs)
            .ok_or_else(|| CommandError::unknown_command(path))?;
        // Validate mutually-exclusive flags up front.
        validate_conflicts(&self.frontend, &canonical_refs, spec.flags)?;
        // Read the session from the shared state so every command operates
        // in the correct working directory (tab-specific in the TUI).
        let session = self
            .session
            .try_read()
            .map_err(|_| CommandError::Other("session is write-locked".into()))?
            .clone();
        // Per-command construction.
        match canonical_refs.as_slice() {
            ["init"] => {
                let agent = self
                    .frontend
                    .flag_enum(&canonical_refs, "agent")?
                    .unwrap_or_else(|| "claude".to_string());
                let aspec = self
                    .frontend
                    .flag_bool(&canonical_refs, "aspec")?
                    .unwrap_or(false);
                Ok(BuiltCommand::Init(InitCommand::new(
                    InitCommandFlags { agent, aspec },
                    self.engines.clone(),
                    session.clone(),
                )))
            }
            ["ready"] => {
                let mut flags = read_ready_flags(&self.frontend, &canonical_refs)?;
                // --json implies --non-interactive
                if flags.json {
                    flags.non_interactive = true;
                }
                Ok(BuiltCommand::Ready(ReadyCommand::new(
                    flags,
                    self.engines.clone(),
                    session.clone(),
                )))
            }
            ["chat"] => {
                let flags = read_chat_flags(&self.frontend, &canonical_refs)?;
                Ok(BuiltCommand::Chat(ChatCommand::new(
                    flags,
                    self.engines.clone(),
                    session.clone(),
                )))
            }
            ["specs", "amend"] => {
                let work_item = self
                    .frontend
                    .argument(&canonical_refs, "work_item")?
                    .ok_or_else(|| {
                        CommandError::missing_required_argument(&canonical_refs, "work_item")
                    })?;
                let non_interactive = self
                    .frontend
                    .flag_bool(&canonical_refs, "non-interactive")?
                    .unwrap_or(false);
                let allow_docker = self
                    .frontend
                    .flag_bool(&canonical_refs, "allow-docker")?
                    .unwrap_or(false);
                Ok(BuiltCommand::Specs(SpecsCommand::new(
                    SpecsSubcommand::Amend(SpecsAmendFlags {
                        work_item,
                        non_interactive,
                        allow_docker,
                    }),
                    self.engines.clone(),
                    session.clone(),
                )))
            }
            ["status"] => {
                let watch = self
                    .frontend
                    .flag_bool(&canonical_refs, "watch")?
                    .unwrap_or(false);
                Ok(BuiltCommand::Status(StatusCommand::new(
                    StatusCommandFlags { watch },
                    self.engines.clone(),
                )))
            }
            ["config", "show"] => Ok(BuiltCommand::Config(ConfigCommand::new(
                ConfigSubcommand::Show(ConfigShowFlags {}),
                self.engines.clone(),
                session.clone(),
            ))),
            ["config", "get"] => {
                let field = self
                    .frontend
                    .argument(&canonical_refs, "field")?
                    .ok_or_else(|| {
                        CommandError::missing_required_argument(&canonical_refs, "field")
                    })?;
                Ok(BuiltCommand::Config(ConfigCommand::new(
                    ConfigSubcommand::Get(ConfigGetFlags { field }),
                    self.engines.clone(),
                    session.clone(),
                )))
            }
            ["config", "set"] => {
                let field = self
                    .frontend
                    .argument(&canonical_refs, "field")?
                    .ok_or_else(|| {
                        CommandError::missing_required_argument(&canonical_refs, "field")
                    })?;
                let value = self
                    .frontend
                    .argument(&canonical_refs, "value")?
                    .ok_or_else(|| {
                        CommandError::missing_required_argument(&canonical_refs, "value")
                    })?;
                let global = self
                    .frontend
                    .flag_bool(&canonical_refs, "global")?
                    .unwrap_or(false);
                Ok(BuiltCommand::Config(ConfigCommand::new(
                    ConfigSubcommand::Set(ConfigSetFlags {
                        field,
                        value,
                        global,
                    }),
                    self.engines.clone(),
                    session.clone(),
                )))
            }
            ["exec", "prompt"] => {
                let prompt = self
                    .frontend
                    .argument(&canonical_refs, "prompt")?
                    .ok_or_else(|| {
                        CommandError::missing_required_argument(&canonical_refs, "prompt")
                    })?;
                if prompt.trim().is_empty() {
                    return Err(CommandError::InvalidArgumentValue {
                        command: canonical_refs.iter().map(|s| s.to_string()).collect(),
                        argument: "prompt".into(),
                        reason: "prompt must not be empty".into(),
                    });
                }
                let flags = read_exec_prompt_flags(&self.frontend, &canonical_refs, prompt)?;
                Ok(BuiltCommand::ExecPrompt(ExecPromptCommand::new(
                    flags,
                    self.engines.clone(),
                    session.clone(),
                )))
            }
            ["exec", "workflow"] => {
                let mut flags = read_exec_workflow_flags(&self.frontend, &canonical_refs)?;
                // Catalogue declares yolo/auto imply worktree; enforce here as well.
                if flags.yolo || flags.auto {
                    flags.worktree = true;
                }
                Ok(BuiltCommand::ExecWorkflow(ExecWorkflowCommand::new(
                    flags,
                    self.engines.clone(),
                    session.clone(),
                )))
            }
            ["api", "start"] => {
                let port = self
                    .frontend
                    .flag_u16(&canonical_refs, "port")?
                    .unwrap_or(9876);
                let workdirs = self.frontend.flag_strings(&canonical_refs, "workdirs")?;
                let background = self
                    .frontend
                    .flag_bool(&canonical_refs, "background")?
                    .unwrap_or(false);
                let refresh_key = self
                    .frontend
                    .flag_bool(&canonical_refs, "refresh-key")?
                    .unwrap_or(false);
                let dangerously_skip_auth = self
                    .frontend
                    .flag_bool(&canonical_refs, "dangerously-skip-auth")?
                    .unwrap_or(false);
                let dangerously_skip_tls = self
                    .frontend
                    .flag_bool(&canonical_refs, "dangerously-skip-tls")?
                    .unwrap_or(false);
                Ok(BuiltCommand::ApiServer(ApiServerCommand::new(
                    ApiServerSubcommand::Start(ApiServerStartFlags {
                        port,
                        workdirs,
                        background,
                        refresh_key,
                        dangerously_skip_auth,
                        dangerously_skip_tls,
                    }),
                    self.engines.clone(),
                )))
            }
            ["api", "kill"] => Ok(BuiltCommand::ApiServer(ApiServerCommand::new(
                ApiServerSubcommand::Kill(ApiServerKillFlags {}),
                self.engines.clone(),
            ))),
            ["api", "logs"] => Ok(BuiltCommand::ApiServer(ApiServerCommand::new(
                ApiServerSubcommand::Logs(ApiServerLogsFlags {}),
                self.engines.clone(),
            ))),
            ["api", "status"] => Ok(BuiltCommand::ApiServer(ApiServerCommand::new(
                ApiServerSubcommand::Status(ApiServerStatusFlags {}),
                self.engines.clone(),
            ))),
            ["remote", "exec", "workflow"] => {
                let workflow = self
                    .frontend
                    .flag_path(&canonical_refs, "workflow")?
                    .or_else(|| {
                        self.frontend
                            .argument(&canonical_refs, "workflow")
                            .ok()
                            .flatten()
                            .map(PathBuf::from)
                    })
                    .ok_or_else(|| {
                        CommandError::missing_required_argument(&canonical_refs, "workflow")
                    })?;
                Ok(BuiltCommand::Remote(RemoteCommand::new(
                    RemoteSubcommand::ExecWorkflow(RemoteExecWorkflowFlags {
                        workflow,
                        work_item: self.frontend.flag_string(&canonical_refs, "work-item")?,
                        agent: self.frontend.flag_string(&canonical_refs, "agent")?,
                        remote_addr: self.frontend.flag_string(&canonical_refs, "remote-addr")?,
                        session: self.frontend.flag_string(&canonical_refs, "session")?,
                        follow: self
                            .frontend
                            .flag_bool(&canonical_refs, "follow")?
                            .unwrap_or(false),
                        api_key: self.frontend.flag_string(&canonical_refs, "api-key")?,
                    }),
                    self.engines.clone(),
                    session.clone(),
                )))
            }
            ["remote", "exec", "prompt"] => {
                let prompt = self
                    .frontend
                    .argument(&canonical_refs, "prompt")?
                    .ok_or_else(|| {
                        CommandError::missing_required_argument(&canonical_refs, "prompt")
                    })?;
                Ok(BuiltCommand::Remote(RemoteCommand::new(
                    RemoteSubcommand::ExecPrompt(RemoteExecPromptFlags {
                        prompt,
                        agent: self.frontend.flag_string(&canonical_refs, "agent")?,
                        remote_addr: self.frontend.flag_string(&canonical_refs, "remote-addr")?,
                        session: self.frontend.flag_string(&canonical_refs, "session")?,
                        follow: self
                            .frontend
                            .flag_bool(&canonical_refs, "follow")?
                            .unwrap_or(false),
                        api_key: self.frontend.flag_string(&canonical_refs, "api-key")?,
                    }),
                    self.engines.clone(),
                    session.clone(),
                )))
            }
            ["remote", "session", "start"] => {
                let session_type = self
                    .frontend
                    .flag_enum(&canonical_refs, "type")?
                    .unwrap_or_else(|| "local".to_string());
                Ok(BuiltCommand::Remote(RemoteCommand::new(
                    RemoteSubcommand::SessionStart(RemoteSessionStartFlags {
                        session_type,
                        workdir: self.frontend.flag_string(&canonical_refs, "workdir")?,
                        repo_url: self.frontend.flag_string(&canonical_refs, "repo-url")?,
                        branch: self.frontend.flag_string(&canonical_refs, "branch")?,
                        remote_addr: self.frontend.flag_string(&canonical_refs, "remote-addr")?,
                        api_key: self.frontend.flag_string(&canonical_refs, "api-key")?,
                    }),
                    self.engines.clone(),
                    session.clone(),
                )))
            }
            ["remote", "session", "kill"] => {
                let session_id = self.frontend.argument(&canonical_refs, "session_id")?;
                let remote_addr = self.frontend.flag_string(&canonical_refs, "remote-addr")?;
                let api_key = self.frontend.flag_string(&canonical_refs, "api-key")?;
                Ok(BuiltCommand::Remote(RemoteCommand::new(
                    RemoteSubcommand::SessionKill(RemoteSessionKillFlags {
                        session_id,
                        remote_addr,
                        api_key,
                    }),
                    self.engines.clone(),
                    session.clone(),
                )))
            }
            ["new", "spec"] => {
                let interview = self
                    .frontend
                    .flag_bool(&canonical_refs, "interview")?
                    .unwrap_or(false);
                let non_interactive = self
                    .frontend
                    .flag_bool(&canonical_refs, "non-interactive")?
                    .unwrap_or(false);
                Ok(BuiltCommand::New(NewCommand::new(
                    NewSubcommand::Spec(NewSpecFlags {
                        interview,
                        non_interactive,
                    }),
                    self.engines.clone(),
                    session.clone(),
                )))
            }
            ["new", "workflow"] => {
                let interview = self
                    .frontend
                    .flag_bool(&canonical_refs, "interview")?
                    .unwrap_or(false);
                let non_interactive = self
                    .frontend
                    .flag_bool(&canonical_refs, "non-interactive")?
                    .unwrap_or(false);
                let global = self
                    .frontend
                    .flag_bool(&canonical_refs, "global")?
                    .unwrap_or(false);
                let format = self
                    .frontend
                    .flag_enum(&canonical_refs, "format")?
                    .unwrap_or_else(|| "toml".to_string());
                Ok(BuiltCommand::New(NewCommand::new(
                    NewSubcommand::Workflow(NewWorkflowFlags {
                        interview,
                        non_interactive,
                        global,
                        format,
                    }),
                    self.engines.clone(),
                    session.clone(),
                )))
            }
            ["new", "skill"] => {
                let interview = self
                    .frontend
                    .flag_bool(&canonical_refs, "interview")?
                    .unwrap_or(false);
                let non_interactive = self
                    .frontend
                    .flag_bool(&canonical_refs, "non-interactive")?
                    .unwrap_or(false);
                let global = self
                    .frontend
                    .flag_bool(&canonical_refs, "global")?
                    .unwrap_or(false);
                Ok(BuiltCommand::New(NewCommand::new(
                    NewSubcommand::Skill(NewSkillFlags {
                        interview,
                        non_interactive,
                        global,
                    }),
                    self.engines.clone(),
                    session.clone(),
                )))
            }
            _ => Err(CommandError::unknown_command(&canonical_refs)),
        }
    }

    /// Tokenize a raw TUI command-box string into typed
    /// [`ParsedCommandBoxInput`]. All command-string interpretation lives
    /// here, never in the TUI.
    pub fn parse_command_box_input(raw: &str) -> Result<ParsedCommandBoxInput, CommandError> {
        parsed_input::parse(raw, CommandCatalogue::get())
    }
}

impl<F: DispatchFrontend> Dispatch<F> {
    /// Build the requested command and drive it to completion, moving the
    /// owned frontend into the matching `Box<dyn *CommandFrontend>`.
    pub async fn run_command(self, path: &[&str]) -> Result<CommandOutcome, CommandError> {
        let built = self.build_command(path)?;
        let frontend = self.frontend;
        match built {
            BuiltCommand::Init(cmd) => {
                let boxed: Box<dyn InitCommandFrontend> = Box::new(frontend);
                cmd.run_with_frontend(boxed).await.map(CommandOutcome::Init)
            }
            BuiltCommand::Ready(cmd) => {
                let boxed: Box<dyn ReadyCommandFrontend> = Box::new(frontend);
                cmd.run_with_frontend(boxed)
                    .await
                    .map(CommandOutcome::Ready)
            }
            BuiltCommand::Chat(cmd) => {
                let boxed: Box<dyn ChatCommandFrontend> = Box::new(frontend);
                cmd.run_with_frontend(boxed).await.map(CommandOutcome::Chat)
            }
            BuiltCommand::Specs(cmd) => {
                let boxed: Box<dyn SpecsCommandFrontend> = Box::new(frontend);
                cmd.run_with_frontend(boxed)
                    .await
                    .map(CommandOutcome::Specs)
            }
            BuiltCommand::Status(cmd) => {
                let boxed: Box<dyn StatusCommandFrontend> = Box::new(frontend);
                cmd.run_with_frontend(boxed)
                    .await
                    .map(CommandOutcome::Status)
            }
            BuiltCommand::Config(cmd) => {
                let boxed: Box<dyn ConfigCommandFrontend> = Box::new(frontend);
                cmd.run_with_frontend(boxed)
                    .await
                    .map(CommandOutcome::Config)
            }
            BuiltCommand::ExecPrompt(cmd) => {
                let boxed: Box<dyn ExecPromptCommandFrontend> = Box::new(frontend);
                cmd.run_with_frontend(boxed)
                    .await
                    .map(CommandOutcome::ExecPrompt)
            }
            BuiltCommand::ExecWorkflow(cmd) => {
                let boxed: Box<dyn ExecWorkflowCommandFrontend> = Box::new(frontend);
                cmd.run_with_frontend(boxed)
                    .await
                    .map(CommandOutcome::ExecWorkflow)
            }
            BuiltCommand::ApiServer(cmd) => {
                let boxed: Box<dyn ApiServerCommandFrontend> = Box::new(frontend);
                cmd.run_with_frontend(boxed)
                    .await
                    .map(CommandOutcome::ApiServer)
            }
            BuiltCommand::Remote(cmd) => {
                let boxed: Box<dyn RemoteCommandFrontend> = Box::new(frontend);
                cmd.run_with_frontend(boxed)
                    .await
                    .map(CommandOutcome::Remote)
            }
            BuiltCommand::New(cmd) => {
                let boxed: Box<dyn NewCommandFrontend> = Box::new(frontend);
                cmd.run_with_frontend(boxed).await.map(CommandOutcome::New)
            }
            BuiltCommand::Auth(cmd) => {
                let boxed: Box<dyn AuthCommandFrontend> = Box::new(frontend);
                cmd.run_with_frontend(boxed).await.map(CommandOutcome::Auth)
            }
            BuiltCommand::Download(cmd) => {
                let boxed: Box<dyn DownloadCommandFrontend> = Box::new(frontend);
                cmd.run_with_frontend(boxed)
                    .await
                    .map(CommandOutcome::Download)
            }
        }
    }
}

/// Run validation pass: any pair of flags both set must not be in each other's
/// `conflicts_with` list.
fn validate_conflicts<F: CommandFrontend>(
    frontend: &F,
    command_path: &[&str],
    flags: &'static [FlagSpec],
) -> Result<(), CommandError> {
    let mut active: Vec<&str> = Vec::new();
    for f in flags {
        let is_set = match f.kind {
            FlagKind::Bool => frontend.flag_bool(command_path, f.long)?.unwrap_or(false),
            FlagKind::String | FlagKind::OptionalString => {
                frontend.flag_string(command_path, f.long)?.is_some()
            }
            FlagKind::Enum(_) => frontend.flag_enum(command_path, f.long)?.is_some(),
            FlagKind::Path | FlagKind::OptionalPath => {
                frontend.flag_path(command_path, f.long)?.is_some()
            }
            FlagKind::VecString => !frontend.flag_strings(command_path, f.long)?.is_empty(),
            FlagKind::U16 => frontend.flag_u16(command_path, f.long)?.is_some(),
        };
        if is_set {
            active.push(f.long);
        }
    }
    for f in flags {
        if !active.contains(&f.long) {
            continue;
        }
        for c in f.conflicts_with {
            if active.contains(c) {
                return Err(CommandError::mutually_exclusive(command_path, f.long, *c));
            }
        }
    }
    Ok(())
}

// ─── Per-command flag readers ───────────────────────────────────────────────

fn read_ready_flags<F: CommandFrontend>(
    f: &F,
    p: &[&str],
) -> Result<ReadyCommandFlags, CommandError> {
    Ok(ReadyCommandFlags {
        refresh: f.flag_bool(p, "refresh")?.unwrap_or(false),
        build: f.flag_bool(p, "build")?.unwrap_or(false),
        no_cache: f.flag_bool(p, "no-cache")?.unwrap_or(false),
        non_interactive: f.flag_bool(p, "non-interactive")?.unwrap_or(false),
        allow_docker: f.flag_bool(p, "allow-docker")?.unwrap_or(false),
        json: f.flag_bool(p, "json")?.unwrap_or(false),
    })
}

fn read_chat_flags<F: CommandFrontend>(
    f: &F,
    p: &[&str],
) -> Result<ChatCommandFlags, CommandError> {
    Ok(ChatCommandFlags {
        non_interactive: f.flag_bool(p, "non-interactive")?.unwrap_or(false),
        plan: f.flag_bool(p, "plan")?.unwrap_or(false),
        allow_docker: f.flag_bool(p, "allow-docker")?.unwrap_or(false),
        yolo: f.flag_bool(p, "yolo")?.unwrap_or(false),
        auto: f.flag_bool(p, "auto")?.unwrap_or(false),
        agent: f.flag_string(p, "agent")?,
        model: f.flag_string(p, "model")?,
        overlay: f.flag_strings(p, "overlay")?,
    })
}

fn read_exec_prompt_flags<F: CommandFrontend>(
    f: &F,
    p: &[&str],
    prompt: String,
) -> Result<ExecPromptCommandFlags, CommandError> {
    Ok(ExecPromptCommandFlags {
        prompt,
        non_interactive: f.flag_bool(p, "non-interactive")?.unwrap_or(false),
        plan: f.flag_bool(p, "plan")?.unwrap_or(false),
        allow_docker: f.flag_bool(p, "allow-docker")?.unwrap_or(false),
        yolo: f.flag_bool(p, "yolo")?.unwrap_or(false),
        auto: f.flag_bool(p, "auto")?.unwrap_or(false),
        agent: f.flag_string(p, "agent")?,
        model: f.flag_string(p, "model")?,
        overlay: f.flag_strings(p, "overlay")?,
    })
}

fn read_exec_workflow_flags<F: CommandFrontend>(
    f: &F,
    p: &[&str],
) -> Result<ExecWorkflowCommandFlags, CommandError> {
    let workflow = f
        .flag_path(p, "workflow")?
        .or_else(|| f.argument(p, "workflow").ok().flatten().map(PathBuf::from))
        .ok_or_else(|| CommandError::missing_required_argument(p, "workflow"))?;
    let yolo = f.flag_bool(p, "yolo")?.unwrap_or(false);
    let worktree = f.flag_bool(p, "worktree")?.unwrap_or(false) || yolo;
    Ok(ExecWorkflowCommandFlags {
        workflow,
        work_item: f.flag_string(p, "work-item")?,
        non_interactive: f.flag_bool(p, "non-interactive")?.unwrap_or(false),
        plan: f.flag_bool(p, "plan")?.unwrap_or(false),
        allow_docker: f.flag_bool(p, "allow-docker")?.unwrap_or(false),
        worktree,
        yolo,
        auto: f.flag_bool(p, "auto")?.unwrap_or(false),
        agent: f.flag_string(p, "agent")?,
        model: f.flag_string(p, "model")?,
        overlay: f.flag_strings(p, "overlay")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Recording frontend used by Dispatch unit tests.
    pub(super) struct FakeCommandFrontend {
        pub bools: std::collections::HashMap<String, bool>,
        pub strings: std::collections::HashMap<String, String>,
        pub strings_vec: std::collections::HashMap<String, Vec<String>>,
        pub paths: std::collections::HashMap<String, PathBuf>,
        pub enums: std::collections::HashMap<String, String>,
        pub u16s: std::collections::HashMap<String, u16>,
        pub args: std::collections::HashMap<String, String>,
        pub args_vec: std::collections::HashMap<String, Vec<String>>,
    }

    impl FakeCommandFrontend {
        pub fn new() -> Self {
            Self {
                bools: Default::default(),
                strings: Default::default(),
                strings_vec: Default::default(),
                paths: Default::default(),
                enums: Default::default(),
                u16s: Default::default(),
                args: Default::default(),
                args_vec: Default::default(),
            }
        }
    }

    impl crate::engine::message::UserMessageSink for FakeCommandFrontend {
        fn write_message(&mut self, _msg: crate::engine::message::UserMessage) {}
        fn replay_queued(&mut self) {}
    }

    impl CommandFrontend for FakeCommandFrontend {
        fn flag_bool(&self, _p: &[&str], flag: &str) -> Result<Option<bool>, CommandError> {
            Ok(self.bools.get(flag).copied())
        }
        fn flag_string(&self, _p: &[&str], flag: &str) -> Result<Option<String>, CommandError> {
            Ok(self.strings.get(flag).cloned())
        }
        fn flag_strings(&self, _p: &[&str], flag: &str) -> Result<Vec<String>, CommandError> {
            Ok(self.strings_vec.get(flag).cloned().unwrap_or_default())
        }
        fn flag_path(&self, _p: &[&str], flag: &str) -> Result<Option<PathBuf>, CommandError> {
            Ok(self.paths.get(flag).cloned())
        }
        fn flag_enum(&self, _p: &[&str], flag: &str) -> Result<Option<String>, CommandError> {
            Ok(self.enums.get(flag).cloned())
        }
        fn flag_u16(&self, _p: &[&str], flag: &str) -> Result<Option<u16>, CommandError> {
            Ok(self.u16s.get(flag).copied())
        }
        fn argument(&self, _p: &[&str], name: &str) -> Result<Option<String>, CommandError> {
            Ok(self.args.get(name).cloned())
        }
        fn arguments(&self, _p: &[&str], name: &str) -> Result<Vec<String>, CommandError> {
            Ok(self.args_vec.get(name).cloned().unwrap_or_default())
        }
    }

    fn make_engines() -> Engines {
        let runtime = Arc::new(crate::engine::container::ContainerRuntime::docker());
        let overlay = Arc::new(crate::engine::overlay::OverlayEngine::with_auth_resolver(
            crate::data::fs::auth_paths::AuthPathResolver::at_home(std::path::PathBuf::from(
                "/tmp",
            )),
        ));
        let git_engine = Arc::new(crate::engine::git::GitEngine::new());
        let agent_engine = Arc::new(crate::engine::agent::AgentEngine::new(
            overlay.clone(),
            runtime.clone(),
        ));
        let auth_engine = Arc::new(crate::engine::auth::AuthEngine::with_paths(
            crate::data::fs::auth_paths::AuthPathResolver::at_home("/tmp"),
            crate::data::fs::api_paths::ApiPaths::at_root("/tmp"),
        ));
        let workflow_state_store = {
            let tmp = tempfile::tempdir().unwrap();
            Arc::new(crate::data::EngineWorkflowStateStore::at_git_root(
                tmp.path(),
            ))
        };
        Engines {
            runtime,
            git_engine,
            overlay_engine: overlay,
            auth_engine,
            agent_engine,
            workflow_state_store,
        }
    }

    fn make_session() -> Arc<RwLock<Session>> {
        let tmp = tempfile::tempdir().unwrap();
        let resolver = crate::data::session::StaticGitRootResolver::new(tmp.path());
        let s = Session::open(
            tmp.path().to_path_buf(),
            &resolver,
            crate::data::session::SessionOpenOptions::default(),
        )
        .unwrap();
        Arc::new(RwLock::new(s))
    }

    #[test]
    fn build_status_command_with_no_flags() {
        let dispatch = Dispatch::new(FakeCommandFrontend::new(), make_session(), make_engines());
        let built = dispatch.build_command(&["status"]).unwrap();
        match built {
            BuiltCommand::Status(_) => {}
            _ => panic!("expected Status"),
        }
    }

    #[test]
    fn build_unknown_command_returns_unknown_command_error() {
        let dispatch = Dispatch::new(FakeCommandFrontend::new(), make_session(), make_engines());
        let result = dispatch.build_command(&["bogus"]);
        match result {
            Err(CommandError::UnknownCommand { .. }) => {}
            Err(other) => panic!("expected UnknownCommand, got {other:?}"),
            Ok(_) => panic!("expected error"),
        }
    }

    #[test]
    fn build_specs_amend_missing_argument_errors() {
        let dispatch = Dispatch::new(FakeCommandFrontend::new(), make_session(), make_engines());
        let result = dispatch.build_command(&["specs", "amend"]);
        match result {
            Err(CommandError::MissingRequiredArgument { .. }) => {}
            Err(other) => panic!("expected MissingRequiredArgument, got {other:?}"),
            Ok(_) => panic!("expected error"),
        }
    }

    #[test]
    fn build_chat_with_yolo_and_plan_returns_mutually_exclusive() {
        let mut frontend = FakeCommandFrontend::new();
        frontend.bools.insert("yolo".into(), true);
        frontend.bools.insert("plan".into(), true);
        let dispatch = Dispatch::new(frontend, make_session(), make_engines());
        let result = dispatch.build_command(&["chat"]);
        match result {
            Err(CommandError::MutuallyExclusive { .. }) => {}
            Err(other) => panic!("expected MutuallyExclusive, got {other:?}"),
            Ok(_) => panic!("expected error"),
        }
    }

    #[test]
    fn ready_json_implies_non_interactive_in_built_command() {
        let mut frontend = FakeCommandFrontend::new();
        frontend.bools.insert("json".into(), true);
        let dispatch = Dispatch::new(frontend, make_session(), make_engines());
        let built = dispatch.build_command(&["ready"]).unwrap();
        match built {
            BuiltCommand::Ready(cmd) => {
                assert!(
                    cmd.flags().non_interactive,
                    "json should imply non_interactive"
                );
            }
            _ => panic!("expected Ready"),
        }
    }

    #[test]
    fn exec_workflow_yolo_implies_worktree_in_built_command() {
        let mut frontend = FakeCommandFrontend::new();
        frontend.bools.insert("yolo".into(), true);
        frontend
            .paths
            .insert("workflow".into(), std::path::PathBuf::from("/tmp/wf.toml"));
        let dispatch = Dispatch::new(frontend, make_session(), make_engines());
        let built = dispatch.build_command(&["exec", "workflow"]).unwrap();
        match built {
            BuiltCommand::ExecWorkflow(cmd) => {
                assert!(
                    cmd.flags().worktree,
                    "yolo should imply worktree on exec workflow"
                );
            }
            _ => panic!("expected ExecWorkflow"),
        }
    }

    #[test]
    fn exec_workflow_auto_implies_worktree_in_built_command() {
        let mut frontend = FakeCommandFrontend::new();
        frontend.bools.insert("auto".into(), true);
        frontend
            .paths
            .insert("workflow".into(), std::path::PathBuf::from("/tmp/wf.toml"));
        let dispatch = Dispatch::new(frontend, make_session(), make_engines());
        let built = dispatch.build_command(&["exec", "workflow"]).unwrap();
        match built {
            BuiltCommand::ExecWorkflow(cmd) => {
                assert!(
                    cmd.flags().worktree,
                    "auto should imply worktree on exec workflow"
                );
                assert!(cmd.flags().auto);
            }
            _ => panic!("expected ExecWorkflow"),
        }
    }

    #[test]
    fn build_config_show_succeeds_with_no_args() {
        let dispatch = Dispatch::new(FakeCommandFrontend::new(), make_session(), make_engines());
        let built = dispatch.build_command(&["config", "show"]).unwrap();
        assert!(matches!(built, BuiltCommand::Config(_)));
    }

    #[test]
    fn build_config_get_with_field_argument() {
        let mut frontend = FakeCommandFrontend::new();
        frontend
            .args
            .insert("field".into(), "terminal_scrollback_lines".into());
        let dispatch = Dispatch::new(frontend, make_session(), make_engines());
        let built = dispatch.build_command(&["config", "get"]).unwrap();
        assert!(matches!(built, BuiltCommand::Config(_)));
    }

    #[test]
    fn build_config_get_missing_field_returns_missing_required_argument() {
        let dispatch = Dispatch::new(FakeCommandFrontend::new(), make_session(), make_engines());
        let result = dispatch.build_command(&["config", "get"]);
        assert!(
            matches!(result, Err(CommandError::MissingRequiredArgument { .. })),
            "missing field must return MissingRequiredArgument"
        );
    }

    #[test]
    fn build_new_workflow_with_format_flag() {
        let mut frontend = FakeCommandFrontend::new();
        frontend.enums.insert("format".into(), "yaml".into());
        let dispatch = Dispatch::new(frontend, make_session(), make_engines());
        let built = dispatch.build_command(&["new", "workflow"]).unwrap();
        assert!(matches!(built, BuiltCommand::New(_)));
    }

    #[test]
    fn build_api_start_with_port() {
        let mut frontend = FakeCommandFrontend::new();
        frontend.u16s.insert("port".into(), 1234);
        let dispatch = Dispatch::new(frontend, make_session(), make_engines());
        let built = dispatch.build_command(&["api", "start"]).unwrap();
        assert!(matches!(built, BuiltCommand::ApiServer(_)));
    }

    #[test]
    fn build_chat_default_flags_all_false() {
        let dispatch = Dispatch::new(FakeCommandFrontend::new(), make_session(), make_engines());
        let built = dispatch.build_command(&["chat"]).unwrap();
        match built {
            BuiltCommand::Chat(cmd) => {
                let f = cmd.flags();
                assert!(!f.yolo && !f.plan && !f.non_interactive && !f.allow_docker);
            }
            _ => panic!("expected Chat"),
        }
    }

    #[test]
    fn build_remote_exec_workflow_with_workflow_argument() {
        let mut frontend = FakeCommandFrontend::new();
        frontend
            .paths
            .insert("workflow".into(), std::path::PathBuf::from("/tmp/wf.toml"));
        let dispatch = Dispatch::new(frontend, make_session(), make_engines());
        let built = dispatch
            .build_command(&["remote", "exec", "workflow"])
            .unwrap();
        assert!(matches!(built, BuiltCommand::Remote(_)));
    }

    #[test]
    fn build_remote_exec_prompt_with_prompt_argument() {
        let mut frontend = FakeCommandFrontend::new();
        frontend.args.insert("prompt".into(), "hello".into());
        let dispatch = Dispatch::new(frontend, make_session(), make_engines());
        let built = dispatch
            .build_command(&["remote", "exec", "prompt"])
            .unwrap();
        assert!(matches!(built, BuiltCommand::Remote(_)));
    }

    #[test]
    fn build_exec_prompt_with_prompt_argument() {
        let mut frontend = FakeCommandFrontend::new();
        frontend.args.insert("prompt".into(), "do something".into());
        let dispatch = Dispatch::new(frontend, make_session(), make_engines());
        let built = dispatch.build_command(&["exec", "prompt"]).unwrap();
        assert!(matches!(built, BuiltCommand::ExecPrompt(_)));
    }

    #[test]
    fn build_exec_prompt_with_empty_prompt_returns_invalid_argument_value() {
        let mut frontend = FakeCommandFrontend::new();
        frontend.args.insert("prompt".into(), "   ".into());
        let dispatch = Dispatch::new(frontend, make_session(), make_engines());
        let result = dispatch.build_command(&["exec", "prompt"]);
        assert!(
            matches!(result, Err(CommandError::InvalidArgumentValue { .. })),
            "empty prompt must return InvalidArgumentValue"
        );
    }

    #[test]
    fn build_exec_workflow_missing_workflow_argument_returns_missing_required_argument() {
        // workflow is required and neither flag nor positional arg is set
        let dispatch = Dispatch::new(FakeCommandFrontend::new(), make_session(), make_engines());
        let result = dispatch.build_command(&["exec", "workflow"]);
        assert!(
            matches!(result, Err(CommandError::MissingRequiredArgument { .. })),
            "missing workflow must return MissingRequiredArgument"
        );
    }

    #[test]
    fn alias_wf_resolves_to_exec_workflow() {
        let mut frontend = FakeCommandFrontend::new();
        frontend
            .paths
            .insert("workflow".into(), std::path::PathBuf::from("/tmp/wf.toml"));
        let dispatch = Dispatch::new(frontend, make_session(), make_engines());
        // "wf" is a string alias under "exec"; dispatch should resolve it.
        let built = dispatch.build_command(&["exec", "wf"]).unwrap();
        assert!(
            matches!(built, BuiltCommand::ExecWorkflow(_)),
            "exec wf must dispatch to ExecWorkflow"
        );
    }

    // ─── parse_command_box_input ──────────────────────────────────────────────

    #[test]
    fn parse_command_box_input_exec_workflow_with_yolo() {
        let parsed = Dispatch::<FakeCommandFrontend>::parse_command_box_input(
            "exec workflow my-workflow.toml --yolo",
        )
        .unwrap();
        assert_eq!(parsed.path, vec!["exec", "workflow"]);
        assert!(matches!(
            parsed.flags.get("yolo"),
            Some(parsed_input::FlagValue::Bool(true))
        ));
        match parsed.arguments.get("workflow") {
            Some(parsed_input::ArgValue::Single(s)) => {
                assert_eq!(s, "my-workflow.toml");
            }
            other => panic!("expected Single workflow argument, got: {other:?}"),
        }
    }

    #[test]
    fn parse_command_box_input_rejects_unknown_top_level_command() {
        let result = Dispatch::<FakeCommandFrontend>::parse_command_box_input("not-a-command");
        assert!(
            matches!(result, Err(CommandError::UnknownCommand { .. })),
            "unknown command must return UnknownCommand, got: {result:?}"
        );
    }

    #[test]
    fn parse_command_box_input_rejects_unknown_flag() {
        let result = Dispatch::<FakeCommandFrontend>::parse_command_box_input("status --bogus");
        assert!(
            matches!(result, Err(CommandError::UnknownFlag { .. })),
            "unknown flag must return UnknownFlag, got: {result:?}"
        );
    }

    #[test]
    fn parse_command_box_input_remote_exec_workflow() {
        let parsed = Dispatch::<FakeCommandFrontend>::parse_command_box_input(
            "remote exec workflow my-workflow.toml --follow",
        )
        .unwrap();
        assert_eq!(parsed.path, vec!["remote", "exec", "workflow"]);
        match parsed.arguments.get("workflow") {
            Some(parsed_input::ArgValue::Single(s)) => {
                assert_eq!(s, "my-workflow.toml");
            }
            other => panic!("expected Single workflow argument, got: {other:?}"),
        }
        assert!(matches!(
            parsed.flags.get("follow"),
            Some(parsed_input::FlagValue::Bool(true))
        ));
    }

    #[test]
    fn parse_command_box_input_short_flag_non_interactive() {
        let parsed = Dispatch::<FakeCommandFrontend>::parse_command_box_input("ready -n").unwrap();
        assert_eq!(parsed.path, vec!["ready"]);
        assert!(matches!(
            parsed.flags.get("non-interactive"),
            Some(parsed_input::FlagValue::Bool(true))
        ));
    }

    #[test]
    fn exec_workflow_no_yolo_no_auto_worktree_false() {
        let mut frontend = FakeCommandFrontend::new();
        frontend
            .paths
            .insert("workflow".into(), std::path::PathBuf::from("/tmp/wf.toml"));
        // Neither yolo nor auto is set; worktree must not be implied.
        let dispatch = Dispatch::new(frontend, make_session(), make_engines());
        let built = dispatch.build_command(&["exec", "workflow"]).unwrap();
        match built {
            BuiltCommand::ExecWorkflow(cmd) => {
                assert!(
                    !cmd.flags().worktree,
                    "worktree must be false when neither yolo nor auto is set"
                );
                assert!(!cmd.flags().yolo);
                assert!(!cmd.flags().auto);
            }
            _ => panic!("expected ExecWorkflow"),
        }
    }

    #[test]
    fn exec_workflow_yolo_plus_explicit_worktree_true_stays_true() {
        let mut frontend = FakeCommandFrontend::new();
        frontend.bools.insert("yolo".into(), true);
        frontend.bools.insert("worktree".into(), true);
        frontend
            .paths
            .insert("workflow".into(), std::path::PathBuf::from("/tmp/wf.toml"));
        let dispatch = Dispatch::new(frontend, make_session(), make_engines());
        let built = dispatch.build_command(&["exec", "workflow"]).unwrap();
        match built {
            BuiltCommand::ExecWorkflow(cmd) => {
                assert!(cmd.flags().yolo);
                assert!(
                    cmd.flags().worktree,
                    "worktree must be true when both yolo and --worktree are set"
                );
            }
            _ => panic!("expected ExecWorkflow"),
        }
    }
}
