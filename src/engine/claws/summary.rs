//! `ClawsSummary` — final report from a `ClawsEngine` run.

use serde::{Deserialize, Serialize};

use crate::engine::step_status::StepStatus;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClawsSummary {
    pub clone: StepStatus,
    pub permissions_check: StepStatus,
    pub image_build: StepStatus,
    pub audit: StepStatus,
    pub configure: StepStatus,
    pub controller: StepStatus,
}

impl Default for ClawsSummary {
    fn default() -> Self {
        Self {
            clone: StepStatus::Pending,
            permissions_check: StepStatus::Pending,
            image_build: StepStatus::Pending,
            audit: StepStatus::Pending,
            configure: StepStatus::Pending,
            controller: StepStatus::Pending,
        }
    }
}
