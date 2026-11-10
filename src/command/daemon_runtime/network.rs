//! Listener state shared with the frontend; no listener is bound in Layer 2.
use crate::command::error::CommandError;
use crate::data::config::{
    daemon::NetworkSettings,
    network::{ListenAddr, ListenerName, ListenerTransport},
};
use crate::engine::auth::{tls::TlsIdentity, AuthEngine};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListenerDesired {
    pub name: ListenerName,
    pub enabled: bool,
    pub listen: ListenAddr,
    pub transport: ListenerTransport,
    pub tls: Option<TlsIdentity>,
    pub bind_generation: u64,
}
impl ListenerDesired {
    pub fn tls_server_config(&self) -> Result<std::sync::Arc<rustls::ServerConfig>, CommandError> {
        self.tls
            .as_ref()
            .ok_or_else(|| CommandError::Other("network listener requires TLS".into()))?
            .material
            .rustls_server_config()
            .map_err(Into::into)
    }
}
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ListenersDesired {
    pub generation: u64,
    pub listeners: BTreeMap<ListenerName, ListenerDesired>,
}
impl ListenersDesired {
    pub fn new(
        settings: &NetworkSettings,
        tls: Option<TlsIdentity>,
        previous: Option<&Self>,
    ) -> Self {
        let generation = previous.map_or(0, |p| p.generation + 1);
        let listeners = settings
            .listeners
            .iter()
            .map(|(name, l)| {
                let old = previous.and_then(|p| p.listeners.get(name));
                let bind_generation = old.map_or(generation, |p| {
                    if p.listen != l.listen || p.transport != l.transport || p.enabled != l.enabled
                    {
                        generation
                    } else {
                        p.bind_generation
                    }
                });
                (
                    name.clone(),
                    ListenerDesired {
                        name: name.clone(),
                        enabled: l.enabled,
                        listen: l.listen.clone(),
                        transport: l.transport,
                        tls: (l.transport == ListenerTransport::Tls)
                            .then(|| tls.clone())
                            .flatten(),
                        bind_generation,
                    },
                )
            })
            .collect();
        Self {
            generation,
            listeners,
        }
    }
}
pub fn prepare_tls(
    auth: &AuthEngine,
    paths: &crate::data::fs::daemon_paths::DaemonPaths,
    settings: &NetworkSettings,
) -> Result<Option<TlsIdentity>, CommandError> {
    let tls: Vec<_> = settings
        .listeners
        .values()
        .filter(|l| l.enabled && l.transport == ListenerTransport::Tls)
        .collect();
    if tls.is_empty() {
        return Ok(None);
    }
    let ips = tls.iter().filter_map(|l| l.listen.ip()).collect::<Vec<_>>();
    let mut extra = tls
        .iter()
        .flat_map(|l| l.tls_sans.clone())
        .collect::<Vec<_>>();
    for l in tls {
        if l.listen.ip().is_none() {
            extra.push(
                crate::data::config::network::TlsSan::parse(
                    l.listen.to_string().rsplit_once(':').unwrap().0,
                )
                .map_err(|e| CommandError::Other(e.to_string()))?,
            );
        }
    }
    Ok(Some(auth.ensure_network_tls(paths, &ips, &extra)?.0))
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalTransport {
    UnixSocket,
    NamedPipe,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalListenerRecord {
    pub transport: LocalTransport,
    pub address: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListenerRecord {
    pub name: ListenerName,
    pub enabled: bool,
    pub listen: String,
    pub bound: Option<std::net::SocketAddr>,
    pub transport: ListenerTransport,
    pub tls: Option<TlsStatus>,
    pub error: Option<String>,
    pub features: Vec<crate::data::daemon_state::FeatureName>,
    pub api_keys: usize,
    pub browser_links: bool,
    pub allowed_origins: Vec<crate::data::config::network::BrowserOrigin>,
    pub browser_grants: usize,
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListenerStatus {
    pub local: Option<LocalListenerRecord>,
    pub network: BTreeMap<ListenerName, ListenerRecord>,
}
pub struct NetworkControl {
    pub(crate) hosting: std::sync::atomic::AtomicBool,
    pub(crate) applied:
        tokio::sync::watch::Sender<(u64, Vec<crate::command::daemon_api::core::ReloadEffect>)>,
    pub(crate) tx: tokio::sync::watch::Sender<ListenersDesired>,
    browser_revocations: std::sync::RwLock<std::sync::Arc<dyn BrowserRevocationSink>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrowserRevocation {
    OriginRemoved(crate::data::config::network::BrowserOrigin),
    Disabled,
    EngineRemoved,
    TransportChanged,
    ListenerRemoved,
}

/// Reload hook for browser grants. The browser-auth lane supplies the store-backed implementation.
pub trait BrowserRevocationSink: Send + Sync {
    fn revoke(&self, listener: &str, reason: BrowserRevocation);
}

#[derive(Default)]
struct NoopBrowserRevocationSink;
impl BrowserRevocationSink for NoopBrowserRevocationSink {
    fn revoke(&self, _: &str, _: BrowserRevocation) {}
}

impl NetworkControl {
    pub fn new(desired: ListenersDesired) -> Self {
        let (tx, _) = tokio::sync::watch::channel(desired);
        Self {
            tx,
            hosting: std::sync::atomic::AtomicBool::new(false),
            applied: tokio::sync::watch::channel((0, Vec::new())).0,
            browser_revocations: std::sync::RwLock::new(std::sync::Arc::new(
                NoopBrowserRevocationSink,
            )),
        }
    }
    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<ListenersDesired> {
        self.tx.subscribe()
    }

    pub fn set_browser_revocation_sink(&self, sink: std::sync::Arc<dyn BrowserRevocationSink>) {
        *self.browser_revocations.write().unwrap() = sink;
    }

    pub fn revoke_browser_grants(&self, changes: Vec<(String, BrowserRevocation)>) {
        let sink = self.browser_revocations.read().unwrap().clone();
        for (listener, reason) in changes {
            sink.revoke(&listener, reason);
        }
    }
}

pub fn browser_revocations_between(
    old: &NetworkSettings,
    new: &NetworkSettings,
) -> Vec<(String, BrowserRevocation)> {
    let mut changes = Vec::new();
    for (name, before) in &old.listeners {
        let after = new.listeners.get(name);
        for origin in &before.allowed_origins {
            if !after.is_some_and(|l| l.allowed_origins.contains(origin)) {
                changes.push((
                    name.clone(),
                    BrowserRevocation::OriginRemoved(origin.clone()),
                ));
            }
        }
        if before.enabled && !after.is_some_and(|l| l.enabled) {
            changes.push((
                name.clone(),
                if after.is_some() {
                    BrowserRevocation::Disabled
                } else {
                    BrowserRevocation::ListenerRemoved
                },
            ));
        }
        if before
            .features
            .contains(&crate::data::daemon_state::FeatureName::Engine)
            && !after.is_some_and(|l| {
                l.features
                    .contains(&crate::data::daemon_state::FeatureName::Engine)
            })
        {
            changes.push((name.clone(), BrowserRevocation::EngineRemoved));
        }
        if let Some(after) = after {
            if before.transport != after.transport {
                changes.push((name.clone(), BrowserRevocation::TransportChanged));
            }
        }
    }
    changes
}

#[cfg(test)]
mod revocation_tests {
    use super::*;
    use crate::data::config::{
        env::EnvSnapshot, ConfigFile, ConfigLayers, EffectiveConfig, FileKind, FlagConfig,
    };
    use std::sync::Mutex;

    fn settings(json: serde_json::Value) -> NetworkSettings {
        EffectiveConfig::resolve(
            ConfigLayers {
                global: ConfigFile::from_value(json, FileKind::Global).unwrap(),
                ..Default::default()
            },
            FlagConfig::default(),
            EnvSnapshot::empty(),
        )
        .unwrap()
        .daemon()
        .network
    }

    #[derive(Default)]
    struct RecordingSink(Mutex<Vec<(String, BrowserRevocation)>>);
    impl BrowserRevocationSink for RecordingSink {
        fn revoke(&self, listener: &str, reason: BrowserRevocation) {
            self.0.lock().unwrap().push((listener.into(), reason));
        }
    }

    #[test]
    fn reload_revocation_hook_reports_origin_disable_and_engine_removal() {
        let old = settings(
            serde_json::json!({"daemon":{"network":{"listeners":{"loopback":{"enabled":true,"features":["engine"],"allowedOrigins":["https://awman.prettysmart.dev"]}}}}}),
        );
        let new = settings(
            serde_json::json!({"daemon":{"network":{"listeners":{"loopback":{"enabled":false,"features":[],"allowedOrigins":[]}}}}}),
        );
        let sink = std::sync::Arc::new(RecordingSink::default());
        let control = NetworkControl::new(ListenersDesired::default());
        control.set_browser_revocation_sink(sink.clone());
        control.revoke_browser_grants(browser_revocations_between(&old, &new));
        let changes = sink.0.lock().unwrap();
        assert!(changes.contains(&(
            "loopback".into(),
            BrowserRevocation::OriginRemoved(
                crate::data::config::network::BrowserOrigin::parse("https://awman.prettysmart.dev")
                    .unwrap()
            )
        )));
        assert!(changes.contains(&("loopback".into(), BrowserRevocation::Disabled)));
        assert!(changes.contains(&("loopback".into(), BrowserRevocation::EngineRemoved)));
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TlsStatus {
    pub fingerprint_sha256: String,
    pub sans: Vec<String>,
}
