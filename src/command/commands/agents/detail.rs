//! `awman agents detail` (`inspect`) — one agent in depth.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use serde::Serialize;

use crate::command::commands::agents::resolve::{map_action_error, resolve_agent, ResolveScope};
use crate::command::commands::agents::snapshot::{AgentSnapshot, AgentState};
use crate::command::commands::agents::AgentsCommandFrontend;
use crate::command::commands::Command;
use crate::command::dispatch::BuildContext;
use crate::command::error::CommandError;
use crate::engine::agent_runtime::{AgentDetail, AgentRuntimeEngine};

#[derive(Debug, Clone, Default)]
pub struct AgentsDetailFlags {
    pub json: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct AgentsDetailOutcome {
    pub snapshot: AgentSnapshot,
    pub detail: AgentDetail,
}

impl AgentsDetailOutcome {
    /// The human-readable key/value blocks, in display order: Identity,
    /// Lifecycle, Resources, Runtime. Every frontend renders from this.
    pub fn groups(&self) -> Vec<(&'static str, Vec<(String, String)>)> {
        let s = &self.snapshot;
        let pair = |k: &str, v: String| (k.to_string(), v);
        let mut runtime = Vec::new();
        if let Some(owner) = &self.detail.owner {
            let liveness = if owner.alive { "running" } else { "exited" };
            runtime.push(pair("owner pid", format!("{} ({liveness})", owner.pid)));
        }
        runtime.extend(
            self.detail
                .runtime_extras
                .iter()
                .map(|(k, v)| (k.clone(), v.clone())),
        );
        vec![
            (
                "Identity",
                vec![
                    pair("id", s.id.clone()),
                    pair("name", s.name.clone()),
                    pair("image", s.image.clone()),
                    pair("source", s.source_cell()),
                ],
            ),
            (
                "Lifecycle",
                vec![
                    pair("state", s.state.label().to_string()),
                    pair("started", s.started_at.to_rfc3339()),
                    pair("uptime", s.uptime_cell()),
                ],
            ),
            (
                "Resources",
                vec![pair("cpu", s.cpu_cell()), pair("memory", s.mem_cell())],
            ),
            ("Runtime", runtime),
        ]
    }
}

pub struct AgentsDetailCommand {
    reference: String,
    flags: AgentsDetailFlags,
    runtime: Arc<dyn AgentRuntimeEngine>,
}

impl AgentsDetailCommand {
    pub fn new(
        reference: String,
        flags: AgentsDetailFlags,
        runtime: Arc<dyn AgentRuntimeEngine>,
    ) -> Self {
        Self {
            reference,
            flags,
            runtime,
        }
    }

    pub fn from_input(ctx: &BuildContext) -> Result<Self, CommandError> {
        Ok(Self::new(
            ctx.args.require("id")?,
            AgentsDetailFlags {
                json: ctx.flags.bool("json"),
            },
            ctx.engines.runtime.clone(),
        ))
    }

    pub fn flags(&self) -> &AgentsDetailFlags {
        &self.flags
    }
}

#[async_trait]
impl Command for AgentsDetailCommand {
    type Frontend = Box<dyn AgentsCommandFrontend>;
    type Outcome = AgentsDetailOutcome;

    async fn run_with_frontend(
        self,
        mut frontend: Self::Frontend,
    ) -> Result<Self::Outcome, CommandError> {
        let runtime = self.runtime.as_ref();
        let resolved = resolve_agent(runtime, &self.reference, ResolveScope::IncludeStopped)?;
        let detail = runtime.inspect(&resolved.handle).map_err(|e| {
            let err = CommandError::from(e);
            if resolved.state == AgentState::Stopped {
                err
            } else {
                map_action_error(runtime, &resolved.handle, err)
            }
        })?;

        let state = if detail.running {
            AgentState::Running
        } else {
            AgentState::Stopped
        };
        let mut snapshot = AgentSnapshot::from_handle(&resolved.handle, state, Utc::now());
        snapshot.cpu_percent = detail.cpu_percent;
        snapshot.memory_mb = detail.memory_mb;
        snapshot.apply_tui_context(frontend.tui_context().as_ref());

        let outcome = AgentsDetailOutcome { snapshot, detail };
        frontend.show_detail(&outcome);
        Ok(outcome)
    }
}
