//! Workflow timing constants.
//!
//! The countdowns are fixed product behaviour. The shipped binary has no
//! switch that shortens them (WI 0144 S6: the former
//! `AWMAN_TEST_YOLO_COUNTDOWN_MS` / `AWMAN_TEST_AGENT_DONE_COUNTDOWN_MS`
//! hooks were removed). Tests drive countdown logic in process on paused
//! Tokio time instead of shortening a real one.

use std::time::Duration;

/// Yolo countdown duration before auto-advancing a stuck step.
pub const YOLO_COUNTDOWN_DURATION: Duration = Duration::from_secs(60);

/// The yolo / failure-retry countdown. Always [`YOLO_COUNTDOWN_DURATION`];
/// kept as a function so callers have one place to read it.
pub fn yolo_countdown_duration() -> Duration {
    YOLO_COUNTDOWN_DURATION
}

/// Minimum interval between yolo countdown messages sent to the message
/// sink for CLI and API frontends.  The TUI dialog renders every tick;
/// CLI/API only emit once per this interval to avoid noise.
pub const YOLO_SINK_THROTTLE_INTERVAL: Duration = Duration::from_secs(10);

/// How long a step waits after its agent reports `done`/`idle` over OSC 7501
/// before the engine kills the container and advances (WI 0141,
/// `CountdownKind::AgentDone`). Short on purpose: the agent has said it is
/// finished, so this is only a window for the user to object.
pub const AGENT_DONE_COUNTDOWN_DURATION: Duration = Duration::from_secs(10);

/// The `AgentDone` countdown. Always [`AGENT_DONE_COUNTDOWN_DURATION`]; the
/// `AgentBlocked` and `AgentError` countdowns use [`yolo_countdown_duration`].
pub fn agent_done_countdown_duration() -> Duration {
    AGENT_DONE_COUNTDOWN_DURATION
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shipped countdowns are the documented constants whatever the
    /// process environment says (S6: no release-reachable test hook).
    #[test]
    fn countdowns_are_the_documented_constants() {
        assert_eq!(yolo_countdown_duration(), Duration::from_secs(60));
        assert_eq!(agent_done_countdown_duration(), Duration::from_secs(10));
        assert!(YOLO_SINK_THROTTLE_INTERVAL < YOLO_COUNTDOWN_DURATION);
    }
}
