//! Resource operations shared by the daemon transports and clients.
//! Feature engines provide handles and typed operations, never HTTP routers.
pub mod capabilities;
pub mod core;
pub mod operations;
pub mod squad;
pub mod worker;
pub use operations::{Feature, FeatureSet, FeatureState, Operation};

use crate::command::commands::squad::daemon_runtime::SquadDaemonHandles;
use crate::command::commands::squad::gateway::TaskGateway;
use crate::command::dispatch::Engines;
use crate::data::config::{
    effective::{ConfigLayers, EffectiveConfig},
    FlagConfig,
};
use crate::engine::auth::AuthMode;
use std::sync::{Arc, Mutex};
use std::time::Instant;

pub struct DaemonApi {
    pub worker: Option<Arc<worker::WorkerApi>>,
    pub squad: Option<Arc<dyn TaskGateway>>,
    pub squad_handles: Option<Arc<SquadDaemonHandles>>,
    pub engines: Engines,
    pub auth_mode: AuthMode,
    pub features: std::sync::RwLock<FeatureSet>,
    pub started_at: Instant,
    pub bound_addr: Mutex<Option<String>>,
    pub listener: Mutex<Vec<core::ListenerRecord>>,
    pub config: Mutex<EffectiveConfig>,
    initial_config: EffectiveConfig,
    pub grants: Mutex<crate::data::fs::prerequisite_grants::PrerequisiteGrantStore>,
}
impl DaemonApi {
    pub(crate) fn effective(engines: &Engines) -> EffectiveConfig {
        // Use the same captured global document that built the engine bundle.
        EffectiveConfig::resolve(
            ConfigLayers {
                global: (*engines.global_config).clone(),
                env: crate::data::config::effective::env_overrides(
                    &crate::data::config::env::Env::from_process(),
                    &[&engines.global_config],
                )
                .expect("validated env config"),
                ..Default::default()
            },
            FlagConfig::default(),
            crate::data::config::env::Env::from_process(),
        )
        .expect("daemon bootstrap validated its configuration")
    }
    pub fn from_worker(worker: Arc<worker::WorkerApi>) -> Self {
        let config = Self::effective(&worker.engines);
        Self::from_worker_with_config(worker, config)
    }
    pub fn from_worker_with_config(
        worker: Arc<worker::WorkerApi>,
        config: EffectiveConfig,
    ) -> Self {
        let push_enabled = config.daemon().worker.push_enabled;
        Self {
            engines: worker.engines.clone(),
            grants: Mutex::new(
                crate::data::fs::prerequisite_grants::PrerequisiteGrantStore::load(
                    worker.paths.data_paths().home(),
                ),
            ),
            auth_mode: worker.auth_mode.clone(),
            features: std::sync::RwLock::new(FeatureSet(
                [(
                    Feature::Worker,
                    if push_enabled {
                        FeatureState::On
                    } else {
                        FeatureState::Off
                    },
                )]
                .into(),
            )),
            started_at: worker.started_at,
            config: Mutex::new(config.clone()),
            initial_config: config,
            worker: Some(worker),
            squad: None,
            squad_handles: None,
            bound_addr: Mutex::new(None),
            listener: Mutex::new(Vec::new()),
        }
    }
    pub fn from_squad(handles: Arc<SquadDaemonHandles>) -> Self {
        let config = Self::effective(handles.engines());
        let squad_enabled = config.daemon().squad.enabled;
        Self {
            worker: None,
            grants: Mutex::new(
                crate::data::fs::prerequisite_grants::PrerequisiteGrantStore::load(
                    handles
                        .engines()
                        .auth_engine
                        .api_paths()
                        .data_paths()
                        .home(),
                ),
            ),
            squad: Some(handles.gateway()),
            engines: handles.engines().clone(),
            auth_mode: handles.auth_mode(),
            features: std::sync::RwLock::new(FeatureSet(
                [(
                    Feature::Squad,
                    if squad_enabled {
                        FeatureState::On
                    } else {
                        FeatureState::Off
                    },
                )]
                .into(),
            )),
            started_at: Instant::now(),
            config: Mutex::new(config.clone()),
            initial_config: config,
            squad_handles: Some(handles),
            bound_addr: Mutex::new(None),
            listener: Mutex::new(Vec::new()),
        }
    }
    pub fn reload_config(
        &self,
    ) -> Result<core::ReloadResponse, crate::command::error::CommandError> {
        let env = self.config.lock().expect("config mutex").env().clone();
        let layers = ConfigLayers::load(&env, None, self.initial_config.layers().set.clone())?;
        let candidate = EffectiveConfig::resolve(layers, FlagConfig::default(), env)?;
        let mut current = self.config.lock().expect("config mutex");
        let mut response = core::ReloadResponse::default();
        for key in crate::data::config::fields::CONFIG_KEYS {
            // Concrete leaves also cover named-map additions/removals below.
            for path in core::changed_paths(
                if key.live {
                    current.value()
                } else {
                    self.initial_config.value()
                },
                candidate.value(),
                key.path,
            ) {
                if key.live {
                    response.applied.push(path);
                } else {
                    response.restart_required.push(path);
                }
            }
        }
        if let Some(worker) = &self.worker {
            // A reload changes only live leaves in the running services. The
            // desired snapshot still reports restart-only edits to the caller.
            let mut live = self.initial_config.value().clone();
            for key in crate::data::config::fields::CONFIG_KEYS
                .iter()
                .filter(|key| key.live)
            {
                for path in
                    core::changed_paths(self.initial_config.value(), candidate.value(), key.path)
                {
                    crate::data::config::config_json::set_config_field(
                        &mut live,
                        &path,
                        core::at(candidate.value(), &path)
                            .cloned()
                            .unwrap_or(serde_json::Value::Null),
                    );
                }
            }
            let runtime = EffectiveConfig::resolve(
                ConfigLayers {
                    global: crate::data::config::ConfigFile::from_value(
                        live,
                        crate::data::config::FileKind::Global,
                    )?,
                    ..Default::default()
                },
                FlagConfig::default(),
                candidate.env().clone(),
            )?;
            if let Some(tx) = &worker.settings_tx {
                let _ = tx.send(runtime.daemon());
            }
            *worker.live_config.write().expect("worker config lock") = Some(runtime);
        }
        *current = candidate;
        let live_settings = current.daemon();
        let mut features = self.features.write().expect("feature set lock");
        if let Some(state) = features.0.get_mut(&Feature::Worker) {
            *state = if live_settings.worker.push_enabled {
                FeatureState::On
            } else {
                FeatureState::Off
            };
        }
        if let Some(state) = features.0.get_mut(&Feature::Squad) {
            *state = if live_settings.squad.enabled {
                FeatureState::On
            } else {
                FeatureState::Off
            };
        }
        Ok(response)
    }
}
impl From<Arc<worker::WorkerApi>> for DaemonApi {
    fn from(value: Arc<worker::WorkerApi>) -> Self {
        Self::from_worker(value)
    }
}
