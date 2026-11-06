//! Listener state shared with the frontend; no listener is bound in Layer 2.
use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkDesired {
    pub enabled: bool,
    pub listen: crate::data::config::network::ListenAddr,
    pub tls: Option<crate::engine::auth::tls::TlsIdentity>,
}
impl NetworkDesired {
    pub fn tls_server_config(
        &self,
    ) -> Result<std::sync::Arc<rustls::ServerConfig>, crate::command::error::CommandError> {
        self.tls
            .as_ref()
            .ok_or_else(|| {
                crate::command::error::CommandError::Other("network listener requires TLS".into())
            })?
            .material
            .rustls_server_config()
            .map_err(Into::into)
    }
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
pub struct NetworkListenerRecord {
    pub enabled: bool,
    pub listen: String,
    pub bound: Option<std::net::SocketAddr>,
    pub tls: Option<TlsStatus>,
    pub error: Option<String>,
    pub features: Vec<crate::data::daemon_state::FeatureName>,
    pub api_keys: usize,
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListenerStatus {
    pub local: Option<LocalListenerRecord>,
    pub network: Option<NetworkListenerRecord>,
}
pub struct NetworkControl {
    pub(crate) tx: tokio::sync::watch::Sender<NetworkDesired>,
}
impl NetworkControl {
    pub fn new(desired: NetworkDesired) -> Self {
        let (tx, _) = tokio::sync::watch::channel(desired);
        Self { tx }
    }
    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<NetworkDesired> {
        self.tx.subscribe()
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TlsStatus {
    pub fingerprint_sha256: String,
    pub sans: Vec<String>,
}
