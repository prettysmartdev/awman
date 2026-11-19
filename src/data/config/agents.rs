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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub automation: Option<AutomationConfig>,
}

/// Lifecycle budgets for owned executions (`agents.automation`, WI 0070).
///
/// They may be configured without `--automation`: a profile-backed launch
/// snapshots the cancel and drain budgets for profile closure and credential
/// replacement. They never change an ordinary unprofiled launch.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AutomationConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub startup_timeout: Option<DurationStr>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancel_timeout: Option<DurationStr>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_drain_timeout: Option<DurationStr>,
}

/// The resolved `agents.automation` budgets a launch snapshots. An active
/// execution keeps its snapshot; a later config change does not reach it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AutomationTimeouts {
    /// From launching the prepared image to admission readiness (interactive)
    /// or structured request acceptance (headless). Never a turn limit.
    pub startup: DurationStr,
    /// One absolute deadline from an accepted cancel to a terminal result.
    pub cancel: DurationStr,
    /// Output/observer drain after the final process exit; never longer than
    /// `cancel`.
    pub output_drain: DurationStr,
}

impl AutomationTimeouts {
    pub const DEFAULT_STARTUP_SECS: u64 = 120;
    pub const DEFAULT_CANCEL_SECS: u64 = 10;
    pub const DEFAULT_OUTPUT_DRAIN_SECS: u64 = 5;
    /// Every budget must be within `1s..=1h`.
    pub const MIN: std::time::Duration = std::time::Duration::from_secs(1);
    pub const MAX: std::time::Duration = std::time::Duration::from_secs(60 * 60);
}

impl Default for AutomationTimeouts {
    fn default() -> Self {
        Self {
            startup: DurationStr::from_secs(Self::DEFAULT_STARTUP_SECS),
            cancel: DurationStr::from_secs(Self::DEFAULT_CANCEL_SECS),
            output_drain: DurationStr::from_secs(Self::DEFAULT_OUTPUT_DRAIN_SECS),
        }
    }
}

impl AutomationConfig {
    const KEYS: [&'static str; 3] = ["startupTimeout", "cancelTimeout", "outputDrainTimeout"];

    fn values(&self) -> [Option<&DurationStr>; 3] {
        [
            self.startup_timeout.as_ref(),
            self.cancel_timeout.as_ref(),
            self.output_drain_timeout.as_ref(),
        ]
    }

    /// Per-file checks: each present value is within `1s..=1h`, and a file
    /// that sets both the drain and cancel budgets keeps drain <= cancel.
    /// The cross-layer check runs on the merged values in
    /// [`resolve`](Self::resolve).
    pub fn validate(&self) -> Result<(), DataError> {
        for (key, value) in Self::KEYS.into_iter().zip(self.values()) {
            let Some(value) = value else { continue };
            let duration = value.to_duration();
            if duration < AutomationTimeouts::MIN || duration > AutomationTimeouts::MAX {
                return Err(DataError::Other(format!(
                    "agents.automation.{key} must be between 1s and 1h, got {}",
                    value.as_str()
                )));
            }
        }
        if let (Some(drain), Some(cancel)) = (&self.output_drain_timeout, &self.cancel_timeout) {
            Self::check_drain_within_cancel(drain, cancel)?;
        }
        Ok(())
    }

    /// The budgets a launch snapshots: merged values over the defaults
    /// (`120s`, `10s`, `5s`), with drain <= cancel enforced on the result.
    pub fn resolve(&self) -> Result<AutomationTimeouts, DataError> {
        self.validate()?;
        let defaults = AutomationTimeouts::default();
        let resolved = AutomationTimeouts {
            startup: self.startup_timeout.clone().unwrap_or(defaults.startup),
            cancel: self.cancel_timeout.clone().unwrap_or(defaults.cancel),
            output_drain: self
                .output_drain_timeout
                .clone()
                .unwrap_or(defaults.output_drain),
        };
        Self::check_drain_within_cancel(&resolved.output_drain, &resolved.cancel)?;
        Ok(resolved)
    }

    fn check_drain_within_cancel(
        drain: &DurationStr,
        cancel: &DurationStr,
    ) -> Result<(), DataError> {
        if drain.to_duration() > cancel.to_duration() {
            return Err(DataError::Other(format!(
                "agents.automation.outputDrainTimeout ({}) must not exceed \
                 agents.automation.cancelTimeout ({})",
                drain.as_str(),
                cancel.as_str()
            )));
        }
        Ok(())
    }
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
        if let Some(automation) = &self.automation {
            automation.validate()?;
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

#[cfg(test)]
mod automation_timeout_tests {
    use super::*;

    fn config(
        startup: Option<&str>,
        cancel: Option<&str>,
        drain: Option<&str>,
    ) -> AutomationConfig {
        let parse = |value: Option<&str>| value.map(|v| v.parse::<DurationStr>().unwrap());
        AutomationConfig {
            startup_timeout: parse(startup),
            cancel_timeout: parse(cancel),
            output_drain_timeout: parse(drain),
        }
    }

    #[test]
    fn defaults_are_120s_10s_5s() {
        let resolved = AutomationConfig::default().resolve().unwrap();
        assert_eq!(resolved, AutomationTimeouts::default());
        assert_eq!(
            (
                resolved.startup.as_str(),
                resolved.cancel.as_str(),
                resolved.output_drain.as_str()
            ),
            ("120s", "10s", "5s")
        );
    }

    #[test]
    fn values_keep_the_configured_spelling() {
        let resolved = config(Some("3m"), Some("30s"), Some("500ms"))
            .validate()
            .map(|()| config(Some("3m"), Some("30s"), Some("500ms")));
        // 500ms is below the 1s floor.
        assert!(resolved.is_err());
        let resolved = config(Some("3m"), Some("30s"), Some("20s"))
            .resolve()
            .unwrap();
        assert_eq!(resolved.startup.as_str(), "3m");
        assert_eq!(resolved.startup.to_duration().as_secs(), 180);
        assert_eq!(resolved.output_drain.as_str(), "20s");
    }

    #[test]
    fn each_budget_must_be_within_one_second_and_one_hour() {
        for ok in ["1s", "1000ms", "59m", "1h", "3600s"] {
            config(Some(ok), None, None)
                .validate()
                .unwrap_or_else(|e| panic!("{ok}: {e}"));
        }
        for bad in ["0s", "999ms", "61m", "2h", "3601s"] {
            for cfg in [
                config(Some(bad), None, None),
                config(None, Some(bad), None),
                config(None, None, Some(bad)),
            ] {
                let error = cfg.validate().unwrap_err().to_string();
                assert!(error.contains("between 1s and 1h"), "{bad}: {error}");
            }
        }
    }

    #[test]
    fn drain_never_exceeds_cancel_including_against_defaults() {
        // Both in one file.
        assert!(config(None, Some("10s"), Some("11s")).validate().is_err());
        config(None, Some("10s"), Some("10s")).validate().unwrap();
        // One layer lowers cancel below the default drain: each file is
        // valid alone, the merged result is not.
        let lowered = config(None, Some("3s"), None);
        lowered.validate().unwrap();
        let error = lowered.resolve().unwrap_err().to_string();
        assert!(error.contains("outputDrainTimeout"), "{error}");
        // Raising drain above the default cancel likewise.
        assert!(config(None, None, Some("30s")).resolve().is_err());
        config(None, Some("30s"), Some("30s")).resolve().unwrap();
    }

    #[test]
    fn the_block_uses_camel_case_keys_and_refuses_unknown_ones() {
        let parsed: AgentsConfig = serde_json::from_value(serde_json::json!({
            "automation": {"startupTimeout": "90s", "cancelTimeout": "20s", "outputDrainTimeout": "5s"}
        }))
        .unwrap();
        parsed.validate().unwrap();
        assert_eq!(
            parsed.automation.unwrap().startup_timeout.unwrap().as_str(),
            "90s"
        );
        assert!(serde_json::from_value::<AgentsConfig>(serde_json::json!({
            "automation": {"turnTimeout": "5s"}
        }))
        .is_err());
    }
}
