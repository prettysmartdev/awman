//! Squad feature over Core's shared task store and executor.
use crate::command::{commands::squad::gateway::LocalTaskGateway, dispatch::Engines};
use crate::data::{
    config::daemon::SquadSettings,
    fs::{
        daemon_env::{EnvPersistence, EnvPersistenceSetting},
        SquadPaths,
    },
};
use crate::engine::{
    daemon::{core::Core, feature::*},
    error::EngineError,
    squad::{
        env_state::DaemonEnvState,
        env_store,
        scheduler::{SchedulerStatus, SquadScheduler},
        TaskEvaluator,
    },
};
use std::sync::{Arc, Mutex, RwLock};
use tokio_util::sync::CancellationToken;
pub struct SquadShared {
    pub gateway: Arc<LocalTaskGateway>,
    pub env_state: Arc<DaemonEnvState>,
    pub scheduler_status: Arc<Mutex<SchedulerStatus>>,
}
pub struct SquadFeature {
    shared: Arc<SquadShared>,
    engines: Engines,
    evaluator: Arc<dyn TaskEvaluator>,
    settings: RwLock<SquadSettings>,
    stop: Mutex<Option<CancellationToken>>,
    done: Mutex<Option<CancellationToken>>,
    paths: SquadPaths,
    initialized: tokio::sync::Mutex<bool>,
    store: Arc<crate::data::fs::task_store::TaskStore>,
}
impl SquadFeature {
    /// Build shared gateway handles before enabling the scheduler. Persistence
    /// is resolved on the first enablement and survives feature restarts.
    pub fn new(
        core: &Core,
        engines: Engines,
        evaluator: Arc<dyn TaskEvaluator>,
        settings: SquadSettings,
    ) -> Result<Arc<Self>, EngineError> {
        let paths = SquadPaths::from_env(core.env())?;
        let env_state = Arc::new(DaemonEnvState::without_store());
        let status = Arc::new(Mutex::new(SchedulerStatus::default()));
        let gateway = Arc::new(
            LocalTaskGateway::new(
                core.task_store().clone(),
                engines.clone(),
                status.clone(),
                paths.clone(),
                env_state.clone(),
            )
            .with_env(core.env().clone()),
        );
        Ok(Arc::new(Self {
            shared: Arc::new(SquadShared {
                gateway,
                env_state,
                scheduler_status: status,
            }),
            engines,
            evaluator,
            settings: RwLock::new(settings),
            stop: Mutex::new(None),
            done: Mutex::new(None),
            paths,
            initialized: tokio::sync::Mutex::new(false),
            store: core.task_store().clone(),
        }))
    }
    pub fn shared(&self) -> &Arc<SquadShared> {
        &self.shared
    }
    pub async fn apply_settings(&self, s: SquadSettings) {
        let initialized = self.initialized.lock().await;
        let changed = self.settings.read().unwrap().env_persistence != s.env_persistence;
        if *initialized && changed {
            self.configure_persistence(s.env_persistence).await;
        }
        *self.settings.write().unwrap() = s;
    }
    async fn configure_persistence(&self, setting: EnvPersistenceSetting) {
        let state = self.shared.env_state.clone();
        let had_backend = state.persistence() != EnvPersistence::None;
        tokio::task::spawn_blocking(move || {
            // Live opt-out clears the old backend behind replace_store's write
            // barrier. On first enable, resolve(None) also clears an item left
            // by a previous process, without loading its values.
            let resolved = if setting == EnvPersistenceSetting::None && had_backend {
                env_store::ResolvedEnvStore {
                    store: Box::new(crate::data::fs::daemon_env::NoStore),
                    fallback: None,
                    probed: None,
                }
            } else {
                env_store::resolve(&crate::data::config::SquadConfig {
                    env_persistence: Some(setting),
                    ..Default::default()
                })
            };
            let persistence = match resolved.fallback {
                Some(reason) => {
                    tracing::warn!("{}", reason.startup_warning());
                    EnvPersistence::Unavailable(reason.to_string())
                }
                None if setting == EnvPersistenceSetting::None => EnvPersistence::None,
                None => EnvPersistence::Keychain,
            };
            state.replace_store(resolved.store, persistence);
            if let Some(loaded) = resolved.probed {
                state.load_from_store(Some(loaded));
            }
        })
        .await
        .expect("persistence transition task panicked");
    }
    pub fn cancel_all(&self) -> Vec<String> {
        let status = self.shared.scheduler_status.lock().unwrap();
        status
            .cancellations
            .values()
            .map(|c| {
                c.cancel();
                c.run_id.to_string()
            })
            .collect()
    }
}
#[async_trait::async_trait]
impl DaemonFeature for SquadFeature {
    fn name(&self) -> FeatureName {
        FeatureName::Squad
    }
    async fn prepare_start(&self, core: &Core) -> Result<(), EngineError> {
        if self.engines.container_runtime.is_none() {
            return Err(EngineError::SquadRuntimeUnsupported {
                runtime: core.runtime().runtime_name().to_string(),
            });
        }
        core.task_store()
            .reconcile_orphaned_runs(chrono::Utc::now())?;
        let mut initialized = self.initialized.lock().await;
        if !*initialized {
            let setting = self.settings.read().unwrap().env_persistence;
            self.configure_persistence(setting).await;
            if let Err(error) = self.shared.gateway.refresh_required_env().await {
                tracing::debug!(%error, "initial squad env refresh failed");
            }
            *initialized = true;
        }
        Ok(())
    }
    async fn start(&self, core: &Core) -> Result<FeatureTask, EngineError> {
        let stop = CancellationToken::new();
        *self.stop.lock().unwrap() = Some(stop.clone());
        let done = CancellationToken::new();
        *self.done.lock().unwrap() = Some(done.clone());
        let done_guard = done.drop_guard();
        let scheduler = SquadScheduler::new(
            core.task_store().clone(),
            self.paths.clone(),
            self.evaluator.clone(),
            core.env().clone(),
        )
        .with_runtime(core.runtime().clone())
        .with_env_state(self.shared.env_state.clone())
        .with_status(self.shared.scheduler_status.clone())
        .with_executor(core.executor().clone());
        Ok(tokio::spawn(async move {
            let _done = done_guard;
            scheduler.run(stop).await;
            Ok(())
        }))
    }
    async fn drain(&self) {
        if let Some(stop) = self.stop.lock().unwrap().take() {
            stop.cancel();
        }
        let done = self.done.lock().unwrap().clone();
        if let Some(done) = done {
            done.cancelled().await;
        }
    }
    async fn stop_now(&self) {
        self.cancel_all();
        self.drain().await;
    }
    fn status(&self) -> FeatureStatus {
        let status = self.shared.scheduler_status.lock().unwrap();
        let tasks = self.store.list().unwrap_or_default();
        FeatureStatus {
            running: status.in_flight,
            queued: status.pending_runs.len(),
            oldest_queued_age: None,
            detail: Some(serde_json::json!({
                "scheduler_last_tick_at": status.last_tick,
                "tick_count": status.tick_count,
                "config_error": status.config_error,
                "env_persistence": self.shared.env_state.persistence(),
                "unmet_env": self.shared.env_state.unmet_names(),
                "task_count": tasks.len(),
                "active_count": tasks.iter().filter(|t| t.status == crate::data::fs::task_store::TaskStatus::Active).count(),
                "in_flight": status.in_flight,
            })),
        }
    }
    fn share(&self) -> usize {
        self.settings.read().unwrap().max_concurrent_jobs
    }
}
