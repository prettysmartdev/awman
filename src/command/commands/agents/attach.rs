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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::commands::agents::test_support::{
        handle, owner_detail, FakeFrontend, FakeRuntime,
    };
    use std::sync::atomic::Ordering;

    fn base() -> FakeRuntime {
        FakeRuntime::new().with_running(vec![handle("abc123def456000", "awman-one", 30)])
    }

    fn attach(reference: &str, runtime: Arc<FakeRuntime>) -> AgentsAttachCommand {
        AgentsAttachCommand::new(reference.into(), runtime)
    }

    #[tokio::test]
    async fn dead_owner_without_reattach_fails_and_never_calls_attach() {
        let mut rt = base();
        rt.reattach = false;
        let h = rt.running.lock().unwrap()[0].clone();
        *rt.inspect_override.lock().unwrap() = Some(owner_detail(&h, 4242, false));
        let rt = Arc::new(rt);
        let (frontend, log) = FakeFrontend::new();
        let err = attach("awman-one", rt.clone())
            .run_with_frontend(frontend.boxed())
            .await
            .unwrap_err();
        assert!(
            matches!(err, CommandError::AgentOwnerGone { .. }),
            "{err:?}"
        );
        assert_eq!(err.exit_code(), 1);
        assert!(err.to_string().contains("awman-one"), "{err}");
        assert!(!rt.called("attach"), "calls: {:?}", rt.calls());
        assert!(log.lock().unwrap().pty_active.is_empty());
    }

    #[tokio::test]
    async fn live_owner_without_reattach_attaches() {
        let mut rt = base();
        rt.reattach = false;
        let h = rt.running.lock().unwrap()[0].clone();
        *rt.inspect_override.lock().unwrap() = Some(owner_detail(&h, 4242, true));
        let rt = Arc::new(rt);
        let (frontend, _log) = FakeFrontend::new();
        attach("awman-one", rt.clone())
            .run_with_frontend(frontend.boxed())
            .await
            .unwrap();
        assert!(rt.called("attach abc123def456000"));
    }

    #[tokio::test]
    async fn reattachable_runtime_skips_the_owner_check() {
        let rt = Arc::new(base()); // reattach = true
        let (frontend, _log) = FakeFrontend::new();
        attach("awman-one", rt.clone())
            .run_with_frontend(frontend.boxed())
            .await
            .unwrap();
        assert!(!rt.called("inspect"));
        assert!(rt.called("attach"));
    }

    #[tokio::test]
    async fn success_drives_the_instance_through_the_frontend() {
        let mut rt = base();
        rt.attach_exit = 7;
        let rt = Arc::new(rt);
        let (frontend, log) = FakeFrontend::new();
        let outcome = attach("abc1", rt.clone())
            .run_with_frontend(frontend.boxed())
            .await
            .unwrap();
        assert_eq!(outcome.exit_code, 7);
        assert_eq!(outcome.name, "awman-one");
        let log = log.lock().unwrap();
        assert_eq!(log.pty_active, [true, false]);
        assert_eq!(log.container_frontends, 1);
        assert_eq!(log.stuck_senders, 1);
        assert!(!rt.called("stop"));
    }

    #[tokio::test]
    async fn detach_cancels_only_the_local_client_and_never_stops_the_agent() {
        let mut rt = base();
        rt.attach_blocks = true;
        let rt = Arc::new(rt);
        let (frontend, log) = FakeFrontend::new();
        frontend.attach_token.cancel();
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            attach("awman-one", rt.clone()).run_with_frontend(frontend.boxed()),
        )
        .await
        .expect("detach must end the session")
        .unwrap();
        assert_eq!(outcome.exit_code, 0);
        assert!(!rt.called("stop"), "calls: {:?}", rt.calls());
        assert!(!rt.called("remove"));
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while rt.detach_cancels.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("detach teardown completes");
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert_eq!(rt.detach_cancels.load(Ordering::SeqCst), 1);
        assert_eq!(log.lock().unwrap().pty_active, [true, false]);
    }

    #[tokio::test]
    async fn unknown_and_stopped_targets_never_reach_attach() {
        let rt = Arc::new(base().with_stopped(vec![handle("s1", "awman-stopped", 9)]));
        for (reference, exit) in [("nope", 4), ("awman-stopped", 4), ("", 2)] {
            let (frontend, _log) = FakeFrontend::new();
            let err = attach(reference, rt.clone())
                .run_with_frontend(frontend.boxed())
                .await
                .unwrap_err();
            assert_eq!(err.exit_code(), exit, "{reference}: {err}");
        }
        assert!(!rt.called("attach"));
    }
}
