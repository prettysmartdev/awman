//! Phase state machine for `ClawsEngine`.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClawsPhase {
    Preflight,
    AwaitingCloneDecision,
    CloningRepo,
    CheckingPermissions,
    BuildingImage,
    AwaitingAuditDecision,
    RunningAudit,
    Configuring,
    LaunchingController,
    Complete,
    Failed(ClawsFailure),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClawsFailure {
    pub phase: String,
    pub message: String,
}
