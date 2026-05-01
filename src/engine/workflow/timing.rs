//! Workflow timing constants and helpers.

use std::time::Duration;

/// Yolo countdown duration before auto-advancing on a stuck step.
pub const YOLO_COUNTDOWN_DURATION: Duration = Duration::from_secs(60);

/// Backoff after a dismissed yolo countdown before re-firing the stuck dialog.
pub const STUCK_DIALOG_BACKOFF: Duration = Duration::from_secs(60);
