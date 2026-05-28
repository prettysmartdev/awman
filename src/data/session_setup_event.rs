//! Session setup event types — Layer 0 data definitions.
//!
//! Used by the API frontend's `SessionSetupBus` to track async session
//! setup progress. These are pure serializable types with no runtime behavior.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::data::ready_phase::ReadyPhase;
use crate::data::ready_summary::ReadySummary;
use crate::data::step_status::StepStatus;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSetupEvent {
    pub timestamp: DateTime<Utc>,
    pub sequence: u64,
    pub payload: SetupEventPayload,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum SetupEventPayload {
    StageChanged { stage: String, message: String },
    ReadyPhaseChanged { phase: ReadyPhase, message: String },
    ReadyStepStatus { step: String, status: StepStatus },
    SetupComplete { ready_summary: Box<ReadySummary> },
    SetupFailed { stage: String, error: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSetupState {
    pub status: SessionSetupStatus,
    pub current_stage: Option<String>,
    pub current_ready_phase: Option<ReadyPhase>,
    pub ready_step_statuses: Vec<ReadyStepEntry>,
    pub ready_summary: Option<ReadySummary>,
    pub error: Option<SessionSetupError>,
}

impl SessionSetupState {
    pub fn new() -> Self {
        Self {
            status: SessionSetupStatus::Initializing,
            current_stage: None,
            current_ready_phase: None,
            ready_step_statuses: Vec::new(),
            ready_summary: None,
            error: None,
        }
    }
}

impl Default for SessionSetupState {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadyStepEntry {
    pub step: String,
    pub status: StepStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSetupError {
    pub stage: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionSetupStatus {
    Initializing,
    CloningRepository,
    SettingUpBranch,
    RunningReady,
    Ready,
    Failed,
}

impl SessionSetupStatus {
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Ready | Self::Failed)
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Initializing => "initializing",
            Self::CloningRepository => "cloning_repository",
            Self::SettingUpBranch => "setting_up_branch",
            Self::RunningReady => "running_ready",
            Self::Ready => "ready",
            Self::Failed => "failed",
        }
    }
}
