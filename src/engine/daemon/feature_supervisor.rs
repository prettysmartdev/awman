//! Isolated feature crash recovery and lifecycle transitions.
use super::{
    core::Core,
    feature::{DaemonFeature, FeatureTask},
};
use crate::data::daemon_state::{
    EnabledBy, FeatureName, FeatureState, FeatureStatus, StopMode, SupervisedState,
};
use crate::engine::error::EngineError;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
#[derive(Debug, Clone)]
pub struct Backoff {
    pub initial: Duration,
    pub max: Duration,
    pub reset_after: Duration,
}
impl Default for Backoff {
    fn default() -> Self {
        Self {
            initial: Duration::from_secs(1),
            max: Duration::from_secs(60),
            reset_after: Duration::from_secs(300),
        }
    }
}
#[derive(Clone)]
struct Run {
    stop: CancellationToken,
    now: CancellationToken,
    task: Arc<tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>>,
}
pub struct FeatureSupervisor {
    feature: Arc<dyn DaemonFeature>,
    core: Arc<Core>,
    state: watch::Sender<SupervisedState>,
    run: Mutex<Option<Run>>,
    transition: tokio::sync::Mutex<()>,
    backoff: Backoff,
    pending_enable: Mutex<Option<(EnabledBy, Option<String>)>>,
}
impl FeatureSupervisor {
    pub fn new(feature: Arc<dyn DaemonFeature>, core: Arc<Core>) -> Arc<Self> {
        Self::new_with_backoff(feature, core, Backoff::default())
    }
    pub fn new_with_backoff(
        feature: Arc<dyn DaemonFeature>,
        core: Arc<Core>,
        backoff: Backoff,
    ) -> Arc<Self> {
        let (state, _) = watch::channel(SupervisedState::default());
        Arc::new(Self {
            feature,
            core,
            state,
            run: Mutex::new(None),
            transition: tokio::sync::Mutex::new(()),
            backoff,
            pending_enable: Mutex::new(None),
        })
    }
    pub fn with_backoff(mut self, backoff: Backoff) -> Self {
        self.backoff = backoff;
        self
    }
    #[cfg(any(test, feature = "test-support"))]
    pub fn with_started_task(
        feature: Arc<dyn DaemonFeature>,
        core: Arc<Core>,
        task: FeatureTask,
    ) -> Arc<Self> {
        let supervisor = Self::new(feature, core);
        supervisor
            .core
            .executor()
            .set_share(supervisor.name(), supervisor.feature.share());
        supervisor.state.send_modify(|s| {
            s.state = FeatureState::On;
            s.enabled_by = Some(EnabledBy::Config);
        });
        let stop = CancellationToken::new();
        let watcher = tokio::spawn(watch_feature(
            task,
            supervisor.feature.clone(),
            supervisor.core.clone(),
            supervisor.state.clone(),
            stop.clone(),
            supervisor.backoff.clone(),
        ));
        *supervisor.run.lock().unwrap() = Some(Run {
            stop,
            now: CancellationToken::new(),
            task: Arc::new(tokio::sync::Mutex::new(Some(watcher))),
        });
        supervisor
    }
    pub fn name(&self) -> FeatureName {
        self.feature.name()
    }
    pub async fn prepare_enable(&self) -> Result<(), EngineError> {
        if !self.is_enabled() {
            self.feature.prepare_start(&self.core).await?;
        }
        Ok(())
    }
    pub async fn enable(&self, by: EnabledBy, reason: Option<String>) -> Result<(), EngineError> {
        let _transition = self.transition.lock().await;
        if self.state().state == FeatureState::Draining {
            *self.pending_enable.lock().unwrap() = Some((by, reason));
            return Ok(());
        }
        self.enable_locked(by, reason, true).await
    }
    /// Start services without letting their queues claim work before commit.
    pub(crate) async fn enable_staged(&self, by: EnabledBy) -> Result<(), EngineError> {
        let _transition = self.transition.lock().await;
        self.enable_locked(by, None, false).await
    }
    async fn enable_locked(
        &self,
        by: EnabledBy,
        reason: Option<String>,
        admit_work: bool,
    ) -> Result<(), EngineError> {
        if self.core.shutdown().is_cancelled() {
            return Err(EngineError::Other("daemon is shutting down".into()));
        }
        if self.is_enabled() {
            if by == EnabledBy::Config {
                self.state.send_modify(|s| {
                    s.enabled_by = Some(by);
                    s.reason = None;
                    if s.state == FeatureState::Ephemeral {
                        s.state = FeatureState::On;
                    }
                });
                tracing::info!(feature = %self.name(), "daemon feature now enabled by config");
            }
            return Ok(());
        }
        self.feature.prepare_start(&self.core).await?;
        self.core.executor().set_share(
            self.name(),
            if admit_work { self.feature.share() } else { 0 },
        );
        let task = match self.feature.start(&self.core).await {
            Ok(task) => task,
            Err(e) => {
                self.core.executor().set_share(self.name(), 0);
                tracing::warn!(feature = %self.name(), error = %e, "daemon feature failed to start");
                return Err(e);
            }
        };
        self.state.send_modify(|s| {
            s.state = if by == EnabledBy::Config {
                FeatureState::On
            } else {
                FeatureState::Ephemeral
            };
            s.enabled_by = Some(by);
            s.reason = reason;
            s.since = chrono::Utc::now();
            s.last_error = None;
        });
        tracing::info!(
            feature = %self.name(),
            enabled_by = enabled_by_label(by),
            "daemon feature started"
        );
        let stop = CancellationToken::new();
        let watcher = tokio::spawn(watch_feature(
            task,
            self.feature.clone(),
            self.core.clone(),
            self.state.clone(),
            stop.clone(),
            self.backoff.clone(),
        ));
        *self.run.lock().unwrap() = Some(Run {
            stop,
            now: CancellationToken::new(),
            task: Arc::new(tokio::sync::Mutex::new(Some(watcher))),
        });
        Ok(())
    }
    /// Begin an owned transition. Dropping the caller never abandons a drain.
    pub async fn disable(self: &Arc<Self>, mode: StopMode) {
        let _transition = self.transition.lock().await;
        self.pending_enable.lock().unwrap().take();
        if !self.is_enabled() {
            return;
        }
        let run = self
            .run
            .lock()
            .unwrap()
            .clone()
            .expect("enabled feature has a run");
        if mode == StopMode::Now {
            run.now.cancel();
        }
        if self.state().state == FeatureState::Draining {
            return;
        }
        self.state.send_modify(|s| {
            s.state = FeatureState::Draining;
            s.since = chrono::Utc::now();
        });
        tracing::info!(
            feature = %self.name(),
            mode = if mode == StopMode::Now { "now" } else { "drain" },
            "daemon feature stopping"
        );
        // Suppress restart before invoking feature drain. The watcher keeps
        // awaiting the feature task so drain cannot orphan a running task.
        run.stop.cancel();
        let supervisor = self.clone();
        tokio::spawn(async move {
            let stopped_now = tokio::select! {
                biased;
                _ = run.now.cancelled() => { supervisor.feature.stop_now().await; true },
                _ = supervisor.feature.drain() => false
            };
            if let Some(mut task) = run.task.lock().await.take() {
                if !stopped_now {
                    tokio::select! {
                        _ = &mut task => {},
                        _ = run.now.cancelled() => { supervisor.feature.stop_now().await; let _ = task.await; }
                    }
                } else {
                    let _ = task.await;
                }
            }
            let _transition = supervisor.transition.lock().await;
            supervisor.run.lock().unwrap().take();
            supervisor.core.executor().set_share(supervisor.name(), 0);
            supervisor.state.send_modify(|s| {
                s.state = FeatureState::Off;
                s.enabled_by = None;
                s.reason = None;
                s.since = chrono::Utc::now();
            });
            tracing::info!(feature = %supervisor.name(), "daemon feature stopped");
            let pending = supervisor.pending_enable.lock().unwrap().take();
            if let Some((by, reason)) = pending {
                if let Err(error) = supervisor.enable_locked(by, reason, true).await {
                    supervisor
                        .state
                        .send_modify(|s| s.last_error = Some(error.to_string()));
                }
            }
        });
    }
    pub async fn wait_stopped(&self) {
        let mut state = self.subscribe();
        while state.borrow_and_update().state != FeatureState::Off {
            if state.changed().await.is_err() {
                break;
            }
        }
    }
    pub fn state(&self) -> SupervisedState {
        self.state.borrow().clone()
    }
    pub fn subscribe(&self) -> watch::Receiver<SupervisedState> {
        self.state.subscribe()
    }
    pub fn status(&self) -> FeatureStatus {
        self.feature.status()
    }
    pub fn is_enabled(&self) -> bool {
        self.state.borrow().state != FeatureState::Off
    }
    pub fn enabled_by(&self) -> Option<EnabledBy> {
        self.state.borrow().enabled_by
    }
}
fn enabled_by_label(by: EnabledBy) -> &'static str {
    match by {
        EnabledBy::Config => "config",
        EnabledBy::Ephemeral => "ephemeral",
    }
}
async fn watch_feature(
    mut task: FeatureTask,
    feature: Arc<dyn DaemonFeature>,
    core: Arc<Core>,
    state: watch::Sender<SupervisedState>,
    stop: CancellationToken,
    backoff: Backoff,
) {
    let mut delay = backoff.initial;
    loop {
        let mut healthy = false;
        let result = tokio::select! {
            result=&mut task=>result,
            _=tokio::time::sleep(backoff.reset_after)=>{
                healthy=true;
                delay=backoff.initial;
                if state.borrow().state == FeatureState::Degraded {
                    tracing::info!(feature = %feature.name(), "daemon feature recovered");
                }
                state.send_modify(|s| { if s.state==FeatureState::Degraded { s.state=if s.enabled_by==Some(EnabledBy::Config) { FeatureState::On } else { FeatureState::Ephemeral }; s.last_error=None; s.since=chrono::Utc::now(); } });
                task.await
            }
        };
        if stop.is_cancelled() {
            return;
        }
        if healthy {
            delay = backoff.initial;
        }
        let error = match result {
            Ok(Ok(())) => "feature exited unexpectedly".into(),
            Ok(Err(e)) => e.to_string(),
            Err(e) => e.to_string(),
        };
        tracing::error!(
            feature = %feature.name(),
            %error,
            retry_in_ms = delay.as_millis() as u64,
            "daemon feature crashed; restarting"
        );
        state.send_modify(|s| {
            s.state = FeatureState::Degraded;
            s.last_error = Some(error);
            s.restarts = s.restarts.saturating_add(1);
            s.since = chrono::Utc::now();
        });
        loop {
            tokio::select! { _=stop.cancelled()=>return, _=tokio::time::sleep(delay)=>{} }
            delay = delay.saturating_mul(2).min(backoff.max);
            let started = tokio::select! { biased; _=stop.cancelled()=>return, result=async {
                feature.prepare_start(&core).await?;
                feature.start(&core).await
            }=>result };
            match started {
                Ok(next) => {
                    tracing::info!(feature = %feature.name(), "daemon feature restarted");
                    task = next;
                    break;
                }
                Err(e) => {
                    tracing::warn!(feature = %feature.name(), error = %e, "daemon feature restart failed");
                    state.send_modify(|s| {
                        s.last_error = Some(e.to_string());
                        s.restarts = s.restarts.saturating_add(1);
                    })
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::Notify;
    struct Fake {
        name: FeatureName,
        starts: AtomicUsize,
        crashes: usize,
        stop: Mutex<CancellationToken>,
        finish: Arc<Notify>,
        draining: Arc<Notify>,
    }
    impl Fake {
        fn new(name: FeatureName, crashes: usize) -> Arc<Self> {
            Arc::new(Self {
                name,
                starts: AtomicUsize::new(0),
                crashes,
                stop: Mutex::new(CancellationToken::new()),
                finish: Arc::new(Notify::new()),
                draining: Arc::new(Notify::new()),
            })
        }
    }
    #[async_trait::async_trait]
    impl DaemonFeature for Fake {
        fn name(&self) -> FeatureName {
            self.name
        }
        async fn start(&self, _: &Core) -> Result<FeatureTask, EngineError> {
            let n = self.starts.fetch_add(1, Ordering::SeqCst);
            let stop = CancellationToken::new();
            *self.stop.lock().unwrap() = stop.clone();
            let finish = self.finish.clone();
            let crashes = self.crashes;
            Ok(tokio::spawn(async move {
                if n < crashes {
                    panic!("injected feature crash");
                }
                stop.cancelled().await;
                finish.notified().await;
                Ok(())
            }))
        }
        async fn drain(&self) {
            self.stop.lock().unwrap().cancel();
            self.draining.notify_one();
        }
        async fn stop_now(&self) {
            self.stop.lock().unwrap().cancel();
            self.finish.notify_one();
        }
        fn status(&self) -> FeatureStatus {
            FeatureStatus::default()
        }
        fn share(&self) -> usize {
            1
        }
    }
    async fn state_is(supervisor: &FeatureSupervisor, want: FeatureState) {
        let mut rx = supervisor.subscribe();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if rx.borrow_and_update().state == want {
                    break;
                }
                rx.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
    }
    #[tokio::test]
    async fn panic_restarts_degraded_and_other_feature_keeps_running() {
        let dir = tempfile::tempdir().unwrap();
        let core = super::super::core::test_core(dir.path());
        let bad = Fake::new(FeatureName::Worker, 1);
        let good = Fake::new(FeatureName::Squad, 0);
        let backoff = Backoff {
            initial: Duration::from_millis(20),
            max: Duration::from_millis(40),
            reset_after: Duration::from_millis(20),
        };
        let a = FeatureSupervisor::new_with_backoff(bad.clone(), core.clone(), backoff);
        let b = FeatureSupervisor::new(good.clone(), core);
        a.enable(EnabledBy::Config, None).await.unwrap();
        b.enable(EnabledBy::Config, None).await.unwrap();
        state_is(&a, FeatureState::Degraded).await;
        assert_eq!(a.state().restarts, 1);
        assert!(a.state().last_error.is_some());
        assert_eq!(b.state().state, FeatureState::On);
        state_is(&a, FeatureState::On).await;
        assert_eq!(bad.starts.load(Ordering::SeqCst), 2);
        assert_eq!(good.starts.load(Ordering::SeqCst), 1);
        a.disable(StopMode::Now).await;
        b.disable(StopMode::Now).await;
    }
    #[tokio::test]
    async fn drain_waits_for_in_flight_and_stop_now_cancels_it() {
        let dir = tempfile::tempdir().unwrap();
        let core = super::super::core::test_core(dir.path());
        let feature = Fake::new(FeatureName::Worker, 0);
        let supervisor = FeatureSupervisor::new(feature.clone(), core);
        supervisor.enable(EnabledBy::Config, None).await.unwrap();
        let cloned = supervisor.clone();
        let draining = tokio::spawn(async move {
            cloned.disable(StopMode::Drain).await;
        });
        feature.draining.notified().await;
        assert_eq!(supervisor.state().state, FeatureState::Draining);
        draining.await.unwrap();
        feature.finish.notify_one();
        supervisor.wait_stopped().await;
        state_is(&supervisor, FeatureState::Off).await;
        assert_eq!(supervisor.state().state, FeatureState::Off);
        supervisor
            .enable(EnabledBy::Ephemeral, Some("test".into()))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), supervisor.disable(StopMode::Now))
            .await
            .unwrap();
        state_is(&supervisor, FeatureState::Off).await;
        assert_eq!(supervisor.state().state, FeatureState::Off);
    }
    #[tokio::test]
    async fn disabling_during_backoff_prevents_restart() {
        let dir = tempfile::tempdir().unwrap();
        let core = super::super::core::test_core(dir.path());
        let feature = Fake::new(FeatureName::Worker, usize::MAX);
        let supervisor = FeatureSupervisor::new_with_backoff(
            feature.clone(),
            core,
            Backoff {
                initial: Duration::from_millis(100),
                max: Duration::from_millis(100),
                reset_after: Duration::from_secs(1),
            },
        );
        supervisor.enable(EnabledBy::Ephemeral, None).await.unwrap();
        state_is(&supervisor, FeatureState::Degraded).await;
        supervisor.disable(StopMode::Now).await;
        tokio::time::sleep(Duration::from_millis(120)).await;
        assert_eq!(feature.starts.load(Ordering::SeqCst), 1);
        state_is(&supervisor, FeatureState::Off).await;
        assert_eq!(supervisor.state().state, FeatureState::Off);
    }
    #[tokio::test]
    async fn timed_out_drain_can_be_escalated_without_losing_the_task() {
        let dir = tempfile::tempdir().unwrap();
        let core = super::super::core::test_core(dir.path());
        let feature = Fake::new(FeatureName::Worker, 0);
        let supervisor = FeatureSupervisor::new(feature, core);
        supervisor.enable(EnabledBy::Config, None).await.unwrap();
        supervisor.disable(StopMode::Drain).await;
        assert!(
            tokio::time::timeout(Duration::from_millis(10), supervisor.wait_stopped())
                .await
                .is_err()
        );
        assert_eq!(supervisor.state().state, FeatureState::Draining);
        tokio::time::timeout(Duration::from_secs(1), supervisor.disable(StopMode::Now))
            .await
            .unwrap();
        state_is(&supervisor, FeatureState::Off).await;
        assert_eq!(supervisor.state().state, FeatureState::Off);
    }
    #[tokio::test]
    async fn reenable_during_owned_drain_starts_again_after_caller_is_gone() {
        let dir = tempfile::tempdir().unwrap();
        let core = super::super::core::test_core(dir.path());
        let feature = Fake::new(FeatureName::Worker, 0);
        let supervisor = FeatureSupervisor::new(feature.clone(), core);
        supervisor.enable(EnabledBy::Config, None).await.unwrap();
        supervisor.disable(StopMode::Drain).await;
        feature.draining.notified().await;
        supervisor.enable(EnabledBy::Config, None).await.unwrap();
        feature.finish.notify_one();
        state_is(&supervisor, FeatureState::On).await;
        assert_eq!(feature.starts.load(Ordering::SeqCst), 2);
        supervisor.disable(StopMode::Now).await;
        supervisor.wait_stopped().await;
    }
}
