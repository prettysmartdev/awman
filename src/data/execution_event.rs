//! `ExecutionEvent` — typed events emitted during command/workflow execution.
//!
//! These are Layer 0 data types: serializable, no runtime behavior. Used by
//! the API frontend's `EventBus` (Layer 3) for SSE streaming and logfile
//! persistence. The engine layer (Layer 1) has no knowledge of these types.

use crate::data::fs::api_db::CommandResult;
use crate::data::workflow_prerequisites::{PrerequisitePhase, WorkflowPrerequisiteReport};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// The first execution of a command. Re-queues after a daemon restart count
/// up from here.
pub const FIRST_ATTEMPT: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionEvent {
    pub timestamp: DateTime<Utc>,
    /// Which execution of the command produced this event (1-based). Lines
    /// written before WI 0136 have no field and read back as attempt 1.
    #[serde(default = "first_attempt")]
    pub attempt: u32,
    /// Position within the attempt; restarts at 0 for every attempt.
    pub sequence: u64,
    /// Where this event came from: the phase, step, container and retry
    /// (WI 0137 §5). Absent on events that no step produced (queue notices,
    /// the final `command_status`, `done`) and on lines written before 0137.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<EventSource>,
    pub payload: EventPayload,
}

fn first_attempt() -> u32 {
    FIRST_ATTEMPT
}

impl ExecutionEvent {
    /// This event's position in the command's stream.
    pub fn cursor(&self) -> EventCursor {
        EventCursor::new(self.attempt, self.sequence)
    }

    /// Whether this event belongs to a `?step=<name>` filtered stream
    /// (WI 0137 §5): events attributed to that step pass, and so do the
    /// command-level terminal events (`command_status`, `done`) that carry no
    /// source, so a filtered client can still finish from the stream alone.
    pub fn matches_step_filter(&self, step: &str) -> bool {
        match &self.payload {
            EventPayload::CommandStatus { .. } | EventPayload::Done => true,
            _ => self
                .source
                .as_ref()
                .and_then(|source| source.step_name.as_deref())
                .is_some_and(|name| name == step),
        }
    }
}

/// The pipeline phase an event's producer belongs to (WI 0137 §5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventSourcePhase {
    /// The dynamic workflow's leader agent.
    Leader,
    /// A workflow `setup:` step.
    Setup,
    /// A main-phase agent step, or the `exec prompt` agent.
    Main,
    /// A workflow `teardown:` step.
    Teardown,
    /// Worktree merge-back.
    Merge,
    /// Remote-session push-back.
    Push,
}

impl std::fmt::Display for EventSourcePhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            EventSourcePhase::Leader => "leader",
            EventSourcePhase::Setup => "setup",
            EventSourcePhase::Main => "main",
            EventSourcePhase::Teardown => "teardown",
            EventSourcePhase::Merge => "merge",
            EventSourcePhase::Push => "push",
        };
        f.write_str(s)
    }
}

/// Attribution of one event to the step, container and retry that produced
/// it (WI 0137 §5). Lives on the envelope so every payload — output lines,
/// status messages, `step_activity`, terminal events — is attributed the
/// same way without each variant defining its own copy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventSource {
    pub phase: EventSourcePhase,
    /// Absent for the leader and for shell setup/teardown output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step_name: Option<String>,
    /// The step's position in the workflow definition (`WorkflowState.steps`),
    /// never a first-seen counter, so it is stable across attempts and
    /// matches `GET /v1/workflows/{id}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step_index: Option<usize>,
    /// Runtime container name, when one exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
    /// 1-based; incremented by `on_failure` retries. Distinct from the
    /// envelope's `attempt`, which counts executions after a daemon restart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step_attempt: Option<u32>,
}

impl EventSource {
    /// A source with only its phase set.
    pub fn phase(phase: EventSourcePhase) -> Self {
        Self {
            phase,
            step_name: None,
            step_index: None,
            container: None,
            step_attempt: None,
        }
    }

    /// A main-phase step source. `container` and `step_attempt` are added
    /// with the builder methods once known.
    pub fn step(phase: EventSourcePhase, step_name: impl Into<String>, step_index: usize) -> Self {
        Self {
            phase,
            step_name: Some(step_name.into()),
            step_index: Some(step_index),
            container: None,
            step_attempt: None,
        }
    }

    pub fn with_container(mut self, container: impl Into<String>) -> Self {
        self.container = Some(container.into());
        self
    }

    pub fn with_step_attempt(mut self, step_attempt: u32) -> Self {
        self.step_attempt = Some(step_attempt);
        self
    }
}

/// What a structured-output agent is doing right now (WI 0137 §4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepActivityKind {
    /// The assistant produced (part of) a message.
    Message,
    /// The assistant invoked a tool.
    ToolUse,
    /// A tool returned.
    ToolResult,
    /// The assistant is thinking.
    Thinking,
}

impl std::fmt::Display for StepActivityKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            StepActivityKind::Message => "message",
            StepActivityKind::ToolUse => "tool_use",
            StepActivityKind::ToolResult => "tool_result",
            StepActivityKind::Thinking => "thinking",
        };
        f.write_str(s)
    }
}

/// Longest `step_activity.summary` the translators emit, in characters. A
/// summary is one bounded line, never a tool payload, so event volume stays
/// bounded for a chatty agent.
pub const STEP_ACTIVITY_SUMMARY_MAX_CHARS: usize = 200;

/// A position in a command's event stream: `<attempt>:<sequence>`. This is the
/// SSE `id:` of every persisted frame and the value of `Last-Event-ID` /
/// `?after=`. Ordering is lexicographic (attempt first), which is the order
/// events are appended to `events.log`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EventCursor {
    pub attempt: u32,
    pub sequence: u64,
}

impl EventCursor {
    pub const fn new(attempt: u32, sequence: u64) -> Self {
        Self { attempt, sequence }
    }

    /// Parse exactly `<digits>:<digits>`. Returns `None` for anything else
    /// (empty, whitespace, signs, missing colon, overflow). Attempt 0 parses;
    /// it is never kept, so [`ReplayFrom::resolve`] treats it as unknown.
    pub fn parse(s: &str) -> Option<Self> {
        let (attempt, sequence) = s.split_once(':')?;
        let all_digits = |part: &str| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit());
        if !all_digits(attempt) || !all_digits(sequence) {
            return None;
        }
        Some(Self::new(attempt.parse().ok()?, sequence.parse().ok()?))
    }
}

impl std::fmt::Display for EventCursor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.attempt, self.sequence)
    }
}

/// Parse `events.log` NDJSON text into events, in file order. Blank and
/// malformed lines (a torn write from a crash) are skipped.
pub fn parse_event_log(raw: &str) -> Vec<ExecutionEvent> {
    raw.lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str::<ExecutionEvent>(line).ok())
        .collect()
}

/// The attempts that still have at least one event in `events`, ascending.
pub fn kept_attempts(events: &[ExecutionEvent]) -> std::collections::BTreeSet<u32> {
    events.iter().map(|e| e.attempt).collect()
}

/// Where a replay of `events.log` starts, given a client's resume cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplayFrom {
    /// No cursor was given: replay everything.
    Beginning,
    /// The cursor's attempt is kept: replay only events strictly after it.
    After(EventCursor),
    /// The cursor names an attempt that is not kept (attempt 0, purged,
    /// unknown, or newer than the command's current attempt): replay the
    /// current attempt from its start.
    StartOfAttempt(u32),
}

impl ReplayFrom {
    /// Decide the replay start for a command whose current attempt is
    /// `current_attempt`. A cursor beyond the head of a kept attempt is still
    /// `After` (nothing replays; the client is ahead), not a fallback.
    pub fn resolve(
        events: &[ExecutionEvent],
        current_attempt: u32,
        requested: Option<EventCursor>,
    ) -> Self {
        match requested {
            None => ReplayFrom::Beginning,
            Some(cursor)
                if cursor.attempt <= current_attempt
                    && events.iter().any(|e| e.attempt == cursor.attempt) =>
            {
                ReplayFrom::After(cursor)
            }
            Some(_) => ReplayFrom::StartOfAttempt(current_attempt),
        }
    }

    /// Whether `event` is part of the replay.
    pub fn includes(&self, event: &ExecutionEvent) -> bool {
        match self {
            ReplayFrom::Beginning => true,
            ReplayFrom::After(cursor) => event.cursor() > *cursor,
            ReplayFrom::StartOfAttempt(attempt) => event.attempt == *attempt,
        }
    }
}

/// How far a workflow step has got, on the wire.
///
/// `WorkflowStepTransition` carried these as two free-form `String`s (WI 0114
/// F-48). Producing them meant a hand-written map from
/// `WorkflowStepStatus` in the API frontend, and consuming them meant
/// `match to_status.as_str()` in the queue worker — a client reading the
/// stream had nothing but those two matches to learn the vocabulary from, and
/// a typo in either was undetectable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepStatusKind {
    Pending,
    Running,
    Succeeded,
    Failed,
    Cancelled,
    Skipped,
}

impl std::fmt::Display for StepStatusKind {
    /// The serialised spelling, so a log line and the wire agree.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            StepStatusKind::Pending => "pending",
            StepStatusKind::Running => "running",
            StepStatusKind::Succeeded => "succeeded",
            StepStatusKind::Failed => "failed",
            StepStatusKind::Cancelled => "cancelled",
            StepStatusKind::Skipped => "skipped",
        };
        f.write_str(s)
    }
}

/// How a whole command or workflow ended, on the wire.
///
/// Was a `String` whose five values were spelled out in
/// `report_workflow_completed` and matched by `as_str()` in the queue worker
/// (WI 0114 F-48).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandStatusKind {
    /// Ran to completion. `exit_code` says whether it succeeded.
    Done,
    /// Stopped at the user's request and can be resumed.
    Paused,
    /// Stopped without finishing and cannot be resumed.
    Aborted,
    /// Ended with an error; `error` carries the reason.
    Error,
    /// Stopped by an external cancel request (`POST /v1/commands/{id}/cancel`).
    Cancelled,
}

impl std::fmt::Display for CommandStatusKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            CommandStatusKind::Done => "done",
            CommandStatusKind::Paused => "paused",
            CommandStatusKind::Aborted => "aborted",
            CommandStatusKind::Error => "error",
            CommandStatusKind::Cancelled => "cancelled",
        };
        f.write_str(s)
    }
}

/// How a setup/teardown *phase* ended, on the wire.
///
/// A sixth free-form status string, produced beside `CommandStatusKind` and
/// never matched anywhere but a log line's equality check against `"failed"`
/// (WI 0114 F-48).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PhaseStatusKind {
    Running,
    Succeeded,
    Failed,
    Paused,
    /// The main phase finished but teardown did not.
    TeardownFailed,
    /// Stopped by an external cancel request, or a step skipped because of one.
    Cancelled,
}

impl std::fmt::Display for PhaseStatusKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            PhaseStatusKind::Running => "running",
            PhaseStatusKind::Succeeded => "succeeded",
            PhaseStatusKind::Failed => "failed",
            PhaseStatusKind::Paused => "paused",
            PhaseStatusKind::TeardownFailed => "teardown_failed",
            PhaseStatusKind::Cancelled => "cancelled",
        };
        f.write_str(s)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum EventPayload {
    WorkflowPrerequisites {
        phase: PrerequisitePhase,
        report: WorkflowPrerequisiteReport,
    },
    StdoutLine(String),
    StderrLine(String),
    StatusMessage {
        phase: String,
        message: String,
    },
    WorkflowStepTransition {
        step_name: String,
        step_index: usize,
        from_status: StepStatusKind,
        to_status: StepStatusKind,
    },
    WorkflowPhaseTransition {
        phase: String,
        step_desc: String,
        status: PhaseStatusKind,
    },
    /// One container in a parallel group (WI-0096) has started running.
    WorkflowParallelStepLaunched {
        step_name: String,
        step_index: usize,
        agent: String,
        model: Option<String>,
    },
    /// One container in a parallel group (WI-0096) has exited.
    WorkflowParallelStepExited {
        step_name: String,
        step_index: usize,
        exit_code: i32,
    },
    /// A parallel group (WI-0096) has fully drained; all its steps completed.
    WorkflowParallelGroupFinished,
    /// A structured-output agent did something (WI 0137 §4). `summary` is one
    /// line of at most [`STEP_ACTIVITY_SUMMARY_MAX_CHARS`] characters. The
    /// step it belongs to is the envelope's `source`.
    StepActivity {
        kind: StepActivityKind,
        summary: String,
    },
    /// How the command ended. Emitted exactly once per command by the queue
    /// worker, after the terminal row is written and before `Done`
    /// (WI 0137 §2). `result` is the same object `GET
    /// /v1/commands/{id}/status` returns; `status`, `exit_code` and `error`
    /// are kept for clients that predate it.
    CommandStatus {
        status: CommandStatusKind,
        exit_code: Option<i32>,
        error: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<Box<CommandResult>>,
    },
    Done,
}

impl EventPayload {
    pub fn sse_event_type(&self) -> &'static str {
        match self {
            EventPayload::WorkflowPrerequisites { .. } => "workflow_prerequisites",
            EventPayload::StdoutLine(_) => "stdout_line",
            EventPayload::StderrLine(_) => "stderr_line",
            EventPayload::StatusMessage { .. } => "status_message",
            EventPayload::WorkflowStepTransition { .. } => "workflow_step_transition",
            EventPayload::WorkflowPhaseTransition { .. } => "workflow_phase_transition",
            EventPayload::WorkflowParallelStepLaunched { .. } => "workflow_parallel_step_launched",
            EventPayload::WorkflowParallelStepExited { .. } => "workflow_parallel_step_exited",
            EventPayload::WorkflowParallelGroupFinished => "workflow_parallel_group_finished",
            EventPayload::StepActivity { .. } => "step_activity",
            EventPayload::CommandStatus { .. } => "command_status",
            EventPayload::Done => "done",
        }
    }

    pub fn to_plain_text(&self) -> Option<String> {
        match self {
            EventPayload::WorkflowPrerequisites { report, .. } => {
                Some(format!("[prerequisites] {}", report.summary_line()))
            }
            EventPayload::StdoutLine(line) => Some(line.clone()),
            EventPayload::StderrLine(line) => Some(line.clone()),
            EventPayload::StatusMessage { phase, message } => Some(format!("[{phase}] {message}")),
            EventPayload::WorkflowStepTransition {
                step_name,
                step_index,
                to_status,
                ..
            } => Some(format!("[step {step_index}] {step_name} → {to_status}")),
            EventPayload::WorkflowPhaseTransition { phase, .. } if phase != "main" => None,
            EventPayload::WorkflowPhaseTransition {
                phase,
                step_desc,
                status,
            } => Some(format!("[{phase}] {step_desc} → {status}")),
            EventPayload::WorkflowParallelStepLaunched {
                step_name,
                agent,
                model,
                ..
            } => Some(format!(
                "[parallel] {step_name} launched ({agent}{})",
                model
                    .as_deref()
                    .map(|m| format!("::{m}"))
                    .unwrap_or_default()
            )),
            EventPayload::WorkflowParallelStepExited {
                step_name,
                exit_code,
                ..
            } => Some(format!("[parallel] {step_name} exited (exit {exit_code})")),
            EventPayload::WorkflowParallelGroupFinished => {
                Some("[parallel] group finished".to_string())
            }
            // The activity feed is additive: `output.log` stays the agent's
            // output lines only, exactly as before WI 0137.
            EventPayload::StepActivity { .. } => None,
            EventPayload::CommandStatus {
                status, exit_code, ..
            } => {
                if let Some(code) = exit_code {
                    Some(format!("[status] {status} (exit code {code})"))
                } else {
                    Some(format!("[status] {status}"))
                }
            }
            EventPayload::Done => None,
        }
    }
}

#[cfg(test)]
mod phase_log_tests {
    use super::*;
    #[test]
    fn additive_phase_events_remain_structured_without_duplicate_text() {
        let phase = EventPayload::WorkflowPhaseTransition {
            phase: "setup".into(),
            step_desc: "prepare".into(),
            status: PhaseStatusKind::Running,
        };
        assert_eq!(phase.to_plain_text(), None);
        assert_eq!(phase.sse_event_type(), "workflow_phase_transition");
        assert_eq!(
            EventPayload::StatusMessage {
                phase: "setup".into(),
                message: "started: prepare".into()
            }
            .to_plain_text(),
            Some("[setup] started: prepare".into())
        );
    }
}

#[cfg(test)]
mod cursor_tests {
    use super::*;

    fn event(attempt: u32, sequence: u64, payload: EventPayload) -> ExecutionEvent {
        ExecutionEvent {
            timestamp: Utc::now(),
            attempt,
            sequence,
            source: None,
            payload,
        }
    }

    fn line(n: &str) -> EventPayload {
        EventPayload::StdoutLine(n.into())
    }

    #[test]
    fn source_is_omitted_when_absent_and_round_trips_when_present() {
        let bare = serde_json::to_value(event(1, 0, line("x"))).unwrap();
        assert!(bare.get("source").is_none());

        let mut attributed = event(1, 1, line("y"));
        attributed.source = Some(
            EventSource::step(EventSourcePhase::Main, "tests", 2)
                .with_container("awman-1")
                .with_step_attempt(1),
        );
        let json = serde_json::to_value(&attributed).unwrap();
        assert_eq!(json["source"]["phase"], "main");
        assert_eq!(json["source"]["step_name"], "tests");
        assert_eq!(json["source"]["step_index"], 2);
        assert_eq!(json["source"]["container"], "awman-1");
        assert_eq!(json["source"]["step_attempt"], 1);
        let back: ExecutionEvent = serde_json::from_value(json).unwrap();
        assert_eq!(back.source, attributed.source);

        let leader = serde_json::to_value(EventSource::phase(EventSourcePhase::Leader)).unwrap();
        assert_eq!(leader, serde_json::json!({"phase": "leader"}));
    }

    #[test]
    fn step_filter_keeps_matching_steps_and_terminal_events() {
        let mut tests = event(1, 0, line("a"));
        tests.source = Some(EventSource::step(EventSourcePhase::Main, "tests", 0));
        let mut docs = event(1, 1, line("b"));
        docs.source = Some(EventSource::step(EventSourcePhase::Main, "docs", 1));
        let unattributed = event(1, 2, line("c"));
        let status = event(
            1,
            3,
            EventPayload::CommandStatus {
                status: CommandStatusKind::Done,
                exit_code: Some(0),
                error: None,
                result: None,
            },
        );
        let done = event(1, 4, EventPayload::Done);
        assert!(tests.matches_step_filter("tests"));
        assert!(!docs.matches_step_filter("tests"));
        assert!(!unattributed.matches_step_filter("tests"));
        assert!(status.matches_step_filter("tests"));
        assert!(done.matches_step_filter("tests"));
    }

    #[test]
    fn step_activity_is_typed_and_absent_from_plain_text() {
        let payload = EventPayload::StepActivity {
            kind: StepActivityKind::ToolUse,
            summary: "Read src/main.rs".into(),
        };
        assert_eq!(payload.sse_event_type(), "step_activity");
        assert_eq!(payload.to_plain_text(), None);
        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(json["type"], "StepActivity");
        assert_eq!(json["data"]["kind"], "tool_use");
    }

    #[test]
    fn command_status_result_is_omitted_when_absent_and_legacy_lines_parse() {
        let without = serde_json::to_value(EventPayload::CommandStatus {
            status: CommandStatusKind::Error,
            exit_code: Some(1),
            error: Some("boom".into()),
            result: None,
        })
        .unwrap();
        assert!(without["data"].get("result").is_none());
        let legacy =
            r#"{"type":"CommandStatus","data":{"status":"done","exit_code":0,"error":null}}"#;
        let parsed: EventPayload = serde_json::from_str(legacy).unwrap();
        assert!(matches!(
            parsed,
            EventPayload::CommandStatus { result: None, .. }
        ));
        let with = EventPayload::CommandStatus {
            status: CommandStatusKind::Done,
            exit_code: Some(0),
            error: None,
            result: Some(Box::new(CommandResult {
                exit_code: Some(0),
                ..CommandResult::default()
            })),
        };
        let json = serde_json::to_value(&with).unwrap();
        assert_eq!(json["data"]["result"]["exit_code"], 0);
    }

    #[test]
    fn cursor_parses_attempt_and_sequence() {
        assert_eq!(EventCursor::parse("1:0"), Some(EventCursor::new(1, 0)));
        assert_eq!(
            EventCursor::parse("12:345"),
            Some(EventCursor::new(12, 345))
        );
        assert_eq!(EventCursor::parse("007:0009"), Some(EventCursor::new(7, 9)));
        // Attempt 0 is syntactically valid; `ReplayFrom` treats it as unknown.
        assert_eq!(EventCursor::parse("0:5"), Some(EventCursor::new(0, 5)));
        assert_eq!(
            EventCursor::parse("4294967295:18446744073709551615"),
            Some(EventCursor::new(u32::MAX, u64::MAX))
        );
    }

    #[test]
    fn cursor_rejects_malformed_input() {
        for bad in [
            "",
            ":",
            "1",
            "1:",
            ":1",
            "1:2:3",
            "a:b",
            "1:b",
            "a:1",
            "-1:2",
            "1:-2",
            "+1:2",
            "1:+2",
            " 1:2",
            "1:2 ",
            "1: 2",
            "1 :2",
            "1;2",
            "1.0:2",
            "0x1:2",
            "١:٢",
            "1:2\n",
            "4294967296:0",
            "1:18446744073709551616",
        ] {
            assert_eq!(EventCursor::parse(bad), None, "{bad:?} must be rejected");
        }
    }

    #[test]
    fn cursor_display_round_trips_through_parse() {
        for cursor in [
            EventCursor::new(1, 0),
            EventCursor::new(3, 41),
            EventCursor::new(u32::MAX, u64::MAX),
        ] {
            assert_eq!(EventCursor::parse(&cursor.to_string()), Some(cursor));
        }
        assert_eq!(EventCursor::new(2, 7).to_string(), "2:7");
    }

    #[test]
    fn cursor_orders_by_attempt_then_sequence() {
        assert!(EventCursor::new(1, 999) < EventCursor::new(2, 0));
        assert!(EventCursor::new(2, 3) < EventCursor::new(2, 4));
        // Numeric, not lexicographic: 10 > 9.
        assert!(EventCursor::new(1, 9) < EventCursor::new(1, 10));
        assert!(EventCursor::new(9, 0) < EventCursor::new(10, 0));
    }

    #[test]
    fn event_cursor_reflects_attempt_and_sequence() {
        assert_eq!(event(3, 8, EventPayload::Done).cursor().to_string(), "3:8");
    }

    #[test]
    fn serialized_event_includes_attempt() {
        let json = serde_json::to_value(event(2, 5, line("x"))).unwrap();
        assert_eq!(json["attempt"], 2);
        assert_eq!(json["sequence"], 5);
    }

    #[test]
    fn legacy_line_without_attempt_deserializes_as_first_attempt() {
        let legacy = r#"{"timestamp":"2026-01-01T00:00:00Z","sequence":4,"payload":{"type":"StdoutLine","data":"hi"}}"#;
        let parsed: ExecutionEvent = serde_json::from_str(legacy).unwrap();
        assert_eq!(parsed.attempt, FIRST_ATTEMPT);
        assert_eq!(parsed.attempt, 1);
        assert_eq!(parsed.cursor(), EventCursor::new(1, 4));
    }

    #[test]
    fn parse_event_log_reads_mixed_legacy_and_new_lines_and_skips_damage() {
        let legacy = r#"{"timestamp":"2026-01-01T00:00:00Z","sequence":0,"payload":{"type":"StdoutLine","data":"old"}}"#;
        let new = serde_json::to_string(&event(2, 0, line("new"))).unwrap();
        let raw = format!("{legacy}\n\n{{\"torn\":\n{new}\nnot json\n");
        let events = parse_event_log(&raw);
        let cursors: Vec<_> = events.iter().map(|e| e.cursor().to_string()).collect();
        assert_eq!(cursors, ["1:0", "2:0"]);
    }

    #[test]
    fn kept_attempts_lists_each_attempt_once() {
        let events = vec![
            event(1, 0, line("a")),
            event(1, 1, line("b")),
            event(3, 0, line("c")),
        ];
        assert_eq!(
            kept_attempts(&events).into_iter().collect::<Vec<_>>(),
            [1, 3]
        );
        assert!(kept_attempts(&[]).is_empty());
    }

    fn two_attempts() -> Vec<ExecutionEvent> {
        vec![
            event(1, 0, line("a1")),
            event(1, 1, line("a2")),
            event(2, 0, line("b1")),
            event(2, 1, line("b2")),
        ]
    }

    #[test]
    fn resolve_without_a_cursor_replays_everything() {
        let events = two_attempts();
        let from = ReplayFrom::resolve(&events, 2, None);
        assert_eq!(from, ReplayFrom::Beginning);
        assert!(events.iter().all(|e| from.includes(e)));
    }

    #[test]
    fn resolve_with_a_kept_attempt_resumes_strictly_after_the_cursor() {
        let events = two_attempts();
        for (cursor, expected) in [
            (EventCursor::new(1, 0), vec!["1:1", "2:0", "2:1"]),
            (EventCursor::new(1, 1), vec!["2:0", "2:1"]),
            (EventCursor::new(2, 0), vec!["2:1"]),
            (EventCursor::new(2, 1), vec![]),
            // Ahead of the head of a kept attempt: nothing replays, no fallback.
            (EventCursor::new(2, 99), vec![]),
        ] {
            let from = ReplayFrom::resolve(&events, 2, Some(cursor));
            assert_eq!(from, ReplayFrom::After(cursor));
            let got: Vec<_> = events
                .iter()
                .filter(|e| from.includes(e))
                .map(|e| e.cursor().to_string())
                .collect();
            assert_eq!(got, expected, "after {cursor}");
        }
    }

    #[test]
    fn resolve_with_an_attempt_that_is_not_kept_restarts_the_current_attempt() {
        let events = vec![event(2, 0, line("b1")), event(2, 1, line("b2"))];
        for cursor in [
            EventCursor::new(1, 5),  // purged / not in the log
            EventCursor::new(0, 0),  // attempt 0 never exists
            EventCursor::new(3, 0),  // newer than the row's attempt
            EventCursor::new(40, 1), // far newer
        ] {
            let from = ReplayFrom::resolve(&events, 2, Some(cursor));
            assert_eq!(from, ReplayFrom::StartOfAttempt(2), "{cursor}");
            assert!(events.iter().all(|e| from.includes(e)));
            assert!(!from.includes(&event(1, 0, line("older"))));
        }
    }

    #[test]
    fn resolve_with_an_empty_log_restarts_the_current_attempt() {
        let from = ReplayFrom::resolve(&[], 3, Some(EventCursor::new(1, 0)));
        assert_eq!(from, ReplayFrom::StartOfAttempt(3));
    }
}

#[cfg(test)]
mod prerequisite_event_tests {
    use super::*;
    use crate::data::workflow_prerequisites::*;

    fn payload() -> EventPayload {
        EventPayload::WorkflowPrerequisites {
            phase: PrerequisitePhase::BeforeTeardown,
            report: WorkflowPrerequisiteReport {
                phase: PrerequisitePhase::BeforeTeardown,
                origin: PrerequisiteOrigin::AssumedAll,
                checks: vec![PrerequisiteCheck {
                    prerequisite: WorkflowPrerequisite::GitHubToken {
                        host: "github.com".into(),
                    },
                    status: PrerequisiteStatus::Missing,
                    explanation: "no granted GitHub token is available".into(),
                    needed_by: vec![],
                    remediations: vec![Remediation::PasteToken, Remediation::Continue],
                }],
                advisories: vec![],
                notes: vec![],
            },
        }
    }

    #[test]
    fn workflow_prerequisites_sse_type_and_plain_text() {
        let event = payload();
        assert_eq!(event.sse_event_type(), "workflow_prerequisites");
        assert_eq!(
            event.to_plain_text().unwrap(),
            "[prerequisites] 1 of 1 workflow prerequisites need your attention"
        );
    }

    #[test]
    fn payload_json_is_tagged_and_secret_free() {
        let json = serde_json::to_value(payload()).unwrap();
        assert_eq!(json["type"], "WorkflowPrerequisites");
        assert_eq!(json["data"]["phase"], "before_teardown");
        assert_eq!(json["data"]["report"]["checks"][0]["status"], "missing");
        assert_eq!(
            json["data"]["report"]["checks"][0]["remediations"][0]["type"],
            "paste_token"
        );
        let text = json.to_string();
        // Remediations name choices; no field exists that could carry a value.
        assert!(!text.contains("ghp_"));
        let back: EventPayload = serde_json::from_value(json).unwrap();
        assert_eq!(back.sse_event_type(), "workflow_prerequisites");
    }
}
