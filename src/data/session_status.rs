//! The lifecycle of one API session row (`sessions.status`).

/// The lifecycle of one API session row. Distinct from
/// [`SessionSetupStatus`](crate::data::session_setup_event::SessionSetupStatus),
/// which tracks how far its workspace has been prepared.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SessionStatus {
    /// Accepting commands.
    Active,
    /// Draining: admits no commands, and closes once running work finishes.
    Closing,
    /// Finished. `closed_at` records when.
    Closed,
}

impl SessionStatus {
    pub const ALL: &'static [SessionStatus] = &[
        SessionStatus::Active,
        SessionStatus::Closing,
        SessionStatus::Closed,
    ];

    /// The wire spelling stored in `sessions.status`.
    pub fn as_str(self) -> &'static str {
        match self {
            SessionStatus::Active => "active",
            SessionStatus::Closing => "closing",
            SessionStatus::Closed => "closed",
        }
    }

    /// Exact, lower-case match on the wire spelling.
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|status| status.as_str() == s)
    }
}

impl std::fmt::Display for SessionStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}
