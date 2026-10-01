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

/// The stage line for a phase. Stable wire text: it is persisted in setup
/// snapshots and served over HTTP, so every frontend reads the same words.
impl std::fmt::Display for ReadyPhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Preflight => f.write_str("Running preflight checks..."),
            Self::AwaitingDockerfileDecision => f.write_str("Checking Dockerfile..."),
            Self::CreatingDockerfile => f.write_str("Creating Dockerfile.dev..."),
            Self::BuildingBaseImage => f.write_str("Building base image..."),
            Self::BuildingAgentImage => f.write_str("Building agent image..."),
            Self::ImportingBaseImage => f.write_str("Checking base image..."),
            Self::ImportingAgentImage => f.write_str("Importing agent image..."),
            Self::CheckingNonDefaultAgents => f.write_str("Checking non-default agent images..."),
            Self::CheckingLocalAgent => f.write_str("Checking local agent..."),
            Self::RunningAudit => f.write_str("Running audit..."),
            Self::RebuildingAfterAudit => f.write_str("Rebuilding after audit..."),
            Self::Complete => f.write_str("Ready checks complete"),
            Self::Failed(failure) => write!(f, "Failed: {}", failure.message),
        }
    }
}
