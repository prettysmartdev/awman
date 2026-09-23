//! `ContainerStatsSampler` — off-thread container resource sampling.
//!
//! A frontend that draws live CPU/memory figures needs three things it should
//! not own: a cadence, a way to run the runtime's blocking stats call without
//! stalling the draw loop, and a rule against piling a second query onto a
//! slot whose first one has not come back. All three used to live in the TUI's
//! `App` as a tuple channel, an `Instant`, and a `HashSet` (WI 0114 F-16).
//!
//! The frontend now says *which* containers it wants sampled — that is a view
//! question, and only the view knows which slots it is drawing — and reads
//! back typed [`StatsSample`]s.

use std::collections::HashSet;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::engine::agent_runtime::execution::AgentStats;
use crate::engine::agent_runtime::AgentRuntimeEngine;

/// How often a sampler dispatches a round of queries.
///
/// Three seconds is what the TUI polled at before F-16 moved the cadence here.
/// It is a compromise: a container-runtime `stats` call shells out, so a
/// tighter interval keeps the runtime CLI permanently busy, and a looser one
/// makes the sparkline visibly lag the agent's work.
const POLL_INTERVAL: Duration = Duration::from_secs(3);

/// Which container a request names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatsTarget {
    /// The container's name is known — query it directly.
    Named(String),
    /// The name has not arrived yet; take the first running container.
    ///
    /// Only correct when the caller knows there is exactly one, which is why
    /// it is the caller's choice and not a fallback the sampler applies.
    FirstRunning,
}

/// One container the caller wants sampled this round.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatsRequest {
    /// Opaque caller key, returned on the sample. The TUI passes a tab index.
    pub key: usize,
    /// The caller's name for the slot, returned on the sample so the caller
    /// can route it back. Empty for a command with a single container.
    pub step_name: String,
    pub target: StatsTarget,
}

/// One container's numbers, tagged with the request that asked for them.
#[derive(Debug, Clone, PartialEq)]
pub struct StatsSample {
    pub key: usize,
    pub step_name: String,
    pub stats: AgentStats,
}

pub struct ContainerStatsSampler {
    runtime: Arc<dyn AgentRuntimeEngine>,
    runtime_handle: tokio::runtime::Handle,
    tx: Sender<StatsSample>,
    rx: Receiver<StatsSample>,
    last_poll: Instant,
    /// `(key, step name)` pairs with a query still running. A `stats` call can
    /// take longer than [`POLL_INTERVAL`] on a busy daemon; without this every
    /// round would pile another query onto the same slot until the runtime CLI
    /// is swamped and no slot's numbers stay current.
    in_flight: Arc<Mutex<HashSet<(usize, String)>>>,
}

impl ContainerStatsSampler {
    pub fn new(
        runtime: Arc<dyn AgentRuntimeEngine>,
        runtime_handle: tokio::runtime::Handle,
    ) -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        Self {
            runtime,
            runtime_handle,
            tx,
            rx,
            // Backdated so the first `due()` after startup is true and the
            // first frame with a container already has numbers coming.
            last_poll: Instant::now() - POLL_INTERVAL,
            in_flight: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    /// Whether a new round is due. The caller checks this before assembling
    /// its requests, which is often the expensive part.
    pub fn due(&self) -> bool {
        self.last_poll.elapsed() >= POLL_INTERVAL
    }

    /// A sender for this sampler's channel.
    ///
    /// Only interesting to a test that wants to feed a sample in without a
    /// runtime; production code goes through [`poll`](Self::poll).
    pub fn sender(&self) -> Sender<StatsSample> {
        self.tx.clone()
    }

    /// Dispatch one round. Each request runs on the blocking pool, because a
    /// container runtime's `stats` shells out to its CLI and must not occupy
    /// an async worker.
    ///
    /// Requests for a slot whose previous query has not returned are skipped;
    /// the running one keeps that slot's turn.
    pub fn poll(&mut self, requests: impl IntoIterator<Item = StatsRequest>) {
        self.last_poll = Instant::now();
        for request in requests {
            let key = (request.key, request.step_name.clone());
            match self.in_flight.lock() {
                Ok(mut guard) => {
                    if !guard.insert(key.clone()) {
                        continue;
                    }
                }
                Err(_) => continue,
            }
            let runtime = self.runtime.clone();
            let tx = self.tx.clone();
            let in_flight = self.in_flight.clone();
            self.runtime_handle.spawn_blocking(move || {
                let stats = match &request.target {
                    // One runtime call, not the list-then-find-then-stats
                    // three the fallback costs.
                    StatsTarget::Named(name) => runtime.stats_by_name(name).ok(),
                    StatsTarget::FirstRunning => runtime
                        .list_running_all()
                        .ok()
                        .and_then(|handles| handles.first().cloned())
                        .and_then(|handle| runtime.stats(&handle).ok()),
                };
                if let Some(stats) = stats {
                    let _ = tx.send(StatsSample {
                        key: request.key,
                        step_name: request.step_name,
                        stats,
                    });
                }
                if let Ok(mut guard) = in_flight.lock() {
                    guard.remove(&key);
                }
            });
        }
    }

    /// Every sample that has arrived since the last drain.
    pub fn drain(&self) -> Vec<StatsSample> {
        self.rx.try_iter().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(key: usize, step: &str) -> StatsSample {
        StatsSample {
            key,
            step_name: step.to_string(),
            stats: AgentStats {
                name: "awman-test".into(),
                cpu_percent: 1.0,
                memory_mb: 2.0,
            },
        }
    }

    fn make_sampler() -> ContainerStatsSampler {
        static RUNTIME: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
        let handle = RUNTIME
            .get_or_init(|| tokio::runtime::Runtime::new().unwrap())
            .handle()
            .clone();
        ContainerStatsSampler::new(
            Arc::new(crate::engine::container::ContainerRuntime::docker()),
            handle,
        )
    }

    #[test]
    fn a_fresh_sampler_is_due_immediately() {
        assert!(
            make_sampler().due(),
            "the first frame with a container must not wait a full interval"
        );
    }

    #[test]
    fn polling_resets_the_interval() {
        let mut sampler = make_sampler();
        sampler.poll([]);
        assert!(!sampler.due(), "a round just ran");
    }

    #[test]
    fn drain_returns_samples_in_arrival_order_and_then_nothing() {
        let sampler = make_sampler();
        let tx = sampler.sender();
        tx.send(sample(0, "build")).unwrap();
        tx.send(sample(1, "test")).unwrap();

        let drained = sampler.drain();
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0].step_name, "build");
        assert_eq!(drained[1].step_name, "test");
        assert!(
            sampler.drain().is_empty(),
            "a drained sample is not re-read"
        );
    }
}
