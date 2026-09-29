//! `BuiltinNetworkConfig` — the `builtin.network` block of global and repo
//! config, and the resolved [`BuiltinNetworkSettings`] the engine enforces.
//!
//! The builtin runtime gives every session VM its own user-space network
//! stack on the host. Nothing a guest sends leaves the host except through
//! that stack, so the policy below is enforced outside the guest, at the VMM
//! boundary, and the guest cannot change it.
//!
//! Modes ([`NetworkMode`]):
//!
//! * `public` (default): DNS through the host-side resolver plus the public
//!   internet. Private/LAN ranges, the host, loopback, link-local and cloud
//!   metadata addresses are refused.
//! * `allowlist`: nothing is reachable except the names in `allow` (exact
//!   names, or `*.example.com` for the domain and its subdomains). DNS
//!   answers only those names, so DNS itself cannot carry data to an
//!   arbitrary domain, and TLS connections must present an allowed SNI name.
//! * `none`: the guest has no network device at all. Cached images still
//!   run; nothing is resolved or dialled.
//!
//! Guest loopback (`127.0.0.1`, `::1`, `localhost`) is always the guest's own
//! loopback, never the host's. A host-local service (for example an MCP
//! server listening on the host's `127.0.0.1:8765`) is reachable only when its
//! TCP port is listed in `hostPorts`; the guest then dials
//! [`HOST_ALIAS`]`:<port>` and the host-side stack connects to the host's
//! loopback on that port. Every other host port stays refused.
//!
//! Layering: the global block is the ceiling a repository cannot raise. A
//! repo block may pick a stricter mode, a subset of the global allow list
//! and a subset of the global `hostPorts`; asking for more is an error, not
//! a silent downgrade. `nameservers` and `trustHostCas` change what the host
//! does on the guest's behalf and are honoured from the global config only.

use std::collections::BTreeSet;
use std::net::{IpAddr, SocketAddr};

use serde::{Deserialize, Serialize};

/// Name a guest dials to reach a host port listed in `hostPorts`.
pub const HOST_ALIAS: &str = "host.microsandbox.internal";

/// Most entries accepted in `allow` or `hostPorts`; each becomes a policy
/// rule evaluated per connection.
pub const MAX_NETWORK_ENTRIES: usize = 256;

/// How much network a session VM gets. Ordered from most to least
/// restrictive, so `a <= b` means `a` is at least as strict as `b`.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "kebab-case")]
pub enum NetworkMode {
    /// No network device.
    None,
    /// Only the names in `allow` (plus authorised host ports).
    Allowlist,
    /// DNS and the public internet.
    #[default]
    Public,
}

impl NetworkMode {
    pub fn as_str(self) -> &'static str {
        match self {
            NetworkMode::None => "none",
            NetworkMode::Allowlist => "allowlist",
            NetworkMode::Public => "public",
        }
    }
}

/// The `builtin.network` block as written in one config file. Every field is
/// optional; see the module docs for how global and repo blocks combine.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BuiltinNetworkConfig {
    /// Network mode. Default `public`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<NetworkMode>,
    /// Allowed destination names for `allowlist` mode: `api.example.com` or
    /// `*.example.com`. IP addresses and CIDRs are refused: name the host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow: Option<Vec<String>>,
    /// Host loopback TCP ports a guest may reach as `HOST_ALIAS:<port>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_ports: Option<Vec<u16>>,
    /// Upstream resolvers the host-side DNS forwarder uses, as `IP` or
    /// `IP:PORT`. Default: the host's own resolver configuration. Global only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nameservers: Option<Vec<String>>,
    /// Install the host's trusted root CAs in the guest at boot (for
    /// corporate TLS-inspecting proxies). Default `false`. Global only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trust_host_cas: Option<bool>,
}

/// One entry of the allow list, validated and lower-cased.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum NetworkAllowEntry {
    /// Exactly this name.
    Domain(String),
    /// This name and every subdomain of it (written `*.name`).
    Suffix(String),
}

impl NetworkAllowEntry {
    /// Parse one `allow` entry.
    pub fn parse(raw: &str) -> Result<Self, String> {
        let trimmed = raw.trim();
        if trimmed.parse::<IpAddr>().is_ok()
            || trimmed.contains('/')
            || trimmed.contains(':')
            || trimmed.starts_with('[')
        {
            return Err(format!(
                "{raw:?} is an address, not a name; allow entries must be DNS names \
                 (IP and CIDR destinations are not supported)"
            ));
        }
        let (suffix, name) = match trimmed.strip_prefix("*.") {
            Some(rest) => (true, rest),
            None => (false, trimmed),
        };
        let lowered = name.trim_end_matches('.').to_ascii_lowercase();
        if is_loopback_name(&lowered) || lowered == HOST_ALIAS {
            return Err(format!(
                "{raw:?} names the guest's own loopback or the host alias; use hostPorts to \
                 authorise a host service"
            ));
        }
        let name = canonical_domain(name).map_err(|e| format!("{raw:?}: {e}"))?;
        Ok(if suffix {
            NetworkAllowEntry::Suffix(name)
        } else {
            NetworkAllowEntry::Domain(name)
        })
    }

    /// The name this entry matches (without the `*.`).
    pub fn name(&self) -> &str {
        match self {
            NetworkAllowEntry::Domain(name) | NetworkAllowEntry::Suffix(name) => name,
        }
    }

    /// Whether every name this entry matches is also matched by `other`.
    fn within(&self, other: &NetworkAllowEntry) -> bool {
        match (self, other) {
            (_, NetworkAllowEntry::Suffix(parent)) => {
                let name = self.name();
                name == parent || name.ends_with(&format!(".{parent}"))
            }
            (NetworkAllowEntry::Domain(a), NetworkAllowEntry::Domain(b)) => a == b,
            (NetworkAllowEntry::Suffix(_), NetworkAllowEntry::Domain(_)) => false,
        }
    }
}

impl std::fmt::Display for NetworkAllowEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NetworkAllowEntry::Domain(name) => f.write_str(name),
            NetworkAllowEntry::Suffix(name) => write!(f, "*.{name}"),
        }
    }
}

/// The effective, validated network settings for session VMs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BuiltinNetworkSettings {
    pub mode: NetworkMode,
    /// Allowed names; non-empty only in `allowlist` mode.
    pub allow: Vec<NetworkAllowEntry>,
    /// Authorised host loopback TCP ports, sorted and de-duplicated.
    pub host_ports: Vec<u16>,
    /// Host-side upstream resolvers; empty means the host's configuration.
    pub nameservers: Vec<SocketAddr>,
    pub trust_host_cas: bool,
}

impl BuiltinNetworkConfig {
    /// Check one config file's block on its own.
    pub fn validate(&self) -> Result<(), String> {
        self.parsed().map(|_| ())
    }

    fn parsed(&self) -> Result<ParsedBlock, String> {
        let allow = match &self.allow {
            None => None,
            Some(entries) => {
                if entries.len() > MAX_NETWORK_ENTRIES {
                    return Err(format!(
                        "allow has {} entries; at most {MAX_NETWORK_ENTRIES} are supported",
                        entries.len()
                    ));
                }
                let parsed = entries
                    .iter()
                    .map(|e| NetworkAllowEntry::parse(e).map_err(|m| format!("allow: {m}")))
                    .collect::<Result<BTreeSet<_>, _>>()?;
                Some(parsed)
            }
        };
        let host_ports = match &self.host_ports {
            None => None,
            Some(ports) => {
                if ports.len() > MAX_NETWORK_ENTRIES {
                    return Err(format!(
                        "hostPorts has {} entries; at most {MAX_NETWORK_ENTRIES} are supported",
                        ports.len()
                    ));
                }
                if ports.contains(&0) {
                    return Err("hostPorts: port 0 is not a valid TCP port".into());
                }
                if ports.contains(&53) {
                    return Err(
                        "hostPorts: port 53 is reserved for the guest's DNS, which the host-side \
                         forwarder answers"
                            .into(),
                    );
                }
                Some(ports.iter().copied().collect::<BTreeSet<_>>())
            }
        };
        let nameservers = match &self.nameservers {
            None => None,
            Some(servers) => Some(
                servers
                    .iter()
                    .map(|s| parse_nameserver(s).map_err(|m| format!("nameservers: {m}")))
                    .collect::<Result<Vec<_>, _>>()?,
            ),
        };
        let block = ParsedBlock {
            mode: self.mode,
            allow,
            host_ports,
            nameservers,
            trust_host_cas: self.trust_host_cas,
        };
        // Contradictions visible inside a single block.
        if let Some(mode) = block.mode {
            if mode != NetworkMode::Allowlist && block.allow.is_some() {
                return Err(format!(
                    "allow only applies to mode \"allowlist\" (mode is {:?})",
                    mode.as_str()
                ));
            }
            if mode == NetworkMode::None && block.host_ports.as_ref().is_some_and(|p| !p.is_empty())
            {
                return Err(
                    "hostPorts cannot be reached in mode \"none\" (no network device)".into(),
                );
            }
        }
        Ok(block)
    }

    /// Combine the global and repo blocks into the settings the engine
    /// enforces. `global` is the ceiling; see the module docs.
    pub fn resolve(
        global: Option<&BuiltinNetworkConfig>,
        repo: Option<&BuiltinNetworkConfig>,
    ) -> Result<BuiltinNetworkSettings, String> {
        let global = global
            .map(|g| {
                g.parsed()
                    .map_err(|e| format!("global builtin.network: {e}"))
            })
            .transpose()?
            .unwrap_or_default();
        let repo = repo
            .map(|r| r.parsed().map_err(|e| format!("repo builtin.network: {e}")))
            .transpose()?
            .unwrap_or_default();

        if repo.nameservers.is_some() {
            return Err(
                "repo builtin.network.nameservers is not allowed: host-side resolvers are set in \
                 the global config only"
                    .into(),
            );
        }
        if repo.trust_host_cas.is_some() {
            return Err(
                "repo builtin.network.trustHostCas is not allowed: host trust is shared into \
                 guests from the global config only"
                    .into(),
            );
        }

        let global_mode = global.mode.unwrap_or_default();
        let mode = match repo.mode {
            Some(requested) if requested > global_mode => {
                return Err(format!(
                    "repo builtin.network.mode {:?} is less restrictive than the global mode {:?}",
                    requested.as_str(),
                    global_mode.as_str()
                ));
            }
            Some(requested) => requested,
            None => global_mode,
        };

        if global.allow.is_some() && global_mode != NetworkMode::Allowlist {
            return Err("global builtin.network.allow only applies to mode \"allowlist\"".into());
        }
        let allow = if mode == NetworkMode::Allowlist {
            let ceiling = global.allow.clone().unwrap_or_default();
            match (global_mode == NetworkMode::Allowlist, &repo.allow) {
                (true, Some(requested)) => {
                    if let Some(extra) = requested
                        .iter()
                        .find(|entry| !ceiling.iter().any(|c| entry.within(c)))
                    {
                        return Err(format!(
                            "repo builtin.network.allow entry {extra} is not permitted by the \
                             global allow list"
                        ));
                    }
                    requested.clone()
                }
                (false, Some(requested)) => requested.clone(),
                (true, None) => ceiling,
                (false, None) => BTreeSet::new(),
            }
        } else {
            if repo.allow.is_some() {
                return Err(format!(
                    "repo builtin.network.allow only applies to mode \"allowlist\" (effective mode \
                     is {:?})",
                    mode.as_str()
                ));
            }
            BTreeSet::new()
        };

        let authorised = global.host_ports.clone().unwrap_or_default();
        let host_ports = match &repo.host_ports {
            Some(requested) => {
                if let Some(port) = requested.iter().find(|p| !authorised.contains(p)) {
                    return Err(format!(
                        "repo builtin.network.hostPorts port {port} is not authorised by the \
                         global hostPorts"
                    ));
                }
                requested.clone()
            }
            None => authorised,
        };
        if mode == NetworkMode::None && !host_ports.is_empty() {
            return Err(
                "builtin.network.hostPorts cannot be reached in mode \"none\" (no network device); \
                 remove hostPorts or choose another mode"
                    .into(),
            );
        }
        let nameservers = global.nameservers.unwrap_or_default();
        let trust_host_cas = global.trust_host_cas.unwrap_or(false);
        if mode == NetworkMode::None && (!nameservers.is_empty() || trust_host_cas) {
            return Err(
                "builtin.network.nameservers and trustHostCas have no effect in mode \"none\"; \
                 remove them or choose another mode"
                    .into(),
            );
        }

        Ok(BuiltinNetworkSettings {
            mode,
            allow: allow.into_iter().collect(),
            host_ports: host_ports.into_iter().collect(),
            nameservers,
            trust_host_cas,
        })
    }
}

#[derive(Default)]
struct ParsedBlock {
    mode: Option<NetworkMode>,
    allow: Option<BTreeSet<NetworkAllowEntry>>,
    host_ports: Option<BTreeSet<u16>>,
    nameservers: Option<Vec<SocketAddr>>,
    trust_host_cas: Option<bool>,
}

/// Lower-case and check a DNS name: 1–63 byte labels of letters, digits and
/// inner hyphens, at least two labels, at most 253 bytes.
fn canonical_domain(raw: &str) -> Result<String, String> {
    let name = raw.trim_end_matches('.').to_ascii_lowercase();
    if name.is_empty() {
        return Err("empty name".into());
    }
    if name.len() > 253 {
        return Err("name longer than 253 bytes".into());
    }
    if name.contains('*') {
        return Err("a wildcard is only allowed as a leading \"*.\"".into());
    }
    let labels: Vec<&str> = name.split('.').collect();
    if labels.len() < 2 {
        return Err("name must have at least two labels (e.g. example.com)".into());
    }
    for label in &labels {
        let valid = !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
        if !valid {
            return Err(format!("invalid DNS label {label:?}"));
        }
    }
    if labels
        .last()
        .is_some_and(|tld| tld.bytes().all(|b| b.is_ascii_digit()))
    {
        return Err("numeric top-level label; name the host, not an address".into());
    }
    Ok(name)
}

fn is_loopback_name(name: &str) -> bool {
    name == "localhost" || name.ends_with(".localhost")
}

fn parse_nameserver(raw: &str) -> Result<SocketAddr, String> {
    let raw = raw.trim();
    if let Ok(addr) = raw.parse::<SocketAddr>() {
        if addr.port() == 0 {
            return Err(format!("{raw:?}: port 0 is not valid"));
        }
        return Ok(addr);
    }
    if let Ok(ip) = raw.parse::<IpAddr>() {
        return Ok(SocketAddr::new(ip, 53));
    }
    Err(format!(
        "{raw:?} is not an IP address or IP:PORT (host names are not accepted, so resolving a \
         resolver never depends on another resolver)"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(json: &str) -> BuiltinNetworkConfig {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn default_is_public_with_nothing_else() {
        let s = BuiltinNetworkConfig::resolve(None, None).unwrap();
        assert_eq!(s, BuiltinNetworkSettings::default());
        assert_eq!(s.mode, NetworkMode::Public);
        assert_eq!(
            serde_json::to_string(&BuiltinNetworkConfig::default()).unwrap(),
            "{}"
        );
    }

    #[test]
    fn unknown_fields_and_modes_are_rejected() {
        assert!(serde_json::from_str::<BuiltinNetworkConfig>(r#"{"proxy":"x"}"#).is_err());
        assert!(serde_json::from_str::<BuiltinNetworkConfig>(r#"{"mode":"host"}"#).is_err());
        assert!(serde_json::from_str::<BuiltinNetworkConfig>(r#"{"hostPorts":[70000]}"#).is_err());
        assert!(serde_json::from_str::<BuiltinNetworkConfig>(r#"{"hostPorts":[-1]}"#).is_err());
    }

    #[test]
    fn allow_entries_are_names_only() {
        assert_eq!(
            NetworkAllowEntry::parse("API.Example.com.").unwrap(),
            NetworkAllowEntry::Domain("api.example.com".into())
        );
        assert_eq!(
            NetworkAllowEntry::parse("*.github.com").unwrap(),
            NetworkAllowEntry::Suffix("github.com".into())
        );
        for bad in [
            "",
            "1.2.3.4",
            "10.0.0.0/8",
            "::1",
            "[::1]",
            "example.com:443",
            "localhost",
            "a.localhost",
            HOST_ALIAS,
            "a.*.example.com",
            "*",
            "com",
            "exa mple.com",
            "-bad.example.com",
            "1.2.3.400",
        ] {
            assert!(NetworkAllowEntry::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn single_block_contradictions_are_errors() {
        assert!(block(r#"{"mode":"public","allow":["a.com"]}"#)
            .validate()
            .is_err());
        assert!(block(r#"{"mode":"none","hostPorts":[80]}"#)
            .validate()
            .is_err());
        assert!(block(r#"{"hostPorts":[0]}"#).validate().is_err());
        assert!(block(r#"{"hostPorts":[53]}"#).validate().is_err());
        assert!(block(r#"{"nameservers":["dns.google"]}"#)
            .validate()
            .is_err());
        assert!(block(r#"{"nameservers":["1.1.1.1:0"]}"#)
            .validate()
            .is_err());
        assert!(
            block(r#"{"mode":"allowlist","allow":["a.com"],"hostPorts":[8765]}"#)
                .validate()
                .is_ok()
        );
    }

    #[test]
    fn repo_can_narrow_but_never_widen() {
        let global = block(
            r#"{"mode":"allowlist","allow":["*.example.com","api.other.com"],"hostPorts":[8765,9000]}"#,
        );
        let narrowed = BuiltinNetworkConfig::resolve(
            Some(&global),
            Some(&block(
                r#"{"allow":["api.example.com"],"hostPorts":[8765]}"#,
            )),
        )
        .unwrap();
        assert_eq!(narrowed.mode, NetworkMode::Allowlist);
        assert_eq!(
            narrowed.allow,
            vec![NetworkAllowEntry::Domain("api.example.com".into())]
        );
        assert_eq!(narrowed.host_ports, vec![8765]);

        for widen in [
            r#"{"mode":"public"}"#,
            r#"{"allow":["evil.com"]}"#,
            r#"{"allow":["*.other.com"]}"#,
            r#"{"hostPorts":[22]}"#,
            r#"{"nameservers":["1.1.1.1"]}"#,
            r#"{"trustHostCas":true}"#,
        ] {
            assert!(
                BuiltinNetworkConfig::resolve(Some(&global), Some(&block(widen))).is_err(),
                "{widen}"
            );
        }
        // An allowlist global without names permits no repo names either.
        assert!(BuiltinNetworkConfig::resolve(
            Some(&block(r#"{"mode":"allowlist"}"#)),
            Some(&block(r#"{"allow":["a.com"]}"#))
        )
        .is_err());
        // A stricter mode is always allowed.
        let none = BuiltinNetworkConfig::resolve(
            Some(&block(r#"{"mode":"allowlist","allow":["a.com"]}"#)),
            Some(&block(r#"{"mode":"none"}"#)),
        )
        .unwrap();
        assert_eq!(none.mode, NetworkMode::None);
        assert!(none.allow.is_empty());
    }

    #[test]
    fn host_ports_need_global_authorisation() {
        assert!(
            BuiltinNetworkConfig::resolve(None, Some(&block(r#"{"hostPorts":[8765]}"#))).is_err()
        );
        let s =
            BuiltinNetworkConfig::resolve(Some(&block(r#"{"hostPorts":[9000,8765,8765]}"#)), None)
                .unwrap();
        assert_eq!(s.host_ports, vec![8765, 9000]);
        // Global host ports cannot survive a repo switch to "none".
        assert!(BuiltinNetworkConfig::resolve(
            Some(&block(r#"{"hostPorts":[8765]}"#)),
            Some(&block(r#"{"mode":"none"}"#))
        )
        .is_err());
        assert_eq!(
            BuiltinNetworkConfig::resolve(
                Some(&block(r#"{"hostPorts":[8765]}"#)),
                Some(&block(r#"{"mode":"none","hostPorts":[]}"#))
            )
            .unwrap()
            .mode,
            NetworkMode::None
        );
    }

    #[test]
    fn repo_allowlist_under_public_global_is_a_narrowing() {
        let s = BuiltinNetworkConfig::resolve(
            None,
            Some(&block(
                r#"{"mode":"allowlist","allow":["api.anthropic.com"]}"#,
            )),
        )
        .unwrap();
        assert_eq!(s.mode, NetworkMode::Allowlist);
        assert_eq!(s.allow.len(), 1);
        assert!(
            BuiltinNetworkConfig::resolve(None, Some(&block(r#"{"allow":["a.com"]}"#))).is_err()
        );
    }

    #[test]
    fn nameservers_default_port_and_global_only_host_settings() {
        let s = BuiltinNetworkConfig::resolve(
            Some(&block(
                r#"{"nameservers":["1.1.1.1","[2606:4700::1111]:5353"],"trustHostCas":true}"#,
            )),
            None,
        )
        .unwrap();
        assert_eq!(
            s.nameservers,
            vec![
                "1.1.1.1:53".parse().unwrap(),
                "[2606:4700::1111]:5353".parse().unwrap()
            ]
        );
        assert!(s.trust_host_cas);
        assert!(BuiltinNetworkConfig::resolve(
            Some(&block(r#"{"mode":"none","trustHostCas":true}"#)),
            None
        )
        .is_err());
    }

    #[test]
    fn oversized_lists_are_rejected_before_use() {
        let many: Vec<String> = (0..=MAX_NETWORK_ENTRIES)
            .map(|i| format!("h{i}.example.com"))
            .collect();
        let cfg = BuiltinNetworkConfig {
            mode: Some(NetworkMode::Allowlist),
            allow: Some(many),
            ..Default::default()
        };
        assert!(cfg.validate().is_err());
    }
}
