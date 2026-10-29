//! `AgentOutputFormat` — the structured output mode an agent CLI can print in
//! non-interactive runs (WI 0137 §4).
//!
//! A capability of the agent table, not a name match: `claude` prints
//! `--output-format stream-json`, `codex exec --json` prints its own JSON
//! lines, and every other agent is plain text. The engine's
//! `agent_output` translators turn a structured stream into `step_activity`
//! events, ordinary output lines and the agent's final message.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentOutputFormat {
    /// Plain text: output lines only, no activity feed, no final message.
    #[default]
    Text,
    /// Claude Code's `--output-format stream-json --verbose` NDJSON.
    ClaudeStreamJson,
    /// Codex's `exec --json` NDJSON.
    CodexJson,
}

impl AgentOutputFormat {
    /// The serialised spelling, as written in a custom agent definition's
    /// `[run] output_format` and shown in logs.
    pub fn as_str(self) -> &'static str {
        match self {
            AgentOutputFormat::Text => "text",
            AgentOutputFormat::ClaudeStreamJson => "claude_stream_json",
            AgentOutputFormat::CodexJson => "codex_json",
        }
    }

    /// `true` when a translator exists for this format, i.e. an API run can
    /// produce `step_activity` events and a `final_message`.
    pub fn is_structured(self) -> bool {
        !matches!(self, AgentOutputFormat::Text)
    }
}

impl std::fmt::Display for AgentOutputFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_spelling_round_trips_and_defaults_to_text() {
        for (format, wire) in [
            (AgentOutputFormat::Text, "\"text\""),
            (
                AgentOutputFormat::ClaudeStreamJson,
                "\"claude_stream_json\"",
            ),
            (AgentOutputFormat::CodexJson, "\"codex_json\""),
        ] {
            assert_eq!(serde_json::to_string(&format).unwrap(), wire);
            assert_eq!(
                serde_json::from_str::<AgentOutputFormat>(wire).unwrap(),
                format
            );
            assert_eq!(format!("\"{format}\""), wire);
        }
        assert_eq!(AgentOutputFormat::default(), AgentOutputFormat::Text);
        assert!(!AgentOutputFormat::Text.is_structured());
        assert!(AgentOutputFormat::ClaudeStreamJson.is_structured());
    }
}
