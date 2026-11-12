//! Agent output translators (WI 0137 §4).
//!
//! A headless agent with a structured [`AgentOutputFormat`] prints a
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
pub mod line_framer;

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
    // WI 0147: apply the same redaction as ordinary output to this message.
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
    // WI 0147: apply the same redaction as ordinary output to this summary.
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

/// Shared container lifecycle presentation used by API commands and setup.
pub fn container_status_message(status: &crate::engine::agent_runtime::AgentStatus) -> String {
    use crate::engine::agent_runtime::AgentStatus;
    match status {
        AgentStatus::Building => "Building container image...".to_string(),
        AgentStatus::Pulling => "Pulling container image...".to_string(),
        AgentStatus::Starting => "Starting container...".to_string(),
        AgentStatus::Running { container_name } => {
            format!("Container running: {container_name}")
        }
        AgentStatus::Stopping => "Stopping container...".to_string(),
        AgentStatus::Exited(code) => format!("Container exited with code {code}"),
        AgentStatus::Failed(reason) => format!("Container failed: {reason}"),
    }
}
