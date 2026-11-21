//! `CiPollEvent` — what one round of the CI poll observed (WI 0114 F-45).
//!
//! `CiPoller` used to hand its caller a `(level, String)` pair it had already
//! composed, and the workflow engine pushed that string straight through
//! `write_message` — Layer 1 authoring transcript text. The poller now
//! reports the fact and the frontend decides how to draw it.
//!
//! [`Display`](std::fmt::Display) reproduces the exact narration the poller
//! composed before, so the default `WorkflowFrontend::report_ci_poll` leaves
//! every frontend's output unchanged.

use std::fmt;

use crate::data::message::MessageLevel;

/// One observation from the CI poll loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CiPollEvent {
    /// A poll attempt is starting. `attempt` is 1-based.
    Attempt { attempt: u32, of: u32 },
    /// CI reported success; the poll is over.
    Passed,
    /// CI is still in progress; the poll will sleep and retry.
    StillRunning,
    /// No CI run exists for this branch/commit yet. Only reported inside the
    /// opening grace window — after it, no run is an error, not an event.
    NoRunYet,
    /// CI reported failure; the poll is over. `detail` is GitHub's reason.
    Failed { detail: String },
}

impl CiPollEvent {
    /// How prominently to render this event. Only a CI failure is a warning;
    /// this reproduces the poller's own `PollMessage` choice.
    pub fn level(&self) -> MessageLevel {
        match self {
            Self::Failed { .. } => MessageLevel::Warning,
            _ => MessageLevel::Info,
        }
    }
}

impl fmt::Display for CiPollEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Attempt { attempt, of } => {
                write!(f, "Polling CI (attempt {attempt}/{of})...")
            }
            Self::Passed => f.write_str("CI passed"),
            Self::StillRunning => f.write_str("CI still running"),
            Self::NoRunYet => {
                f.write_str("No CI run found yet (may not have been created); will retry")
            }
            Self::Failed { detail } => write!(f, "CI failed: {detail}"),
        }
    }
}
