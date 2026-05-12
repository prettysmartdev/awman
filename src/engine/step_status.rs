//! Shared `StepStatus` for Ready/Init/Agent engine summaries — Layer 1.

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
}
