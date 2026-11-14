//! Network policy for session VMs: compiles the validated
//! [`BuiltinNetworkSettings`] into the plan the SDK driver applies.
//!
//! Enforcement happens in the SDK's user-space network stack, which runs in
//! the host-side worker and carries every packet the guest sends: TCP and UDP
//! are proxied per flow, every DNS query (UDP or TCP port 53, to any address)
//! is answered by the host-side forwarder, and each flow and query is
//! checked against the policy compiled here. The guest can neither see nor
//! change it. This module is SDK-independent: [`NetworkPlan::policy`]
//! serialises to the SDK's `NetworkPolicy` wire form, and the driver must
//! refuse to create a VM whose plan it cannot apply.
//!
//! What each mode compiles to:
//!
//! | mode | device | DNS | egress |
//! |---|---|---|---|
//! | `none` | disabled | none | none |
//! | `public` | enabled | any name via the host forwarder | public internet; authorised host ports |
//! | `allowlist` | enabled | allowed names only (others get NXDOMAIN) | allowed names; authorised host ports |
//!
//! In every mode: inbound connections are refused (awman publishes no guest
//! ports), private/LAN, loopback, link-local and cloud-metadata destinations
//! are refused, the host is reachable only on authorised ports via
//! [`HOST_ALIAS`], TLS is never intercepted (API credentials stay end-to-end
//! between the agent and its service), the host's proxy environment is not
//! applied to guest traffic, and DNS rebinding protection stays on.
//! `allowlist` also turns on strict hostname checking, so a TLS connection
//! must name an allowed host in its SNI rather than reuse an allowed name's
//! address for another site. This checks visible SNI and its DNS binding;
//! encrypted HTTP authority, domain fronting by an allowed server, and ECH
//! inner names cannot be inspected without TLS interception. Name rules
//! authorize TCP only, so UDP/QUIC cannot bypass the SNI boundary.

use std::net::SocketAddr;

use serde::Serialize;

use crate::data::config::builtin_network::{
    BuiltinNetworkSettings, NetworkAllowEntry, NetworkMode, HOST_ALIAS,
};

/// Everything the SDK driver applies to one VM's network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkPlan {
    /// Whether the guest gets a network device at all.
    pub enabled: bool,
    /// Egress/ingress rules in the SDK's `NetworkPolicy` wire form.
    pub policy: PolicyWire,
    /// Require visible TLS SNI and its exact DNS binding for name-based allows.
    pub strict: bool,
    /// Host-side upstream resolvers; empty means the host's configuration.
    pub nameservers: Vec<SocketAddr>,
    /// Install the host's trusted root CAs in the guest.
    pub trust_host_cas: bool,
}

impl NetworkPlan {
    /// The policy as JSON for `serde_json::from_value::<NetworkPolicy>`.
    pub fn policy_json(&self) -> serde_json::Value {
        serde_json::to_value(&self.policy).expect("policy wire types always serialise")
    }
}

impl Default for NetworkPlan {
    /// The plan for default settings (`public`).
    fn default() -> Self {
        compile(&BuiltinNetworkSettings::default())
    }
}

/// `microsandbox_network::policy::NetworkPolicy` wire form.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PolicyWire {
    pub default_egress: ActionWire,
    pub default_ingress: ActionWire,
    pub rules: Vec<RuleWire>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionWire {
    Allow,
    Deny,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RuleWire {
    pub direction: DirectionWire,
    pub destination: DestinationWire,
    pub protocols: Vec<ProtocolWire>,
    pub ports: Vec<PortRangeWire>,
    pub action: ActionWire,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DirectionWire {
    Egress,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DestinationWire {
    Domain(String),
    DomainSuffix(String),
    Group(GroupWire),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GroupWire {
    Public,
    Host,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProtocolWire {
    Tcp,
    Udp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct PortRangeWire {
    pub start: u16,
    pub end: u16,
}

fn rule(
    destination: DestinationWire,
    protocols: Vec<ProtocolWire>,
    ports: Vec<u16>,
    action: ActionWire,
) -> RuleWire {
    RuleWire {
        direction: DirectionWire::Egress,
        destination,
        protocols,
        ports: ports
            .into_iter()
            .map(|p| PortRangeWire { start: p, end: p })
            .collect(),
        action,
    }
}

fn dns_protocols() -> Vec<ProtocolWire> {
    vec![ProtocolWire::Udp, ProtocolWire::Tcp]
}

/// Compile validated settings. Rules are first-match-wins, so the order is
/// part of the contract: authorised host ports, then DNS, then an explicit
/// refusal of every other host port, then the mode's destinations. The host
/// refusal precedes name allows so that a flow to the gateway address can
/// never match an allowed name (e.g. by SNI) and reach the host.
pub fn compile(settings: &BuiltinNetworkSettings) -> NetworkPlan {
    use ActionWire::{Allow, Deny};
    use DestinationWire::{Domain, DomainSuffix, Group};

    let deny_all = PolicyWire {
        default_egress: Deny,
        default_ingress: Deny,
        rules: Vec::new(),
    };
    if settings.mode == NetworkMode::None {
        return NetworkPlan {
            enabled: false,
            policy: deny_all,
            strict: true,
            nameservers: Vec::new(),
            trust_host_cas: false,
        };
    }

    let mut rules = Vec::new();
    if !settings.host_ports.is_empty() {
        rules.push(rule(
            Group(GroupWire::Host),
            vec![ProtocolWire::Tcp],
            settings.host_ports.clone(),
            Allow,
        ));
    }
    match settings.mode {
        NetworkMode::Public => {
            // Any name may be resolved through the host forwarder.
            rules.push(rule(
                Group(GroupWire::Host),
                dns_protocols(),
                vec![53],
                Allow,
            ));
        }
        NetworkMode::Allowlist => {
            // DNS queries match name rules regardless of protocol and port,
            // so resolving the host alias is allowed only when host ports
            // are; the UDP/53 filter keeps it from matching real flows.
            if !settings.host_ports.is_empty() {
                rules.push(rule(
                    Domain(HOST_ALIAS.into()),
                    vec![ProtocolWire::Udp],
                    vec![53],
                    Allow,
                ));
            }
        }
        NetworkMode::None => unreachable!("handled above"),
    }
    // Every other host port. Port 53 is left out because a `host` rule also
    // decides DNS queries (which are addressed to the gateway on port 53);
    // flows to the gateway on port 53 are DNS and never reach the host.
    rules.push(RuleWire {
        ports: vec![
            PortRangeWire { start: 0, end: 52 },
            PortRangeWire {
                start: 54,
                end: u16::MAX,
            },
        ],
        ..rule(Group(GroupWire::Host), dns_protocols(), Vec::new(), Deny)
    });
    match settings.mode {
        NetworkMode::Public => {
            rules.push(rule(
                Group(GroupWire::Public),
                Vec::new(),
                Vec::new(),
                Allow,
            ));
        }
        NetworkMode::Allowlist => {
            for entry in &settings.allow {
                let destination = match entry {
                    NetworkAllowEntry::Domain(name) => Domain(name.clone()),
                    NetworkAllowEntry::Suffix(name) => DomainSuffix(name.clone()),
                };
                // The SDK inspects TCP authority. Its UDP path only has a
                // DNS cache binding, which cannot authenticate QUIC names.
                rules.push(rule(
                    destination,
                    vec![ProtocolWire::Tcp],
                    Vec::new(),
                    Allow,
                ));
            }
        }
        NetworkMode::None => unreachable!("handled above"),
    }
    NetworkPlan {
        enabled: true,
        policy: PolicyWire {
            default_egress: Deny,
            default_ingress: Deny,
            rules,
        },
        strict: settings.mode == NetworkMode::Allowlist,
        nameservers: settings.nameservers.clone(),
        trust_host_cas: settings.trust_host_cas,
    }
}
