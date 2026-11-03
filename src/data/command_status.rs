//! The lifecycle of one API command row (`commands.status`).
//!
//! The column stays free text; `SqliteSessionStore` converts at its boundary,
//! so records and mutations carry this enum and an unknown stored value is a
//! read error rather than a state nothing matches.

use crate::data::execution_event::CommandStatusKind;

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

/// A command's terminal outcome, written to its `completion.json` sidecar
/// before the row. A restart finalizes a `running` row from this record instead
/// of running the command's side effects again.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CommandCompletion {
    /// `CommandRecord.attempt` at completion. A sidecar from another attempt
    /// says nothing about the current one.
    pub attempt: u32,
    pub status: CommandStatus,
    pub exit_code: i32,
    /// Exactly what `commands.result` receives.
    pub result_json: String,
    /// RFC 3339.
    pub finished_at: String,
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

    /// The status with the legacy `Pending` spelling folded into `Queued`.
    pub fn canonical(self) -> Self {
        match self {
            CommandStatus::Pending => CommandStatus::Queued,
            status => status,
        }
    }

    /// The wire event status of a terminal row. A row never holds the
    /// `paused`/`aborted` refinements; `CommandResult::outcome` carries those.
    pub fn terminal_wire_kind(self) -> Option<CommandStatusKind> {
        match self {
            CommandStatus::Done => Some(CommandStatusKind::Done),
            CommandStatus::Error => Some(CommandStatusKind::Error),
            CommandStatus::Cancelled => Some(CommandStatusKind::Cancelled),
            CommandStatus::Queued | CommandStatus::Pending | CommandStatus::Running => None,
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_persisted_spelling_round_trips() {
        for (status, spelling) in [
            (CommandStatus::Queued, "queued"),
            (CommandStatus::Pending, "pending"),
            (CommandStatus::Running, "running"),
            (CommandStatus::Done, "done"),
            (CommandStatus::Error, "error"),
            (CommandStatus::Cancelled, "cancelled"),
        ] {
            assert_eq!(status.as_str(), spelling);
            assert_eq!(CommandStatus::parse(spelling), Some(status));
            assert_eq!(spelling.parse::<CommandStatus>(), Ok(status));
            assert_eq!(status.to_string(), spelling);
            assert_eq!(serde_json::to_value(status).unwrap(), spelling);
        }
        assert_eq!(CommandStatus::ALL.len(), 6);
    }

    #[test]
    fn parse_rejects_outcomes_that_are_not_row_statuses() {
        for invalid in ["", "DONE", "aborted", "paused", "failed", " done"] {
            assert_eq!(CommandStatus::parse(invalid), None, "{invalid:?}");
            assert!(invalid.parse::<CommandStatus>().is_err());
        }
    }

    #[test]
    fn legacy_pending_behaves_as_queued() {
        assert_eq!(CommandStatus::Pending.canonical(), CommandStatus::Queued);
        assert!(CommandStatus::Pending.is_claimable());
        assert!(!CommandStatus::Pending.is_terminal());
        for status in CommandStatus::ALL.iter().copied() {
            if status != CommandStatus::Pending {
                assert_eq!(status.canonical(), status);
            }
        }
    }

    #[test]
    fn only_terminal_rows_have_a_wire_kind() {
        for status in CommandStatus::ALL.iter().copied() {
            let kind = status.terminal_wire_kind();
            assert_eq!(kind.is_some(), status.is_terminal(), "{status}");
            if let Some(kind) = kind {
                assert_eq!(kind.to_string(), status.as_str());
            }
        }
    }
}
