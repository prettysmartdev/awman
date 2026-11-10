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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::worker::LeaseRelease;

    #[test]
    fn queue_row_preserves_wsp_fields_and_0139_shape() {
        let at = DateTime::parse_from_rfc3339("2026-11-09T12:00:00.000Z")
            .unwrap()
            .with_timezone(&Utc);
        let item = awman_wsp::QueueItem {
            body: Some("# Full task\nDo the work".into()),
            source: Some(awman_wsp::Source {
                url: "https://example.test/issues/1".into(),
            }),
            id: "item".into(),
            title: "A task".into(),
            summary: Some("summary".into()),
            kind: awman_wsp::WorkKind::Prompt,
            labels: BTreeMap::from([("project".into(), "example".into())]),
            priority: 5,
            enqueued_at: at,
            assigned_to_me: true,
        };
        let row = QueueRow::from_wsp(
            "team",
            item.clone(),
            at + chrono::Duration::milliseconds(500),
        );
        let json = serde_json::to_value(&row).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "id":"item", "server":"team", "title":"A task", "summary":"summary",
                "kind":"prompt", "labels":{"project":"example"}, "priority":5,
                "enqueued_at":"2026-11-09T12:00:00.000Z", "age_ms":500, "for_you":true,
                "body":"# Full task\nDo the work", "source_url":"https://example.test/issues/1",
            })
        );
        assert_eq!(serde_json::from_value::<QueueRow>(json).unwrap(), row);
        let mut row = QueueRow::from_wsp("team", item, at - chrono::Duration::seconds(1));
        assert_eq!(row.age_ms, 0);
        row.summary = None;
        assert!(serde_json::to_value(row).unwrap().get("summary").is_none());
    }

    #[test]
    fn pull_status_and_server_summary_keep_the_status_shape() {
        let status = PullStatus {
            servers: vec![ServerSummary {
                name: "team".into(),
                state: "registered".into(),
                running: 1,
                queued_eligible: 2,
            }],
        };
        let value = serde_json::to_value(&status).unwrap();
        assert_eq!(
            value,
            serde_json::json!({"servers":[{
                "name":"team", "state":"registered", "running":1, "queued_eligible":2,
            }]})
        );
        assert_eq!(serde_json::from_value::<PullStatus>(value).unwrap(), status);
        assert_eq!(
            serde_json::to_value(PullStatus::default()).unwrap(),
            serde_json::json!({"servers":[]})
        );
    }

    #[test]
    fn release_body_defaults_and_rejects_unknown_fields() {
        let empty: ReleaseBody = serde_json::from_str("{}").unwrap();
        assert_eq!(empty, ReleaseBody::default());
        assert_eq!(
            serde_json::to_value(empty).unwrap(),
            serde_json::json!({"reason":null,"message":null})
        );
        let value = serde_json::json!({"reason":"user_declined","message":"later"});
        let release: ReleaseBody = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(serde_json::to_value(release).unwrap(), value);
        assert!(
            serde_json::from_value::<ReleaseBody>(serde_json::json!({"reason":"unknown"})).is_err()
        );
        assert!(serde_json::from_value::<ReleaseBody>(serde_json::json!({"extra":true})).is_err());
    }

    #[test]
    fn wsp_result_embeds_the_real_command_result_unchanged() {
        use crate::data::fs::api_db::CommandResult;
        let command: CommandResult = serde_json::from_value(serde_json::json!({
            "exit_code":1, "outcome":"error",
            "failure":{"stage":"step","code":"step_failed","message":"failed","step":"test"},
            "failed_steps":[{"name":"test","index":0,"phase":"main","exit_code":1,"step_attempt":2}],
            "git":{"branch":"wsp/item","head_sha":"abc","dirty":false,"changed_files":[]},
            "pushed":{"remote":"origin","branch":"wsp/item","sha":"abc"},
            "pull_request":{"url":"https://example.invalid/pr/1"},
            "final_message":"complete",
            "outputs":{"files":1,"bytes":2}
        })).unwrap();
        let expected = serde_json::to_value(&command).unwrap();
        let result = awman_wsp::LeaseResult {
            result: command.clone(),
            outputs: Some(BTreeMap::new()),
            outputs_mounted: true,
            duration_ms: 500,
        };
        let json = serde_json::to_value(&result).unwrap();
        assert_eq!(json["result"], expected);
        let back: awman_wsp::LeaseResult<CommandResult> = serde_json::from_value(json).unwrap();
        assert_eq!(back.result, command);
        // Preserve 0137's distinction between an absent push and explicit null.
        let null_push: CommandResult =
            serde_json::from_value(serde_json::json!({"exit_code":0,"pushed":null})).unwrap();
        let json = serde_json::to_value(awman_wsp::LeaseResult {
            result: null_push,
            outputs: None,
            outputs_mounted: false,
            duration_ms: 0,
        })
        .unwrap();
        assert_eq!(json["result"].get("pushed"), Some(&serde_json::Value::Null));
    }

    #[test]
    fn lease_resource_uses_lease_ids_and_wire_conventions() {
        let wire = awman_wsp::Lease {
            id: "srv-7".into(),
            epoch: 2,
            expires_at: Utc::now(),
            item: awman_wsp::conformance::work_item("01J"),
            terminal: true,
            remote_input: false,
        };
        let mut record = LeaseRecord::claimed("team", &wire, LeaseHost::Daemon, Utc::now());
        record.release = Some(LeaseRelease {
            reason: awman_wsp::ReleaseReason::Ineligible,
            message: Some("runtime unavailable".into()),
        });
        let value = serde_json::to_value(LeaseResource::from(&record)).unwrap();
        assert!(value["id"].as_str().unwrap().starts_with("lease_"));
        assert_eq!(value["server_lease_id"], "srv-7");
        assert_eq!(value["state"], "claimed");
        assert_eq!(value["host"]["kind"], "daemon");
        assert_eq!(value["release_reason"], "ineligible");
        assert_eq!(value["release_message"], "runtime unavailable");
        assert!(value["claimed_at"].as_str().unwrap().ends_with('Z'));
        assert!(value["ended_at"].is_null());
        let back: LeaseResource = serde_json::from_value(value).unwrap();
        assert_eq!(back.server_lease_id, "srv-7");
    }
}
