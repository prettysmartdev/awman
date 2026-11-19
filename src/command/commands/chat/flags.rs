//! Catalogue input adapter and flags for `chat`.

use crate::command::dispatch::catalogue::SessionWorktreeTarget;
use crate::command::dispatch::BuildContext;
use crate::command::error::CommandError;
use crate::data::config::agents::LaunchMode;

#[derive(Debug, Clone)]
pub struct ChatCommandFlags {
    pub non_interactive: bool,
    pub plan: bool,
    pub allow_docker: bool,
    pub yolo: bool,
    pub yolo_authorized: bool,
    pub auto: bool,
    /// `--auto` was supplied by the caller, not a frontend default.
    pub auto_explicit: bool,
    pub agent: Option<String>,
    pub model: Option<String>,
    pub launch_mode: Option<LaunchMode>,
    pub overlay: Vec<String>,
    /// `--agent-flag` values; `None` lets config `agentFlags` apply.
    pub agent_flags: Option<Vec<String>>,
    pub session_worktree: Option<SessionWorktreeTarget>,
    /// `--automation[=auto|require_full]` (WI 0070); `None` when omitted.
    pub automation: Option<crate::data::automation::AutomationMode>,
}

impl super::ChatCommand {
    /// Construct from the catalogue-resolved input (WI 0113 F-10).
    pub fn from_input(ctx: &BuildContext) -> Result<Self, CommandError> {
        Ok(Self::new(
            ChatCommandFlags {
                non_interactive: ctx.flags.bool("headless"),
                plan: ctx.flags.bool("plan"),
                allow_docker: ctx.flags.bool("allow-docker"),
                yolo: ctx.flags.bool("yolo"),
                yolo_authorized: ctx.flags.explicit("yolo"),
                auto: ctx.flags.bool("auto"),
                auto_explicit: ctx.flags.explicit("auto"),
                agent: ctx.flags.string("agent"),
                model: ctx.flags.string("model"),
                launch_mode: None, // `--set agents.launchMode=…` (WI 0139)
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
                automation: ctx
                    .flags
                    .r#enum(crate::data::automation::AUTOMATION_FLAG)
                    .and_then(crate::data::automation::AutomationMode::parse),
            },
            ctx.engines.clone(),
            ctx.session.clone(),
        )
        .with_cancellation(ctx.cancel.clone())
        .with_profile(crate::command::commands::launch_policy::settings_profile::ProfileRequest::from_context(ctx)?)
        .with_workspace_access(crate::data::workspace::WorkspaceAccess::from_read_only(
            ctx.flags
                .bool(crate::data::workspace::READ_ONLY_WORKSPACE_FLAG),
        )))
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
