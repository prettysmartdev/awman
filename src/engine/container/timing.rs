//! Container-layer timing constants.
//!
//! Each frontend chooses its own grace + stuck timeouts via the
//! `AgentFrontend` trait; the constants here are the defaults baked into
//! that trait. CLI/TUI accept the defaults (30s/30s); API overrides to
//! 15min grace so a long image pull or model warm-up doesn't kill the run.

use std::time::Duration;

/// Default stuck timeout — applied after the container has emitted its first
/// byte of output. Silence publishes `StuckEvent::Stuck` only when no OSC 7501
/// records supersede it. Accepted reports latch support until execution end or
/// a new step/container; clear (and RIS, which keeps the latch) resumes the
/// silence clock from the clear. Startup grace is unchanged. Sandbox currently
/// publishes reports without driving this silence timeout.
pub const DEFAULT_STUCK_TIMEOUT: Duration = Duration::from_secs(30);

/// Default startup-grace timeout — the window between container launch and
/// its first byte of output. If no byte arrives before this elapses, the
/// container is killed and the step transitions to `Failed`.
pub const DEFAULT_GRACE_TIMEOUT: Duration = Duration::from_secs(30);

/// Apple's `container` runtime prints its own startup chatter ("creating
/// container…", "starting container…") on stdout before the actual workload
/// process produces a byte. Those bytes would otherwise satisfy the
/// detector's "first byte" check and prematurely end the grace phase. We
/// suppress activity / first_byte tracking for this long after launch so
/// only the real workload's output counts.
pub const APPLE_CONTAINER_START_DELAY: Duration = Duration::from_secs(10);
