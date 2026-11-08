//! One-time construction and runtime admission.
use super::{
    features::{
        engine::EngineFeature,
        squad::SquadFeature,
        worker::{WorkerFeature, WorkerShared},
    },
    network::{ListenerStatus, NetworkControl, NetworkDesired},
    DaemonRuntime,
};
use crate::command::{
    commands::{
        api_server::{
            queue_worker::ApiCommandFrontendFactory, runtime::WorkerServices, WorkerConfig,
        },
        squad::{
            evaluation::{LocalTaskEvaluator, SquadRunFrontends},
            runtime_guard::require_container_tier,
        },
    },
    dispatch::Engines,
    error::CommandError,
};
use crate::data::{
    config::{env::EnvSnapshot, ConfigLayers, EffectiveConfig, FlagConfig},
    daemon_state::{EnabledBy, FeatureName},
    fs::{prerequisite_grants::PrerequisiteGrantStore, DataPaths},
};
use crate::engine::{
    auth::api_keys::ApiKeyStore,
    daemon::{
        core::{Core, CoreDeps, TerminalHub},
        feature_supervisor::FeatureSupervisor,
        DaemonClaim, DaemonGuard,
    },
};
use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, Mutex, RwLock},
};
pub struct DaemonBootstrap<F: ApiCommandFrontendFactory + 'static> {
    pub env: EnvSnapshot,
    pub set: crate::data::config::overrides::ConfigOverrides,
    pub squad_run_frontends: Arc<dyn SquadRunFrontends>,
    pub push_frontend_factory: F,
    pub terminal_hub: Arc<TerminalHub>,
}
impl DaemonRuntime {
    pub async fn bootstrap<F: ApiCommandFrontendFactory + 'static>(
        b: DaemonBootstrap<F>,
    ) -> Result<(Arc<Self>, DaemonClaim), CommandError> {
        let engines = Engines::for_env(b.env.clone())?;
        let config = EffectiveConfig::resolve(
            ConfigLayers {
                global: (*engines.global_config).clone(),
                set: b.set.clone(),
                env: crate::data::config::effective::env_overrides(
                    &b.env,
                    &[&engines.global_config],
                )?,
                ..Default::default()
            },
            FlagConfig::default(),
            b.env.clone(),
        )?;
        let settings = config.daemon();
        if !settings.squad.enabled && !settings.worker.enabled && !settings.engine.enabled {
            return Err(CommandError::DaemonNoFeatures);
        }
        if settings.squad.enabled || settings.worker.enabled {
            require_container_tier(&engines)?;
        }
        let data_paths = DataPaths::from_env(&b.env)?;
        let guard = DaemonGuard::new(data_paths.daemon());
        let claim = guard.acquire(std::process::id()).map_err(|e| match e {
            crate::engine::daemon::AcquireError::AlreadyRunning { pid } => {
                CommandError::DaemonAlreadyRunning { pid }
            }
            other => CommandError::from(other.into_engine_error()),
        })?;
        let result = Self::build_claimed(b, engines, config, guard).await;
        match result {
            Ok(runtime) => Ok((runtime, claim)),
            Err(error) => {
                let process =
                    crate::data::fs::daemon_process::DaemonProcess::unified(data_paths.daemon());
                let _ = process.release_pidfile_owned_by(std::process::id());
                Err(error)
            }
        }
    }
    async fn build_claimed<F: ApiCommandFrontendFactory + 'static>(
        b: DaemonBootstrap<F>,
        built: Engines,
        config: EffectiveConfig,
        guard: DaemonGuard,
    ) -> Result<Arc<Self>, CommandError> {
        let settings = config.daemon();
        let core = Core::open(CoreDeps {
            env: b.env.clone(),
            data_paths: DataPaths::from_env(&b.env)?,
            daemon_paths: guard.paths().clone(),
            runtime: built.runtime.clone(),
            container_runtime: built.container_runtime.clone(),
            sandbox_runtime: built.sandbox_runtime.clone(),
            auth_engine: built.auth_engine.clone(),
            overlay_engine: built.overlay_engine.clone(),
            git_engine: built.git_engine.clone(),
            agent_engine: built.agent_engine.clone(),
            credential_monitor: built.credential_monitor.clone(),
            max_concurrent_jobs: settings.max_concurrent_jobs,
            terminal_hub: b.terminal_hub,
        })?;
        let engines = Engines::from_core(&core, Arc::new(config.config().clone()));
        let keys = ApiKeyStore::open(core.daemon_paths().api_keys_file())?;
        let tls = if settings.network.enabled {
            Some(
                engines
                    .auth_engine
                    .ensure_network_tls(
                        core.daemon_paths(),
                        settings
                            .network
                            .listen
                            .ip()
                            .unwrap_or(std::net::Ipv4Addr::UNSPECIFIED.into()),
                        &settings.network.tls_sans,
                    )?
                    .0,
            )
        } else {
            None
        };
        if settings.network.features.contains(&FeatureName::Engine)
            && !settings.network.listen.is_loopback()
        {
            tracing::warn!("engine is listed on a non-loopback daemon network listener");
        }
        let recovered = WorkerServices::from_core(
            WorkerConfig {
                port: settings.network.listen.port(),
                bind_ip: std::net::Ipv4Addr::LOCALHOST.into(),
                workdirs: config
                    .worker_push_work_dirs()
                    .into_iter()
                    .map(Into::into)
                    .collect(),
                workdir_roots: config
                    .worker_push_work_dir_roots()
                    .into_iter()
                    .map(Into::into)
                    .collect(),
                env: b.env.clone(),
            },
            engines.clone(),
            &core,
        )?;
        let command_cancels = recovered.command_cancels.clone();
        let api = Arc::new(crate::command::daemon_api::worker::WorkerApi {
            live_config: RwLock::new(Some(config.clone())),
            settings_tx: None,
            store: recovered.store.clone(),
            lifecycle_store: recovered.lifecycle_store.clone(),
            canceller: recovered.canceller(),
            setup_cancels: recovered.setup_cancels.clone(),
            paths: recovered.paths.clone(),
            workdirs: recovered.workdirs.clone(),
            workdir_roots: recovered.workdir_roots.clone(),
            started_at: core.started_at(),
            task_handles: tokio::sync::Mutex::new(Vec::new()),
            engines: engines.clone(),
            sessions: recovered.sessions.clone(),
            event_buses: recovered.event_buses.clone(),
            setup_buses: tokio::sync::Mutex::new(HashMap::new()),
        });
        let event_buses = api.event_buses.clone();
        let worker = Arc::new(WorkerShared {
            api,
            command_cancels,
        });
        let worker_feature = WorkerFeature::new(
            worker.clone(),
            b.push_frontend_factory,
            settings.worker.clone(),
        );
        // Squad reads `daemon.features.squad.interaction` and
        // `daemon.terminal.{cols,rows}` live (WI 0141) from the same
        // reloadable config the worker admits against.
        let evaluator = Arc::new(
            LocalTaskEvaluator::new(engines.clone(), b.env, b.squad_run_frontends)
                .with_daemon_settings(super::features::worker::live_daemon_settings(
                    worker.clone(),
                )),
        );
        let squad_feature =
            SquadFeature::new(&core, engines.clone(), evaluator, settings.squad.clone())?;
        let engine_feature = EngineFeature::new(settings.engine.clone());
        let supervisors: BTreeMap<_, _> = [
            (
                FeatureName::Worker,
                FeatureSupervisor::new(worker_feature.clone(), core.clone()),
            ),
            (
                FeatureName::Squad,
                FeatureSupervisor::new(squad_feature.clone(), core.clone()),
            ),
            (
                FeatureName::Engine,
                FeatureSupervisor::new(engine_feature.clone(), core.clone()),
            ),
        ]
        .into();
        let runtime = Arc::new(Self {
            grants: Mutex::new(PrerequisiteGrantStore::load(core.data_paths().home())),
            core,
            engines,
            config: RwLock::new(config.clone()),
            initial_config: config,
            supervisors,
            preparing: RwLock::new(BTreeMap::new()),
            squad: squad_feature.shared().clone(),
            worker,
            event_buses,
            keys,
            network: NetworkControl::new(NetworkDesired {
                enabled: settings.network.enabled,
                listen: settings.network.listen.clone(),
                tls,
            }),
            listeners: RwLock::new(ListenerStatus::default()),
            guard,
            exit: tokio_util::sync::CancellationToken::new(),
            pid: std::process::id(),
            engine_feature,
            squad_feature,
            apply_worker: Box::new(move |s| worker_feature.apply_settings(s)),
            changes: tokio::sync::Mutex::new(()),
            shutdown_done: std::sync::atomic::AtomicBool::new(false),
            terminals: Default::default(),
        });
        // Before any feature starts a PTY: the lifecycle publisher subscribes here.
        runtime.terminals();
        for (f, enabled) in [
            (FeatureName::Worker, settings.worker.enabled),
            (FeatureName::Squad, settings.squad.enabled),
            (FeatureName::Engine, settings.engine.enabled),
        ] {
            if enabled {
                if let Err(error) = runtime.supervisors[&f]
                    .enable(EnabledBy::Config, None)
                    .await
                {
                    runtime
                        .shutdown(crate::data::daemon_state::StopMode::Now)
                        .await;
                    return Err(error.into());
                }
            }
        }
        Self::spawn_idle_watch(&runtime, std::time::Duration::from_secs(5));
        Ok(runtime)
    }
}
