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
    /// `integrations/<name>.rs`; `enabled` is shared by every integration.
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

/// Whether a named TUI integration participates in a session.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntegrationSwitch {
    /// Let the integration decide based on host detection.
    #[default]
    Auto,
    /// Always enable the integration when it is available.
    On,
    /// Do not detect or use the integration.
    Off,
}

/// The shared part of a `tui.integrations.<name>` block. Integration-private
/// options remain in the opaque JSON map until their integration owns them.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TuiIntegrationConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<IntegrationSwitch>,
}

impl TuiConfig {
    /// Resolve the common enabled setting for one integration. Missing blocks
    /// and missing settings both mean automatic detection.
    pub fn integration_switch(&self, name: &str) -> IntegrationSwitch {
        self.integrations
            .as_ref()
            .and_then(|integrations| integrations.get(name))
            .and_then(|value| serde_json::from_value::<TuiIntegrationConfig>(value.clone()).ok())
            .and_then(|config| config.enabled)
            .unwrap_or_default()
    }
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
