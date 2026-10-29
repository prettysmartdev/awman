//! Agent output translators (WI 0137 §4).
//!
//! A non-interactive agent with a structured [`AgentOutputFormat`] prints a
//! machine-readable stream instead of plain text (claude `--output-format
//! stream-json --verbose`, codex `exec --json`). One translator per format
//! turns that stream into three things, from a single parse:
//!
//! 1. [`Translated::Activity`] — a `step_activity` event while the agent
//!    works: one bounded summary line, never the full tool payload;
//! 2. [`Translated::Line`] — the agent's final message as ordinary output
//!    lines, so text-only clients see the same output as before;
//! 3. [`Translated::FinalMessage`] — the same final message once, whole, for
//!    `CommandResult::final_message` (`exec prompt`).
//!
//! An unparseable line passes through as an output line; nothing is dropped.
//! [`AgentOutputFormat::Text`] uses [`PassthroughTranslator`], which emits
//! every line unchanged and no activity or final message.
//!
//! Layering: this is Layer 1. It knows nothing about event buses or result
//! rows; the API frontend (Layer 3) feeds it lines from the container's
//! stdout and maps each [`Translated`] onto an event or a result field.
//!
//! The per-format translators are registered in [`translator_for`].

mod claude_stream_json;
mod codex_json;

use std::sync::Arc;

pub use crate::data::agent_output_format::AgentOutputFormat;
pub use crate::data::execution_event::{StepActivityKind, STEP_ACTIVITY_SUMMARY_MAX_CHARS};

/// Which container stream a line arrived on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputStream {
    Stdout,
    Stderr,
}

/// One thing a translator produced from the agent's stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Translated {
    /// An ordinary output line, emitted as `StdoutLine` / `StderrLine`.
    Line { stream: OutputStream, text: String },
    /// A `step_activity` event. `summary` is already bounded to
    /// [`STEP_ACTIVITY_SUMMARY_MAX_CHARS`] characters and contains no newline.
    Activity {
        kind: StepActivityKind,
        summary: String,
    },
    /// The agent's final message, whole. Emitted at most once per run, after
    /// the same text has been emitted as [`Translated::Line`]s.
    FinalMessage(String),
}

/// Turns an agent's output stream into [`Translated`] items, one input line
/// at a time. Implementations are plain state machines: no I/O, no clocks.
pub trait AgentOutputTranslator: Send {
    /// Feed one line (without its trailing newline). Returns what the line
    /// produced, in emission order; an empty vector is fine.
    fn feed_line(&mut self, stream: OutputStream, line: &str) -> Vec<Translated>;

    /// The stream ended. Flush anything buffered (a final message that was
    /// never terminated, a partial frame passed through as a line).
    fn finish(&mut self) -> Vec<Translated>;
}

/// The translator for an agent that prints plain text: every line passes
/// through unchanged, and there is no activity feed and no final message.
#[derive(Debug, Default)]
pub struct PassthroughTranslator;

impl AgentOutputTranslator for PassthroughTranslator {
    fn feed_line(&mut self, stream: OutputStream, line: &str) -> Vec<Translated> {
        vec![Translated::Line {
            stream,
            text: line.to_string(),
        }]
    }

    fn finish(&mut self) -> Vec<Translated> {
        Vec::new()
    }
}

/// The translator for `format`.
///
/// `Text` is the passthrough; structured formats each have one parser.
pub fn translator_for(format: AgentOutputFormat) -> Box<dyn AgentOutputTranslator> {
    match format {
        AgentOutputFormat::Text => Box::new(PassthroughTranslator),
        AgentOutputFormat::ClaudeStreamJson => Box::new(claude_stream_json::Translator::default()),
        AgentOutputFormat::CodexJson => Box::new(codex_json::Translator::default()),
    }
}

/// Shared emission keeps the displayed transcript and result message identical.
fn final_message(message: String) -> Vec<Translated> {
    // WI 0141: apply the same redaction as ordinary output to this message.
    let mut items: Vec<_> = message
        .lines()
        .map(|line| Translated::Line {
            stream: OutputStream::Stdout,
            text: line.to_owned(),
        })
        .collect();
    items.push(Translated::FinalMessage(message));
    items
}

fn activity(kind: StepActivityKind, summary: &str) -> Translated {
    // WI 0141: apply the same redaction as ordinary output to this summary.
    Translated::Activity {
        kind,
        summary: bound_summary(summary),
    }
}

fn passthrough(stream: OutputStream, line: &str) -> Vec<Translated> {
    vec![Translated::Line {
        stream,
        text: line.to_owned(),
    }]
}

/// Where a translated [`Translated::FinalMessage`] goes: the API frontend
/// builds one from `CommandStateWriter::record_final_message`, so the message
/// reaches `CommandResult::final_message` without the translator or the
/// frontend knowing about result rows.
pub type FinalMessageSink = Arc<dyn Fn(String) + Send + Sync>;

/// Bound a `step_activity` summary: first line only, control characters
/// dropped, at most [`STEP_ACTIVITY_SUMMARY_MAX_CHARS`] characters with a
/// trailing ellipsis when cut. Translators pass every summary through this.
pub fn bound_summary(text: &str) -> String {
    let first_line = text
        .split(['\n', '\r', '\u{85}', '\u{2028}', '\u{2029}'])
        .next()
        .unwrap_or_default();
    let cleaned: String = first_line.chars().filter(|c| !c.is_control()).collect();
    let trimmed = cleaned.trim();
    if trimmed.chars().count() <= STEP_ACTIVITY_SUMMARY_MAX_CHARS {
        return trimmed.to_string();
    }
    let mut cut: String = trimmed
        .chars()
        .take(STEP_ACTIVITY_SUMMARY_MAX_CHARS - 1)
        .collect();
    cut.push('…');
    cut
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passthrough_emits_every_line_unchanged_and_nothing_else() {
        let mut translator = translator_for(AgentOutputFormat::Text);
        assert_eq!(
            translator.feed_line(OutputStream::Stdout, "{\"not\": \"parsed\"}"),
            vec![Translated::Line {
                stream: OutputStream::Stdout,
                text: "{\"not\": \"parsed\"}".into()
            }]
        );
        assert_eq!(
            translator.feed_line(OutputStream::Stderr, "warn"),
            vec![Translated::Line {
                stream: OutputStream::Stderr,
                text: "warn".into()
            }]
        );
        assert!(translator.finish().is_empty());
    }

    #[test]
    fn every_format_has_a_translator() {
        for format in [
            AgentOutputFormat::Text,
            AgentOutputFormat::ClaudeStreamJson,
            AgentOutputFormat::CodexJson,
        ] {
            let mut translator = translator_for(format);
            assert!(
                !translator.feed_line(OutputStream::Stdout, "x").is_empty()
                    || !format.is_structured()
            );
        }
    }

    #[test]
    fn summaries_are_one_bounded_line() {
        assert_eq!(
            bound_summary("  Read src/main.rs\nsecond line"),
            "Read src/main.rs"
        );
        assert_eq!(bound_summary("tab\tand\u{7}bell"), "tabandbell");
        let long = "x".repeat(STEP_ACTIVITY_SUMMARY_MAX_CHARS + 50);
        let bounded = bound_summary(&long);
        assert_eq!(bounded.chars().count(), STEP_ACTIVITY_SUMMARY_MAX_CHARS);
        assert!(bounded.ends_with('…'));
        let exact = "y".repeat(STEP_ACTIVITY_SUMMARY_MAX_CHARS);
        assert_eq!(bound_summary(&exact), exact);
    }
}

#[cfg(test)]
mod adversarial_tests {
    use super::*;

    #[test]
    fn valid_unknown_frames_never_publish_full_tool_payloads() {
        for (format, frames) in [
            (
                AgentOutputFormat::ClaudeStreamJson,
                vec![
                    r#"{"type":"assistant","message":{"content":[{"type":"text","text":"visible"},{"type":"server_tool_use","input":"SECRET_PAYLOAD"}]}}"#,
                    r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"x","name":"MCP","input":"SECRET_PAYLOAD"}]}}"#,
                    r#"{"type":"user","message":{"content":"SECRET_PAYLOAD"}}"#,
                    r#"{"type":"result","is_error":true,"permission_denials":[{"input":"SECRET_PAYLOAD"}]}"#,
                ],
            ),
            (
                AgentOutputFormat::CodexJson,
                vec![
                    r#"{"type":"item.completed","item":{"type":"future_tool","result":"SECRET_PAYLOAD"}}"#,
                    r#"{"type":"item.updated","item":{"type":"command_execution","aggregated_output":"SECRET_PAYLOAD"}}"#,
                    r#"{"type":"turn.failed","error":{"payload":"SECRET_PAYLOAD"}}"#,
                ],
            ),
        ] {
            let mut translator = translator_for(format);
            for frame in frames {
                let output = translator.feed_line(OutputStream::Stdout, frame);
                assert!(!format!("{output:?}").contains("SECRET_PAYLOAD"));
                assert!(!output
                    .iter()
                    .any(|item| matches!(item, Translated::Line { .. })));
            }
            assert!(translator.finish().is_empty());
            assert_eq!(
                translator.feed_line(OutputStream::Stdout, "malformed: {"),
                passthrough(OutputStream::Stdout, "malformed: {")
            );
        }
    }

    #[test]
    fn terminal_errors_discard_interim_answers() {
        for (format, interim, failed) in [
            (
                AgentOutputFormat::ClaudeStreamJson,
                r#"{"type":"assistant","message":{"content":[{"type":"text","text":"interim"}]}}"#,
                r#"{"type":"result","subtype":"error_max_turns","is_error":true}"#,
            ),
            (
                AgentOutputFormat::CodexJson,
                r#"{"type":"item.completed","item":{"type":"agent_message","text":"interim"}}"#,
                r#"{"type":"turn.failed","error":{"message":"failed"}}"#,
            ),
        ] {
            let mut translator = translator_for(format);
            translator.feed_line(OutputStream::Stdout, interim);
            let output = translator.feed_line(OutputStream::Stdout, failed);
            assert!(!output
                .iter()
                .any(|item| matches!(item, Translated::FinalMessage(_) | Translated::Line { .. })));
            assert!(translator.finish().is_empty());
        }
    }

    #[test]
    fn codex_thinking_is_constant_and_payload_updates_are_not_activities() {
        let mut translator = translator_for(AgentOutputFormat::CodexJson);
        let output = translator.feed_line(
            OutputStream::Stdout,
            r#"{"type":"item.started","item":{"type":"reasoning","text":"private reasoning"}}"#,
        );
        assert_eq!(
            output,
            vec![activity(StepActivityKind::Thinking, "thinking")]
        );
        assert!(translator.feed_line(OutputStream::Stdout, r#"{"type":"item.updated","item":{"type":"reasoning","text":"private reasoning update"}}"#).is_empty());
    }
}
