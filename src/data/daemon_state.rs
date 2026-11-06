//! Shared daemon feature identities and lifecycle state.
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeatureName {
    Squad,
    Worker,
    Engine,
}
impl FeatureName {
    pub const ALL: [Self; 3] = [Self::Squad, Self::Worker, Self::Engine];
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Squad => "squad",
            Self::Worker => "worker",
            Self::Engine => "engine",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|f| f.as_str() == s)
    }
}
impl std::fmt::Display for FeatureName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeatureState {
    Off,
    On,
    Ephemeral,
    Draining,
    Degraded,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnabledBy {
    Config,
    Ephemeral,
}
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FeatureStatus {
    pub running: usize,
    pub queued: usize,
    pub oldest_queued_age: Option<Duration>,
    pub detail: Option<serde_json::Value>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SupervisedState {
    pub state: FeatureState,
    pub enabled_by: Option<EnabledBy>,
    pub reason: Option<String>,
    pub restarts: u32,
    pub last_error: Option<String>,
    pub since: chrono::DateTime<chrono::Utc>,
}
impl Default for SupervisedState {
    fn default() -> Self {
        Self {
            state: FeatureState::Off,
            enabled_by: None,
            reason: None,
            restarts: 0,
            last_error: None,
            since: chrono::Utc::now(),
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopMode {
    Drain,
    Now,
}
impl PartialEq<str> for FeatureName {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}
/// Source listener recorded with a pushed command, independent of HTTP types.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandListener {
    #[default]
    Local,
    Network,
}
impl CommandListener {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Network => "network",
        }
    }
}
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommandRequestContext {
    pub listener: CommandListener,
    pub trace_parent: Option<String>,
}
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CommandQueueStatus {
    pub command_id: String,
    pub listener: String,
    pub trace_parent: Option<String>,
}
impl CommandRequestContext {
    /// Version 00 W3C trace context. Invalid and all-zero identifiers are ignored.
    pub fn from_header(listener: CommandListener, header: Option<&str>) -> Self {
        let valid = |value: &&str| {
            let parts: Vec<_> = value.split('-').collect();
            parts.len() == 4
                && parts[0] == "00"
                && parts[1].len() == 32
                && parts[2].len() == 16
                && parts[3].len() == 2
                && parts.iter().all(|p| {
                    p.bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                })
                && parts[1].bytes().any(|b| b != b'0')
                && parts[2].bytes().any(|b| b != b'0')
        };
        Self {
            listener,
            trace_parent: header.filter(valid).map(str::to_string),
        }
    }
}
