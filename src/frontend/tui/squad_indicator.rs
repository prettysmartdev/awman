//! The bottom-row squad health indicator (WI 0112 Part 2).
//!
//! An app-level poller — independent of the squad tab, which may never have
//! been opened, and of the tab's own `SquadTaskPoller`, which fetches only
//! while that tab is focused — probes the squad daemon every ten seconds and
//! publishes the result for the renderer to paint a coloured `●` for on every
//! tab.
//!
//! The poller holds no policy. *What the daemon's state is* is
//! `SquadGatewayResolver::health_from_env`, classified by
//! `SquadSupervisor::health` at Layer 1 (WI 0114 F-17) — including the case
//! where no resolver can be built at all. What is left here is the cadence,
//! the shared slot and — in `render::command_box` — the colour.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::command::commands::squad::supervisor::SquadGatewayResolver;
use crate::data::config::env::Env;
use crate::engine::squad::SquadHealth;

/// How often the indicator re-probes the daemon.
pub const SQUAD_INDICATOR_INTERVAL: Duration = Duration::from_secs(10);
/// How long one probe may take before it is reported as unreachable. Keeps a
/// hung daemon from stacking probes: at most one is ever in flight.
pub const SQUAD_INDICATOR_PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// Cross-thread handle: the poller writes, the renderer reads.
pub type SharedSquadIndicator = Arc<Mutex<SquadHealth>>;

/// The background probe. Started once per TUI process from `tui::run`, never
/// from `App::new`, so unit-test apps never touch `~/.awman/squad`.
pub struct SquadIndicatorPoller {
    shared: SharedSquadIndicator,
}

impl SquadIndicatorPoller {
    pub fn new(shared: SharedSquadIndicator) -> Self {
        Self { shared }
    }

    /// Ticks every [`SQUAD_INDICATOR_INTERVAL`] until `cancel` fires. The
    /// first probe runs immediately so the indicator leaves `Unknown` on the
    /// first tick rather than ten seconds in.
    ///
    /// `Env` is re-read on every tick so a key minted mid-session (published
    /// by the supervisor) is picked up without a restart.
    pub fn start(self, cancel: CancellationToken) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(SQUAD_INDICATOR_INTERVAL);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    _ = ticker.tick() => {
                        let state = SquadGatewayResolver::health_from_env(
                            &Env::from_process(),
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
