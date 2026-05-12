//! `InitSummary` — final report from an `InitEngine` run.

use serde::{Deserialize, Serialize};

use crate::engine::step_status::StepStatus;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitSummary {
    pub config: StepStatus,
    pub aspec_folder: StepStatus,
    pub dockerfile: StepStatus,
    pub audit: StepStatus,
    pub image_build: StepStatus,
    pub agent_image_build: StepStatus,
    /// Result of rebuilding images after the audit phase modifies Dockerfile.dev.
    pub image_rebuild: StepStatus,
    pub work_items_setup: StepStatus,
}

impl Default for InitSummary {
    fn default() -> Self {
        Self {
            config: StepStatus::Pending,
            aspec_folder: StepStatus::Pending,
            dockerfile: StepStatus::Pending,
            audit: StepStatus::Pending,
            image_build: StepStatus::Pending,
            agent_image_build: StepStatus::Pending,
            image_rebuild: StepStatus::Pending,
            work_items_setup: StepStatus::Pending,
        }
    }
}
