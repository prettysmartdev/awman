//! `awman agents status` — a one-shot summary of running agents, or a live
//! dashboard with `--watch`.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::Serialize;

use crate::command::commands::agents::snapshot::{
    collect_available, collect_overview, AgentsOverview, CollectOptions,
};
use crate::command::commands::agents::{tips, AgentsCommandFrontend};
use crate::command::commands::Command;
use crate::command::dispatch::BuildContext;
use crate::command::error::CommandError;
use crate::engine::agent_runtime::AgentRuntimeEngine;

/// How often `--watch` refreshes the dashboard.
pub const WATCH_INTERVAL: Duration = Duration::from_secs(2);

const STATUS_COLLECT: CollectOptions = CollectOptions {
    include_stopped: false,
    with_stats: true,
};

#[derive(Debug, Clone, Default)]
pub struct AgentsStatusFlags {
    pub watch: bool,
    pub json: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct AgentsStatusOutcome {
    /// The overview a one-shot run collected. A `--watch` run has no
    /// outcome: it ends only when cancelled, as `CommandError::Aborted`
    /// (exit 130), after drawing its own frames.
    pub overview: AgentsOverview,
}

pub struct AgentsStatusCommand {
    flags: AgentsStatusFlags,
    runtime: Arc<dyn AgentRuntimeEngine>,
}

impl AgentsStatusCommand {
    pub fn new(flags: AgentsStatusFlags, runtime: Arc<dyn AgentRuntimeEngine>) -> Self {
        Self { flags, runtime }
    }

    /// `status` reports on the runtime rather than the repo, so it holds no
    /// session and works outside a Git repo.
    pub fn from_input(ctx: &BuildContext) -> Result<Self, CommandError> {
        Ok(Self::new(
            AgentsStatusFlags {
                watch: ctx.flags.bool("watch"),
                json: ctx.flags.bool("json"),
            },
            ctx.engines.runtime.clone(),
        ))
    }

    pub fn flags(&self) -> &AgentsStatusFlags {
        &self.flags
    }

    /// Collect once and render once. An unreachable runtime is an error with
    /// the runtime's own exit class.
    fn run_once(
        &self,
        frontend: &mut dyn AgentsCommandFrontend,
        tip: &str,
    ) -> Result<AgentsStatusOutcome, CommandError> {
        self.runtime.availability()?;
        let tui = frontend.tui_context();
        let overview = collect_available(self.runtime.as_ref(), tui.as_ref(), STATUS_COLLECT, tip)?;
        frontend.render_overview(&overview, false);
        Ok(AgentsStatusOutcome { overview })
    }

    /// Render a frame every [`WATCH_INTERVAL`] until the frontend's token is
    /// cancelled. A runtime that goes away shows an "unavailable" frame and
    /// is retried on the next tick, so a daemon restart is picked up live.
    /// Cancelling is a user abort, so this never returns `Ok`.
    async fn run_watch(
        &self,
        frontend: &mut dyn AgentsCommandFrontend,
        tip: &str,
    ) -> Result<std::convert::Infallible, CommandError> {
        let cancel = frontend.begin_watch();
        let _cancel_on_drop = cancel.clone().drop_guard();
        while !cancel.is_cancelled() {
            let runtime = self.runtime.clone();
            let tui = frontend.tui_context();
            let tip = tip.to_owned();
            let worker_cancel = cancel.clone();
            // A separate observation thread cannot hold Tokio shutdown hostage
            // if a native provider cannot interrupt an in-flight read.
            let (send, mut collect) = tokio::sync::oneshot::channel();
            std::thread::spawn(move || {
                let overview =
                    crate::engine::host_cli::with_collection_cancel(worker_cancel, || {
                        collect_overview(runtime.as_ref(), tui.as_ref(), STATUS_COLLECT, &tip)
                            .unwrap_or_else(|e| {
                                AgentsOverview::unavailable(runtime.as_ref(), e.to_string(), &tip)
                            })
                    });
                let _ = send.send(overview);
            });
            let overview = tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    // The runtime observes the same cancellation and reaps pending CLI calls.
                    let _ = tokio::time::timeout(Duration::from_millis(250), &mut collect).await;
                    break;
                },
                result = &mut collect => match result {
                    Ok(overview) => overview,
                    Err(e) => {
                        frontend.end_watch();
                        return Err(CommandError::from(crate::engine::error::EngineError::Other(e.to_string())));
                    }
                },
            };
            if cancel.is_cancelled() {
                break;
            }
            frontend.render_overview(&overview, true);
            tokio::select! {
                biased;
                _ = cancel.cancelled() => break,
                _ = tokio::time::sleep(WATCH_INTERVAL) => {}
            }
        }
        frontend.end_watch();
        Err(CommandError::Aborted)
    }
}

#[async_trait]
impl Command for AgentsStatusCommand {
    type Frontend = Box<dyn AgentsCommandFrontend>;
    type Outcome = AgentsStatusOutcome;

    /// Whether this loops depends only on `--watch` and the cancel token;
    /// no frontend can turn a one-shot into a watch.
    async fn run_with_frontend(
        self,
        mut frontend: Self::Frontend,
    ) -> Result<Self::Outcome, CommandError> {
        // One tip per run, so a watch footer does not flicker between tips.
        let tip = tips::select_random_tip();
        if !self.flags.watch {
            return self.run_once(frontend.as_mut(), tip);
        }
        match self.run_watch(frontend.as_mut(), tip).await? {}
    }
}
