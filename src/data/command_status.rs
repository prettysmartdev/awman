//! The lifecycle of one API command row (`commands.status`).
//!
//! The column itself stays a free `String` in `SqliteSessionStore`; this enum
//! is where the wire spellings are named and matched, so a status check reads
//! `CommandStatus::parse(s).is_some_and(CommandStatus::is_terminal)` instead of
//! a string literal list repeated at every call site.

/// Exit code recorded on a `cancelled` row: the SIGINT convention, the same
/// code `CommandError::Aborted` maps to.
pub const CANCELLED_EXIT_CODE: i32 = 130;

/// The `result` JSON of a `cancelled` row: `{"exit_code":130,"error":"cancelled"}`,
/// plus `"cancelled_at"` when the command was cancelled while running.
pub fn cancelled_result_json(cancelled_at: Option<&str>) -> String {
    let mut result = serde_json::json!({
        "exit_code": CANCELLED_EXIT_CODE,
        "error": "cancelled",
    });
    if let Some(at) = cancelled_at {
        result["cancelled_at"] = serde_json::Value::String(at.to_string());
    }
    result.to_string()
}

/// The lifecycle of one API command row (`commands.status`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandStatus {
    /// Enqueued, waiting for a worker and for its session to be ready.
    Queued,
    /// Legacy spelling of `Queued` from pre-0079 databases; never written.
    Pending,
    /// Claimed by a worker. A cancel request leaves the row `running` until
    /// the worker finalises it.
    Running,
    /// Finished with exit 0.
    Done,
    /// Finished non-zero, failed to start, push failed, or its session setup
    /// failed.
    Error,
    /// Cancelled while queued, while running, or during session setup.
    Cancelled,
}

impl CommandStatus {
    pub const ALL: &'static [CommandStatus] = &[
        CommandStatus::Queued,
        CommandStatus::Pending,
        CommandStatus::Running,
        CommandStatus::Done,
        CommandStatus::Error,
        CommandStatus::Cancelled,
    ];

    pub const TERMINAL: &'static [CommandStatus] = &[
        CommandStatus::Done,
        CommandStatus::Error,
        CommandStatus::Cancelled,
    ];

    /// The wire spelling stored in `commands.status`.
    pub fn as_str(self) -> &'static str {
        match self {
            CommandStatus::Queued => "queued",
            CommandStatus::Pending => "pending",
            CommandStatus::Running => "running",
            CommandStatus::Done => "done",
            CommandStatus::Error => "error",
            CommandStatus::Cancelled => "cancelled",
        }
    }

    /// Exact, lower-case match on the wire spelling.
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|status| status.as_str() == s)
    }

    pub fn is_terminal(self) -> bool {
        Self::TERMINAL.contains(&self)
    }

    /// Whether a worker may claim a row in this status.
    pub fn is_claimable(self) -> bool {
        matches!(self, CommandStatus::Queued | CommandStatus::Pending)
    }
}

impl std::str::FromStr for CommandStatus {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s).ok_or_else(|| format!("unknown command status: {s}"))
    }
}

impl std::fmt::Display for CommandStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}
