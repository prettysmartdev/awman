//! Launch context metadata. Environment values have no persistent/wire record.
use super::ids::LaunchContextId;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LaunchContextRecord {
    pub id: LaunchContextId,
    pub label: String,
    pub root: PathBuf,
    pub created_at: DateTime<Utc>,
    pub refreshed_at: DateTime<Utc>,
    pub is_default: bool,
}
#[derive(Debug, Clone, Serialize)]
pub struct LaunchContextSummary {
    #[serde(flatten)]
    pub record: LaunchContextRecord,
    pub env_state: &'static str,
}
#[derive(Debug, Clone, Serialize)]
pub struct LaunchContextList {
    pub items: Vec<LaunchContextSummary>,
    pub default_id: Option<LaunchContextId>,
    pub next_cursor: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateLaunchContext {
    pub root: PathBuf,
    pub label: Option<String>,
    #[serde(default)]
    pub default: bool,
    #[serde(default)]
    pub env: crate::data::frontend_protocol::HelloEnv,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RefreshLaunchContext {
    pub root: PathBuf,
    pub env: crate::data::frontend_protocol::HelloEnv,
}
