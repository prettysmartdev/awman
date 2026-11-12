//! The `engine` daemon feature (WI 0142 Part A §1): a Layer 1
//! [`DaemonFeature`] whose only knowledge of instances is the [`EngineHost`]
//! hook below, which Layer 2's `EngineInstanceHost` implements. Nothing here
//! may import `Dispatch`, `src/command`, a TUI type, an HTTP router or a
//! presenter.
//!
//! The feature owns the reusable runtime mechanics only: its settings, the
//! idle clock (driven by [`EngineHost::activity`], so a frontend reconnecting
//! within its grace period keeps the engine alive), the executor share and
//! the drain/stop plumbing. Contract: `/awman/context/workflow/0142-design.md`
//! §4.6 (D5).

use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;

use crate::data::config::daemon::EngineSettings;
use crate::data::daemon_state::FeatureStatus;
use crate::engine::daemon::core::Core;
use crate::engine::daemon::feature::{DaemonFeature, FeatureName, FeatureTask};
use crate::engine::error::EngineError;

/// How often the feature task samples [`EngineHost::activity`] for the idle
/// clock. `idle_for` also samples, so the clock never lags a reader.
pub const ACTIVITY_SAMPLE_INTERVAL: Duration = Duration::from_millis(500);

/// What the engine is doing right now, as the idle policy sees it. The feature
/// is idle only when every field is zero; a frontend reconnecting within its
/// grace period counts as present (WI 0142 Part A, edge cases).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EngineActivity {
    pub frontends: usize,
    pub reconnecting_frontends: usize,
    pub instances: usize,
    pub running_jobs: usize,
    pub presenter_holds: usize,
}

impl EngineActivity {
    /// True when nothing at all keeps the engine busy.
    pub fn is_idle(&self) -> bool {
        self.frontends == 0
            && self.reconnecting_frontends == 0
            && self.instances == 0
            && self.running_jobs == 0
            && self.presenter_holds == 0
    }
}

/// Why the feature is asking its host to wind down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineStopReason {
    /// `daemon.features.engine.enabled` turned off, or the feature was cancelled.
    Disabled,
    /// An ephemeral engine passed `idleTimeout` with nothing to do.
    IdleTimeout,
    /// The daemon process is stopping.
    DaemonShutdown,
}

/// The Layer 1 → Layer 2 seam (grand architecture, Tenet 1). Layer 2 owns
/// instances, Dispatch, admission and exit policy; the feature only asks it
/// these questions and gives it these orders.
#[async_trait::async_trait]
pub trait EngineHost: Send + Sync + 'static {
    fn activity(&self) -> EngineActivity;
    /// Refuse new instances and frontends, let running commands finish, then
    /// detach every frontend with the reason. Resolves when no instance is left.
    async fn drain(&self, reason: EngineStopReason);
    /// Cancel running commands and detach every frontend now.
    async fn stop_now(&self, reason: EngineStopReason);
    /// Accept instances and frontends again after a drain or stop (the
    /// feature was re-enabled).
    fn resume(&self) {}
    fn status(&self) -> FeatureStatus;
    fn apply_settings(&self, settings: &EngineSettings);
    fn set_executor_share(&self, share: usize);
}

/// An [`EngineHost`] that hosts nothing: used where no Layer 2 host is wired
/// (unit tests of the daemon lifecycle).
pub struct NoopEngineHost;

#[async_trait::async_trait]
impl EngineHost for NoopEngineHost {
    fn activity(&self) -> EngineActivity {
        EngineActivity::default()
    }
    async fn drain(&self, _reason: EngineStopReason) {}
    async fn stop_now(&self, _reason: EngineStopReason) {}
    fn status(&self) -> FeatureStatus {
        FeatureStatus::default()
    }
    fn apply_settings(&self, _settings: &EngineSettings) {}
    fn set_executor_share(&self, _share: usize) {}
}

/// When the running feature last became idle. `None` outside a run; inside
/// a run, `Some(None)` while busy and `Some(Some(t))` idle since `t`.
#[derive(Default)]
struct IdleClock(Mutex<Option<Option<Instant>>>);

impl IdleClock {
    fn start(&self) {
        *self.0.lock().unwrap_or_else(|p| p.into_inner()) = Some(None);
    }

    fn stop(&self) {
        *self.0.lock().unwrap_or_else(|p| p.into_inner()) = None;
    }

    /// Fold one activity sample in; the idle time so far, if idle.
    fn sample(&self, host: &dyn EngineHost) -> Option<Duration> {
        let busy = !host.activity().is_idle();
        let mut clock = self.0.lock().unwrap_or_else(|p| p.into_inner());
        let running = clock.as_mut()?;
        if busy {
            *running = None;
            return None;
        }
        Some(running.get_or_insert_with(Instant::now).elapsed())
    }
}

/// The `engine` feature. Its task samples the host's activity for the idle
/// clock and waits for the stop token; it runs no commands itself.
pub struct EngineFeature {
    settings: RwLock<EngineSettings>,
    host: Arc<dyn EngineHost>,
    /// Shared with the feature task, which samples it.
    idle: Arc<IdleClock>,
    stop_reason: Mutex<EngineStopReason>,
    task: Mutex<Option<CancellationToken>>,
    done: Mutex<Option<CancellationToken>>,
}

impl EngineFeature {
    pub fn new(settings: EngineSettings, host: Arc<dyn EngineHost>) -> Arc<Self> {
        host.apply_settings(&settings);
        host.set_executor_share(settings.max_concurrent_jobs);
        Arc::new(Self {
            settings: RwLock::new(settings),
            host,
            idle: Arc::new(IdleClock::default()),
            stop_reason: Mutex::new(EngineStopReason::Disabled),
            task: Mutex::new(None),
            done: Mutex::new(None),
        })
    }

    pub fn host(&self) -> &Arc<dyn EngineHost> {
        &self.host
    }

    /// The host's current activity.
    pub fn activity(&self) -> EngineActivity {
        self.host.activity()
    }

    fn sample(&self) -> Option<Duration> {
        self.idle.sample(self.host.as_ref())
    }

    /// `Some(elapsed)` while the running feature has been idle, `None` while
    /// it is busy or not running.
    pub fn idle_for(&self) -> Option<Duration> {
        self.sample()
    }

    pub fn idle_timeout(&self) -> Duration {
        self.settings
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .idle_timeout
    }

    pub fn allow_ephemeral(&self) -> bool {
        self.settings
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .allow_ephemeral
    }

    /// Apply live settings and forward them (and the executor share) to the host.
    pub fn apply_settings(&self, settings: EngineSettings) {
        self.host.apply_settings(&settings);
        self.host.set_executor_share(settings.max_concurrent_jobs);
        *self.settings.write().unwrap_or_else(|p| p.into_inner()) = settings;
    }

    /// The reason the next drain/stop passes to the host (the daemon sets
    /// `DaemonShutdown` before stopping the process).
    pub fn set_stop_reason(&self, reason: EngineStopReason) {
        *self.stop_reason.lock().unwrap_or_else(|p| p.into_inner()) = reason;
    }

    fn take_stop_reason(&self) -> EngineStopReason {
        std::mem::replace(
            &mut *self.stop_reason.lock().unwrap_or_else(|p| p.into_inner()),
            EngineStopReason::Disabled,
        )
    }

    async fn end_task(&self) {
        if let Some(stop) = self.task.lock().unwrap_or_else(|p| p.into_inner()).take() {
            stop.cancel();
        }
        self.idle.stop();
        let done = self.done.lock().unwrap_or_else(|p| p.into_inner()).clone();
        if let Some(done) = done {
            done.cancelled().await;
        }
    }
}

#[async_trait::async_trait]
impl DaemonFeature for EngineFeature {
    fn name(&self) -> FeatureName {
        FeatureName::Engine
    }

    async fn start(&self, _core: &Core) -> Result<FeatureTask, EngineError> {
        self.host.resume();
        let stop = CancellationToken::new();
        *self.task.lock().unwrap_or_else(|p| p.into_inner()) = Some(stop.clone());
        let done = CancellationToken::new();
        *self.done.lock().unwrap_or_else(|p| p.into_inner()) = Some(done.clone());
        let done_guard = done.drop_guard();
        self.idle.start();
        self.sample();
        let host = Arc::clone(&self.host);
        let idle = Arc::clone(&self.idle);
        Ok(tokio::spawn(async move {
            let _done = done_guard;
            loop {
                tokio::select! {
                    _ = stop.cancelled() => return Ok(()),
                    _ = tokio::time::sleep(ACTIVITY_SAMPLE_INTERVAL) => {
                        // Keep the idle clock current even when nobody reads it.
                        idle.sample(host.as_ref());
                    }
                }
            }
        }))
    }

    async fn drain(&self) {
        let reason = self.take_stop_reason();
        self.host.drain(reason).await;
        self.end_task().await;
    }

    async fn stop_now(&self) {
        let reason = self.take_stop_reason();
        self.host.stop_now(reason).await;
        self.end_task().await;
    }

    fn status(&self) -> FeatureStatus {
        self.host.status()
    }

    fn share(&self) -> usize {
        self.settings
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .max_concurrent_jobs
    }
}
