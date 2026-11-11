//! Unified Layer 2 daemon policy and shared feature state.
pub mod bootstrap;
pub mod features;
pub mod network;
mod reload;
mod terminals;
#[cfg(any(test, feature = "test-support"))]
mod test_support;
use crate::command::{
    commands::api_server::event_bus::EventBus, dispatch::Engines, error::CommandError,
};
use crate::data::{
    config::EffectiveConfig,
    daemon_state::*,
    fs::{daemon_paths::DaemonPaths, prerequisite_grants::PrerequisiteGrantStore},
};
use crate::engine::{
    auth::api_keys::ApiKeyStore,
    daemon::{core::Core, feature_supervisor::FeatureSupervisor, DaemonGuard},
    error::EngineError,
};
pub use bootstrap::DaemonBootstrap;
use features::{
    engine::EngineFeature,
    squad::{SquadFeature, SquadShared},
    worker::WorkerShared,
};
pub use network::{ListenerStatus, ListenersDesired, LocalTransport};
use std::{
    collections::{BTreeMap, HashMap},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, RwLock,
    },
    time::Duration,
};
use tokio_util::sync::CancellationToken;
pub struct DaemonRuntime {
    pull: Option<Arc<dyn crate::command::daemon_api::worker_pull::PullHandle>>,
    core: Arc<Core>,
    engines: Engines,
    config: RwLock<EffectiveConfig>,
    initial_config: EffectiveConfig,
    supervisors: BTreeMap<FeatureName, Arc<FeatureSupervisor>>,
    /// Public states retained while new starts are prepared for a reload.
    preparing: RwLock<BTreeMap<FeatureName, SupervisedState>>,
    squad: Arc<SquadShared>,
    worker: Arc<WorkerShared>,
    event_buses: Arc<tokio::sync::Mutex<HashMap<String, Arc<EventBus>>>>,
    keys: Arc<ApiKeyStore>,
    /// Browser grants, pending links and upgrade tickets (WI 0159).
    browser: Arc<crate::command::daemon_api::browser::BrowserAuth>,
    network: network::NetworkControl,
    listeners: RwLock<ListenerStatus>,
    grants: Mutex<PrerequisiteGrantStore>,
    guard: DaemonGuard,
    exit: CancellationToken,
    pid: u32,
    engine_feature: Arc<EngineFeature>,
    /// The Layer 2 instance host behind the `engine` feature (WI 0142).
    engine_host: Arc<crate::command::instance::host::EngineInstanceHost>,
    squad_feature: Arc<SquadFeature>,
    apply_worker: Box<dyn Fn(crate::data::config::daemon::WorkerSettings) + Send + Sync>,
    changes: tokio::sync::Mutex<()>,
    shutdown_done: AtomicBool,
    /// Built on first use by `terminals()` (WI 0141).
    terminals: std::sync::OnceLock<Arc<crate::command::terminals::TerminalOperations>>,
}
#[derive(Debug, thiserror::Error)]
pub enum FeatureChangeError {
    #[error("unknown feature")]
    UnknownFeature,
    #[error("ephemeral enablement is not allowed for {0}")]
    EphemeralNotAllowed(FeatureName),
    #[error("{0} is enabled by config; change it with `awman config set`")]
    EnabledByConfig(FeatureName),
    #[error(transparent)]
    Engine(#[from] EngineError),
}
impl DaemonRuntime {
    /// The engine instance host whose typed operations the `/v1/engine/*`
    /// routes and the frontend WebSocket adapt (WI 0142).
    pub fn engine_host(&self) -> &Arc<crate::command::instance::host::EngineInstanceHost> {
        &self.engine_host
    }
    pub fn pull_handle(
        &self,
    ) -> Option<Arc<dyn crate::command::daemon_api::worker_pull::PullHandle>> {
        self.pull.clone()
    }
    pub fn core(&self) -> &Arc<Core> {
        &self.core
    }
    pub fn engines(&self) -> &Engines {
        &self.engines
    }
    pub fn config(&self) -> EffectiveConfig {
        self.config.read().unwrap().clone()
    }
    pub fn daemon_paths(&self) -> &DaemonPaths {
        self.core.daemon_paths()
    }
    pub fn keys(&self) -> &Arc<ApiKeyStore> {
        &self.keys
    }
    /// The browser authority: grants, links, tickets (WI 0159).
    pub fn browser_auth(&self) -> &crate::command::daemon_api::browser::BrowserAuth {
        &self.browser
    }
    /// Route reload revocations (origin removed, listener closed, engine
    /// no longer served) to the browser authority. Called by every constructor.
    fn install_browser_revocations(&self) {
        self.network
            .set_browser_revocation_sink(self.browser.clone());
    }
    pub fn worker(&self) -> &Arc<WorkerShared> {
        &self.worker
    }
    pub fn squad(&self) -> &Arc<SquadShared> {
        &self.squad
    }
    pub fn grants(&self) -> &Mutex<PrerequisiteGrantStore> {
        &self.grants
    }
    pub fn event_buses(&self) -> &Arc<tokio::sync::Mutex<HashMap<String, Arc<EventBus>>>> {
        &self.event_buses
    }
    pub fn feature_state(&self, f: FeatureName) -> SupervisedState {
        if let Some(state) = self.preparing.read().unwrap().get(&f) {
            return state.clone();
        }
        self.supervisors[&f].state()
    }
    pub fn feature_status(&self, f: FeatureName) -> FeatureStatus {
        let mut status = self.supervisors[&f].status();
        status.running = self.core.executor().snapshot().per_feature[&f].running;
        status
    }
    pub async fn enable_ephemeral(
        &self,
        f: FeatureName,
        reason: Option<String>,
    ) -> Result<SupervisedState, FeatureChangeError> {
        let _changes = self.changes.lock().await;
        if self.core.shutdown().is_cancelled() || self.exit.is_cancelled() {
            return Err(EngineError::Other("daemon is shutting down".into()).into());
        }
        if self.supervisors[&f].enabled_by() == Some(EnabledBy::Config) {
            return Ok(self.feature_state(f));
        }
        if f == FeatureName::Engine && !self.engine_feature.allow_ephemeral() {
            return Err(FeatureChangeError::EphemeralNotAllowed(f));
        }
        if f != FeatureName::Engine && self.engines.container_runtime.is_none() {
            return Err(
                EngineError::Other("this feature requires a container runtime".into()).into(),
            );
        }
        self.supervisors[&f]
            .enable(EnabledBy::Ephemeral, reason)
            .await?;
        Ok(self.feature_state(f))
    }
    pub async fn disable_ephemeral(
        self: &Arc<Self>,
        f: FeatureName,
    ) -> Result<SupervisedState, FeatureChangeError> {
        let _changes = self.changes.lock().await;
        if self.supervisors[&f].enabled_by() == Some(EnabledBy::Config) {
            return Err(FeatureChangeError::EnabledByConfig(f));
        }
        let was_ephemeral = self.supervisors[&f].enabled_by() == Some(EnabledBy::Ephemeral);
        self.supervisors[&f].disable(StopMode::Drain).await;
        if was_ephemeral {
            let runtime = self.clone();
            tokio::spawn(async move {
                runtime.supervisors[&f].wait_stopped().await;
                let _changes = runtime.changes.lock().await;
                if !runtime.any_feature_enabled() {
                    tracing::info!("last ephemeral daemon feature disabled; exiting");
                    runtime.exit.cancel();
                }
            });
        }
        Ok(self.feature_state(f))
    }
    pub fn listener_settings(
        &self,
        name: &str,
    ) -> Option<crate::data::config::daemon::ListenerSettings> {
        self.config().daemon().network.listeners.remove(name)
    }
    pub fn listener_serves(&self, name: &str, f: FeatureName) -> bool {
        self.listener_settings(name)
            .is_some_and(|l| l.enabled && l.features.contains(&f))
            && self.feature_state(f).enabled_by == Some(EnabledBy::Config)
    }
    pub fn any_feature_enabled(&self) -> bool {
        FeatureName::ALL
            .into_iter()
            .any(|f| self.feature_state(f).state != FeatureState::Off)
    }
    pub fn listeners(&self) -> ListenerStatus {
        let mut status = self.listeners.read().unwrap().clone();
        for (name, desired) in &self.network.tx.borrow().listeners {
            let old = status.network.remove(name);
            if desired.enabled || old.as_ref().is_some_and(|r| r.bound.is_some()) {
                if let Some(record) = self.network_record(
                    name,
                    desired.listen.to_string(),
                    old.as_ref().and_then(|r| r.bound),
                    old.and_then(|r| r.error),
                ) {
                    status.network.insert(name.clone(), record);
                }
            }
        }
        status
    }
    pub fn set_local_listener(&self, transport: LocalTransport, address: String) {
        self.listeners.write().unwrap().local =
            Some(network::LocalListenerRecord { transport, address });
    }
    pub fn network_control(&self) -> tokio::sync::watch::Receiver<ListenersDesired> {
        self.network.subscribe()
    }
    pub fn set_browser_revocation_sink(&self, sink: Arc<dyn network::BrowserRevocationSink>) {
        self.network.set_browser_revocation_sink(sink);
    }
    fn network_record(
        &self,
        name: &str,
        listen: String,
        bound: Option<std::net::SocketAddr>,
        error: Option<String>,
    ) -> Option<network::ListenerRecord> {
        let all = self.network.tx.borrow();
        let desired = all.listeners.get(name)?;
        let features: Vec<_> = FeatureName::ALL
            .into_iter()
            .filter(|f| self.listener_serves(name, *f))
            .collect();
        Some(network::ListenerRecord {
            name: name.into(),
            enabled: desired.enabled,
            listen,
            bound,
            error,
            transport: desired.transport,
            tls: desired.tls.as_ref().map(|identity| network::TlsStatus {
                fingerprint_sha256: identity.material.fingerprint_sha256_hex.clone(),
                sans: identity.sans.clone(),
            }),
            browser_links: desired.transport
                == crate::data::config::network::ListenerTransport::LoopbackHttp
                && features.contains(&FeatureName::Engine),
            allowed_origins: self
                .listener_settings(name)
                .map(|l| l.allowed_origins)
                .unwrap_or_default(),
            browser_grants: self.browser.count_for_listener(name),
            features,
            api_keys: self.keys.count(),
        })
    }
    pub fn report_listener_bound(&self, name: &str, bound: std::net::SocketAddr) {
        let Some(listen) = self
            .network
            .tx
            .borrow()
            .listeners
            .get(name)
            .map(|l| l.listen.to_string())
        else {
            return;
        };
        let Some(record) = self.network_record(name, listen, Some(bound), None) else {
            return;
        };
        self.listeners
            .write()
            .unwrap()
            .network
            .insert(name.into(), record);
    }
    pub fn report_listener_error(&self, name: &str, listen: String, error: String) {
        let Some(record) = self.network_record(name, listen, None, Some(error)) else {
            return;
        };
        self.listeners
            .write()
            .unwrap()
            .network
            .insert(name.into(), record);
    }
    pub fn report_listener_closed(&self, name: &str) {
        self.listeners.write().unwrap().network.remove(name);
    }
    pub fn report_listener_rebound(&self, name: &str, disconnected: usize) {
        tracing::info!(
            listener = name,
            disconnected_clients = disconnected,
            "listener rebound"
        );
    }
    pub fn listener_host_started(&self) {
        self.network.hosting.store(true, Ordering::Release);
    }
    pub fn report_listeners_applied(
        &self,
        generation: u64,
        effects: Vec<crate::command::daemon_api::core::ReloadEffect>,
    ) {
        self.network.applied.send_replace((generation, effects));
    }
    pub fn exit_signal(&self) -> CancellationToken {
        self.exit.clone()
    }
    pub async fn shutdown(&self, mode: StopMode) {
        if self.shutdown_done.load(Ordering::Acquire) {
            return;
        }
        self.core.shutdown().cancel();
        // Every browser grant ends with the daemon; live browser sockets close
        // with 1008 "daemon stopping".
        self.browser
            .revoke_all(crate::data::browser_auth::GrantRevocationReason::DaemonStopping);
        // Engine frontends are told the daemon is stopping, not that the
        // feature was disabled.
        self.engine_feature
            .set_stop_reason(features::engine::EngineStopReason::DaemonShutdown);
        let stop = |mode| async move {
            for supervisor in self.supervisors.values() {
                supervisor.disable(mode).await;
            }
            futures_util::future::join_all(self.supervisors.values().map(|s| s.wait_stopped()))
                .await;
        };
        stop(mode).await;
        let mut desired = self.network.tx.borrow().clone();
        for l in desired.listeners.values_mut() {
            l.enabled = false;
        }
        self.network.tx.send_replace(desired);
        // Only the owner removes endpoint metadata; a successor's identity survives.
        if self
            .guard
            .supervisor()
            .process()
            .owns_pidfile(self.pid)
            .unwrap_or(false)
        {
            if let Some(local) = self.listeners().local {
                let _ = self
                    .daemon_paths()
                    .remove_local_endpoint(std::path::Path::new(&local.address));
            }
            let _ = crate::data::fs::daemon_sidecar::ServerSidecar::remove(
                &self.daemon_paths().server_meta_file(),
            );
            let _ = self.guard.release_owned_by(self.pid);
        }
        self.exit.cancel();
        self.shutdown_done.store(true, Ordering::Release);
    }
    pub async fn cancel_feature_jobs(&self, f: FeatureName) -> Result<Vec<String>, CommandError> {
        match f {
            FeatureName::Worker => self.worker.cancel_all(),
            FeatureName::Squad => Ok(self.squad_feature.cancel_all()),
            FeatureName::Engine => Ok(Vec::new()),
        }
    }
    fn spawn_idle_watch(runtime: &Arc<Self>, interval: Duration) {
        let weak = Arc::downgrade(runtime);
        let stop = runtime.core.shutdown();
        tokio::spawn(async move {
            loop {
                tokio::select! { _=stop.cancelled()=>return,_=tokio::time::sleep(interval)=>{} }
                let Some(runtime) = weak.upgrade() else {
                    return;
                };
                runtime.expire_idle().await;
            }
        });
    }
    async fn expire_idle(self: &Arc<Self>) {
        if self.feature_state(FeatureName::Engine).enabled_by == Some(EnabledBy::Ephemeral)
            && self
                .engine_feature
                .idle_for()
                .is_some_and(|idle| idle >= self.engine_feature.idle_timeout())
        {
            let _ = self.disable_ephemeral(FeatureName::Engine).await;
        }
    }
}
impl DaemonRuntime {
    pub fn feature_record(
        &self,
        f: FeatureName,
    ) -> crate::command::daemon_api::core::FeatureRecord {
        let state = self.feature_state(f);
        let status = self.feature_status(f);
        let slots = self.core.executor().snapshot();
        crate::command::daemon_api::core::FeatureRecord {
            name: f.as_str().into(),
            state: state.state,
            enabled_by: state.enabled_by,
            reason: state.reason,
            max_concurrent_jobs: Some(match f {
                FeatureName::Worker => self.config().daemon().worker.max_concurrent_jobs,
                FeatureName::Squad => self.config().daemon().squad.max_concurrent_jobs,
                FeatureName::Engine => self.config().daemon().engine.max_concurrent_jobs,
            }),
            running: if state.state == FeatureState::Off {
                0
            } else {
                status.running
            },
            queued: if state.state == FeatureState::Off {
                0
            } else {
                status.queued
            },
            oldest_queued_age_ms: if state.state == FeatureState::Off {
                None
            } else {
                status
                    .oldest_queued_age
                    .or(slots.per_feature[&f].oldest_wait)
                    .map(|d| d.as_millis().min(u64::MAX as u128) as u64)
            },
            detail: status.detail,
            push: None,
            pull: None,
            restarts: state.restarts,
            last_error: state.last_error,
            since_at: state.since,
            idle_timeout_ms: (f == FeatureName::Engine)
                .then(|| self.engine_feature.idle_timeout().as_millis() as u64),
            idle_ms: if f == FeatureName::Engine {
                self.engine_feature.idle_for().map(|d| d.as_millis() as u64)
            } else {
                None
            },
        }
    }
    pub fn status_snapshot(&self) -> crate::command::daemon_api::core::StatusResponse {
        crate::command::daemon_api::core::StatusResponse {
            status: if self.any_feature_enabled() {
                "ok"
            } else {
                "no_features"
            }
            .into(),
            version: env!("CARGO_PKG_VERSION").into(),
            protocol: [("api".into(), 1), ("frontend".into(), 1)].into(),
            capabilities: crate::command::daemon_api::capabilities::CAPABILITIES
                .iter()
                .map(|s| s.to_string())
                .collect(),
            features: FeatureName::ALL
                .into_iter()
                .map(|f| (f.as_str().into(), self.feature_record(f)))
                .collect(),
            max_concurrent_jobs: self.core.executor().snapshot().cap,
            pid: self.pid,
            uptime_ms: self.core.started_at().elapsed().as_millis() as u64,
            agents: crate::engine::agent::AgentRegistry::list(self.core.env(), None).names(),
            listeners: self.listeners(),
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::{
        commands::api_server::{runtime::WorkerServices, WorkerConfig},
        daemon_api::worker::WorkerApi,
    };
    use crate::data::config::{ConfigFile, ConfigLayers, FileKind};
    struct Evaluator;
    #[async_trait::async_trait]
    impl crate::engine::squad::TaskEvaluator for Evaluator {
        async fn evaluate(
            &self,
            _: crate::engine::squad::EvaluationRequest,
        ) -> crate::engine::squad::EvaluationOutcome {
            panic!("no test task should run")
        }
    }
    struct Passive(FeatureName);
    #[async_trait::async_trait]
    impl crate::engine::daemon::feature::DaemonFeature for Passive {
        fn name(&self) -> FeatureName {
            self.0
        }
        async fn start(
            &self,
            _: &Core,
        ) -> Result<crate::engine::daemon::feature::FeatureTask, EngineError> {
            panic!("passive feature must stay off")
        }
        async fn drain(&self) {}
        async fn stop_now(&self) {}
        fn status(&self) -> FeatureStatus {
            FeatureStatus::default()
        }
        fn share(&self) -> usize {
            1
        }
    }
    fn config(core: &Core, enabled: bool, allow: bool) -> EffectiveConfig {
        EffectiveConfig::resolve(ConfigLayers { global:ConfigFile::from_value(serde_json::json!({"daemon":{"features":{"engine":{"enabled":enabled,"allowEphemeral":allow,"idleTimeout":"1ms"},"squad":{"envPersistence":"none"}},"network":{"listeners":{"default":{"enabled":true,"features":["engine"]}}}}}),FileKind::Global).unwrap(),..Default::default() },Default::default(),core.env().clone()).unwrap()
    }
    async fn fixture(enabled: bool, allow: bool) -> (tempfile::TempDir, Arc<DaemonRuntime>) {
        let dir = tempfile::tempdir().unwrap();
        let core = crate::engine::daemon::core::test_core(dir.path());
        let config = config(&core, enabled, allow);
        let settings = config.daemon();
        core.executor().set_cap(settings.max_concurrent_jobs);
        let engines = Engines::from_core(&core, Arc::new(config.config().clone()));
        let recovered = WorkerServices::from_core(
            WorkerConfig {
                port: 9876,
                bind_ip: std::net::Ipv4Addr::LOCALHOST.into(),
                workdirs: Vec::new(),
                workdir_roots: Vec::new(),
                env: core.env().clone(),
            },
            engines.clone(),
            &core,
        )
        .unwrap();
        let api = Arc::new(WorkerApi {
            live_config: RwLock::new(Some(config.clone())),
            settings_tx: None,
            store: recovered.store.clone(),
            lifecycle_store: recovered.lifecycle_store.clone(),
            canceller: recovered.canceller(),
            setup_cancels: recovered.setup_cancels.clone(),
            paths: recovered.paths.clone(),
            workdirs: Vec::new(),
            workdir_roots: Vec::new(),
            started_at: core.started_at(),
            task_handles: tokio::sync::Mutex::new(Vec::new()),
            engines: engines.clone(),
            sessions: recovered.sessions.clone(),
            event_buses: recovered.event_buses.clone(),
            setup_buses: tokio::sync::Mutex::new(HashMap::new()),
        });
        let worker = Arc::new(WorkerShared {
            api,
            command_cancels: recovered.command_cancels.clone(),
        });
        let squad_feature = SquadFeature::new(
            &core,
            engines.clone(),
            Arc::new(Evaluator),
            settings.squad.clone(),
        )
        .unwrap();
        let engine_host = super::features::engine::engine_host(
            &core,
            Arc::new(crate::command::instance::frontend::PlainPresentation),
        );
        let engine_feature = EngineFeature::new(settings.engine.clone(), engine_host.clone());
        let supervisors = [
            (
                FeatureName::Worker,
                FeatureSupervisor::new(Arc::new(Passive(FeatureName::Worker)), core.clone()),
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
        let runtime = Arc::new(DaemonRuntime {
            pull: None,
            guard: DaemonGuard::new(core.daemon_paths().clone()),
            grants: Mutex::new(PrerequisiteGrantStore::load(dir.path())),
            event_buses: worker.event_buses.clone(),
            keys: ApiKeyStore::open(core.daemon_paths().api_keys_file()).unwrap(),
            browser: crate::command::daemon_api::browser::BrowserAuth::open(
                core.daemon_paths().browser_grants_file(),
            )
            .unwrap(),
            core,
            engines,
            config: RwLock::new(config.clone()),
            initial_config: config,
            supervisors,
            preparing: RwLock::new(BTreeMap::new()),
            squad: squad_feature.shared().clone(),
            worker,
            network: network::NetworkControl::new(ListenersDesired::new(
                &settings.network,
                None,
                None,
            )),
            listeners: RwLock::new(ListenerStatus::default()),
            exit: CancellationToken::new(),
            pid: std::process::id(),
            engine_host,
            engine_feature,
            squad_feature,
            apply_worker: Box::new(|_| {}),
            changes: tokio::sync::Mutex::new(()),
            shutdown_done: AtomicBool::new(false),
            terminals: Default::default(),
        });
        runtime.install_browser_revocations();
        if enabled {
            runtime.supervisors[&FeatureName::Engine]
                .enable(EnabledBy::Config, None)
                .await
                .unwrap();
        }
        (dir, runtime)
    }
    #[tokio::test]
    async fn ephemeral_engine_idles_out_and_exits_but_is_never_network_served() {
        let (_dir, r) = fixture(false, true).await;
        r.enable_ephemeral(FeatureName::Engine, Some("handoff".into()))
            .await
            .unwrap();
        assert_eq!(
            r.feature_state(FeatureName::Engine).state,
            FeatureState::Ephemeral
        );
        assert!(!r.listener_serves("default", FeatureName::Engine));
        DaemonRuntime::spawn_idle_watch(&r, Duration::from_millis(2));
        tokio::time::timeout(Duration::from_secs(1), r.exit_signal().cancelled())
            .await
            .unwrap();
        assert!(!r.any_feature_enabled());
        r.shutdown(StopMode::Now).await;
    }
    #[tokio::test]
    async fn allow_ephemeral_false_refuses_without_starting() {
        let (_dir, r) = fixture(false, false).await;
        assert!(matches!(
            r.enable_ephemeral(FeatureName::Engine, None).await,
            Err(FeatureChangeError::EphemeralNotAllowed(FeatureName::Engine))
        ));
        assert_eq!(
            r.feature_state(FeatureName::Engine).state,
            FeatureState::Off
        );
        assert!(!r.exit_signal().is_cancelled());
        r.shutdown(StopMode::Now).await;
    }
    #[tokio::test]
    async fn persistent_disable_stays_up_and_ephemeral_cannot_disable_config() {
        let (_dir, r) = fixture(true, true).await;
        assert!(matches!(
            r.disable_ephemeral(FeatureName::Engine).await,
            Err(FeatureChangeError::EnabledByConfig(_))
        ));
        assert!(r.listener_serves("default", FeatureName::Engine));
        r.apply_config_locked(config(&r.core, false, true))
            .await
            .unwrap();
        r.supervisors[&FeatureName::Engine].wait_stopped().await;
        r.expire_idle().await;
        assert!(!r.any_feature_enabled());
        assert!(!r.exit_signal().is_cancelled());
        assert_eq!(r.status_snapshot().status, "no_features");
        r.apply_config_locked(config(&r.core, true, true))
            .await
            .unwrap();
        assert_eq!(r.feature_state(FeatureName::Engine).state, FeatureState::On);
        r.shutdown(StopMode::Drain).await;
        r.shutdown(StopMode::Now).await;
    }
    #[tokio::test]
    async fn status_snapshot_round_trip_contains_supervision_and_listener_objects() {
        let (_dir, r) = fixture(false, true).await;
        r.enable_ephemeral(FeatureName::Engine, Some("status test".into()))
            .await
            .unwrap();
        r.set_local_listener(LocalTransport::UnixSocket, "/tmp/example.sock".into());
        let snapshot = r.status_snapshot();
        let json = serde_json::to_value(&snapshot).unwrap();
        assert_eq!(json["features"]["engine"]["enabled_by"], "ephemeral");
        assert_eq!(json["features"]["engine"]["reason"], "status test");
        assert_eq!(json["listeners"]["local"]["transport"], "unix_socket");
        assert!(json["features"]["worker"]["oldest_queued_age_ms"].is_null());
        let roundtrip: crate::command::daemon_api::core::StatusResponse =
            serde_json::from_value(json).unwrap();
        assert_eq!(snapshot, roundtrip);
        r.shutdown(StopMode::Now).await;
    }
    #[tokio::test]
    async fn a_late_start_failure_never_exposes_an_earlier_staged_feature() {
        use crate::engine::daemon::feature::{DaemonFeature, FeatureTask};
        struct Start {
            name: FeatureName,
            stop: CancellationToken,
            entered: Arc<tokio::sync::Notify>,
            fail: Arc<tokio::sync::Notify>,
        }
        #[async_trait::async_trait]
        impl DaemonFeature for Start {
            fn name(&self) -> FeatureName {
                self.name
            }
            async fn start(&self, _: &Core) -> Result<FeatureTask, EngineError> {
                if self.name == FeatureName::Squad {
                    self.entered.notify_one();
                    self.fail.notified().await;
                    return Err(EngineError::Other("injected late start failure".into()));
                }
                let stop = self.stop.clone();
                Ok(tokio::spawn(async move {
                    stop.cancelled().await;
                    Ok(())
                }))
            }
            async fn drain(&self) {
                self.stop.cancel();
            }
            async fn stop_now(&self) {
                self.stop.cancel();
            }
            fn status(&self) -> FeatureStatus {
                FeatureStatus::default()
            }
            fn share(&self) -> usize {
                1
            }
        }
        let (_dir, mut r) = fixture(false, true).await;
        let entered = Arc::new(tokio::sync::Notify::new());
        let fail = Arc::new(tokio::sync::Notify::new());
        for name in [FeatureName::Worker, FeatureName::Squad] {
            let supervisor = FeatureSupervisor::new(
                Arc::new(Start {
                    name,
                    stop: CancellationToken::new(),
                    entered: entered.clone(),
                    fail: fail.clone(),
                }),
                r.core.clone(),
            );
            Arc::get_mut(&mut r)
                .unwrap()
                .supervisors
                .insert(name, supervisor);
        }
        let mut value = r.config().value().clone();
        value["daemon"]["network"]["listeners"]["default"]["features"] =
            serde_json::json!(["worker"]);
        let resolve = |value| {
            EffectiveConfig::resolve(
                ConfigLayers {
                    global: ConfigFile::from_value(value, FileKind::Global).unwrap(),
                    ..Default::default()
                },
                Default::default(),
                r.core.env().clone(),
            )
            .unwrap()
        };
        *r.config.write().unwrap() = resolve(value.clone());
        value["daemon"]["features"]["worker"]["enabled"] = serde_json::json!(true);
        value["daemon"]["features"]["squad"]["enabled"] = serde_json::json!(true);
        value["daemon"]["maxConcurrentJobs"] = serde_json::json!(9);
        let candidate = resolve(value);
        let cloned = r.clone();
        let reload = tokio::spawn(async move { cloned.apply_config_locked(candidate).await });
        entered.notified().await;
        assert_eq!(
            r.supervisors[&FeatureName::Worker].state().state,
            FeatureState::On
        );
        assert_eq!(
            r.feature_state(FeatureName::Worker).state,
            FeatureState::Off
        );
        assert!(!r.listener_serves("default", FeatureName::Worker));
        assert_eq!(
            r.core.executor().snapshot().per_feature[&FeatureName::Worker].share,
            0
        );
        fail.notify_one();
        assert!(reload.await.unwrap().is_err());
        assert!(!r.config().daemon().worker.enabled);
        assert_eq!(r.core.executor().snapshot().cap, 4);
        assert_eq!(
            r.supervisors[&FeatureName::Worker].state().state,
            FeatureState::Off
        );
        r.shutdown(StopMode::Now).await;
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn failed_squad_preparation_leaves_config_capacity_and_other_features_unchanged() {
        let (_dir, r) = fixture(true, true).await;
        // test_core uses an in-memory shared connection. Refuse the real squad
        // preparation query, rather than locking an unrelated on-disk database.
        let db = r.core.session_store().shared_connection();
        db.lock()
            .unwrap()
            .execute_batch("ALTER TABLE squad_runs RENAME TO refused_squad_runs")
            .unwrap();
        let mut value = r.config().value().clone();
        value["daemon"]["maxConcurrentJobs"] = serde_json::json!(9);
        value["daemon"]["network"]["listeners"]["default"]["enabled"] = serde_json::json!(true);
        value["daemon"]["features"]["squad"]["enabled"] = serde_json::json!(true);
        value["daemon"]["features"]["engine"]["enabled"] = serde_json::json!(false);
        let candidate = EffectiveConfig::resolve(
            ConfigLayers {
                global: ConfigFile::from_value(value, FileKind::Global).unwrap(),
                ..Default::default()
            },
            Default::default(),
            r.core.env().clone(),
        )
        .unwrap();
        assert!(r.apply_config_locked(candidate).await.is_err());
        assert!(!r.daemon_paths().tls_dir().join("cert.pem").exists());
        assert_eq!(r.config().daemon().max_concurrent_jobs, 4);
        assert_eq!(r.core.executor().snapshot().cap, 4);
        assert_eq!(r.feature_state(FeatureName::Engine).state, FeatureState::On);
        assert_eq!(r.feature_state(FeatureName::Squad).state, FeatureState::Off);
        db.lock()
            .unwrap()
            .execute_batch("ALTER TABLE refused_squad_runs RENAME TO squad_runs")
            .unwrap();
        r.shutdown(StopMode::Now).await;
    }
    #[tokio::test]
    async fn a_cancelled_reload_caller_does_not_cancel_the_owned_transaction() {
        let (_dir, r) = fixture(true, true).await;
        let file = crate::data::config::file::global_config_path(r.core.env()).unwrap();
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, r#"{"daemon":{"maxConcurrentJobs":9,"features":{"engine":{"enabled":true},"squad":{"envPersistence":"none"}}}}"#).unwrap();
        let held = r.changes.lock().await;
        let cloned = r.clone();
        let request = tokio::spawn(async move { cloned.reload_config().await });
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        request.abort();
        drop(held);
        tokio::time::timeout(Duration::from_secs(2), async {
            while r.config().daemon().max_concurrent_jobs != 9 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(r.core.executor().snapshot().cap, 9);
        r.shutdown(StopMode::Now).await;
    }
    #[tokio::test(start_paused = true)]
    async fn plain_shutdown_keeps_draining_after_thirty_seconds() {
        use crate::engine::daemon::feature::{DaemonFeature, FeatureTask};
        struct Slow {
            stop: CancellationToken,
            finish: CancellationToken,
            cancelled: AtomicBool,
        }
        #[async_trait::async_trait]
        impl DaemonFeature for Slow {
            fn name(&self) -> FeatureName {
                FeatureName::Worker
            }
            async fn start(&self, _: &Core) -> Result<FeatureTask, EngineError> {
                let finish = self.finish.clone();
                Ok(tokio::spawn(async move {
                    finish.cancelled().await;
                    Ok(())
                }))
            }
            async fn drain(&self) {
                self.stop.cancel();
                self.finish.cancelled().await;
            }
            async fn stop_now(&self) {
                self.cancelled.store(true, Ordering::Release);
                self.finish.cancel();
            }
            fn status(&self) -> FeatureStatus {
                FeatureStatus::default()
            }
            fn share(&self) -> usize {
                1
            }
        }
        let (_dir, mut r) = fixture(false, true).await;
        let feature = Arc::new(Slow {
            stop: CancellationToken::new(),
            finish: CancellationToken::new(),
            cancelled: AtomicBool::new(false),
        });
        let supervisor = FeatureSupervisor::new(feature.clone(), r.core.clone());
        supervisor.enable(EnabledBy::Config, None).await.unwrap();
        Arc::get_mut(&mut r)
            .unwrap()
            .supervisors
            .insert(FeatureName::Worker, supervisor);
        let cloned = r.clone();
        let shutdown = tokio::spawn(async move { cloned.shutdown(StopMode::Drain).await });
        feature.stop.cancelled().await;
        tokio::time::advance(Duration::from_secs(60)).await;
        assert!(!shutdown.is_finished());
        assert!(!feature.cancelled.load(Ordering::Acquire));
        feature.finish.cancel();
        shutdown.await.unwrap();
    }
    #[tokio::test]
    async fn shutdown_now_does_not_wait_for_the_configuration_lock() {
        let (_dir, r) = fixture(true, true).await;
        let _held = r.changes.lock().await;
        tokio::time::timeout(Duration::from_secs(1), r.shutdown(StopMode::Now))
            .await
            .unwrap();
        assert_eq!(
            r.feature_state(FeatureName::Engine).state,
            FeatureState::Off
        );
    }
}
