//! Shared automation records. No admission policy or provider observations live here.
use crate::data::{
    agent_policy::{NativePlanKind, NativeStartMode},
    automation::*,
    ids::{CommandId, ExecutionId, TurnId},
    settings_profile::{NativeSessionId, SettingsProfileName},
    workspace::WorkspaceAccess,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const MAX_SAFE_COUNTER: u64 = (1u64 << 53) - 1;
pub const MAX_PROMPT_BYTES: usize = 64 * 1024;
pub fn next_counter(value: u64) -> Result<u64, &'static str> {
    value
        .checked_add(1)
        .filter(|v| *v <= MAX_SAFE_COUNTER)
        .ok_or("agent_protocol_error")
}
pub fn safe_counter<'de, D: serde::Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
    let n = u64::deserialize(d)?;
    if n > MAX_SAFE_COUNTER {
        return Err(serde::de::Error::custom("unsafe JSON counter"));
    }
    Ok(n)
}
pub fn required_nullable<'de, D: serde::Deserializer<'de>, T: Deserialize<'de>>(
    d: D,
) -> Result<Option<T>, D::Error> {
    Option::<T>::deserialize(d)
}
pub fn native_turn_id<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    let value = Option::<String>::deserialize(d)?;
    if let Some(id) = &value {
        NativeSessionId::parse(id).map_err(serde::de::Error::custom)?;
    }
    Ok(value)
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutomationSnapshot {
    pub execution_id: ExecutionId,
    pub provider: String,
    #[serde(deserialize_with = "required_nullable")]
    pub provider_version: Option<String>,
    #[serde(deserialize_with = "required_nullable")]
    pub native_session_id: Option<NativeSessionId>,
    pub automation_mode: AutomationMode,
    pub minimum_status_support: MinimumStatusSupport,
    pub state: AutomationState,
    #[serde(deserialize_with = "safe_counter")]
    pub revision: u64,
    #[serde(deserialize_with = "required_nullable")]
    pub active_turn_id: Option<TurnId>,
    pub native_start_mode: NativeStartMode,
    pub workspace_access: WorkspaceAccess,
    pub status_support: StatusSupport,
    pub status_source: StatusSource,
    #[serde(deserialize_with = "required_nullable")]
    pub status_reason: Option<String>,
    pub capabilities: AutomationCapabilities,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutomationFailure {
    pub code: String,
    pub message: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnResult {
    pub status: TurnStatus,
    pub status_support: StatusSupport,
    pub readiness_source: ReadinessSource,
    pub completion_source: CompletionSource,
    pub completion_verified: bool,
    #[serde(deserialize_with = "native_turn_id")]
    pub native_turn_id: Option<String>,
    #[serde(deserialize_with = "required_nullable")]
    pub final_message: Option<String>,
    pub output_status: OutputStatus,
    #[serde(deserialize_with = "required_nullable")]
    pub failure: Option<AutomationFailure>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnRecord {
    pub request_id: Uuid,
    pub turn_id: TurnId,
    pub execution_id: ExecutionId,
    pub command_id: CommandId,
    pub attempt: u32,
    #[serde(deserialize_with = "safe_counter")]
    pub ordinal: u64,
    #[serde(deserialize_with = "required_nullable")]
    pub native_session_id: Option<NativeSessionId>,
    #[serde(deserialize_with = "native_turn_id")]
    pub native_turn_id: Option<String>,
    #[serde(with = "crate::data::wire::timestamp")]
    pub accepted_at: DateTime<Utc>,
    #[serde(with = "crate::data::wire::opt_timestamp")]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(with = "crate::data::wire::opt_timestamp")]
    pub completed_at: Option<DateTime<Utc>>,
    #[serde(deserialize_with = "safe_counter")]
    pub accepted_revision: u64,
    pub status_support: StatusSupport,
    pub status_source: StatusSource,
    #[serde(deserialize_with = "required_nullable")]
    pub result: Option<TurnResult>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandAutomationResult {
    pub execution_id: ExecutionId,
    #[serde(deserialize_with = "required_nullable")]
    pub native_session_id: Option<NativeSessionId>,
    #[serde(deserialize_with = "required_nullable")]
    pub last_turn_id: Option<TurnId>,
    pub native_start_mode: NativeStartMode,
    pub workspace_access: WorkspaceAccess,
    pub status_support: StatusSupport,
    pub completion_verified: bool,
    pub output_status: OutputStatus,
    pub cleanup_status: CleanupStatus,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubmitTurnRequest {
    pub request_id: Uuid,
    pub execution_id: ExecutionId,
    #[serde(deserialize_with = "safe_counter")]
    pub expected_revision: u64,
    pub prompt: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloseExecutionRequest {
    pub request_id: Uuid,
    pub execution_id: ExecutionId,
    #[serde(deserialize_with = "safe_counter")]
    pub expected_revision: u64,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutomationCapabilities {
    pub agent: String,
    pub runtime: String,
    #[serde(deserialize_with = "required_nullable")]
    pub provider_version: Option<String>,
    #[serde(deserialize_with = "required_nullable")]
    pub image_digest: Option<String>,
    #[serde(deserialize_with = "required_nullable")]
    pub minimum_version: Option<String>,
    #[serde(deserialize_with = "required_nullable")]
    pub version_warning: Option<String>,
    pub evidence: CapabilityEvidence,
    pub native_plan: NativePlanCapability,
    pub headless_turns: SupportCapability,
    pub terminal_turns: TerminalTurnsCapability,
    pub native_resume: NativeResumeCapability,
    pub settings_profiles: bool,
    pub credential_updates: crate::data::credential_update::CredentialUpdateCapabilities,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativePlanCapability {
    #[serde(deserialize_with = "required_nullable")]
    pub interactive: Option<bool>,
    #[serde(deserialize_with = "required_nullable")]
    pub headless: Option<bool>,
    #[serde(deserialize_with = "required_nullable")]
    pub acp: Option<bool>,
    #[serde(deserialize_with = "required_nullable")]
    pub kind: Option<NativePlanKind>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SupportCapability {
    #[serde(deserialize_with = "required_nullable")]
    pub supported: Option<bool>,
    #[serde(deserialize_with = "required_nullable")]
    pub reason: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalTurnsCapability {
    #[serde(deserialize_with = "required_nullable")]
    pub supported: Option<bool>,
    #[serde(deserialize_with = "required_nullable")]
    pub status_support: Option<StatusSupport>,
    pub sources: Vec<ReadinessSource>,
    #[serde(deserialize_with = "required_nullable")]
    pub reason: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeResumeCapability {
    #[serde(deserialize_with = "required_nullable")]
    pub interactive: Option<bool>,
    #[serde(deserialize_with = "required_nullable")]
    pub headless: Option<bool>,
    #[serde(deserialize_with = "required_nullable")]
    pub reason: Option<String>,
}

/// Immutable profile identity only. The profile manifest owns resources and cleanup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionProfileBinding {
    pub repository_key: String,
    pub root_sha256: String,
    pub provider: String,
    pub name: SettingsProfileName,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaunchPhase {
    Reserved,
    Launching,
    Running,
    Ended,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryPhase {
    Reserved,
    WriteStarted,
    Written,
    Acknowledged,
    Terminal,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionRecord {
    pub execution_id: ExecutionId,
    pub command_id: CommandId,
    pub attempt: u32,
    pub owner_key: String,
    pub execution_ordinal: u64,
    #[serde(deserialize_with = "required_nullable")]
    pub terminal_id: Option<crate::data::ids::TerminalId>,
    pub provider: String,
    pub runtime: String,
    #[serde(deserialize_with = "required_nullable")]
    pub provider_version: Option<String>,
    #[serde(deserialize_with = "required_nullable")]
    pub image_digest: Option<String>,
    pub profile_binding: ExecutionProfileBinding,
    #[serde(deserialize_with = "required_nullable")]
    pub requested_native_session_id: Option<NativeSessionId>,
    #[serde(deserialize_with = "required_nullable")]
    pub native_session_id: Option<NativeSessionId>,
    #[serde(deserialize_with = "required_nullable")]
    pub automation_mode: Option<AutomationMode>,
    #[serde(deserialize_with = "required_nullable")]
    pub minimum_status_support: Option<MinimumStatusSupport>,
    pub native_start_mode: NativeStartMode,
    pub workspace_access: WorkspaceAccess,
    pub timeouts: crate::data::config::agents::AutomationTimeouts,
    #[serde(deserialize_with = "required_nullable")]
    pub snapshot: Option<AutomationSnapshot>,
    pub revision: u64,
    #[serde(deserialize_with = "required_nullable")]
    pub active_turn_id: Option<TurnId>,
    pub launch_phase: LaunchPhase,
    #[serde(with = "crate::data::wire::opt_timestamp")]
    pub process_started_at: Option<DateTime<Utc>>,
    #[serde(with = "crate::data::wire::timestamp")]
    pub created_at: DateTime<Utc>,
    #[serde(with = "crate::data::wire::opt_timestamp")]
    pub ended_at: Option<DateTime<Utc>>,
    #[serde(deserialize_with = "required_nullable")]
    pub end_failure: Option<AutomationFailure>,
    #[serde(deserialize_with = "required_nullable")]
    pub final_assurance: Option<CommandAutomationResult>,
    #[serde(deserialize_with = "required_nullable")]
    pub pending_events: Option<Vec<crate::data::execution_event::ExecutionEvent>>,
    #[serde(deserialize_with = "required_nullable")]
    pub pending_event_cursor: Option<crate::data::execution_event::EventCursor>,
}
