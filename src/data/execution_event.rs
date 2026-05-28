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
        from_status: String,
        to_status: String,
    },
    WorkflowPhaseTransition {
        phase: String,
        step_desc: String,
        status: String,
    },
    CommandStatus {
        status: String,
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
