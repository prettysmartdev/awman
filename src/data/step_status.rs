use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum StepStatus {
    Pending,
    Skipped,
    Running,
    Done,
    Warn(String),
    Failed(String),
}

impl StepStatus {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            StepStatus::Skipped | StepStatus::Done | StepStatus::Warn(_) | StepStatus::Failed(_)
        )
    }

    pub fn label(&self) -> String {
        match self {
            StepStatus::Pending => "pending".to_string(),
            StepStatus::Running => "running".to_string(),
            StepStatus::Done => "done".to_string(),
            StepStatus::Skipped => "skipped".to_string(),
            StepStatus::Warn(msg) if msg.is_empty() => "warn".to_string(),
            StepStatus::Warn(msg) => format!("warn: {msg}"),
            StepStatus::Failed(reason) if reason.is_empty() => "failed".to_string(),
            StepStatus::Failed(reason) => format!("failed: {reason}"),
        }
    }
}
