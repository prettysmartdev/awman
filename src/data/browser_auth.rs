//! Browser pairing wire types (WI 0159): browser links, grants and WebSocket
//! upgrade tickets.
//!
//! These are the `/v1/browser-links*`, `/v1/browsers*` and
//! `/v1/browser-tickets` request and response bodies plus the fixed
//! lifetimes from the design's D4. Secrets travelling on the wire (polling
//! secrets, grant tokens, tickets) use [`BrowserSecret`], which serializes
//! as a plain string but never prints in `Debug` output.

use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::data::ids::{BrowserGrantId, BrowserLinkId, InstanceId, TerminalId};

/// A pending link lives this long (D4).
pub const LINK_TTL: Duration = Duration::from_secs(300);
/// The poll interval handed to the browser (D4).
pub const LINK_POLL_INTERVAL: Duration = Duration::from_secs(2);
/// Absolute grant lifetime from approval; grants never renew (D4, D7).
pub const GRANT_TTL: Duration = Duration::from_secs(12 * 60 * 60);
/// WebSocket upgrade ticket lifetime (D4).
pub const TICKET_TTL: Duration = Duration::from_secs(30);
/// Live tickets per grant; the oldest is evicted beyond this.
pub const MAX_TICKETS_PER_GRANT: usize = 64;
/// Largest HTTP request body accepted from a browser.
pub const BROWSER_BODY_LIMIT: usize = 64 * 1024;
/// Largest `Hello` frame accepted on a browser frontend socket.
pub const BROWSER_HELLO_LIMIT: usize = 16 * 1024;

/// Link creations per origin per minute on one listener.
pub const LINK_CREATES_PER_MINUTE: u32 = 5;
/// Pending (undecided, unexpired) links per listener at once.
pub const MAX_PENDING_LINKS_PER_LISTENER: usize = 3;
/// Poll failures per origin per minute before polling is locked out.
pub const POLL_FAILURES_PER_MINUTE: u32 = 10;
/// Owner-side lookup failures per minute before lookups are locked out.
pub const LOOKUP_FAILURES_PER_MINUTE: u32 = 5;
/// How long a tripped failure limit locks the caller out.
pub const FAILURE_LOCKOUT: Duration = Duration::from_secs(60);

/// Prefix of a browser grant bearer token.
pub const GRANT_TOKEN_PREFIX: &str = "awb_";
/// Prefix of a WebSocket upgrade ticket.
pub const TICKET_PREFIX: &str = "awt_";
/// Prefix of the user-facing link code.
pub const LINK_CODE_PREFIX: &str = "web";
/// A ticket is offered as the WebSocket subprotocol `awman.ticket.<ticket>`.
pub const TICKET_SUBPROTOCOL_PREFIX: &str = "awman.ticket.";

/// A credential carried on the wire. Serializes as the bare string, prints as
/// `<redacted>`.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BrowserSecret(String);

impl BrowserSecret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
    /// The only accessor; every caller is a place a reviewer must check.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for BrowserSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("BrowserSecret(<redacted>)")
    }
}

/// What a grant may do. v0 has exactly one scope; the daemon assigns it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrowserScope {
    Engine,
}

/// `201` body of `POST /v1/browser-links`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserLinkCreateResponse {
    pub id: BrowserLinkId,
    pub code: String,
    pub polling_secret: BrowserSecret,
    pub interval_ms: u64,
    pub expires_at: DateTime<Utc>,
    pub listener: String,
    pub origin: String,
    pub scope: BrowserScope,
}

/// Body of `POST /v1/browser-links/{id}/poll`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrowserLinkPollRequest {
    pub polling_secret: BrowserSecret,
}

/// The grant handed to the poller exactly once, on the first poll after
/// approval.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserGrantIssued {
    pub id: BrowserGrantId,
    pub token: BrowserSecret,
    pub scope: BrowserScope,
    pub origin: String,
    pub listener: String,
    pub expires_at: DateTime<Utc>,
}

/// `200` body of a poll.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum BrowserLinkPollResponse {
    Pending { retry_after_ms: u64 },
    Approved { grant: BrowserGrantIssued },
    Denied,
}

/// The state of a link as the owner sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrowserLinkStatus {
    Pending,
    Approved,
    Denied,
}

/// `GET /v1/browser-links?code=…` (owner socket only).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserLinkView {
    pub id: BrowserLinkId,
    pub code: String,
    pub origin: String,
    pub listener: String,
    pub scope: BrowserScope,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub status: BrowserLinkStatus,
}

/// Body of `POST /v1/browser-links/{id}/decision` (owner socket only).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrowserLinkDecisionRequest {
    pub approve: bool,
}

/// Answer to a decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserLinkDecisionResponse {
    pub status: BrowserLinkStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grant_id: Option<BrowserGrantId>,
}

/// One grant in `GET /v1/browsers`: never the hash, never the token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserGrantView {
    pub id: BrowserGrantId,
    pub origin: String,
    pub listener: String,
    pub scope: BrowserScope,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// Which WebSocket a ticket opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TicketRoute {
    Frontend,
    Terminal,
}

/// Body of `POST /v1/browser-tickets`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "route", rename_all = "snake_case", deny_unknown_fields)]
pub enum BrowserTicketRequest {
    Frontend { instance_id: InstanceId },
    Terminal { terminal_id: TerminalId },
}

impl BrowserTicketRequest {
    pub fn route(&self) -> TicketRoute {
        match self {
            Self::Frontend { .. } => TicketRoute::Frontend,
            Self::Terminal { .. } => TicketRoute::Terminal,
        }
    }
    /// The resource the ticket is bound to, as its id string.
    pub fn resource(&self) -> &str {
        match self {
            Self::Frontend { instance_id } => instance_id.as_str(),
            Self::Terminal { terminal_id } => terminal_id.as_str(),
        }
    }
}

/// `201` body of `POST /v1/browser-tickets`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserTicketResponse {
    pub ticket: BrowserSecret,
    pub expires_in_ms: u64,
}

/// Why a grant stopped working; also the WebSocket close reason (code 1008).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrantRevocationReason {
    Deleted,
    Expired,
    ListenerClosed,
    OriginRemoved,
    DaemonStopping,
}

impl GrantRevocationReason {
    pub fn close_reason(self) -> &'static str {
        match self {
            Self::Deleted => "grant revoked",
            Self::Expired => "grant expired",
            Self::ListenerClosed => "listener closed",
            Self::OriginRemoved => "origin removed",
            Self::DaemonStopping => "daemon stopping",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_serialize_bare_and_never_debug_print() {
        let secret = BrowserSecret::new("awb_secretvalue");
        assert_eq!(
            serde_json::to_string(&secret).unwrap(),
            "\"awb_secretvalue\""
        );
        let response = BrowserTicketResponse {
            ticket: BrowserSecret::new("awt_ticketvalue"),
            expires_in_ms: 30_000,
        };
        let debug = format!("{response:?}");
        assert!(!debug.contains("ticketvalue"), "{debug}");
        let poll = BrowserLinkPollRequest {
            polling_secret: BrowserSecret::new("pollsecret"),
        };
        assert!(!format!("{poll:?}").contains("pollsecret"));
    }

    #[test]
    fn poll_responses_use_the_status_tag() {
        assert_eq!(
            serde_json::to_value(BrowserLinkPollResponse::Pending {
                retry_after_ms: 2000
            })
            .unwrap(),
            serde_json::json!({"status":"pending","retry_after_ms":2000})
        );
        assert_eq!(
            serde_json::to_value(BrowserLinkPollResponse::Denied).unwrap(),
            serde_json::json!({"status":"denied"})
        );
        let grant = BrowserGrantIssued {
            id: BrowserGrantId::new(),
            token: BrowserSecret::new("awb_x"),
            scope: BrowserScope::Engine,
            origin: "http://localhost:3000".into(),
            listener: "loopback".into(),
            expires_at: Utc::now(),
        };
        let value = serde_json::to_value(BrowserLinkPollResponse::Approved { grant }).unwrap();
        assert_eq!(value["status"], "approved");
        assert_eq!(value["grant"]["token"], "awb_x");
        assert_eq!(value["grant"]["scope"], "engine");
    }

    #[test]
    fn ticket_requests_are_route_tagged_and_strict() {
        let instance = InstanceId::new();
        let parsed: BrowserTicketRequest = serde_json::from_value(
            serde_json::json!({"route":"frontend","instance_id":instance.as_str()}),
        )
        .unwrap();
        assert_eq!(parsed.route(), TicketRoute::Frontend);
        assert_eq!(parsed.resource(), instance.as_str());
        assert!(serde_json::from_value::<BrowserTicketRequest>(
            serde_json::json!({"route":"frontend","instance_id":instance.as_str(),"x":1})
        )
        .is_err());
        assert!(serde_json::from_value::<BrowserTicketRequest>(
            serde_json::json!({"route":"terminal","terminal_id":"term_nope"})
        )
        .is_err());
    }

    #[test]
    fn poll_and_decision_bodies_refuse_extra_fields() {
        assert!(serde_json::from_str::<BrowserLinkPollRequest>(
            r#"{"polling_secret":"a","origin":"x"}"#
        )
        .is_err());
        assert!(
            serde_json::from_str::<BrowserLinkDecisionRequest>(r#"{"approve":true,"x":1}"#)
                .is_err()
        );
    }
}
