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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::commands::agents::test_support::{handle, FakeFrontend, FakeRuntime};
    use crate::command::commands::agents::AgentState;

    fn runtime() -> Arc<FakeRuntime> {
        Arc::new(
            FakeRuntime::new()
                .with_running(vec![handle("a1", "awman-a", 10)])
                .with_stopped(vec![handle("s1", "awman-stopped", 500)]),
        )
    }

    fn list(flags: AgentsListFlags, rt: Arc<FakeRuntime>) -> AgentsListCommand {
        AgentsListCommand::new(flags, rt)
    }

    #[tokio::test]
    async fn lists_running_only_by_default_without_stats() {
        let rt = runtime();
        let (frontend, log) = FakeFrontend::new();
        let out = list(AgentsListFlags::default(), rt.clone())
            .run_with_frontend(frontend.boxed())
            .await
            .unwrap();
        assert_eq!(out.agents.len(), 1);
        assert!(!out.include_stopped);
        assert!(!rt.called("stats"), "list stays fast");
        assert!(!rt.called("list_stopped"));
        assert_eq!(log.lock().unwrap().list_outcomes, 1);
    }

    #[tokio::test]
    async fn all_includes_stopped_rows_last() {
        let (frontend, _log) = FakeFrontend::new();
        let flags = AgentsListFlags {
            all: true,
            ..Default::default()
        };
        let out = list(flags, runtime())
            .run_with_frontend(frontend.boxed())
            .await
            .unwrap();
        assert_eq!(out.agents.len(), 2);
        assert_eq!(out.agents[1].state, AgentState::Stopped);
        assert!(out.include_stopped);
    }

    #[tokio::test]
    async fn quiet_and_no_trunc_ride_on_the_outcome() {
        let (frontend, _log) = FakeFrontend::new();
        let flags = AgentsListFlags {
            quiet: true,
            no_trunc: true,
            ..Default::default()
        };
        let out = list(flags, runtime())
            .run_with_frontend(frontend.boxed())
            .await
            .unwrap();
        assert!(out.quiet && out.no_trunc);
    }

    #[tokio::test]
    async fn unreachable_runtime_is_a_typed_error() {
        let rt = runtime();
        *rt.available.lock().unwrap() = Err("down".into());
        let (frontend, log) = FakeFrontend::new();
        let err = list(AgentsListFlags::default(), rt)
            .run_with_frontend(frontend.boxed())
            .await
            .unwrap_err();
        assert!(matches!(err, CommandError::Engine(_)));
        assert_eq!(log.lock().unwrap().list_outcomes, 0);
    }
}
