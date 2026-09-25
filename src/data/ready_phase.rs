use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReadyPhase {
    Preflight,
    AwaitingDockerfileDecision,
    CreatingDockerfile,
    BuildingBaseImage,
    BuildingAgentImage,
    /// Import-acquisition runtimes (builtin): the project base image is a build
    /// input, not a runtime image, so this phase only records that it is skipped.
    ImportingBaseImage,
    /// Import-acquisition runtimes (builtin): acquire the agent image from its
    /// configured source and load it into the runtime's store.
    ImportingAgentImage,
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
