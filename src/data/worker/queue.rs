//! Shared manual queue presentation and foreground handoff values.
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueEntry {
    pub id: String,
    pub server: String,
    pub title: String,
    pub summary: Option<String>,
    pub kind: String,
    pub labels: BTreeMap<String, String>,
    pub priority: i32,
    pub age_ms: u64,
    pub for_you: bool,
    pub body: Option<String>,
    pub source_url: Option<String>,
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueSnapshot {
    pub items: Vec<QueueEntry>,
    pub for_you: usize,
    pub new_since_seen: usize,
    pub error: Option<String>,
    pub loaded: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PickRun {
    pub lease_id: String,
    pub title: String,
    pub server: String,
    pub cwd: PathBuf,
    pub command_line: String,
    /// Engine-ready argv; browsers do not parse shell syntax themselves.
    #[serde(default)]
    pub argv: Vec<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "code", content = "error", rename_all = "snake_case")]
pub enum PickError {
    Ineligible(String),
    Taken,
    Failed(String),
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LeaseCloseChoice {
    Release,
    Cancel,
    KeepRunning,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabLease {
    pub lease_id: String,
    pub server: String,
    pub title: String,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckoutChoice {
    Worktree,
    FreshClone,
}

/// Instance-local operations. The engine retains the source and every lease;
/// a presenter can only name the instance's existing work.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum QueueRequest {
    Snapshot,
    Refresh,
    Seen,
    Pick {
        server: String,
        item_id: String,
    },
    CheckoutOffer {
        lease_id: String,
    },
    Checkout {
        lease_id: String,
        choice: CheckoutChoice,
    },
    Finish {
        lease_id: String,
        choice: LeaseCloseChoice,
    },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", content = "value", rename_all = "snake_case")]
pub enum QueueReply {
    Snapshot(QueueSnapshot),
    Picked(PickRun),
    CheckoutOffer(bool),
    Done,
    Error(PickError),
}
