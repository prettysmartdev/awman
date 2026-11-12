//! `awman agents attach` — connect to a running agent's live terminal.
//!
//! The attaching process never owns the agent: detaching, Ctrl-C or a
//! frontend crash ends only the local attach client, and nothing on this
//! path calls `stop`.

use std::sync::Arc;

use async_trait::async_trait;
use serde::Serialize;

use crate::command::commands::agents::resolve::{map_action_error, resolve_agent, ResolveScope};
use crate::command::commands::agents::AgentsCommandFrontend;
use crate::command::commands::Command;
use crate::command::dispatch::BuildContext;
use crate::command::error::CommandError;
use crate::data::session::AgentHandle;
use crate::engine::agent_runtime::AgentRuntimeEngine;

#[derive(Debug, Clone, Serialize)]
pub struct AgentsAttachOutcome {
    pub id: String,
    pub name: String,
    /// The attach session's exit code; 0 when the frontend detached.
    pub exit_code: i32,
}

pub struct AgentsAttachCommand {
    reference: String,
    runtime: Arc<dyn AgentRuntimeEngine>,
}

impl AgentsAttachCommand {
    pub fn new(reference: String, runtime: Arc<dyn AgentRuntimeEngine>) -> Self {
        Self { reference, runtime }
    }

    pub fn from_input(ctx: &BuildContext) -> Result<Self, CommandError> {
        Ok(Self::new(
            ctx.args.require("id")?,
            ctx.engines.runtime.clone(),
        ))
    }

    /// On runtimes whose attach endpoint lives in the launching process,
    /// fail fast with an explanation when that process has exited, instead
    /// of an opaque socket error.
    fn check_owner_alive(&self, handle: &AgentHandle) -> Result<(), CommandError> {
        if self.runtime.reattach_after_owner_exit() {
            return Ok(());
        }
        let detail = self
            .runtime
            .inspect(handle)
            .map_err(|e| map_action_error(self.runtime.as_ref(), handle, e.into()))?;
        match detail.owner {
            Some(owner) if !owner.alive => Err(CommandError::AgentOwnerGone {
                name: handle.name.clone(),
                runtime: self.runtime.display_name().to_string(),
            }),
            _ => Ok(()),
        }
    }
}

#[async_trait]
impl Command for AgentsAttachCommand {
    type Frontend = Box<dyn AgentsCommandFrontend>;
    type Outcome = AgentsAttachOutcome;

    async fn run_with_frontend(
        self,
        mut frontend: Self::Frontend,
    ) -> Result<Self::Outcome, CommandError> {
        let runtime = self.runtime.as_ref();
        let handle = resolve_agent(runtime, &self.reference, ResolveScope::RunningOnly)?.handle;
        self.check_owner_alive(&handle)?;
        let instance = runtime
            .attach(&handle)
            .map_err(|e| map_action_error(runtime, &handle, e.into()))?;

        let cancel = frontend.attach_cancel();
        frontend.set_pty_active(true);
        let container_frontend = frontend.container_frontend_for_pty();
        let mut execution = match instance.run_with_frontend(container_frontend) {
            Ok(execution) => execution,
            Err(e) => {
                frontend.set_pty_active(false);
                frontend.replay_queued();
                return Err(map_action_error(runtime, &handle, e.into()));
            }
        };
        frontend.set_stuck_sender(execution.stuck_sender());
        // An attach execution's cancel ends only the local attach client
        // (the `docker attach` child, or the attach-socket connection).
        // Poll wait once even if already cancelled. Its RAII guard is the sole
        // owner of cancellation; dropping it detaches the local client once.
        let exit = {
            let wait = execution.wait();
            tokio::pin!(wait);
            tokio::select! {
                biased;
                exit = &mut wait => Some(exit),
                _ = cancel.cancelled() => None,
            }
        };
        let exit_code = match exit {
            None => 0,
            Some(Ok(info)) => info.exit_code,
            Some(Err(e)) => {
                frontend.set_pty_active(false);
                frontend.replay_queued();
                return Err(map_action_error(runtime, &handle, e.into()));
            }
        };
        frontend.set_pty_active(false);
        frontend.replay_queued();

        Ok(AgentsAttachOutcome {
            id: handle.id,
            name: handle.name,
            exit_code,
        })
    }
}
