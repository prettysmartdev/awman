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

// WI 0160: reload-safe reauthorization (proof of a non-extractable browser
// key bound to one approved grant). See the work item's C1–C3.

/// The only reauthorization protocol version.
pub const REAUTH_VERSION: u32 = 1;
/// The only enrollment algorithm: ECDSA P-256 over SHA-256, raw `r || s`.
pub const REAUTH_ALGORITHM: &str = "ECDSA_P256_SHA256";
/// Domain tag that starts every signed reauthorization transcript (followed
/// by one zero byte).
pub const REAUTH_TRANSCRIPT_DOMAIN: &[u8] = b"awman.browser.reauth.v1";
/// The completion route, which is also signed into the transcript.
pub const REAUTH_COMPLETE_PATH: &str = "/v1/browser-reauth/complete";
/// A challenge ends this long after issue, by monotonic or wall time,
/// whichever passes first.
pub const REAUTH_CHALLENGE_TTL: Duration = Duration::from_secs(30);
/// Outstanding challenges per presented credential ID; the oldest is dropped
/// to admit a new one.
pub const REAUTH_CHALLENGES_PER_CREDENTIAL: usize = 8;
/// Outstanding challenges per listener; beyond this, `429`.
pub const REAUTH_CHALLENGES_PER_LISTENER: usize = 256;
/// `Retry-After` when a listener's challenge table is full.
pub const REAUTH_CHALLENGES_FULL_RETRY: Duration = Duration::from_secs(30);
/// Challenge requests per (listener, Origin) per minute.
pub const REAUTH_CHALLENGES_PER_MINUTE: u32 = 60;
/// Completion requests per (listener, Origin) per minute.
pub const REAUTH_COMPLETES_PER_MINUTE: u32 = 120;
/// Enrollment/binding/tombstone slots per listener.
pub const REAUTH_SLOTS_PER_LISTENER: usize = 4096;
/// `Retry-After` when a listener's enrollment slots are full.
pub const REAUTH_SLOTS_FULL_RETRY: Duration = Duration::from_secs(60);
/// A tombstone (public key and termination reason, never a bearer) is kept
/// until the original grant expiry plus this, or daemon stop.
pub const REAUTH_TOMBSTONE_RETENTION: Duration = Duration::from_secs(24 * 60 * 60);
/// Live grant watches re-evaluate both deadlines at least this often, so a
/// system suspend cannot keep a socket open past its grant.
pub const GRANT_WATCH_RECHECK: Duration = Duration::from_secs(30);

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
    /// WI 0160: present only for a key-bound link.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reauth: Option<BrowserReauthBinding>,
}

/// WI 0160: the key-bound body of `POST /v1/browser-links`
/// (`{"reauth":{…}}`). The legacy absent/`{}` body is recognised by the
/// adapter before this type is consulted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrowserLinkCreateRequest {
    pub reauth: BrowserReauthEnrollment,
}

/// WI 0160: the public key a browser enrolls with a new link.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrowserReauthEnrollment {
    pub version: u32,
    pub credential_id: String,
    pub algorithm: String,
    pub public_key: String,
}

/// WI 0160: the public binding of a key-bound link or grant. Public
/// identifiers only, never a credential.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrowserReauthBinding {
    pub version: u32,
    pub credential_id: String,
    pub daemon_epoch: String,
    pub listener_epoch: String,
}

/// WI 0160: what the owner sees of a key-bound link or grant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserReauthKeyView {
    pub version: u32,
    pub credential_id: String,
    /// Unpadded base64url SHA-256 of the 65 raw public-key bytes.
    pub key_sha256: String,
}

/// WI 0160: body of `POST /v1/browser-reauth/challenge`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrowserReauthChallengeRequest {
    pub version: u32,
    pub credential_id: String,
    pub daemon_epoch: String,
    pub listener_epoch: String,
    pub client_nonce: String,
}

/// WI 0160: `201` body of `POST /v1/browser-reauth/challenge`. The same
/// shape for known, unknown, expired and revoked credentials.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrowserReauthChallengeResponse {
    pub version: u32,
    pub credential_id: String,
    pub daemon_epoch: String,
    pub listener_epoch: String,
    pub origin: String,
    pub listener: String,
    pub challenge_id: String,
    pub nonce: String,
    pub client_nonce: String,
    /// UTC RFC 3339 with exactly three fractional digits and `Z`; signed
    /// byte-for-byte, never reformatted.
    pub expires_at: String,
}

/// WI 0160: body of `POST /v1/browser-reauth/complete`. The signature is
/// proof material and never prints.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrowserReauthCompleteRequest {
    pub version: u32,
    pub challenge_id: String,
    pub signature: BrowserSecret,
}

/// WI 0160: `200` body of `POST /v1/browser-reauth/complete`: the same
/// grant, bearer and original expiry the approval minted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserReauthCompleteResponse {
    pub grant: BrowserGrantIssued,
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
    /// WI 0160: the public binding; omitted for keyless grants.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reauth: Option<BrowserReauthBinding>,
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
    /// WI 0160: the enrolled key, for a key-bound link.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reauth: Option<BrowserReauthKeyView>,
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
    /// WI 0160: present when the browser can recover this grant after reload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reauth: Option<BrowserReauthKeyView>,
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
