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
    #[serde(alias = "emitted_at")]
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
    /// Shared SSE/WSP projection. Stored logs retain their legacy shape.
    pub fn public_value(&self) -> serde_json::Value {
        let mut value = serde_json::to_value(self).expect("execution events serialize");
        let object = value.as_object_mut().expect("event object");
        object.remove("timestamp");
        object.insert(
            "emitted_at".into(),
            crate::data::wire::format_timestamp(self.timestamp).into(),
        );
        object.get_mut("payload").expect("payload")["type"] = self.payload.sse_event_type().into();
        value
    }

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
    /// Stopped by an external cancel request (`POST /v1/worker/commands/{id}/cancel`).
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
    #[serde(alias = "workflow_prerequisites")]
    WorkflowPrerequisites {
        phase: PrerequisitePhase,
        report: WorkflowPrerequisiteReport,
    },
    #[serde(alias = "stdout_line")]
    StdoutLine(String),
    #[serde(alias = "stderr_line")]
    StderrLine(String),
    #[serde(alias = "status_message")]
    StatusMessage { phase: String, message: String },
    #[serde(alias = "workflow_step_transition")]
    WorkflowStepTransition {
        step_name: String,
        step_index: usize,
        from_status: StepStatusKind,
        to_status: StepStatusKind,
    },
    #[serde(alias = "workflow_phase_transition")]
    WorkflowPhaseTransition {
        phase: String,
        step_desc: String,
        status: PhaseStatusKind,
    },
    /// One container in a parallel group (WI-0096) has started running.
    #[serde(alias = "workflow_parallel_step_launched")]
    WorkflowParallelStepLaunched {
        step_name: String,
        step_index: usize,
        agent: String,
        model: Option<String>,
    },
    /// One container in a parallel group (WI-0096) has exited.
    #[serde(alias = "workflow_parallel_step_exited")]
    WorkflowParallelStepExited {
        step_name: String,
        step_index: usize,
        exit_code: i32,
    },
    /// A parallel group (WI-0096) has fully drained; all its steps completed.
    #[serde(alias = "workflow_parallel_group_finished")]
    WorkflowParallelGroupFinished,
    /// A structured-output agent did something (WI 0137 §4). `summary` is one
    /// line of at most [`STEP_ACTIVITY_SUMMARY_MAX_CHARS`] characters. The
    /// step it belongs to is the envelope's `source`.
    #[serde(alias = "step_activity")]
    StepActivity {
        kind: StepActivityKind,
        summary: String,
    },
    /// How the command ended. Emitted exactly once per command by the queue
    /// worker, after the terminal row is written and before `Done`
    /// (WI 0137 §2). `result` is the same object `GET
    /// /v1/worker/commands/{id}` returns; `status`, `exit_code` and `error`
    /// are kept for clients that predate it.
    #[serde(alias = "command_status")]
    CommandStatus {
        status: CommandStatusKind,
        exit_code: Option<i32>,
        error: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<Box<CommandResult>>,
    },
    /// The agent's effective OSC 7501 status changed (WI 0141). Values use
    /// the spec spellings; the step it belongs to is the envelope's `source`.
    #[serde(alias = "program_status")]
    ProgramStatus {
        state: crate::data::program_status::ProgramState,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        kind: Option<crate::data::program_status::BlockedKind>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        progress: Option<u8>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        msg: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        app: Option<String>,
    },
    /// A PTY-bridged container registered a hub terminal (WI 0141). Step,
    /// phase, container and attempt come from the envelope's `source`.
    #[serde(alias = "terminal_opened")]
    TerminalOpened {
        terminal_id: crate::data::ids::TerminalId,
        agent: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        cols: u16,
        rows: u16,
    },
    /// The terminal's PTY exited (WI 0141).
    #[serde(alias = "terminal_closed")]
    TerminalClosed {
        terminal_id: crate::data::ids::TerminalId,
        exit_code: i32,
    },
    /// The input grant on a terminal was taken or released (WI 0141).
    /// `holder` is the client's `x-awman-actor`, or `local`. Keystrokes are
    /// never logged.
    #[serde(alias = "terminal_control")]
    TerminalControl {
        terminal_id: crate::data::ids::TerminalId,
        holder: String,
        #[serde(
            default,
            skip_serializing_if = "crate::data::annotations::Annotations::is_empty"
        )]
        annotations: crate::data::terminal::Annotations,
    },
    /// A local attach subscription ended (WI 0141); published by the engine
    /// to host presenters (0142, 0157).
    #[serde(alias = "terminal_viewer_detached")]
    TerminalViewerDetached {
        terminal_id: crate::data::ids::TerminalId,
        reason: crate::data::terminal::DetachReason,
        #[serde(
            default,
            skip_serializing_if = "crate::data::annotations::Annotations::is_empty"
        )]
        annotations: crate::data::terminal::Annotations,
    },
    /// The complete `GET /v1/worker/commands/{id}/workflow` snapshot,
    /// published every time `workflow.state.json` is persisted (WI 0143),
    /// including the initial and final snapshots. Same ordered log as push
    /// SSE and WSP event delivery.
    #[serde(rename = "workflow_state")]
    #[serde(alias = "workflow_state")]
    WorkflowState(serde_json::Value),
    #[serde(alias = "done")]
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
            EventPayload::ProgramStatus { .. } => "program_status",
            EventPayload::TerminalOpened { .. } => "terminal_opened",
            EventPayload::TerminalClosed { .. } => "terminal_closed",
            EventPayload::TerminalControl { .. } => "terminal_control",
            EventPayload::TerminalViewerDetached { .. } => "terminal_viewer_detached",
            EventPayload::WorkflowState(_) => "workflow_state",
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
            // Status and terminal lifecycle are structured signals, not agent
            // output: `output.log` stays the agent's own lines (WI 0141).
            EventPayload::ProgramStatus { .. }
            | EventPayload::TerminalOpened { .. }
            | EventPayload::TerminalClosed { .. }
            | EventPayload::TerminalControl { .. }
            | EventPayload::TerminalViewerDetached { .. } => None,
            // A structured snapshot, not a log line.
            EventPayload::WorkflowState(_) => None,
            EventPayload::Done => None,
        }
    }
}
