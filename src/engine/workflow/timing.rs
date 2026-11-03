//! Workflow timing constants.

use std::time::Duration;

/// Yolo countdown duration before auto-advancing a stuck step.
///
/// Engine and frontend code read the effective value through
/// [`yolo_countdown_duration`], which is this constant in every normal run.
pub const YOLO_COUNTDOWN_DURATION: Duration = Duration::from_secs(60);

/// The yolo / failure-retry countdown: [`YOLO_COUNTDOWN_DURATION`], unless
/// `AWMAN_TEST_ISOLATION` is on and `AWMAN_TEST_YOLO_COUNTDOWN_MS` names a
/// shorter one (see `EnvSnapshot::test_yolo_countdown`). The override exists
/// so integration tests that drive a real `awman` process through a countdown
/// need not wait out a minute; a normal run never honours it.
pub fn yolo_countdown_duration() -> Duration {
    crate::data::config::env::Env::from_process()
        .test_yolo_countdown()
        .unwrap_or(YOLO_COUNTDOWN_DURATION)
}

/// Minimum interval between yolo countdown messages sent to the message
/// sink for CLI and API frontends.  The TUI dialog renders every tick;
/// CLI/API only emit once per this interval to avoid noise.
pub const YOLO_SINK_THROTTLE_INTERVAL: Duration = Duration::from_secs(10);
