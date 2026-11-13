//! `awman agents kill` (`rm`) — stop and remove one running agent.
//!
//! Only agents in awman's own listing can be targeted, so this can never
//! stop a non-awman container. Stopped-agent cleanup stays `awman clean`'s
//! job.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use serde::Serialize;

use crate::command::commands::agents::resolve::{map_action_error, resolve_agent, ResolveScope};
use crate::command::commands::agents::snapshot::AgentSnapshot;
use crate::command::commands::agents::AgentsCommandFrontend;
use crate::command::commands::Command;
use crate::command::dispatch::BuildContext;
use crate::command::error::CommandError;
use crate::data::message::{MessageLevel, UserMessage};
use crate::data::session::AgentHandle;
use crate::engine::agent_runtime::AgentRuntimeEngine;
use crate::engine::error::EngineError;

/// The answer to the kill confirmation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KillChoice {
    Kill,
    Keep,
}

#[derive(Debug, Clone, Default)]
pub struct AgentsKillFlags {
    /// Skip the confirmation prompt.
    pub yes: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct AgentsKillOutcome {
    pub id: String,
    pub short_id: String,
    pub name: String,
    /// False only when the agent stopped but its removal failed.
    pub removed: bool,
}

pub struct AgentsKillCommand {
    reference: String,
    flags: AgentsKillFlags,
    runtime: Arc<dyn AgentRuntimeEngine>,
}

impl AgentsKillCommand {
    pub fn new(
        reference: String,
        flags: AgentsKillFlags,
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
            AgentsKillFlags {
                yes: ctx.flags.bool("yes"),
            },
            ctx.engines.runtime.clone(),
        ))
    }

    pub fn flags(&self) -> &AgentsKillFlags {
        &self.flags
    }

    /// Best-effort removal after a successful stop; see [`remove_stopped_agent`].
    fn remove(&self, frontend: &mut dyn AgentsCommandFrontend, snapshot: &AgentSnapshot) -> bool {
        match remove_stopped_agent(self.runtime.as_ref(), &snapshot.id) {
            Ok(()) => true,
            Err(e) => {
                frontend.write_message(UserMessage {
                    level: MessageLevel::Warning,
                    text: format!(
                        "agent {} stopped but could not be removed: {e}; `awman clean` can remove it",
                        snapshot.name
                    ),
                });
                false
            }
        }
    }
}

/// Best-effort removal of an agent that was just stopped. Some backends
/// already remove resources on stop, and unsupported removal is accepted, so
/// this is `Ok` unless the removal failed *and* the agent is still listed;
/// then the removal error comes back.
fn remove_stopped_agent(runtime: &dyn AgentRuntimeEngine, id: &str) -> Result<(), EngineError> {
    match runtime.remove_agent(id) {
        Ok(()) | Err(EngineError::UnsupportedOnRuntime { .. }) => Ok(()),
        Err(e) => {
            let still_present = runtime
                .list_running_all()
                .and_then(|mut running| {
                    running.extend(runtime.list_stopped()?);
                    Ok(running)
                })
                .map(|agents| agents.iter().any(|h| h.id == id))
                .unwrap_or(true);
            if still_present {
                Err(e)
            } else {
                Ok(())
            }
        }
    }
}

/// `stop` then best-effort `remove_agent`; `Ok(removed)`. `UnsupportedOnRuntime`
/// on removal counts as removed, exactly as `awman agents kill` does. Used by
/// an API cancel, which stops a workflow's containers the same way.
pub fn stop_and_remove_agent(
    runtime: &dyn AgentRuntimeEngine,
    handle: &AgentHandle,
) -> Result<bool, EngineError> {
    runtime.stop(handle)?;
    Ok(remove_stopped_agent(runtime, &handle.id).is_ok())
}

#[async_trait]
impl Command for AgentsKillCommand {
    type Frontend = Box<dyn AgentsCommandFrontend>;
    type Outcome = AgentsKillOutcome;

    async fn run_with_frontend(
        self,
        mut frontend: Self::Frontend,
    ) -> Result<Self::Outcome, CommandError> {
        let runtime = self.runtime.as_ref();
        let resolved = resolve_agent(runtime, &self.reference, ResolveScope::RunningOnly)?;
        let mut snapshot = AgentSnapshot::from_handle(&resolved.handle, resolved.state, Utc::now());
        snapshot.apply_tui_context(frontend.tui_context().as_ref());

        if !self.flags.yes {
            let prompt = crate::command::prompts::agents_kill_confirm(&snapshot);
            if frontend.confirm_kill(&prompt)? == KillChoice::Keep {
                return Err(CommandError::Aborted);
            }
        }

        runtime.stop(&resolved.handle).map_err(|e| match e {
            // Never stop the newer sandbox that reused the name.
            EngineError::BuiltinAgentReplaced { .. } => CommandError::AgentReplaced {
                reference: self.reference.clone(),
            },
            other => map_action_error(runtime, &resolved.handle, other.into()),
        })?;
        let removed = self.remove(frontend.as_mut(), &snapshot);

        let outcome = AgentsKillOutcome {
            id: snapshot.id,
            short_id: snapshot.short_id,
            name: snapshot.name,
            removed,
        };
        frontend.show_kill_result(&outcome);
        Ok(outcome)
    }
}
