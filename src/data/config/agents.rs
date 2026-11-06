//! `agents` section: which agent runs, how it is launched, and how
//! credentials reach it.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::data::config::units::DurationStr;
use crate::data::error::DataError;

/// The `agents` block.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentsConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stuck_timeout: Option<DurationStr>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_mode: Option<LaunchMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acp_fallback: Option<AcpFallback>,
    /// Extra launch arguments keyed by agent name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flags: Option<BTreeMap<String, Vec<String>>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub yolo: Option<YoloConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<AgentAuthConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub program_status: Option<ProgramStatusConfig>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct YoloConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disallowed_tools: Option<Vec<String>>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentAuthConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<AgentAuthMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh: Option<AuthRefreshConfig>,
}

/// Live credential-refresh settings (`agents.auth.refresh`).
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AuthRefreshConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub threshold: Option<DurationStr>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tick: Option<DurationStr>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProgramStatusConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
}

/// Credential injection mode for agent containers.
///
/// `keychain` (default) injects whatever the host keychain resolves, subject
/// to injection-time deduplication. `passthrough` skips keychain injection;
/// the harness supplies credentials through `env(VAR)` overlays. `none`
/// disables keychain injection; declared `env()` overlays still apply.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentAuthMode {
    #[default]
    Keychain,
    Passthrough,
    None,
}

/// How an agent process is connected to awman.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaunchMode {
    #[default]
    Stdio,
    Acp,
}

/// What happens when ACP is requested for an agent that does not support it.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcpFallback {
    Stdio,
    #[default]
    Error,
}

impl AgentsConfig {
    /// Semantic checks the key table cannot express.
    pub fn validate(&self) -> Result<(), DataError> {
        for (agent, values) in self.flags.iter().flatten() {
            validate_agent_flag_values(&format!("agents.flags.{agent}"), values)?;
        }
        let refresh = self.auth.as_ref().and_then(|a| a.refresh.as_ref());
        for (name, value) in [
            ("threshold", refresh.and_then(|r| r.threshold.as_ref())),
            ("tick", refresh.and_then(|r| r.tick.as_ref())),
        ] {
            if value.is_some_and(|d| d.to_duration().is_zero()) {
                return Err(DataError::Other(format!(
                    "agents.auth.refresh.{name} must be greater than zero"
                )));
            }
        }
        Ok(())
    }
}

/// Reject empty or whitespace-only entries in an agent-flag list. `source`
/// names where the list came from (e.g. `agents.flags.codex`, `--agent-flag`).
pub fn validate_agent_flag_values(source: &str, values: &[String]) -> Result<(), DataError> {
    if values.iter().any(|v| v.trim().is_empty()) {
        return Err(DataError::Other(format!(
            "{source} contains an empty entry; each entry must be a non-empty argument"
        )));
    }
    Ok(())
}
