//! Engine feature slot; instance and frontend counters are extended in 0142.
use crate::data::config::daemon::EngineSettings;
use crate::engine::{
    daemon::{core::Core, feature::*},
    error::EngineError,
};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;
pub struct EngineFeature {
    settings: RwLock<EngineSettings>,
    idle_since: Mutex<Option<Instant>>,
    task: Mutex<Option<CancellationToken>>,
    done: Mutex<Option<CancellationToken>>,
}
impl EngineFeature {
    pub fn new(settings: EngineSettings) -> Arc<Self> {
        Arc::new(Self {
            settings: RwLock::new(settings),
            idle_since: Mutex::new(None),
            task: Mutex::new(None),
            done: Mutex::new(None),
        })
    }
    pub fn idle_for(&self) -> Option<Duration> {
        self.idle_since.lock().unwrap().map(|t| t.elapsed())
    }
    pub fn idle_timeout(&self) -> Duration {
        self.settings.read().unwrap().idle_timeout
    }
    pub fn allow_ephemeral(&self) -> bool {
        self.settings.read().unwrap().allow_ephemeral
    }
    pub fn apply_settings(&self, settings: EngineSettings) {
        *self.settings.write().unwrap() = settings;
    }
}
#[async_trait::async_trait]
impl DaemonFeature for EngineFeature {
    fn name(&self) -> FeatureName {
        FeatureName::Engine
    }
    async fn start(&self, _: &Core) -> Result<FeatureTask, EngineError> {
        let stop = CancellationToken::new();
        *self.task.lock().unwrap() = Some(stop.clone());
        let done = CancellationToken::new();
        *self.done.lock().unwrap() = Some(done.clone());
        let done_guard = done.drop_guard();
        *self.idle_since.lock().unwrap() = Some(Instant::now());
        Ok(tokio::spawn(async move {
            let _done = done_guard;
            stop.cancelled().await;
            Ok(())
        }))
    }
    async fn drain(&self) {
        self.stop_now().await;
    }
    async fn stop_now(&self) {
        if let Some(stop) = self.task.lock().unwrap().take() {
            stop.cancel();
        }
        *self.idle_since.lock().unwrap() = None;
        let done = self.done.lock().unwrap().clone();
        if let Some(done) = done {
            done.cancelled().await;
        }
    }
    fn status(&self) -> FeatureStatus {
        FeatureStatus::default()
    }
    fn share(&self) -> usize {
        self.settings.read().unwrap().max_concurrent_jobs
    }
}
