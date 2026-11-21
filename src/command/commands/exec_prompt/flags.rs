//! Catalogue input adapter and flags for `exec prompt`.

use crate::command::dispatch::catalogue::SessionWorktreeTarget;
use crate::command::dispatch::BuildContext;
use crate::command::error::CommandError;

#[derive(Debug, Clone)]
pub struct ExecPromptCommandFlags {
    pub prompt: Option<String>,
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
    pub launch_mode: Option<crate::data::config::agents::LaunchMode>,
    pub overlay: Vec<String>,
    /// `--agent-flag` values; `None` lets config `agentFlags` apply.
    pub agent_flags: Option<Vec<String>>,
    /// Raw `--work-item` reference, resolved at run time (WI 0147).
    pub work_item: Option<String>,
    pub session_worktree: Option<SessionWorktreeTarget>,
    /// `--automation[=auto|require_full]` (WI 0070); `None` when omitted.
    pub automation: Option<crate::data::automation::AutomationMode>,
}

impl super::ExecPromptCommand {
    /// Construct from the catalogue-resolved input (WI 0113 F-10). A
    /// whitespace-only positional prompt is normalised to `None`; the
    /// prompt-or-work-item requirement is checked at run time.
    pub fn from_input(ctx: &BuildContext) -> Result<Self, CommandError> {
        // Only `--prerequisite-identity` exists on a prompt. An API client may only
        // narrow what the server grants, exactly as for `exec workflow`.
        use crate::command::commands::workflow_prerequisites as prerequisites_flow;
        let mut prerequisites = prerequisites_flow::parse_identity_only(ctx)?;
        let ceiling = prerequisites_flow::remote_ceiling(&ctx.session);
        prerequisites_flow::check_flag_widening(ctx.caller.frontend(), &prerequisites, &ceiling)?;
        prerequisites_flow::apply_identity_ceiling(
            ctx.caller.frontend(),
            &mut prerequisites,
            &ceiling,
        );
        let prompt = match ctx.args.get("prompt") {
            Some(prompt) if prompt.trim().is_empty() => None,
            other => other.map(str::to_string),
        };
        if ctx.session.inline_work_item().is_none() {
            require_task(prompt.as_deref(), ctx.flags.str("work-item"), &ctx.path())?;
        }
        Ok(Self::new(
            ExecPromptCommandFlags {
                prompt,
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
                work_item: ctx.flags.string("work-item"),
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
        .with_output_dir(ctx.output_dir.clone())
        .with_profile(crate::command::commands::launch_policy::settings_profile::ProfileRequest::from_context(ctx)?)
        .with_workspace_access(crate::data::workspace::WorkspaceAccess::from_read_only(
            ctx.flags
                .bool(crate::data::workspace::READ_ONLY_WORKSPACE_FLAG),
        ))
        .with_prerequisites(prerequisites))
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

/// Task presence is independent of interaction mode and provider selection.
pub(super) fn require_task(
    prompt: Option<&str>,
    work_item: Option<&str>,
    path: &[&str],
) -> Result<(), CommandError> {
    if prompt.is_some_and(|p| !p.trim().is_empty())
        || work_item.is_some_and(|p| !p.trim().is_empty())
    {
        return Ok(());
    }
    Err(CommandError::InvalidArgumentValue {
        command: path.iter().map(|s| (*s).to_string()).collect(),
        argument: "prompt".into(),
        reason: "exec prompt requires a nonblank task or --work-item; use chat --automation for an idle terminal where available.".into(),
    })
}
