//! Wire projection of awman's lease resources (`/v1/worker/leases`),
//! following 0139's conventions: `snake_case`, `_at` RFC 3339 timestamps.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::data::worker::{LeaseHost, LeaseRecord, LeaseState};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LeaseResource {
    /// awman's ID (`lease_…`).
    pub id: String,
    pub server: String,
    /// The work server's own lease ID.
    pub server_lease_id: String,
    pub epoch: u64,
    pub item_id: String,
    pub item_title: String,
    pub job_id: Option<String>,
    pub state: LeaseState,
    pub host: LeaseHost,
    #[serde(with = "crate::data::wire::timestamp")]
    pub claimed_at: DateTime<Utc>,
    #[serde(with = "crate::data::wire::timestamp")]
    pub expires_at: DateTime<Utc>,
    #[serde(with = "crate::data::wire::opt_timestamp", default)]
    pub ended_at: Option<DateTime<Utc>>,
    pub terminal: bool,
    pub remote_input: bool,
    pub release_reason: Option<awman_wsp::ReleaseReason>,
    #[serde(default)]
    pub release_message: Option<String>,
}

impl From<&LeaseRecord> for LeaseResource {
    fn from(lease: &LeaseRecord) -> Self {
        Self {
            id: lease.id.to_string(),
            server: lease.server.clone(),
            server_lease_id: lease.server_lease_id.clone(),
            epoch: lease.epoch,
            item_id: lease.item_id.clone(),
            item_title: lease.item.title.clone(),
            job_id: lease.job_id.clone(),
            state: lease.state,
            host: lease.host.clone(),
            claimed_at: lease.claimed_at,
            expires_at: lease.expires_at,
            ended_at: lease.ended_at,
            terminal: lease.terminal,
            remote_input: lease.remote_input,
            release_reason: lease.release.as_ref().map(|r| r.reason),
            release_message: lease.release.as_ref().and_then(|r| r.message.clone()),
        }
    }
}

/// `worker.pull.servers[]` in `/v1/status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerSummary {
    pub name: String,
    pub state: String,
    pub running: usize,
    pub queued_eligible: usize,
}
/// `worker.pull` in `/v1/status`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullStatus {
    pub servers: Vec<ServerSummary>,
}

/// One eligible queue item (`GET /v1/worker/queue`). WSP queue items carry
/// no body; it arrives with the lease.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueRow {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_url: Option<String>,
    pub id: String,
    pub server: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    pub kind: awman_wsp::WorkKind,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    #[serde(default)]
    pub priority: i32,
    #[serde(with = "crate::data::wire::timestamp")]
    pub enqueued_at: chrono::DateTime<chrono::Utc>,
    pub age_ms: u64,
    /// The server assigned it to this worker (or its owner).
    #[serde(default)]
    pub for_you: bool,
}
impl QueueRow {
    pub fn from_wsp(
        server: &str,
        item: awman_wsp::QueueItem,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Self {
        Self {
            body: item.body,
            source_url: item.source.map(|source| source.url),
            age_ms: (now - item.enqueued_at).num_milliseconds().max(0) as u64,
            id: item.id,
            server: server.into(),
            title: item.title,
            summary: item.summary,
            kind: item.kind,
            labels: item.labels,
            priority: item.priority,
            enqueued_at: item.enqueued_at,
            for_you: item.assigned_to_me,
        }
    }
}

/// `POST /v1/worker/leases/{id}/release` body.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseBody {
    #[serde(default)]
    pub reason: Option<awman_wsp::ReleaseReason>,
    #[serde(default)]
    pub message: Option<String>,
}

/// Local frontend completion; upstream uses the shared WSP result unchanged.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ForegroundResult {
    #[serde(flatten)]
    pub completion: awman_wsp::LeaseResult<crate::data::fs::api_db::CommandResult>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_reason: Option<awman_wsp::ReleaseReason>,
}
