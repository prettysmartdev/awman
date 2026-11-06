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
