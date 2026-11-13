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

    // ── Transitions ─────────────────────────────────────────────────────────
    //
    // Every mutation a setup run makes to this state is one of the methods
    // below (WI 0114 F-41). They live here, on the Layer 0 type, rather than on
    // the API frontend's bus: the bus broadcasts, it does not decide what a
    // transition means, and a second multi-session frontend must not have to
    // re-derive the rules.

    /// Enter a new lifecycle status, leaving every other field alone.
    pub fn enter_status(&mut self, status: SessionSetupStatus) {
        self.status = status;
    }

    /// Replace the human-readable "current stage" line.
    pub fn set_stage(&mut self, message: &str) {
        self.current_stage = Some(message.to_string());
    }

    /// Record a terminal failure: `Failed`, the stage that failed, and a
    /// stage line that reads as one.
    pub fn mark_failed(&mut self, stage: &str, message: &str) {
        self.status = SessionSetupStatus::Failed;
        self.current_stage = Some(format!("Failed: {message}"));
        self.error = Some(SessionSetupError {
            stage: stage.to_string(),
            message: message.to_string(),
            code: None,
        });
    }

    /// Record successful completion and the summary the ready checks produced.
    pub fn set_ready(&mut self, summary: ReadySummary) {
        self.status = SessionSetupStatus::Ready;
        self.ready_summary = Some(summary);
        self.current_stage = Some("Setup complete".to_string());
    }

    /// Enter the ready-checks phase `phase`, setting the stage line to its
    /// display message. Returns that message so the caller can broadcast it
    /// without recomputing it.
    pub fn apply_ready_phase(&mut self, phase: &ReadyPhase) -> String {
        let message = Self::ready_phase_display(phase);
        self.status = SessionSetupStatus::RunningReady;
        self.current_ready_phase = Some(phase.clone());
        self.current_stage = Some(message.clone());
        message
    }

    /// Record a ready step's status, replacing the entry if the step has been
    /// seen before so a step never appears twice.
    pub fn apply_ready_step(&mut self, step: &str, status: StepStatus) {
        if let Some(entry) = self.ready_step_statuses.iter_mut().find(|e| e.step == step) {
            entry.status = status;
        } else {
            self.ready_step_statuses.push(ReadyStepEntry {
                step: step.to_string(),
                status,
            });
        }
    }

    /// The stage line for a ready phase.
    ///
    /// This is wire data, not frontend rendering: it is written into
    /// `current_stage`, persisted to `setup_state.json`, and served over HTTP,
    /// so every consumer of a setup snapshot reads the same words.
    pub fn ready_phase_display(phase: &ReadyPhase) -> String {
        phase.to_string()
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

#[derive(Debug, Clone, Deserialize)]
pub struct SessionSetupError {
    pub stage: String,
    pub message: String,
    /// An explicit stable code (WI 0145 `credentials_lost`). When absent the
    /// code is derived from `stage` (see [`SessionSetupError::code`]).
    #[serde(default)]
    pub code: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionSetupStatus {
    Initializing,
    CloningRepository,
    SettingUpBranch,
    /// Merging the session's `merge_refs` into its branch (WI 0133).
    MergingRefs,
    RunningReady,
    Ready,
    Failed,
}

impl SessionSetupStatus {
    pub const ALL: &'static [SessionSetupStatus] = &[
        Self::Initializing,
        Self::CloningRepository,
        Self::SettingUpBranch,
        Self::MergingRefs,
        Self::RunningReady,
        Self::Ready,
        Self::Failed,
    ];

    /// Exact, lower-case match on the wire spelling.
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|status| status.as_str() == s)
    }

    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Ready | Self::Failed)
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Initializing => "initializing",
            Self::CloningRepository => "cloning_repository",
            Self::SettingUpBranch => "setting_up_branch",
            Self::MergingRefs => "merging_refs",
            Self::RunningReady => "running_ready",
            Self::Ready => "ready",
            Self::Failed => "failed",
        }
    }
}

impl SessionSetupError {
    /// Derived from the typed stage so existing setup-state constructors stay valid.
    pub fn code(&self) -> Option<&str> {
        self.code.as_deref().or_else(|| {
            (self.stage == crate::data::fs::api_db::failure_stages::GIT_AUTH)
                .then_some(crate::data::fs::api_db::failure_codes::GIT_AUTH_FAILED)
        })
    }

    /// Recording a terminal failure with a stable code of its own.
    pub fn with_code(stage: &str, code: &str, message: &str) -> Self {
        Self {
            stage: stage.into(),
            message: message.into(),
            code: Some(code.into()),
        }
    }
}
impl Serialize for SessionSetupError {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut fields = serializer.serialize_struct(
            "SessionSetupError",
            if self.code().is_some() { 3 } else { 2 },
        )?;
        fields.serialize_field("stage", &self.stage)?;
        fields.serialize_field("message", &self.message)?;
        if let Some(code) = self.code() {
            fields.serialize_field("code", code)?;
        }
        fields.end()
    }
}

#[cfg(test)]
mod transport_tests {
    use super::*;
    #[test]
    fn auth_setup_code_survives_persistence_and_other_setup_errors_remain_compatible() {
        for (stage, code) in [("git_auth", Some("git_auth_failed")), ("clone", None)] {
            let error = SessionSetupError {
                stage: stage.into(),
                message: "safe".into(),
                code: None,
            };
            let value = serde_json::to_value(&error).unwrap();
            assert_eq!(value.get("code").and_then(|c| c.as_str()), code);
            let decoded: SessionSetupError = serde_json::from_value(value).unwrap();
            assert_eq!(decoded.code(), code);
        }
    }

    #[test]
    fn an_explicit_credentials_lost_code_wins_over_the_stage_and_survives_persistence() {
        let error = SessionSetupError::with_code(
            "git_auth",
            "credentials_lost",
            "the daemon restarted; open a new session",
        );
        let value = serde_json::to_value(&error).unwrap();
        assert_eq!(value["code"], "credentials_lost");
        let decoded: SessionSetupError = serde_json::from_value(value).unwrap();
        assert_eq!(decoded.code(), Some("credentials_lost"));
    }
}
