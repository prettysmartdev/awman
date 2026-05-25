//! Container-layer timing constants.

use std::time::Duration;

/// A container is considered "stuck" when no stdout/stderr output has
/// arrived for this duration.
pub const STUCK_TIMEOUT: Duration = Duration::from_secs(30);
