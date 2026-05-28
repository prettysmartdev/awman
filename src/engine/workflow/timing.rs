//! Workflow timing constants.

use std::time::Duration;

/// Yolo countdown duration before auto-advancing a stuck step.
pub const YOLO_COUNTDOWN_DURATION: Duration = Duration::from_secs(60);

/// Minimum interval between yolo countdown messages sent to the message
/// sink for CLI and API frontends.  The TUI dialog renders every tick;
/// CLI/API only emit once per this interval to avoid noise.
pub const YOLO_SINK_THROTTLE_INTERVAL: Duration = Duration::from_secs(10);
