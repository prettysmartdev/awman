//! `tui` section: interactive frontend settings.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The `tui` block.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TuiConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scrollback_lines: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<TuiBackend>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub program_status: Option<TuiProgramStatusConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker: Option<TuiWorkerConfig>,
    /// Per-integration blocks, opaque until each integration adds its
    /// `integrations/<name>.rs`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub integrations: Option<BTreeMap<String, serde_json::Value>>,
}

/// Where the TUI runs commands (`tui.backend`).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TuiBackend {
    #[default]
    Auto,
    Daemon,
    Builtin,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TuiProgramStatusConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub emit: Option<bool>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TuiWorkerConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
}
