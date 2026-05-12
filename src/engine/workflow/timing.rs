//! Workflow timing constants.

use std::time::Duration;

/// Yolo countdown duration before auto-advancing a stuck step.
pub const YOLO_COUNTDOWN_DURATION: Duration = Duration::from_secs(60);
