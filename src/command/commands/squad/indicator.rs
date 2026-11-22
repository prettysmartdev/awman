//! The squad health indicator poller (WI 0112 Part 2; Layer 2 since WI 0142).
//!
//! An instance-level poller — independent of the squad tab, which may never
//! have been opened, and of the tab's own `SquadTaskPoller`, which fetches only
//! while that tab is focused — probes the squad daemon every ten seconds and
//! publishes the result for a presenter to paint (the TUI draws a coloured `●`
//! on every tab).
//!
//! The poller holds no policy. *What the daemon's state is* is
//! `DaemonResolver::health_from_env`, classified by
//! `SquadSupervisor::health` at Layer 1 (WI 0114 F-17) — including the case
//! where no resolver can be built at all. What is left here is the cadence and
//! the shared slot.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::command::commands::squad::supervisor::DaemonResolver;
use crate::data::config::env::EnvSnapshot;
use crate::engine::squad::SquadHealth;

/// How often the indicator re-probes the daemon.
pub const SQUAD_INDICATOR_INTERVAL: Duration = Duration::from_secs(10);
/// How long one probe may take before it is reported as unreachable. Keeps a
/// hung daemon from stacking probes: at most one is ever in flight.
pub const SQUAD_INDICATOR_PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// Cross-thread handle: the poller writes, the presenter reads.
pub type SharedSquadHealth = Arc<Mutex<SquadHealth>>;

/// Where each probe reads the instance environment from. The builtin TUI
/// re-captures the process environment on every tick so a key minted
/// mid-session (published by the supervisor) is picked up without a restart;
/// a hosted instance supplies its frontend's snapshot.
pub type EnvSource = Arc<dyn Fn() -> EnvSnapshot + Send + Sync>;

/// The background probe. Started once per instance by its host (for the
/// builtin TUI, `tui::run`), never by a unit-test app, so tests never touch
/// `~/.awman/squad`.
pub struct SquadIndicatorPoller {
    shared: SharedSquadHealth,
    env: EnvSource,
}

impl SquadIndicatorPoller {
    pub fn new(shared: SharedSquadHealth, env: EnvSource) -> Self {
        Self { shared, env }
    }

    /// Ticks every [`SQUAD_INDICATOR_INTERVAL`] until `cancel` fires. The
    /// first probe runs immediately so the indicator leaves `Unknown` on the
    /// first tick rather than ten seconds in. The environment is read from
    /// the [`EnvSource`] on every tick.
    pub fn start(self, cancel: CancellationToken) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(SQUAD_INDICATOR_INTERVAL);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    _ = ticker.tick() => {
                        let env = (self.env)();
                        let state = DaemonResolver::health_from_env(
                            &env,
                            SQUAD_INDICATOR_PROBE_TIMEOUT,
                        )
                        .await;
                        if let Ok(mut guard) = self.shared.lock() {
                            *guard = state;
                        }
                    }
                }
            }
        })
    }
}
