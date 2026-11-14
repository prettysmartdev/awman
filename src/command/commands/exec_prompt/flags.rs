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
    pub auto: bool,
    pub agent: Option<String>,
    pub model: Option<String>,
    pub launch_mode: Option<crate::data::config::agents::LaunchMode>,
    pub overlay: Vec<String>,
    /// `--agent-flag` values; `None` lets config `agentFlags` apply.
    pub agent_flags: Option<Vec<String>>,
    /// Raw `--work-item` reference, resolved at run time (WI 0147).
    pub work_item: Option<String>,
    pub session_worktree: Option<SessionWorktreeTarget>,
}

impl super::ExecPromptCommand {
    /// Construct from the catalogue-resolved input (WI 0113 F-10). A
    /// whitespace-only positional prompt is normalised to `None`; the
    /// prompt-or-work-item requirement is checked at run time.
    pub fn from_input(ctx: &BuildContext) -> Result<Self, CommandError> {
        refuse_selector_in_prompt(ctx.args.trailing("prompt"), &ctx.path())?;
        let prompt = match ctx.args.get("prompt") {
            Some(prompt) if prompt.trim().is_empty() => None,
            other => other.map(str::to_string),
        };
        Ok(Self::new(
            ExecPromptCommandFlags {
                prompt,
                non_interactive: ctx.flags.bool("headless"),
                plan: ctx.flags.bool("plan"),
                allow_docker: ctx.flags.bool("allow-docker"),
                yolo: ctx.flags.bool("yolo"),
                auto: ctx.flags.bool("auto"),
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
            },
            ctx.engines.clone(),
            ctx.session.clone(),
        )
        .with_cancellation(ctx.cancel.clone())
        .with_output_dir(ctx.output_dir.clone()))
    }
}

/// The prompt is a greedy trailing argument, so a worktree selector written
/// after the prompt text would silently become prompt words and the agent
/// would run in the invoking checkout instead of the requested worktree.
/// Refuse that rather than launch un-isolated. A prompt that really contains
/// one of these words is still accepted when quoted as one argument.
pub(crate) fn refuse_selector_in_prompt(
    tokens: &[String],
    path: &[&str],
) -> Result<(), CommandError> {
    const SELECTORS: [&str; 3] = ["branch", "pr", "worktree"];
    let Some(token) = tokens.iter().find(|token| {
        token.strip_prefix("--").is_some_and(|rest| {
            let name = rest.split_once('=').map_or(rest, |(name, _)| name);
            SELECTORS.contains(&name)
        })
    }) else {
        return Ok(());
    };
    let flag = token
        .split_once('=')
        .map_or(token.as_str(), |(flag, _)| flag);
    Err(CommandError::InvalidFlagValue {
        command: path.iter().map(|segment| (*segment).to_string()).collect(),
        flag: flag.trim_start_matches('-').to_string(),
        reason: format!(
            "`{flag}` appears after the prompt text, where it would be sent to the agent as part of the prompt and the session would run in the current checkout. Put {flag} before the prompt (for example `exec prompt {flag} <value> \"<prompt>\"`), or quote the whole prompt as one argument if the text is intended."
        ),
    })
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
