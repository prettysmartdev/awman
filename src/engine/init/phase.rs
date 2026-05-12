//! Phase state machine for `InitEngine`.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum InitPhase {
    Preflight,
    AwaitingAspecDecision,
    CreatingAspecFolder,
    SettingUpDockerfile,
    SettingUpAgentDockerfile,
    WritingConfig,
    AwaitingAuditDecision,
    BuildingImage,
    BuildingAgentImage,
    RunningAudit,
    RebuildingAfterAudit,
    AwaitingWorkItemsDecision,
    WritingWorkItemsConfig,
    Complete,
    Failed(InitFailure),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitFailure {
    pub phase: String,
    pub message: String,
}
