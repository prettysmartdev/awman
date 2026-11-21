//! `awman agents list` (`ps`, `ls`) — a docker `ps`-style listing.
//!
//! Lists only; it never fetches per-agent stats, so it stays fast. Stats
//! belong to `status` and `detail`.

use std::sync::Arc;

use async_trait::async_trait;
use serde::Serialize;

use crate::command::commands::agents::snapshot::{
    collect_available, AgentSnapshot, CollectOptions,
};
use crate::command::commands::agents::AgentsCommandFrontend;
use crate::command::commands::Command;
use crate::command::dispatch::BuildContext;
use crate::command::error::CommandError;
use crate::engine::agent_runtime::AgentRuntimeEngine;

#[derive(Debug, Clone, Default)]
pub struct AgentsListFlags {
    pub all: bool,
    pub quiet: bool,
    pub json: bool,
    pub no_trunc: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct AgentsListOutcome {
    pub agents: Vec<AgentSnapshot>,
    /// `--quiet`: print only short ids, one per line.
    pub quiet: bool,
    /// `--no-trunc`: print full ids and untruncated names and images.
    pub no_trunc: bool,
    /// `--all`: stopped agents are included.
    pub include_stopped: bool,
}

pub struct AgentsListCommand {
    flags: AgentsListFlags,
    runtime: Arc<dyn AgentRuntimeEngine>,
}

impl AgentsListCommand {
    pub fn new(flags: AgentsListFlags, runtime: Arc<dyn AgentRuntimeEngine>) -> Self {
        Self { flags, runtime }
    }

    pub fn from_input(ctx: &BuildContext) -> Result<Self, CommandError> {
        Ok(Self::new(
            AgentsListFlags {
                all: ctx.flags.bool("all"),
                quiet: ctx.flags.bool("quiet"),
                json: ctx.flags.bool("json"),
                no_trunc: ctx.flags.bool("no-trunc"),
            },
            ctx.engines.runtime.clone(),
        ))
    }

    pub fn flags(&self) -> &AgentsListFlags {
        &self.flags
    }
}

#[async_trait]
impl Command for AgentsListCommand {
    type Frontend = Box<dyn AgentsCommandFrontend>;
    type Outcome = AgentsListOutcome;

    async fn run_with_frontend(
        self,
        mut frontend: Self::Frontend,
    ) -> Result<Self::Outcome, CommandError> {
        self.runtime.availability()?;
        let tui = frontend.tui_context();
        let overview = collect_available(
            self.runtime.as_ref(),
            tui.as_ref(),
            CollectOptions {
                include_stopped: self.flags.all,
                with_stats: false,
            },
            "",
        )?;
        let outcome = AgentsListOutcome {
            agents: overview.agents,
            quiet: self.flags.quiet,
            no_trunc: self.flags.no_trunc,
            include_stopped: self.flags.all,
        };
        frontend.show_list(&outcome);
        Ok(outcome)
    }
}
