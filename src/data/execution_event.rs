//! `ExecutionEvent` — typed events emitted during command/workflow execution.
//!
//! These are Layer 0 data types: serializable, no runtime behavior. Used by
//! the API frontend's `EventBus` (Layer 3) for SSE streaming and logfile
//! persistence. The engine layer (Layer 1) has no knowledge of these types.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionEvent {
    pub timestamp: DateTime<Utc>,
    pub sequence: u64,
    pub payload: EventPayload,
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
}

impl std::fmt::Display for CommandStatusKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            CommandStatusKind::Done => "done",
            CommandStatusKind::Paused => "paused",
            CommandStatusKind::Aborted => "aborted",
            CommandStatusKind::Error => "error",
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
}

impl std::fmt::Display for PhaseStatusKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            PhaseStatusKind::Running => "running",
            PhaseStatusKind::Succeeded => "succeeded",
            PhaseStatusKind::Failed => "failed",
            PhaseStatusKind::Paused => "paused",
            PhaseStatusKind::TeardownFailed => "teardown_failed",
        };
        f.write_str(s)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum EventPayload {
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
    CommandStatus {
        status: CommandStatusKind,
        exit_code: Option<i32>,
        error: Option<String>,
    },
    Done,
}

impl EventPayload {
    pub fn sse_event_type(&self) -> &'static str {
        match self {
            EventPayload::StdoutLine(_) => "stdout_line",
            EventPayload::StderrLine(_) => "stderr_line",
            EventPayload::StatusMessage { .. } => "status_message",
            EventPayload::WorkflowStepTransition { .. } => "workflow_step_transition",
            EventPayload::WorkflowPhaseTransition { .. } => "workflow_phase_transition",
            EventPayload::WorkflowParallelStepLaunched { .. } => "workflow_parallel_step_launched",
            EventPayload::WorkflowParallelStepExited { .. } => "workflow_parallel_step_exited",
            EventPayload::WorkflowParallelGroupFinished => "workflow_parallel_group_finished",
            EventPayload::CommandStatus { .. } => "command_status",
            EventPayload::Done => "done",
        }
    }

    pub fn to_plain_text(&self) -> Option<String> {
        match self {
            EventPayload::StdoutLine(line) => Some(line.clone()),
            EventPayload::StderrLine(line) => Some(line.clone()),
            EventPayload::StatusMessage { phase, message } => Some(format!("[{phase}] {message}")),
            EventPayload::WorkflowStepTransition {
                step_name,
                step_index,
                to_status,
                ..
            } => Some(format!("[step {step_index}] {step_name} → {to_status}")),
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
