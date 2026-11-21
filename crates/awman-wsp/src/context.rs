//! Optional context snapshots and workflow artifacts for WSP v1.
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
/// `WorkItem.context`. Strict: the worker must act on it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ItemContext {
    #[serde(default)]
    pub files: Vec<ItemContextFile>,
    /// Cap on decoded bytes, both for this snapshot and for returned content.
    #[serde(default = "ItemContext::default_max_bytes")]
    pub max_bytes: u64,
}
impl ItemContext {
    pub const fn default_max_bytes() -> u64 {
        1024 * 1024
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ItemContextFile {
    pub path: String,
    pub content_base64: String,
} // standard base64, padded

/// `WorkItem.workflow_context`. Strict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ItemWorkflowContext {
    #[serde(default = "ItemWorkflowContext::default_max_bytes")]
    pub max_bytes: u64,
}
impl ItemWorkflowContext {
    pub const fn default_max_bytes() -> u64 {
        1024 * 1024
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextChangeKind {
    Added,
    Modified,
    Deleted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextChangeItem {
    pub path: String,
    pub change: ContextChangeKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_base64: Option<String>,
}
/// Result `context_changes`: `{items, next_cursor: null}`. `next_cursor` is always null.
pub type ContextChanges = crate::wire::List<ContextChangeItem>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowContextItem {
    pub path: String,
    pub size_bytes: u64,
    pub sha256: String,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::wire::opt_timestamp"
    )]
    pub modified_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_base64: Option<String>,
}
/// A fixed code; collection diagnostics never carry host paths on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowContextCollectionError {
    Unavailable,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowContextResult {
    #[serde(default)]
    pub items: Vec<WorkflowContextItem>,
    #[serde(default)]
    pub next_cursor: Option<String>, // always None; serialised as null
    #[serde(default)]
    pub truncated: bool,
    /// Enumeration failed, so metadata may be absent as well as content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collection_error: Option<WorkflowContextCollectionError>,
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn optional_requests_and_results_round_trip_with_old_messages() {
        let mut value = serde_json::to_value(crate::conformance::work_item("memory")).unwrap();
        let old: crate::WorkItem = serde_json::from_value(value.clone()).unwrap();
        assert!(old.context.is_none() && old.workflow_context.is_none());
        value["context"] =
            serde_json::json!({"files":[{"path":"note.md","content_base64":"YQ=="}]});
        value["workflow_context"] = serde_json::json!({});
        let item: crate::WorkItem = serde_json::from_value(value).unwrap();
        assert_eq!(item.context.as_ref().unwrap().max_bytes, 1 << 20);
        assert_eq!(item.workflow_context.as_ref().unwrap().max_bytes, 1 << 20);
        assert_eq!(
            serde_json::from_value::<crate::WorkItem>(serde_json::to_value(&item).unwrap())
                .unwrap(),
            item
        );
        assert!(
            serde_json::from_value::<ItemContext>(serde_json::json!({"unknown":true})).is_err()
        );
        assert!(
            serde_json::from_value::<ItemWorkflowContext>(serde_json::json!({"unknown":true}))
                .is_err()
        );
        let old: crate::LeaseResult<serde_json::Value> = serde_json::from_value(
            serde_json::json!({"result":{},"outputs_mounted":false,"duration_ms":0}),
        )
        .unwrap();
        assert!(old.context_changes.is_none() && old.workflow_context.is_none());
        let release: crate::ReleaseRequest<serde_json::Value> =
            serde_json::from_value(serde_json::json!({"reason":"ineligible"})).unwrap();
        assert!(release.context_changes.is_none());
        let legacy: WorkflowContextResult = serde_json::from_value(
            serde_json::json!({"items":[],"next_cursor":null,"truncated":true}),
        )
        .unwrap();
        assert!(legacy.collection_error.is_none());
        assert!(serde_json::to_value(&legacy)
            .unwrap()
            .get("collection_error")
            .is_none());
        let failed = WorkflowContextResult {
            collection_error: Some(WorkflowContextCollectionError::Unavailable),
            ..legacy
        };
        let json = serde_json::to_value(&failed).unwrap();
        assert_eq!(json["collection_error"], "unavailable");
        assert_eq!(
            serde_json::from_value::<WorkflowContextResult>(json).unwrap(),
            failed
        );
        let changes = ContextChanges {
            items: vec![ContextChangeItem {
                path: "a".into(),
                change: ContextChangeKind::Deleted,
                size_bytes: None,
                sha256: None,
                content_base64: None,
            }],
            next_cursor: None,
        };
        let json = serde_json::to_value(&changes).unwrap();
        assert_eq!(json["next_cursor"], serde_json::Value::Null);
        assert!(json["items"][0].get("content_base64").is_none());
        assert_eq!(
            serde_json::from_value::<ContextChanges>(json).unwrap(),
            changes
        );
        assert_eq!(
            crate::ApiError {
                code: crate::codes::CONTEXT_TOO_LARGE.into(),
                error: "too large".into()
            }
            .status(),
            413
        );
    }
}
