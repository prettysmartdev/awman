//! Which `HostOutputPolicy` a host-terminal writer gets (WI 0141, Layer 2).
//!
//! The frontend never chooses the policy. It is resolved here from config
//! and host-integration state and handed to Layer 3's `HostOutputFilter`.
//! Today the inputs are the defaults (notifications stripped, OSC 7501
//! re-encoded) and `agents.programStatus.enabled`: with program status
//! turned off, agent reports are stripped instead of re-encoded. 0157 maps
//! `tui.integrations.rex.forwardAgentNotifications` onto
//! `allow_notifications` through this function.

use crate::data::config::EffectiveConfig;
use crate::data::terminal::{HostOutputPolicy, ProgramStatusMode};

/// The policy for agent bytes reaching this process's host terminal.
pub fn resolve_host_output_policy(config: &EffectiveConfig) -> HostOutputPolicy {
    HostOutputPolicy {
        allow_notifications: false,
        program_status: if config.program_status_enabled() {
            ProgramStatusMode::Reencode
        } else {
            ProgramStatusMode::Strip
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::config::ConfigFile;

    fn config(json: serde_json::Value) -> EffectiveConfig {
        EffectiveConfig::from_files(
            ConfigFile::test_json(json),
            Default::default(),
            Default::default(),
            Default::default(),
        )
        .unwrap()
    }

    #[test]
    fn default_strips_notifications_and_reencodes_status() {
        let policy = resolve_host_output_policy(&EffectiveConfig::default());
        assert_eq!(policy, HostOutputPolicy::default());
        assert!(!policy.allow_notifications);
        assert_eq!(policy.program_status, ProgramStatusMode::Reencode);
    }

    #[test]
    fn program_status_disabled_strips_reports() {
        let policy = resolve_host_output_policy(&config(serde_json::json!({
            "agents": { "programStatus": { "enabled": false } }
        })));
        assert_eq!(policy.program_status, ProgramStatusMode::Strip);
        assert!(!policy.allow_notifications);
    }
}
