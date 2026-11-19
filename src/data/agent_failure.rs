//! Stable failure codes for native modes, profiles, automation and
//! credential updates (WI 0070) — Layer 0.
//!
//! One spelling per code, shared by the error envelope (`code`), command
//! results (`result.failure.code`), turn results and `operation_error`
//! frames. A code is defined here once; the layer that detects the failure
//! decides when to raise it.
//!
//! [`SYNCHRONOUS_STATUS`] records the HTTP status class of the codes a
//! request can be refused with directly. The others describe an outcome of
//! work that was already accepted and appear only in `result.failure` (or a
//! frame), so they have no status of their own.
//!
//! `credentials_lost` already exists as
//! [`failure_codes::CREDENTIALS_LOST`](crate::data::fs::api_db::failure_codes::CREDENTIALS_LOST)
//! and is reused for a credential update whose request-only material was
//! lost; it is deliberately not redefined here. Likewise `control_held`,
//! `input_not_allowed`, `input_backpressure` and `idempotency_conflict` keep
//! their existing definitions.

/// Contradictory effective mode selectors (`--plan` against a configured
/// provider mode/sandbox flag, a profile or an override).
pub const AGENT_MODE_CONFLICT: &str = "agent_mode_conflict";
/// The launch's minimum status tier is not met for automated task delivery.
pub const STATUS_SUPPORT_INSUFFICIENT: &str = "status_support_insufficient";
/// A native facility the operation needs is proven missing on this route.
pub const NATIVE_CAPABILITY_UNAVAILABLE: &str = "native_capability_unavailable";
/// `--agent-session` names no exact saved conversation in the profile.
pub const NATIVE_SESSION_NOT_FOUND: &str = "native_session_not_found";
/// Another writer holds the profile, or it is still closing.
pub const SETTINGS_PROFILE_BUSY: &str = "settings_profile_busy";
/// The execution cannot accept the operation in its current state.
pub const AGENT_NOT_READY: &str = "agent_not_ready";
/// The request names an execution that has been replaced or has ended.
pub const STALE_EXECUTION: &str = "stale_execution";
/// The request's `expected_revision` is not the current revision.
pub const STALE_REVISION: &str = "stale_revision";
/// Readiness was not established within `agents.automation.startupTimeout`.
pub const STARTUP_TIMEOUT: &str = "startup_timeout";
/// A crash between turn reservation and observed consumption.
pub const DELIVERY_UNKNOWN: &str = "delivery_unknown";
/// The provider reported the turn as failed.
pub const AGENT_TURN_FAILED: &str = "agent_turn_failed";
/// Malformed, missing or contradictory required native records.
pub const AGENT_PROTOCOL_ERROR: &str = "agent_protocol_error";
/// Waiting on the runtime for the agent's exit failed.
pub const RUNTIME_WAIT_FAILED: &str = "runtime_wait_failed";
/// The provider refused the replacement credential, or applying it failed.
pub const CREDENTIAL_UPDATE_FAILED: &str = "credential_update_failed";
/// Output capture or drain did not finish within its budget.
pub const OUTPUT_INCOMPLETE: &str = "output_incomplete";
/// Owned-resource cleanup was not confirmed within its budget.
pub const CLEANUP_TIMEOUT: &str = "cleanup_timeout";
/// A started execution did not survive a daemon restart.
pub const EXECUTION_LOST: &str = "execution_lost";

/// Every code this module defines.
pub const ALL: &[&str] = &[
    AGENT_MODE_CONFLICT,
    STATUS_SUPPORT_INSUFFICIENT,
    NATIVE_CAPABILITY_UNAVAILABLE,
    NATIVE_SESSION_NOT_FOUND,
    SETTINGS_PROFILE_BUSY,
    AGENT_NOT_READY,
    STALE_EXECUTION,
    STALE_REVISION,
    STARTUP_TIMEOUT,
    DELIVERY_UNKNOWN,
    AGENT_TURN_FAILED,
    AGENT_PROTOCOL_ERROR,
    RUNTIME_WAIT_FAILED,
    CREDENTIAL_UPDATE_FAILED,
    OUTPUT_INCOMPLETE,
    CLEANUP_TIMEOUT,
    EXECUTION_LOST,
];

/// The HTTP status of each code a request can be refused with directly:
/// invalid policy is 400, missing native state 404, and an ownership,
/// readiness, staleness, capability or minimum-status conflict 409.
pub const SYNCHRONOUS_STATUS: &[(&str, u16)] = &[
    (AGENT_MODE_CONFLICT, 400),
    (NATIVE_SESSION_NOT_FOUND, 404),
    (STATUS_SUPPORT_INSUFFICIENT, 409),
    (NATIVE_CAPABILITY_UNAVAILABLE, 409),
    (SETTINGS_PROFILE_BUSY, 409),
    (AGENT_NOT_READY, 409),
    (STALE_EXECUTION, 409),
    (STALE_REVISION, 409),
];

/// The synchronous HTTP status of `code`, if it has one.
pub fn synchronous_status(code: &str) -> Option<u16> {
    SYNCHRONOUS_STATUS
        .iter()
        .find(|(known, _)| *known == code)
        .map(|(_, status)| *status)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_unique_snake_case_and_do_not_shadow_existing_ones() {
        let mut seen = std::collections::BTreeSet::new();
        for code in ALL {
            assert!(seen.insert(*code), "duplicate code {code}");
            assert!(
                code.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'),
                "{code}"
            );
        }
        assert_eq!(ALL.len(), 17);
        // Reused, not redefined.
        assert!(!seen.contains(crate::data::fs::api_db::failure_codes::CREDENTIALS_LOST));
        for reused in [
            "control_held",
            "input_not_allowed",
            "input_backpressure",
            "idempotency_conflict",
        ] {
            assert!(!seen.contains(reused), "{reused}");
        }
        // No version-qualification admission error exists.
        assert!(!seen.iter().any(|code| code.contains("unqualified")));
    }

    #[test]
    fn synchronous_statuses_cover_only_defined_codes() {
        for (code, status) in SYNCHRONOUS_STATUS {
            assert!(ALL.contains(code), "{code}");
            assert!(matches!(status, 400 | 404 | 409), "{code}: {status}");
            assert_eq!(synchronous_status(code), Some(*status));
        }
        assert_eq!(synchronous_status(AGENT_MODE_CONFLICT), Some(400));
        assert_eq!(synchronous_status(NATIVE_SESSION_NOT_FOUND), Some(404));
        assert_eq!(synchronous_status(STALE_REVISION), Some(409));
        // Outcomes of accepted work have no status of their own.
        for code in [
            STARTUP_TIMEOUT,
            DELIVERY_UNKNOWN,
            AGENT_TURN_FAILED,
            RUNTIME_WAIT_FAILED,
            EXECUTION_LOST,
        ] {
            assert_eq!(synchronous_status(code), None, "{code}");
        }
    }
}
