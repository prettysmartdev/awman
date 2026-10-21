//! `awman agents …` — inspect and manage running code agents.
//!
//! Every subcommand is written once against the cross-runtime
//! [`AgentRuntimeEngine`](crate::engine::agent_runtime::AgentRuntimeEngine)
//! trait and works on every runtime. Where a runtime cannot do something the
//! command reports the runtime's typed error; nothing here branches on which
//! runtime is active, only on `capabilities()` and
//! `reattach_after_owner_exit()`.

use async_trait::async_trait;
use serde::Serialize;
use tokio_util::sync::CancellationToken;

use crate::command::commands::agent_setup::AgentLaunchFrontend;
use crate::command::commands::Command;
use crate::command::dispatch::BuildContext;
use crate::command::error::CommandError;
use crate::data::prompt::Prompt;

pub mod attach;
pub mod detail;
pub mod kill;
pub mod list;
pub mod resolve;
pub mod snapshot;
pub mod status;
pub mod tips;

#[cfg(test)]
pub(crate) mod test_support;

pub use attach::{AgentsAttachCommand, AgentsAttachOutcome};
pub use detail::{AgentsDetailCommand, AgentsDetailFlags, AgentsDetailOutcome};
pub use kill::{AgentsKillCommand, AgentsKillFlags, AgentsKillOutcome, KillChoice};
pub use list::{AgentsListCommand, AgentsListFlags, AgentsListOutcome};
pub use resolve::{match_reference, resolve_agent, ResolveScope, ResolvedAgent};
pub use snapshot::{
    collect_overview, humanize_uptime, AgentSnapshot, AgentSource, AgentState, AgentTotals,
    AgentsOverview, AgentsTuiContext, CollectOptions, TuiTabSnapshot, LIST_COLUMNS, STATUS_COLUMNS,
};
pub use status::{AgentsStatusCommand, AgentsStatusFlags, AgentsStatusOutcome, WATCH_INTERVAL};

/// One frontend trait for all five subcommands. Every method has a default,
/// so frontends can override the presentation they need. The API emits JSON
/// through the result callbacks.
///
/// The supertrait is what CLI, TUI and API already implement for `chat`; it
/// supplies `container_frontend_for_pty()` and `set_pty_active()`, which
/// `attach` uses to bind the agent's terminal.
pub trait AgentsCommandFrontend: AgentLaunchFrontend + Send + Sync {
    /// Live TUI tab data, re-read on every tick. See [`AgentsTuiContext`]
    /// for why this is a frontend hook rather than caller context.
    fn tui_context(&self) -> Option<AgentsTuiContext> {
        None
    }

    /// `status --watch` is starting. Returns the token whose cancellation
    /// ends the watch loop (Ctrl-C in the CLI, Esc / a new command / closing
    /// the tab in the TUI). Called at most once per run. The default never
    /// fires, which is only safe because frontends without a human (the API)
    /// cannot request `--watch`.
    fn begin_watch(&mut self) -> CancellationToken {
        CancellationToken::new()
    }

    /// Draw one dashboard frame. `watching` is true inside a `--watch` loop.
    /// A one-shot `status` calls this once with `watching == false`; the
    /// frontend may also render from the returned outcome instead.
    fn render_overview(&mut self, _overview: &AgentsOverview, _watching: bool) {}

    /// `status --watch` ended (cancelled or failed). Restore whatever
    /// [`Self::begin_watch`] changed.
    fn end_watch(&mut self) {}

    /// One-shot results, for frontends that do not render outcomes
    /// themselves (the TUI). The CLI renders from the outcome on stdout.
    fn show_list(&mut self, _outcome: &AgentsListOutcome) {}
    fn show_detail(&mut self, _outcome: &AgentsDetailOutcome) {}
    fn show_kill_result(&mut self, _outcome: &AgentsKillOutcome) {}

    /// Ask whether to stop and remove an agent. The default refuses: there
    /// is no human to ask, so the caller must pass `--yes` (exit 2).
    fn confirm_kill(&mut self, _prompt: &Prompt<KillChoice>) -> Result<KillChoice, CommandError> {
        Err(CommandError::InteractiveInputUnavailable {
            prompt: "yes".into(),
        })
    }

    /// Token that ends an `attach` session without stopping the agent (the
    /// TUI tab closed or started another command). The default never fires;
    /// the CLI overrides this token to detach locally on Ctrl-C.
    fn attach_cancel(&self) -> CancellationToken {
        CancellationToken::new()
    }
}

/// The `agents` subcommand dispatch built (one builder serves every leaf).
pub enum AgentsCommand {
    Status(AgentsStatusCommand),
    List(AgentsListCommand),
    Detail(AgentsDetailCommand),
    Kill(AgentsKillCommand),
    Attach(AgentsAttachCommand),
}

impl AgentsCommand {
    /// Select the subcommand from the canonical leaf; the catalogue has
    /// already turned aliases (`ps`, `ls`, `rm`, `inspect`) into it.
    pub fn from_input(ctx: &BuildContext) -> Result<Self, CommandError> {
        Ok(match ctx.caller.leaf() {
            "status" => Self::Status(AgentsStatusCommand::from_input(ctx)?),
            "list" => Self::List(AgentsListCommand::from_input(ctx)?),
            "detail" => Self::Detail(AgentsDetailCommand::from_input(ctx)?),
            "kill" => Self::Kill(AgentsKillCommand::from_input(ctx)?),
            "attach" => Self::Attach(AgentsAttachCommand::from_input(ctx)?),
            _ => return Err(CommandError::unknown_command(&ctx.path())),
        })
    }
}

/// What any `agents` subcommand returns.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", content = "payload", rename_all = "snake_case")]
pub enum AgentsOutcome {
    Status(AgentsStatusOutcome),
    List(AgentsListOutcome),
    Detail(AgentsDetailOutcome),
    Kill(AgentsKillOutcome),
    Attach(AgentsAttachOutcome),
}

impl AgentsOutcome {
    /// The process/API exit code: `attach` reports the session's own.
    pub fn exit_code(&self) -> i32 {
        match self {
            AgentsOutcome::Attach(outcome) => outcome.exit_code,
            _ => 0,
        }
    }
}

#[async_trait]
impl Command for AgentsCommand {
    type Frontend = Box<dyn AgentsCommandFrontend>;
    type Outcome = AgentsOutcome;

    async fn run_with_frontend(
        self,
        frontend: Self::Frontend,
    ) -> Result<Self::Outcome, CommandError> {
        Ok(match self {
            Self::Status(c) => AgentsOutcome::Status(c.run_with_frontend(frontend).await?),
            Self::List(c) => AgentsOutcome::List(c.run_with_frontend(frontend).await?),
            Self::Detail(c) => AgentsOutcome::Detail(c.run_with_frontend(frontend).await?),
            Self::Kill(c) => AgentsOutcome::Kill(c.run_with_frontend(frontend).await?),
            Self::Attach(c) => AgentsOutcome::Attach(c.run_with_frontend(frontend).await?),
        })
    }
}
