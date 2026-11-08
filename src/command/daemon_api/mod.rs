//! Resource operations shared by the daemon transports and clients.
pub mod capabilities;
pub mod core;
pub mod keys;
pub mod operations;
pub mod squad;
pub mod terminals;
pub mod worker;
use crate::command::commands::squad::gateway::TaskGateway;
use crate::command::daemon_runtime::DaemonRuntime;
use crate::command::dispatch::Engines;
pub use operations::{Feature, FeatureState, Operation};
use std::sync::{Arc, Mutex};

pub struct DaemonApi {
    pub runtime: Arc<DaemonRuntime>,
    pub worker: Option<Arc<worker::WorkerApi>>,
    pub squad: Option<Arc<dyn TaskGateway>>,
    pub engines: Engines,
    /// `/v1/terminals` (WI 0141): the daemon's one set of terminal operations,
    /// shared by both listeners so the input grant is daemon-wide.
    pub terminals: Arc<crate::command::terminals::TerminalOperations>,
}
impl DaemonApi {
    pub fn new(runtime: Arc<DaemonRuntime>) -> Self {
        Self {
            worker: Some(runtime.worker().api.clone()),
            squad: Some(runtime.squad().gateway.clone()),
            engines: runtime.engines().clone(),
            terminals: runtime.terminals(),
            runtime,
        }
    }
    pub fn grant_store(
        &self,
    ) -> &Mutex<crate::data::fs::prerequisite_grants::PrerequisiteGrantStore> {
        self.runtime.grants()
    }
    #[cfg(any(test, feature = "test-support"))]
    pub fn for_worker_tests(worker: Arc<worker::WorkerApi>) -> Self {
        let config = worker
            .live_config
            .read()
            .unwrap()
            .clone()
            .unwrap_or_else(|| {
                crate::data::config::EffectiveConfig::resolve(
                    crate::data::config::ConfigLayers {
                        global: {
                            let mut doc = worker.engines.global_config.to_value();
                            crate::data::config::config_json::set_config_field(
                                &mut doc,
                                "daemon.features.worker.enabled",
                                serde_json::json!(true),
                            );
                            crate::data::config::ConfigFile::from_value(
                                doc,
                                crate::data::config::FileKind::Global,
                            )
                            .unwrap()
                        },
                        ..Default::default()
                    },
                    Default::default(),
                    crate::data::config::env::EnvSnapshot::with_overrides([(
                        "AWMAN_HOME",
                        worker.paths.data_paths().home().to_string_lossy().as_ref(),
                    )]),
                )
                .unwrap()
            });
        Self::for_worker_tests_with_config(worker, config)
    }
    #[cfg(any(test, feature = "test-support"))]
    pub fn for_worker_tests_with_config(
        worker: Arc<worker::WorkerApi>,
        config: crate::data::config::EffectiveConfig,
    ) -> Self {
        Self::new(DaemonRuntime::for_worker_tests(worker, config))
    }
}
#[cfg(any(test, feature = "test-support"))]
impl From<Arc<worker::WorkerApi>> for DaemonApi {
    fn from(value: Arc<worker::WorkerApi>) -> Self {
        Self::for_worker_tests(value)
    }
}
