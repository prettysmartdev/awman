//! Unified Layer 2 daemon policy and shared feature state.
pub mod bootstrap;
pub mod features;
pub mod network;
mod reload;
mod terminals;
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
            capabilities: crate::command::daemon_api::capabilities::advertised(
                self.engines.runtime.capabilities().mounts_outputs(true),
                &[],
            ),
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
