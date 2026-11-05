//! Catalogue input adapter and flags for `chat`.

use crate::command::dispatch::catalogue::SessionWorktreeTarget;
use crate::command::dispatch::BuildContext;
use crate::command::error::CommandError;
use crate::data::config::repo::LaunchMode;

#[derive(Debug, Clone)]
pub struct ChatCommandFlags {
    pub non_interactive: bool,
    pub plan: bool,
    pub allow_docker: bool,
    pub yolo: bool,
    pub auto: bool,
    pub agent: Option<String>,
    pub model: Option<String>,
    pub launch_mode: Option<LaunchMode>,
    pub overlay: Vec<String>,
    /// `--agent-flag` values; `None` lets config `agentFlags` apply.
    pub agent_flags: Option<Vec<String>>,
    pub session_worktree: Option<SessionWorktreeTarget>,
}

impl super::ChatCommand {
    /// Construct from the catalogue-resolved input (WI 0113 F-10).
    pub fn from_input(ctx: &BuildContext) -> Result<Self, CommandError> {
        Ok(Self::new(
            ChatCommandFlags {
                non_interactive: ctx.flags.bool("non-interactive"),
                plan: ctx.flags.bool("plan"),
                allow_docker: ctx.flags.bool("allow-docker"),
                yolo: ctx.flags.bool("yolo"),
                auto: ctx.flags.bool("auto"),
                agent: ctx.flags.string("agent"),
                model: ctx.flags.string("model"),
                launch_mode: crate::command::dispatch::parse_launch_mode(
                    ctx.flags.string("launch-mode"),
                    &ctx.path(),
                )?,
                overlay: ctx.flags.strs("overlay").to_vec(),
                agent_flags: crate::command::dispatch::parse_agent_flags(
                    ctx.flags.strs("agent-flag"),
                    &ctx.path(),
                )?,
                session_worktree: parse_session_worktree(
                    ctx.flags.string("branch"),
                    ctx.flags.string("pr"),
                    ctx.flags.string("worktree"),
                ),
            },
            ctx.engines.clone(),
            ctx.session.clone(),
        ))
    }
}

fn parse_session_worktree(
    branch: Option<String>,
    pull_request: Option<String>,
    existing: Option<String>,
) -> Option<SessionWorktreeTarget> {
    branch
        .map(SessionWorktreeTarget::Branch)
        .or_else(|| pull_request.map(SessionWorktreeTarget::PullRequest))
        .or_else(|| existing.map(SessionWorktreeTarget::Existing))
}
