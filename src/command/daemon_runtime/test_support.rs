//! Dependency injection for router tests. No process host or alternate auth path.
use super::*;
use crate::engine::daemon::{core::CoreDeps, feature::*};
struct IdleWorker(Mutex<CancellationToken>);
#[async_trait::async_trait]
impl DaemonFeature for IdleWorker {
    fn name(&self) -> FeatureName {
        FeatureName::Worker
    }
    async fn start(&self, _: &Core) -> Result<FeatureTask, EngineError> {
        let stop = CancellationToken::new();
        *self.0.lock().unwrap() = stop.clone();
        Ok(tokio::spawn(async move {
            stop.cancelled().await;
            Ok(())
        }))
    }
    async fn drain(&self) {
        self.0.lock().unwrap().cancel();
    }
    async fn stop_now(&self) {
        self.drain().await;
    }
    fn status(&self) -> FeatureStatus {
        FeatureStatus::default()
    }
    fn share(&self) -> usize {
        4
    }
}
struct NeverEvaluate;
#[async_trait::async_trait]
impl crate::engine::squad::TaskEvaluator for NeverEvaluate {
    async fn evaluate(
        &self,
        _: crate::engine::squad::EvaluationRequest,
    ) -> crate::engine::squad::EvaluationOutcome {
        panic!("router fixture must not evaluate tasks")
    }
}
impl DaemonRuntime {
    pub fn for_worker_tests(
        api: Arc<crate::command::daemon_api::worker::WorkerApi>,
        config: EffectiveConfig,
    ) -> Arc<Self> {
        Self::for_engine_tests(api, config, None)
    }

    /// Use the real core, feature lifecycle and transports with injected
    /// instance engines/presenters. No alternate admission or socket path.
    pub fn for_engine_tests(
        api: Arc<crate::command::daemon_api::worker::WorkerApi>,
        config: EffectiveConfig,
        host_parts: Option<crate::command::instance::host::HostParts>,
    ) -> Arc<Self> {
        let engines = api.engines.clone();
        let paths = api.paths.data_paths();
        let core = Core::with_session_store(
            CoreDeps {
                env: config.env().clone(),
                data_paths: paths.clone(),
                daemon_paths: paths.daemon(),
                runtime: engines.runtime.clone(),
                container_runtime: engines.container_runtime.clone(),
                sandbox_runtime: engines.sandbox_runtime.clone(),
                auth_engine: engines.auth_engine.clone(),
                overlay_engine: engines.overlay_engine.clone(),
                git_engine: engines.git_engine.clone(),
                agent_engine: engines.agent_engine.clone(),
                credential_monitor: engines.credential_monitor.clone(),
                max_concurrent_jobs: config.daemon().max_concurrent_jobs,
                terminal_hub: Arc::new(crate::engine::daemon::core::TerminalHub::new(
                    crate::engine::daemon::core::TerminalHubConfig::default(),
                )),
            },
            api.store.clone(),
        )
        .unwrap();
        let squad_feature = SquadFeature::new(
            &core,
            engines.clone(),
            Arc::new(NeverEvaluate),
            config.daemon().squad,
        )
        .unwrap();
        let engine_host = match host_parts {
            Some(mut parts) => {
                parts.hub = core.terminal_hub().clone();
                parts.executor = Some(core.executor().clone());
                crate::command::instance::host::EngineInstanceHost::new(parts)
            }
            None => super::features::engine::engine_host(
                &core,
                Arc::new(crate::command::instance::frontend::PlainPresentation),
            ),
        };
        let engine_feature = EngineFeature::new(config.daemon().engine, engine_host.clone());
        let stop = CancellationToken::new();
        let feature = Arc::new(IdleWorker(Mutex::new(stop.clone())));
        let task = tokio::spawn(async move {
            stop.cancelled().await;
            Ok(())
        });
        let worker_supervisor = FeatureSupervisor::with_started_task(feature, core.clone(), task);
        Arc::new(Self {
            config: RwLock::new(config.clone()),
            initial_config: config.clone(),
            supervisors: [
                (FeatureName::Worker, worker_supervisor),
                (
                    FeatureName::Squad,
                    FeatureSupervisor::new(squad_feature.clone(), core.clone()),
                ),
                (
                    FeatureName::Engine,
                    FeatureSupervisor::new(engine_feature.clone(), core.clone()),
                ),
            ]
            .into(),
            preparing: RwLock::new(BTreeMap::new()),
            squad: squad_feature.shared().clone(),
            worker: Arc::new(WorkerShared {
                command_cancels: Arc::new(
                    crate::command::commands::api_server::cancel::TokenRegistry::default(),
                ),
                api: api.clone(),
            }),
            event_buses: api.event_buses.clone(),
            keys: ApiKeyStore::open(paths.daemon().api_keys_file()).unwrap(),
            network: network::NetworkControl::new(NetworkDesired {
                enabled: false,
                listen: config.daemon().network.listen,
                tls: None,
            }),
            listeners: RwLock::new(ListenerStatus::default()),
            grants: Mutex::new(PrerequisiteGrantStore::load(paths.home())),
            guard: DaemonGuard::new(paths.daemon()),
            exit: CancellationToken::new(),
            pid: std::process::id(),
            engine_host,
            engine_feature,
            squad_feature,
            apply_worker: Box::new(|_| {}),
            changes: tokio::sync::Mutex::new(()),
            shutdown_done: AtomicBool::new(false),
            terminals: Default::default(),
            core,
            engines,
        })
    }
}
