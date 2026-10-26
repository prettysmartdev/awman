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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::commands::agents::test_support::{handle, FakeFrontend, FakeRuntime};

    fn command(watch: bool, runtime: Arc<FakeRuntime>) -> AgentsStatusCommand {
        AgentsStatusCommand::new(AgentsStatusFlags { watch, json: false }, runtime)
    }

    fn runtime_with_two() -> Arc<FakeRuntime> {
        Arc::new(FakeRuntime::new().with_running(vec![
            handle("a1", "awman-a", 10),
            handle("b2", "awman-b", 20),
        ]))
    }

    /// Regression: "watch is default". A frontend that is ready to loop
    /// (its watch token is live and would allow endless ticks) must still see
    /// exactly one render when `--watch` is absent.
    #[tokio::test]
    async fn cancellation_during_uninterruptible_native_sampling_releases_the_frontend() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let gate = Arc::new(AtomicBool::new(false));
        let mut rt = FakeRuntime::new().with_running(vec![handle("a1", "awman-a", 5)]);
        rt.stats_gate = Some(gate.clone());
        let rt = Arc::new(rt);
        let (frontend, log) = FakeFrontend::new();
        let cancel = frontend.watch_token.clone();
        let runtime = rt.clone();
        let task = tokio::spawn(command(true, rt).run_with_frontend(frontend.boxed()));
        while !runtime.called("stats") {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        cancel.cancel();
        let result = tokio::time::timeout(Duration::from_secs(1), task).await;
        gate.store(true, Ordering::SeqCst);
        let err = result
            .expect("native sampling must not hold cancellation")
            .unwrap()
            .unwrap_err();
        assert_eq!(err.exit_code(), 130);
        let log = log.lock().unwrap();
        assert_eq!(log.end_watch, 1);
        assert!(
            log.renders.is_empty(),
            "a cancelled frame must never render"
        );
    }

    #[tokio::test]
    async fn without_watch_renders_exactly_once_even_if_frontend_would_loop() {
        let (frontend, log) = FakeFrontend::new();
        let outcome = tokio::time::timeout(
            Duration::from_secs(5),
            command(false, runtime_with_two()).run_with_frontend(frontend.boxed()),
        )
        .await
        .expect("one-shot status must return without waiting")
        .unwrap();
        let log = log.lock().unwrap();
        assert_eq!(log.renders.len(), 1);
        assert!(!log.renders[0].1, "one-shot render is not `watching`");
        assert_eq!(log.begin_watch, 0);
        assert_eq!(log.end_watch, 0);
        assert_eq!(outcome.overview.agents.len(), 2);
    }

    #[tokio::test]
    async fn one_shot_on_unreachable_runtime_is_the_runtimes_typed_error() {
        let runtime = Arc::new(FakeRuntime::new());
        *runtime.available.lock().unwrap() = Err("daemon down".into());
        let (frontend, log) = FakeFrontend::new();
        let err = command(false, runtime)
            .run_with_frontend(frontend.boxed())
            .await
            .unwrap_err();
        assert!(matches!(err, CommandError::Engine(_)), "{err:?}");
        assert!(log.lock().unwrap().renders.is_empty());
    }

    #[tokio::test]
    async fn watch_with_precancelled_token_returns_aborted_without_collecting() {
        let (frontend, log) = FakeFrontend::new();
        frontend.watch_token.cancel();
        let started = tokio::time::Instant::now();
        let err = command(true, runtime_with_two())
            .run_with_frontend(frontend.boxed())
            .await
            .unwrap_err();
        assert_eq!(err.exit_code(), 130);
        // Paused time only advances when the runtime idles on a timer; a
        // return that waited out the interval would show up here.
        assert!(started.elapsed() < WATCH_INTERVAL);
        let log = log.lock().unwrap();
        assert!(log.renders.is_empty());
        assert_eq!((log.begin_watch, log.end_watch), (1, 1));
    }

    #[tokio::test]
    async fn watch_renders_once_per_tick_until_cancelled() {
        let (mut frontend, log) = FakeFrontend::new();
        frontend.cancel_after_renders = Some(4);
        let started = tokio::time::Instant::now();
        let err = command(true, runtime_with_two())
            .run_with_frontend(frontend.boxed())
            .await
            .unwrap_err();
        assert_eq!(err.exit_code(), 130);
        let log = log.lock().unwrap();
        assert_eq!(log.renders.len(), 4);
        assert!(log
            .renders
            .iter()
            .all(|(n, watching, _)| *n == 2 && *watching));
        // Three sleeps between four frames; the cancelled fourth does not sleep.
        assert!(started.elapsed() >= WATCH_INTERVAL * 3);
        assert!(started.elapsed() < WATCH_INTERVAL * 4);
        assert_eq!((log.begin_watch, log.end_watch), (1, 1));
    }

    #[tokio::test]
    async fn watch_shows_the_same_tip_on_every_tick() {
        let (mut frontend, log) = FakeFrontend::new();
        frontend.cancel_after_renders = Some(5);
        command(true, runtime_with_two())
            .run_with_frontend(frontend.boxed())
            .await
            .unwrap_err();
        let log = log.lock().unwrap();
        let first = &log.renders[0].2;
        assert!(!first.is_empty());
        assert!(log.renders.iter().all(|(_, _, tip)| tip == first));
    }

    #[tokio::test]
    async fn watch_survives_an_unavailable_runtime_and_recovers() {
        let runtime = runtime_with_two();
        *runtime.available.lock().unwrap() = Err("daemon down".into());
        let (mut frontend, log) = FakeFrontend::new();
        frontend.cancel_after_renders = Some(2);
        let rt = runtime.clone();
        let frames = log.clone();
        let recover = tokio::spawn(async move {
            loop {
                if !frames.lock().unwrap().renders.is_empty() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            *rt.available.lock().unwrap() = Ok(());
        });
        let err = command(true, runtime.clone())
            .run_with_frontend(frontend.boxed())
            .await
            .unwrap_err();
        recover.await.unwrap();
        assert_eq!(err.exit_code(), 130);
        let log = log.lock().unwrap();
        assert_eq!(log.renders.len(), 2);
        assert_eq!(log.renders[0].0, 0);
        assert_eq!(log.renders[1].0, 2, "recovered frame has agents");
        assert!(
            runtime
                .availability_checks
                .load(std::sync::atomic::Ordering::SeqCst)
                >= 2
        );
        assert_eq!(log.end_watch, 1);
    }
}
