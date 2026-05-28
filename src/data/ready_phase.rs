use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReadyPhase {
    Preflight,
    AwaitingDockerfileDecision,
    CreatingDockerfile,
    BuildingBaseImage,
    BuildingAgentImage,
    CheckingNonDefaultAgents,
    CheckingLocalAgent,
    RunningAudit,
    RebuildingAfterAudit,
    Complete,
    Failed(ReadyFailure),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadyFailure {
    pub phase: String,
    pub message: String,
}
