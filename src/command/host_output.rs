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
