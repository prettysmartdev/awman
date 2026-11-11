//! Value types for the daemon's network listener configuration.

use std::fmt;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ListenAddr {
    host: ListenHost,
    port: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ListenHost {
    Ip(IpAddr),
    Name(String),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid listen address {0:?}; expected host:port (IPv6 literals must be bracketed)")]
pub struct ListenAddrError(String);

impl ListenAddr {
    pub fn parse(input: &str) -> Result<Self, ListenAddrError> {
        let invalid = || ListenAddrError(input.to_string());
        let (host, port) = if let Some(rest) = input.strip_prefix('[') {
            let (host, suffix) = rest.split_once(']').ok_or_else(invalid)?;
            let port = suffix.strip_prefix(':').ok_or_else(invalid)?;
            let ip = host.parse::<IpAddr>().map_err(|_| invalid())?;
            if !ip.is_ipv6() {
                return Err(invalid());
            }
            (ListenHost::Ip(ip), port)
        } else {
            let (host, port) = input.rsplit_once(':').ok_or_else(invalid)?;
            if host.is_empty() || host.contains(':') {
                return Err(invalid());
            }
            let parsed = host
                .parse::<IpAddr>()
                .map(ListenHost::Ip)
                .unwrap_or_else(|_| ListenHost::Name(host.to_string()));
            (parsed, port)
        };
        let port = port.parse::<u16>().map_err(|_| invalid())?;
        if port == 0 {
            return Err(invalid());
        }
        if let ListenHost::Name(name) = &host {
            if !valid_dns_name(name) {
                return Err(invalid());
            }
        }
        Ok(Self { host, port })
    }

    pub fn ip(&self) -> Option<IpAddr> {
        match self.host {
            ListenHost::Ip(ip) => Some(ip),
            ListenHost::Name(_) => None,
        }
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn is_loopback(&self) -> bool {
        self.ip().is_some_and(|ip| ip.is_loopback())
    }

    pub fn is_unspecified(&self) -> bool {
        self.ip().is_some_and(|ip| ip.is_unspecified())
    }

    pub fn to_socket_addrs(&self) -> std::io::Result<Vec<SocketAddr>> {
        self.to_string().to_socket_addrs().map(Iterator::collect)
    }
}

impl fmt::Display for ListenAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.host {
            ListenHost::Ip(IpAddr::V6(ip)) => write!(f, "[{ip}]:{}", self.port),
            ListenHost::Ip(ip) => write!(f, "{ip}:{}", self.port),
            ListenHost::Name(ref name) => write!(f, "{name}:{}", self.port),
        }
    }
}

impl TryFrom<String> for ListenAddr {
    type Error = ListenAddrError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(&value)
    }
}

impl From<ListenAddr> for String {
    fn from(value: ListenAddr) -> Self {
        value.to_string()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum TlsSan {
    Dns(String),
    Ip(IpAddr),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid TLS subject alternative name {0:?}; expected a DNS name or IP literal")]
pub struct TlsSanError(String);

impl TlsSan {
    pub fn parse(input: &str) -> Result<Self, TlsSanError> {
        if let Ok(ip) = input.parse::<IpAddr>() {
            return Ok(Self::Ip(ip));
        }
        if valid_dns_name(input) {
            return Ok(Self::Dns(input.to_ascii_lowercase()));
        }
        Err(TlsSanError(input.to_string()))
    }
}

impl TryFrom<String> for TlsSan {
    type Error = TlsSanError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(&value)
    }
}

impl From<TlsSan> for String {
    fn from(value: TlsSan) -> Self {
        match value {
            TlsSan::Dns(name) => name,
            TlsSan::Ip(ip) => ip.to_string(),
        }
    }
}

fn valid_dns_name(name: &str) -> bool {
    if name.is_empty() || name.len() > 253 || name.ends_with('.') {
        return false;
    }
    name.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && label.as_bytes()[0].is_ascii_alphanumeric()
            && label.as_bytes()[label.len() - 1].is_ascii_alphanumeric()
            && label
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listen_addresses_require_a_port_and_support_ip_literals() {
        for value in ["127.0.0.1:9876", "[::]:9876", "[2001:db8::1]:443"] {
            assert_eq!(ListenAddr::parse(value).unwrap().to_string(), value);
        }
        for value in ["127.0.0.1", "::1", "[::1]", "host:"] {
            assert!(ListenAddr::parse(value).is_err(), "{value}");
        }
    }

    #[test]
    fn tls_sans_accept_dns_or_ip_only() {
        assert!(TlsSan::parse("daemon.example.org").is_ok());
        assert!(TlsSan::parse("2001:db8::1").is_ok());
        assert!(TlsSan::parse("bad_name.example").is_err());
        assert!(TlsSan::parse("*.example.org").is_err());
    }
}

pub type ListenerName = String;
pub const DEFAULT_LISTENER: &str = "default";
pub const LOOPBACK_LISTENER: &str = "loopback";
pub const HOSTED_WEBAPP_ORIGIN: &str = "https://awman.prettysmart.dev";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ListenerTransport {
    #[default]
    Tls,
    LoopbackHttp,
}
impl fmt::Display for ListenerTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Tls => "tls",
            Self::LoopbackHttp => "loopback_http",
        })
    }
}

/// An exact ASCII HTTP origin. Explicit default ports remain significant.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct BrowserOrigin(String);
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "invalid origin {0:?}; exact origin like https://host or http://localhost:3000; no wildcard, null, path or userinfo"
)]
pub struct BrowserOriginError(String);
impl BrowserOrigin {
    pub fn parse(input: &str) -> Result<Self, BrowserOriginError> {
        let invalid = || BrowserOriginError(input.into());
        let (scheme, authority) = input.split_once("://").ok_or_else(invalid)?;
        if !matches!(scheme, "http" | "https")
            || !input.is_ascii()
            || authority.contains(['/', '?', '#', '@', '\\'])
        {
            return Err(invalid());
        }
        let (host, port) = if let Some(rest) = authority.strip_prefix('[') {
            let (host, suffix) = rest.split_once(']').ok_or_else(invalid)?;
            let ip = host.parse::<std::net::Ipv6Addr>().map_err(|_| invalid())?;
            (
                format!("[{ip}]"),
                if suffix.is_empty() {
                    None
                } else {
                    Some(suffix.strip_prefix(':').ok_or_else(invalid)?)
                },
            )
        } else {
            let (host, port) = authority
                .split_once(':')
                .map_or((authority, None), |(h, p)| (h, Some(p)));
            if !valid_dns_name(host) {
                return Err(invalid());
            }
            if host.rsplit('.').next().is_some_and(|last| {
                last.bytes().all(|b| b.is_ascii_digit()) || last.starts_with("0x")
            }) && host.parse::<std::net::Ipv4Addr>().is_err()
            {
                return Err(invalid());
            }
            (host.to_ascii_lowercase(), port)
        };
        let port = port
            .map(|p| {
                if p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) {
                    return Err(invalid());
                }
                p.parse::<u16>().map_err(|_| invalid()).and_then(|p| {
                    if p == 0 {
                        Err(invalid())
                    } else {
                        Ok(p)
                    }
                })
            })
            .transpose()?;
        Ok(Self(format!(
            "{scheme}://{host}{}",
            port.map(|p| format!(":{p}")).unwrap_or_default()
        )))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl fmt::Display for BrowserOrigin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl TryFrom<String> for BrowserOrigin {
    type Error = BrowserOriginError;
    fn try_from(v: String) -> Result<Self, Self::Error> {
        Self::parse(&v)
    }
}
impl From<BrowserOrigin> for String {
    fn from(v: BrowserOrigin) -> Self {
        v.0
    }
}

use super::daemon::{DaemonNetworkConfig, ListenerSettings};
use crate::data::daemon_state::FeatureName;
use std::collections::BTreeMap;

pub fn builtin_listener(name: &str) -> Option<ListenerSettings> {
    let (listen, transport, features, origins) = match name {
        DEFAULT_LISTENER => (
            "127.0.0.1:9876",
            ListenerTransport::Tls,
            vec![FeatureName::Worker],
            vec![],
        ),
        LOOPBACK_LISTENER => (
            "127.0.0.1:9877",
            ListenerTransport::LoopbackHttp,
            vec![FeatureName::Engine],
            [
                HOSTED_WEBAPP_ORIGIN,
                "http://127.0.0.1:3000",
                "http://localhost:3000",
            ]
            .into_iter()
            .map(|origin| BrowserOrigin::parse(origin).unwrap())
            .collect(),
        ),
        _ => return None,
    };
    Some(ListenerSettings {
        name: name.into(),
        enabled: false,
        listen: ListenAddr::parse(listen).unwrap(),
        transport,
        features,
        tls_sans: vec![],
        allowed_origins: origins,
        builtin: true,
    })
}
pub fn builtin_leaf(name: &str, leaf: &str) -> Option<serde_json::Value> {
    let l = builtin_listener(name)?;
    Some(match leaf {
        "enabled" => serde_json::json!(l.enabled),
        "listen" => serde_json::json!(l.listen),
        "transport" => serde_json::json!(l.transport),
        "features" => serde_json::json!(l.features),
        "tlsSans" => serde_json::json!(l.tls_sans),
        "allowedOrigins" => serde_json::json!(l.allowed_origins),
        _ => return None,
    })
}
pub fn resolve_listeners(config: &DaemonNetworkConfig) -> BTreeMap<ListenerName, ListenerSettings> {
    let mut all: BTreeMap<_, _> = [DEFAULT_LISTENER, LOOPBACK_LISTENER]
        .into_iter()
        .map(|n| (n.into(), builtin_listener(n).unwrap()))
        .collect();
    for (name, config) in config.listeners.as_ref().into_iter().flatten() {
        let mut l = builtin_listener(name).unwrap_or_else(|| ListenerSettings {
            name: name.clone(),
            enabled: false,
            // A missing custom address is rejected before the resolved view is used.
            listen: ListenAddr::parse("127.0.0.1:9876").unwrap(),
            transport: ListenerTransport::Tls,
            features: vec![],
            tls_sans: vec![],
            allowed_origins: vec![],
            builtin: false,
        });
        if let Some(v) = config.enabled {
            l.enabled = v;
        }
        if let Some(v) = &config.listen {
            l.listen = v.clone();
        }
        if let Some(v) = config.transport {
            l.transport = v;
        }
        if let Some(v) = &config.features {
            l.features = v.clone();
        }
        if let Some(v) = &config.tls_sans {
            l.tls_sans = v.clone();
        }
        if let Some(v) = &config.allowed_origins {
            l.allowed_origins = v.clone();
        }
        all.insert(name.clone(), l);
    }
    all
}
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ListenerValidationError {
    #[error("listener_listen_required: listener {name} needs a listen address")]
    ListenRequired { name: String },
    #[error("listener {name}: loopback_http requires literal 127.0.0.1 or [::1], got {listen}")]
    LoopbackHttpRequiresLoopbackLiteral { name: String, listen: ListenAddr },
    #[error("listener {name}: engine requires a loopback IP, got {listen}")]
    EngineRequiresLoopback { name: String, listen: ListenAddr },
    #[error("listener {name}: tlsSans requires tls transport")]
    TlsSansRequireTls { name: String },
    #[error("listeners {a} and {b} have overlapping binds on port {port}")]
    OverlappingBind { a: String, b: String, port: u16 },
    #[error("invalid or duplicate listener name {name}")]
    InvalidName { name: String },
}
pub fn validate_network_config(config: &DaemonNetworkConfig) -> Result<(), String> {
    for (name, l) in config.listeners.as_ref().into_iter().flatten() {
        if builtin_listener(name).is_none() && l.listen.is_none() {
            return Err(ListenerValidationError::ListenRequired { name: name.clone() }.to_string());
        }
    }
    validate_listeners(&resolve_listeners(config)).map_err(|errors| {
        errors
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("; ")
    })
}
pub fn binds_overlap(a: &ListenAddr, b: &ListenAddr) -> bool {
    if a.port() != b.port() {
        return false;
    }
    match (a.ip(), b.ip()) {
        (Some(a), Some(b)) => {
            a == b
                || (a.is_unspecified() && (a.is_ipv6() || b.is_ipv4()))
                || (b.is_unspecified() && (b.is_ipv6() || a.is_ipv4()))
        }
        _ => true,
    }
}
pub fn validate_listeners(
    all: &BTreeMap<ListenerName, ListenerSettings>,
) -> Result<(), Vec<ListenerValidationError>> {
    let mut errors = Vec::new();
    let mut names = std::collections::BTreeSet::new();
    for (name, l) in all {
        if !super::fields::is_valid_map_name(name) || !names.insert(name.replace('-', "_")) {
            errors.push(ListenerValidationError::InvalidName { name: name.clone() });
        }
        if !l.enabled {
            continue;
        }
        if l.transport == ListenerTransport::LoopbackHttp
            && !matches!(l.listen.ip(), Some(IpAddr::V4(v)) if v == std::net::Ipv4Addr::LOCALHOST)
            && l.listen.ip() != Some(IpAddr::V6(std::net::Ipv6Addr::LOCALHOST))
        {
            errors.push(
                ListenerValidationError::LoopbackHttpRequiresLoopbackLiteral {
                    name: name.clone(),
                    listen: l.listen.clone(),
                },
            );
        }
        if l.features.contains(&FeatureName::Engine) && !l.listen.is_loopback() {
            errors.push(ListenerValidationError::EngineRequiresLoopback {
                name: name.clone(),
                listen: l.listen.clone(),
            });
        }
        if l.transport != ListenerTransport::Tls && !l.tls_sans.is_empty() {
            errors.push(ListenerValidationError::TlsSansRequireTls { name: name.clone() });
        }
        for (other_name, other) in all.range(..name.clone()).filter(|(_, o)| o.enabled) {
            if binds_overlap(&l.listen, &other.listen) {
                errors.push(ListenerValidationError::OverlappingBind {
                    a: other_name.clone(),
                    b: name.clone(),
                    port: l.listen.port(),
                });
            }
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

#[cfg(test)]
mod listener_tests {
    use super::*;
    use crate::data::config::{ConfigFile, EffectiveConfig, FileKind};
    use serde_json::json;
    fn validate(listeners: serde_json::Value) -> Result<ConfigFile, crate::data::error::DataError> {
        ConfigFile::from_value(
            json!({"daemon":{"network":{"listeners":listeners}}}),
            FileKind::Global,
        )
    }
    #[test]
    fn builtin_defaults_and_custom_defaults_are_exact() {
        let config = EffectiveConfig::default();
        let all = config.daemon().network.listeners;
        assert_eq!(all.len(), 2);
        let d = &all[DEFAULT_LISTENER];
        let l = &all[LOOPBACK_LISTENER];
        assert!(!d.enabled && !l.enabled && d.builtin && l.builtin);
        assert_eq!(d.listen.to_string(), "127.0.0.1:9876");
        assert_eq!(l.listen.to_string(), "127.0.0.1:9877");
        assert_eq!(d.transport, ListenerTransport::Tls);
        assert_eq!(l.transport, ListenerTransport::LoopbackHttp);
        assert_eq!(d.features, [FeatureName::Worker]);
        assert_eq!(l.features, [FeatureName::Engine]);
        assert!(d.allowed_origins.is_empty() && d.tls_sans.is_empty() && l.tls_sans.is_empty());
        assert_eq!(
            l.allowed_origins,
            [
                HOSTED_WEBAPP_ORIGIN,
                "http://127.0.0.1:3000",
                "http://localhost:3000"
            ]
            .map(|origin| BrowserOrigin::parse(origin).unwrap())
        );
        let file = validate(json!({"custom":{"listen":"127.0.0.1:9999"}})).unwrap();
        let resolved = resolve_listeners(&file.daemon.unwrap().network.unwrap());
        let c = &resolved["custom"];
        assert!(
            !c.enabled
                && !c.builtin
                && c.features.is_empty()
                && c.tls_sans.is_empty()
                && c.allowed_origins.is_empty()
        );
        assert_eq!(c.transport, ListenerTransport::Tls);
        assert!(validate(json!({"custom":{}}))
            .unwrap_err()
            .to_string()
            .contains("listener_listen_required"));
        assert!(validate(json!({"custom":{"listen":null}})).is_err());
    }
    #[test]
    fn loopback_http_accepts_only_two_literal_hosts() {
        for host in ["127.0.0.1", "[::1]"] {
            assert!(
                validate(json!({"loopback":{"enabled":true,"listen":format!("{host}:9877")}}))
                    .is_ok(),
                "{host}"
            );
        }
        for host in [
            "localhost",
            "127.0.0.2",
            "0.0.0.0",
            "[::]",
            "192.0.2.1",
            "[::ffff:127.0.0.1]",
        ] {
            assert!(validate(json!({"loopback":{"enabled":true,"features":[],"listen":format!("{host}:9877")}})).is_err(), "{host}");
        }
        // Disabled listeners may be staged; enabling revalidates the combination.
        assert!(validate(json!({"loopback":{"listen":"0.0.0.0:9877"}})).is_ok());
    }
    #[test]
    fn engine_requires_loopback_on_every_transport() {
        for transport in ["tls", "loopback_http"] {
            for host in ["localhost", "0.0.0.0", "[::]", "192.0.2.1"] {
                let error = validate(json!({"default":{"enabled":true,"transport":transport,"features":["engine"],"listen":format!("{host}:9876")}})).unwrap_err();
                assert!(error.to_string().contains("engine requires"), "{error}");
            }
        }
        assert!(validate(json!({"loopback":{"enabled":true,"tlsSans":["localhost"]}})).is_err());
        assert!(validate(json!({"default":{"enabled":true,"tlsSans":["localhost"]}})).is_ok());
    }
    #[test]
    fn duplicate_and_overlapping_binds_are_rejected() {
        for (a, b) in [
            ("127.0.0.1:9000", "127.0.0.1:9000"),
            ("0.0.0.0:9000", "127.0.0.1:9000"),
            ("[::]:9000", "127.0.0.1:9000"),
            ("[::]:9000", "[::1]:9000"),
            ("localhost:9000", "127.0.0.1:9000"),
        ] {
            assert!(
                validate(json!({"a":{"enabled":true,"listen":a},"b":{"enabled":true,"listen":b}}))
                    .unwrap_err()
                    .to_string()
                    .contains("overlapping"),
                "{a} {b}"
            );
        }
        assert!(validate(json!({"a":{"enabled":true,"listen":"127.0.0.1:9000"},"b":{"enabled":true,"listen":"127.0.0.1:9001"}})).is_ok());
        assert!(validate(
            json!({"foo-bar":{"listen":"127.0.0.1:9000"},"foo_bar":{"listen":"127.0.0.1:9001"}})
        )
        .is_err());
        assert!(validate(json!({"Bad":{"listen":"127.0.0.1:9000"}})).is_err());
    }
    #[test]
    fn exact_origins_reject_non_origins() {
        for origin in [
            HOSTED_WEBAPP_ORIGIN,
            "http://localhost:3000",
            "http://127.0.0.1:9877",
            "https://[::1]:9877",
            "https://example.org:443",
        ] {
            assert!(BrowserOrigin::parse(origin).is_ok(), "{origin}");
        }
        for origin in [
            "*",
            "null",
            "https://*.example.org",
            "https://a/",
            "https://a/path",
            "https://a?q=x",
            "https://a#x",
            "https://u@a",
            "https://u:p@a",
            "https://é.example",
            "ftp://a",
            "https://a:",
            "https://a:65536",
            "https://a:0",
            "https://a:foo",
            "https://a\n",
            "https://a:443:80",
            "https://[::1",
            "",
            "https://a https://b",
        ] {
            assert!(BrowserOrigin::parse(origin).is_err(), "{origin}");
        }
        assert_ne!(
            BrowserOrigin::parse("https://a").unwrap(),
            BrowserOrigin::parse("https://a:443").unwrap()
        );
        assert_eq!(
            BrowserOrigin::parse("https://EXAMPLE.org")
                .unwrap()
                .as_str(),
            "https://example.org"
        );
        for origin in ["*", "null", "https://a/path"] {
            assert!(validate(json!({"default":{"allowedOrigins":[origin]}})).is_err());
        }
    }
    #[test]
    fn old_keys_are_plain_unknown_without_hints() {
        for leaf in ["enabled", "listen", "features", "tlsSans"] {
            let path = format!("daemon.network.{leaf}");
            let error = super::super::fields::resolve(&path).unwrap_err();
            assert!(matches!(
                error,
                super::super::fields::KeyError::Unknown { .. }
            ));
            assert!(!error.to_string().contains("moved"));
            assert!(ConfigFile::from_value(
                json!({"daemon":{"network":{leaf:true}}}),
                FileKind::Global
            )
            .is_err());
        }
    }
}
