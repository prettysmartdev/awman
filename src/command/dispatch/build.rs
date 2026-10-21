//! The catalogue's command constructors.
//!
//! [`CommandSpec::build`](crate::command::dispatch::catalogue::CommandSpec)
//! holds one of these function pointers, which is the whole of
//! `Dispatch::build_command`'s per-command knowledge after WI 0113 F-10: look
//! the spec up, hand its `build` a [`BuildContext`], and return what it
//! constructs. Each function is a one-line adaptor over the command's own
//! `from_input` constructor, so the construction logic itself lives with the
//! command in `src/command/commands/`, never here.
//!
//! A command with several subcommands (`agents`, `config`, `api`, `squad`, `new`,
//! `remote`, `specs`) registers the *same* function on every leaf: its
//! `from_input` selects the subcommand from
//! [`CallerContext::leaf`](crate::command::dispatch::CallerContext::leaf).

use crate::command::commands::agents::AgentsCommand;
use crate::command::commands::api_server::ApiServerCommand;
use crate::command::commands::chat::ChatCommand;
use crate::command::commands::clean::CleanCommand;
use crate::command::commands::config::ConfigCommand;
use crate::command::commands::exec_prompt::ExecPromptCommand;
use crate::command::commands::exec_workflow::ExecWorkflowCommand;
use crate::command::commands::init::InitCommand;
use crate::command::commands::new::NewCommand;
use crate::command::commands::ready::ReadyCommand;
use crate::command::commands::remote::RemoteCommand;
use crate::command::commands::specs::SpecsCommand;
use crate::command::commands::squad::attach::SquadAttachCommand;
use crate::command::commands::squad::commands::SquadCommand;
use crate::command::dispatch::{BuildContext, BuiltCommand};
use crate::command::error::CommandError;

/// The signature every `CommandSpec::build` holds.
pub type CommandBuilder = fn(&BuildContext) -> Result<BuiltCommand, CommandError>;

/// The builder for a spec that is not itself runnable: the catalogue root, a
/// grouping parent such as `exec` or `config`.
///
/// `projections::parity_test::every_runnable_command_builds_and_every_grouping_spec_refuses`
/// pins which specs may legitimately use it, so a new leaf cannot quietly
/// arrive without a constructor — the omission that hid `squad attach` from
/// `Dispatch` (WI 0113 F-01).
pub fn unsupported(ctx: &BuildContext) -> Result<BuiltCommand, CommandError> {
    Err(CommandError::unknown_command(&ctx.path()))
}

pub fn init(ctx: &BuildContext) -> Result<BuiltCommand, CommandError> {
    Ok(BuiltCommand::Init(InitCommand::from_input(ctx)?))
}

pub fn ready(ctx: &BuildContext) -> Result<BuiltCommand, CommandError> {
    Ok(BuiltCommand::Ready(ReadyCommand::from_input(ctx)?))
}

pub fn chat(ctx: &BuildContext) -> Result<BuiltCommand, CommandError> {
    Ok(BuiltCommand::Chat(ChatCommand::from_input(ctx)?))
}

pub fn specs(ctx: &BuildContext) -> Result<BuiltCommand, CommandError> {
    Ok(BuiltCommand::Specs(SpecsCommand::from_input(ctx)?))
}

pub fn agents(ctx: &BuildContext) -> Result<BuiltCommand, CommandError> {
    Ok(BuiltCommand::Agents(AgentsCommand::from_input(ctx)?))
}

pub fn config(ctx: &BuildContext) -> Result<BuiltCommand, CommandError> {
    Ok(BuiltCommand::Config(ConfigCommand::from_input(ctx)?))
}

pub fn exec_prompt(ctx: &BuildContext) -> Result<BuiltCommand, CommandError> {
    Ok(BuiltCommand::ExecPrompt(ExecPromptCommand::from_input(
        ctx,
    )?))
}

pub fn exec_workflow(ctx: &BuildContext) -> Result<BuiltCommand, CommandError> {
    Ok(BuiltCommand::ExecWorkflow(ExecWorkflowCommand::from_input(
        ctx,
    )?))
}

pub fn api_server(ctx: &BuildContext) -> Result<BuiltCommand, CommandError> {
    Ok(BuiltCommand::ApiServer(ApiServerCommand::from_input(ctx)?))
}

pub fn squad(ctx: &BuildContext) -> Result<BuiltCommand, CommandError> {
    Ok(BuiltCommand::Squad(SquadCommand::from_input(ctx)?))
}

pub fn squad_attach(ctx: &BuildContext) -> Result<BuiltCommand, CommandError> {
    Ok(BuiltCommand::SquadAttach(SquadAttachCommand::from_input(
        ctx,
    )?))
}

pub fn remote(ctx: &BuildContext) -> Result<BuiltCommand, CommandError> {
    Ok(BuiltCommand::Remote(RemoteCommand::from_input(ctx)?))
}

pub fn new(ctx: &BuildContext) -> Result<BuiltCommand, CommandError> {
    Ok(BuiltCommand::New(NewCommand::from_input(ctx)?))
}

pub fn clean(ctx: &BuildContext) -> Result<BuiltCommand, CommandError> {
    Ok(BuiltCommand::Clean(CleanCommand::from_input(ctx)?))
}
