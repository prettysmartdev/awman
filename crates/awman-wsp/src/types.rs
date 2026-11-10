//! JSON messages shared by work servers and workers.
use crate::Affinity;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub use crate::wire::List;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiError {
    pub error: String,
    pub code: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkStart {
    pub user_code: String,
    pub verification_uri: String,
    pub link_id: String,
    pub interval_ms: u64,
}

/// A pending 202 response may increase the polling interval.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkPending {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval_ms: Option<u64>,
}
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkToken {
    pub refresh_token: String,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccessToken {
    pub access_token: String,
    #[serde(default)]
    #[serde(with = "crate::wire::opt_timestamp")]
    pub expires_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisterRequest {
    pub name: String,
    pub labels: BTreeMap<String, String>,
    pub capacity: usize,
    pub features: Vec<String>,
}

impl RegisterRequest {
    /// The `awman/` namespace is reported by awman, never an owner config.
    pub fn with_labels(
        name: String,
        reported: BTreeMap<String, String>,
        configured: BTreeMap<String, String>,
        capacity: usize,
        features: Vec<String>,
    ) -> Result<Self, &'static str> {
        let labels = crate::labels::WorkerLabels::new(reported, configured)
            .map_err(|_| "invalid worker labels")?
            .merged();
        Ok(Self {
            name,
            labels,
            capacity,
            features,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisterResponse {
    pub worker_id: String,
    #[serde(default)]
    pub server_labels: BTreeMap<String, String>,
    #[serde(default = "default_heartbeat_ms")]
    pub heartbeat_ms: u64,
    #[serde(default = "default_poll_ms")]
    pub poll_ms: u64,
}

const fn default_heartbeat_ms() -> u64 {
    15_000
}
const fn default_poll_ms() -> u64 {
    1_000
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueItem {
    /// Optional Markdown preview; contains no repository credentials.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<Source>,
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub summary: Option<String>,
    pub kind: WorkKind,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    #[serde(default)]
    pub priority: i32,
    #[serde(with = "crate::wire::timestamp")]
    pub enqueued_at: DateTime<Utc>,
    #[serde(default)]
    pub assigned_to_me: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkKind {
    Workflow,
    Prompt,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkItem {
    #[serde(default)]
    pub summary: Option<String>,
    pub id: String,
    pub title: String,
    pub kind: WorkKind,
    pub body: String,
    #[serde(default)]
    pub source: Option<Source>,
    pub repo: Repo,
    pub exec: Exec,
    #[serde(default)]
    pub push: Option<Push>,
    #[serde(default)]
    pub outputs: Option<Outputs>,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    #[serde(default)]
    pub priority: i32,
    #[serde(default)]
    pub affinity: Affinity,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Source {
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Repo {
    pub url: String,
    pub base_ref: String,
    pub branch: String,
    #[serde(default)]
    pub merge_refs: Vec<String>,
    #[serde(default)]
    pub credentials: Option<Credentials>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialKind {
    Bearer,
    Basic,
    None,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Credentials {
    pub kind: CredentialKind,
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    #[serde(with = "crate::wire::opt_timestamp")]
    pub expires_at: Option<DateTime<Utc>>,
}

impl std::fmt::Debug for LinkToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LinkToken")
            .field("refresh_token", &"[redacted]")
            .finish()
    }
}
impl std::fmt::Debug for AccessToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccessToken")
            .field("access_token", &"[redacted]")
            .field("expires_at", &self.expires_at)
            .finish()
    }
}
impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("kind", &self.kind)
            .field("token", &"[redacted]")
            .field("username", &"[redacted]")
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Exec {
    #[serde(default)]
    pub dynamic: bool,
    #[serde(default)]
    pub leader: Option<String>,
    #[serde(default)]
    pub workflow_toml: Option<String>,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub read_only: bool,
    #[serde(default)]
    pub overlays: Vec<String>,
    #[serde(default)]
    pub interaction: Option<Interaction>,
    #[serde(default)]
    pub yolo: bool,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Interaction {
    Interactive,
    Headless,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Push {
    pub enabled: bool,
    pub branch: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Outputs {
    pub max_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Lease {
    pub id: String,
    pub epoch: u64,
    #[serde(with = "crate::wire::timestamp")]
    pub expires_at: DateTime<Utc>,
    pub item: WorkItem,
    #[serde(default)]
    pub terminal: bool,
    #[serde(default)]
    pub remote_input: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimResponse {
    pub lease: Lease,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HeartbeatState {
    Preparing,
    Running,
    AwaitingInput,
    Pushing,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HeartbeatRequest {
    pub state: HeartbeatState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub step: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeartbeatResponse {
    #[serde(with = "crate::wire::timestamp")]
    pub expires_at: DateTime<Utc>,
    pub cancel: bool,
    #[serde(default)]
    pub cancel_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventBatch<E> {
    pub events: Vec<E>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LeaseResult<R> {
    pub result: R,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outputs: Option<BTreeMap<String, String>>,
    pub outputs_mounted: bool,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReleaseReason {
    UserDeclined,
    Ineligible,
    Shutdown,
    Error,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReleaseRequest<R> {
    pub reason: ReleaseReason,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<R>,
}

pub const WSP_VERSION: &str = "1";
pub const VERSION_HEADER: &str = "x-wsp-version";
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenRequest {
    pub refresh_token: String,
}
impl std::fmt::Debug for TokenRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenRequest")
            .field("refresh_token", &"[redacted]")
            .finish()
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventEnvelope {
    pub attempt: u32,
    pub sequence: u64,
    #[serde(flatten)]
    pub rest: serde_json::Value,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventsResponse {
    pub accepted: usize,
}
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::large_enum_variant)]
pub enum ClaimOutcome {
    Leased(Lease),
    Empty,
}
impl ApiError {
    pub fn status(&self) -> u16 {
        match self.code.as_str() {
            "taken" | "lease_lost" | "ineligible" => 409,
            "unauthorized" => 401,
            "not_found" | "worker_not_found" => 404,
            "expired" => 410,
            _ => 400,
        }
    }
}
pub mod codes {
    pub const TAKEN: &str = "taken";
    pub const LEASE_LOST: &str = "lease_lost";
    pub const INELIGIBLE: &str = "ineligible";
    pub const NOT_FOUND: &str = "not_found";
    pub const UNAUTHORIZED: &str = "unauthorized";
    pub const INVALID_EVENT: &str = "invalid_event";
    pub const LINK_PENDING: &str = "authorization_pending";
    pub const LINK_DENIED: &str = "access_denied";
}
pub mod features {
    pub const TERMINAL: &str = "terminal";
    pub const INTERACTIVE: &str = "interactive";
    pub const PROMPT: &str = "prompt";
    pub const WORKFLOW_DYNAMIC: &str = "workflow_dynamic";
    pub const WORKFLOW_FILE: &str = "workflow_file";
    pub const OUTPUTS: &str = "outputs";
    pub const OUTPUTS_MOUNT_STATE: &str = "outputs_mount_state";
    pub const WORKFLOW_STATE_EVENTS: &str = "workflow_state_events";
    pub const LEASE_CREDENTIALS: &str = "lease_credentials";
    pub const LEASE_INLINE_WORK_ITEM: &str = "lease_inline_work_item";
    pub const LEASE_READ_ONLY: &str = "lease_read_only";
    pub const LEASE_GIT_AUTH_FAILURE: &str = "lease_git_auth_failure";
    pub const LEASE_PUSH_RETRY: &str = "lease_push_retry";
    pub const ALL: &[&str] = &[
        TERMINAL,
        INTERACTIVE,
        PROMPT,
        WORKFLOW_DYNAMIC,
        WORKFLOW_FILE,
        OUTPUTS,
        OUTPUTS_MOUNT_STATE,
        WORKFLOW_STATE_EVENTS,
        LEASE_CREDENTIALS,
        LEASE_INLINE_WORK_ITEM,
        LEASE_READ_ONLY,
        LEASE_GIT_AUTH_FAILURE,
        LEASE_PUSH_RETRY,
    ];
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_shapes_use_wsp_conventions() {
        let request = RegisterRequest::with_labels(
            "box".into(),
            BTreeMap::from([("awman/os".into(), "linux".into())]),
            BTreeMap::from([("pool".into(), "build".into())]),
            2,
            vec!["workflow_dynamic".into()],
        )
        .unwrap();
        assert_eq!(
            serde_json::to_value(request).unwrap()["labels"]["awman/os"],
            "linux"
        );
        assert!(RegisterRequest::with_labels(
            "x".into(),
            BTreeMap::new(),
            BTreeMap::from([("awman/os".into(), "fake".into())]),
            1,
            vec![]
        )
        .is_err());
        let list = List::<QueueItem> {
            items: vec![],
            next_cursor: None,
        };
        assert_eq!(
            serde_json::to_value(list).unwrap(),
            serde_json::json!({"items":[],"next_cursor":null})
        );
        let err = ApiError {
            error: "taken".into(),
            code: "taken".into(),
        };
        assert_eq!(
            serde_json::to_value(err).unwrap(),
            serde_json::json!({"error":"taken","code":"taken"})
        );
        let response: RegisterResponse =
            serde_json::from_value(serde_json::json!({"worker_id":"w","server_labels":{}}))
                .unwrap();
        assert_eq!(response.heartbeat_ms, 15_000);
    }
}
