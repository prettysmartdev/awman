//! Credential-update capability and operation vocabulary — Layer 0.
//!
//! Replacing an agent's credentials is advertised per route (credential
//! kind, delivery and model provider), never as one provider-wide yes/no,
//! and applied either live or through an owned restart. These enums are the
//! wire spellings of the `credential_updates` capability, the effective
//! per-execution selection and the rotation operation's status.
//!
//! `Option<bool>` capability fields (`live_update`, `restart_supported`)
//! keep their three meanings on the wire: `true`, `false` (evidence that the
//! route lacks it) and `null` (not yet established).

use serde::{Deserialize, Serialize};

/// The `/v1/status` capability for credential-update discovery and rotation.
pub const AGENT_CREDENTIAL_UPDATES_CAPABILITY: &str = "agent_credential_updates";

/// Events published when an execution's credentials or identity change.
pub const AGENT_CREDENTIALS_CHANGED_EVENT: &str = "agent_credentials_changed";
pub const AGENT_EXECUTION_REPLACED_EVENT: &str = "agent_execution_replaced";

/// How the effective route applies a replacement (`update_method`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialUpdateMethod {
    /// Native reload in the same execution.
    Live,
    /// An owned restart with fresh process environment/private files.
    Restart,
    /// A required mechanism is proven missing.
    Unavailable,
    /// Not yet established for this route.
    Unknown,
}

/// Who issues/refreshes the credential (`refresh_owner`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefreshOwner {
    Awman,
    Provider,
    Caller,
    None,
}

/// When a live update takes effect (`update_boundary`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdateBoundary {
    Immediate,
    NextRequest,
    Idle,
}

/// What a restart replaces (`restart_scope`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RestartScope {
    Process,
    Execution,
}

/// Whether the conversation survives a restart (`conversation_continuity`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConversationContinuity {
    NativeResume,
    NewSessionRequired,
}

/// A rotation operation's status, as its pollable record reports it.
///
/// A file rewrite or an accepted request is not `applied`: that needs the
/// provider's acknowledgement or observed consumption of the new generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialUpdateStatus {
    Pending,
    Applying,
    Restarting,
    Applied,
    Failed,
}

impl CredentialUpdateStatus {
    pub const ALL: [Self; 5] = [
        Self::Pending,
        Self::Applying,
        Self::Restarting,
        Self::Applied,
        Self::Failed,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Applying => "applying",
            Self::Restarting => "restarting",
            Self::Applied => "applied",
            Self::Failed => "failed",
        }
    }

    /// Whether the operation has finished, successfully or not.
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Applied | Self::Failed)
    }
}

/// The reason a staged next-request delivery reports while it waits.
pub const AWAITING_NEXT_REQUEST_REASON: &str = "awaiting_next_request";

/// Trusted, secret-free failure classification supplied by the controller.
pub struct CredentialUpdateFailure {
    pub code: &'static str,
    pub reason: &'static str,
    pub exposed: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wire<T: Serialize>(value: T) -> serde_json::Value {
        serde_json::to_value(value).unwrap()
    }

    #[test]
    fn wire_spellings() {
        assert_eq!(wire(CredentialUpdateMethod::Live), "live");
        assert_eq!(wire(CredentialUpdateMethod::Restart), "restart");
        assert_eq!(wire(CredentialUpdateMethod::Unavailable), "unavailable");
        assert_eq!(wire(CredentialUpdateMethod::Unknown), "unknown");
        assert_eq!(wire(RefreshOwner::Awman), "awman");
        assert_eq!(wire(RefreshOwner::Provider), "provider");
        assert_eq!(wire(RefreshOwner::Caller), "caller");
        assert_eq!(wire(RefreshOwner::None), "none");
        assert_eq!(wire(UpdateBoundary::Immediate), "immediate");
        assert_eq!(wire(UpdateBoundary::NextRequest), "next_request");
        assert_eq!(wire(UpdateBoundary::Idle), "idle");
        assert_eq!(wire(RestartScope::Process), "process");
        assert_eq!(wire(RestartScope::Execution), "execution");
        assert_eq!(wire(ConversationContinuity::NativeResume), "native_resume");
        assert_eq!(
            wire(ConversationContinuity::NewSessionRequired),
            "new_session_required"
        );
        for status in CredentialUpdateStatus::ALL {
            assert_eq!(wire(status), status.as_str());
        }
    }

    #[test]
    fn nullable_fields_keep_null_distinct_from_false() {
        assert_eq!(wire(None::<UpdateBoundary>), serde_json::Value::Null);
        assert_eq!(wire(Some(false)), false);
        assert_eq!(wire(None::<bool>), serde_json::Value::Null);
    }

    #[test]
    fn only_applied_and_failed_are_terminal() {
        for status in CredentialUpdateStatus::ALL {
            assert_eq!(
                status.is_terminal(),
                matches!(
                    status,
                    CredentialUpdateStatus::Applied | CredentialUpdateStatus::Failed
                )
            );
        }
    }
}

/// Discovery only; empty routes means no information yet.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CredentialUpdateCapabilities {
    pub routes: Vec<CredentialUpdateRoute>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CredentialUpdateRoute {
    pub credential_kind: CredentialKind,
    pub delivery: CredentialDelivery,
    #[serde(deserialize_with = "crate::data::automation_records::required_nullable")]
    pub model_provider: Option<String>,
    pub live_update: Option<bool>,
    #[serde(deserialize_with = "crate::data::automation_records::required_nullable")]
    pub restart_supported: Option<bool>,
    pub refresh_owner: RefreshOwner,
    #[serde(deserialize_with = "crate::data::automation_records::required_nullable")]
    pub update_boundary: Option<UpdateBoundary>,
    #[serde(deserialize_with = "nullable_milliseconds")]
    pub max_apply_delay: Option<u64>,
    #[serde(deserialize_with = "crate::data::automation_records::required_nullable")]
    pub restart_scope: Option<RestartScope>,
    pub conversation_continuity: Option<ConversationContinuity>,
    #[serde(deserialize_with = "crate::data::automation_records::required_nullable")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialKind {
    ApiKey,
    OauthAccessToken,
    GithubToken,
    ProviderManaged,
    Unknown,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialDelivery {
    Env,
    File,
    Helper,
    NativeStore,
    Unknown,
}
fn nullable_milliseconds<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<u64>, D::Error> {
    let value = Option::<u64>::deserialize(d)?;
    if value.is_some_and(|n| n > crate::data::automation_records::MAX_SAFE_COUNTER) {
        return Err(serde::de::Error::custom("unsafe milliseconds"));
    }
    Ok(value)
}

/// Owner-private execution projection. Slot metadata never contains credentials.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectiveCredentialUpdates {
    pub execution_id: crate::data::ids::ExecutionId,
    #[serde(deserialize_with = "crate::data::automation_records::safe_counter")]
    pub revision: u64,
    pub slots: Vec<EffectiveCredentialSlot>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GenerationState {
    Confirmed,
    Unknown,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdentityContinuity {
    Verified,
    Unverified,
    Unknown,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CredentialOperationAvailability {
    pub operation: String,
    pub replacement_secrets: bool,
    pub reread_source: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectiveCredentialSlot {
    pub route: CredentialUpdateRoute,
    pub update_method: CredentialUpdateMethod,
    pub credential_slot: String,
    pub secret_names: Vec<String>,
    #[serde(deserialize_with = "crate::data::automation_records::safe_counter")]
    pub credential_generation: u64,
    #[serde(deserialize_with = "nullable_milliseconds")]
    pub pending_generation: Option<u64>,
    pub generation_state: GenerationState,
    pub admission_while_pending: bool,
    #[serde(deserialize_with = "crate::data::automation_records::required_nullable")]
    pub available_operation: Option<CredentialOperationAvailability>,
    pub latest_update: Option<CredentialUpdateOperation>,
    pub identity_continuity: IdentityContinuity,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CredentialUpdateOperation {
    pub request_id: uuid::Uuid,
    pub command_id: crate::data::ids::CommandId,
    pub credential_slot: String,
    pub status: CredentialUpdateStatus,
    pub method: CredentialUpdateMethod,
    pub old_execution_id: crate::data::ids::ExecutionId,
    #[serde(deserialize_with = "crate::data::automation_records::required_nullable")]
    pub new_execution_id: Option<crate::data::ids::ExecutionId>,
    #[serde(deserialize_with = "crate::data::automation_records::safe_counter")]
    pub old_generation: u64,
    #[serde(deserialize_with = "crate::data::automation_records::safe_counter")]
    pub new_generation: u64,
    #[serde(deserialize_with = "nullable_milliseconds")]
    pub pending_generation: Option<u64>,
    #[serde(deserialize_with = "crate::data::automation_records::safe_counter")]
    pub credential_generation: u64,
    pub generation_state: GenerationState,
    pub refresh_owner: RefreshOwner,
    #[serde(deserialize_with = "crate::data::automation_records::required_nullable")]
    pub conversation_continuity: Option<ConversationContinuity>,
    pub restart_scope: Option<RestartScope>,
    #[serde(deserialize_with = "crate::data::automation_records::required_nullable")]
    pub reason: Option<String>,
    pub failure: Option<crate::data::automation_records::AutomationFailure>,
    #[serde(with = "crate::data::wire::timestamp")]
    pub created_at: chrono::DateTime<chrono::Utc>,
    #[serde(with = "crate::data::wire::timestamp")]
    pub apply_deadline: chrono::DateTime<chrono::Utc>,
    #[serde(with = "crate::data::wire::opt_timestamp")]
    pub completed_at: Option<chrono::DateTime<chrono::Utc>>,
}
/// Canonical non-secret dedup metadata. Never hash a request body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialUpdateIntent {
    pub request_id: uuid::Uuid,
    pub execution_id: crate::data::ids::ExecutionId,
    #[serde(deserialize_with = "crate::data::automation_records::safe_counter")]
    pub expected_revision: u64,
    pub credential_slot: String,
    #[serde(deserialize_with = "crate::data::automation_records::safe_counter")]
    pub expected_credential_generation: u64,
    pub reread_source: bool,
    pub secret_names: Vec<String>,
}
/// Request-only values: neither this carrier nor its material implement Serialize.
#[derive(Debug)]
pub struct CredentialUpdateRequest {
    pub intent: CredentialUpdateIntent,
    pub secrets: Option<std::sync::Arc<crate::data::secret::CommandSecrets>>,
}

/// Public terminal certainty projection; contains no slot or secret names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CredentialUpdateState {
    pub execution_id: crate::data::ids::ExecutionId,
    #[serde(deserialize_with = "crate::data::automation_records::safe_counter")]
    pub revision: u64,
    pub generations: Vec<CredentialGenerationState>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CredentialGenerationState {
    #[serde(deserialize_with = "crate::data::automation_records::safe_counter")]
    pub credential_generation: u64,
    #[serde(deserialize_with = "nullable_milliseconds")]
    pub pending_generation: Option<u64>,
    pub generation_state: GenerationState,
    pub refresh_owner: RefreshOwner,
    #[serde(deserialize_with = "crate::data::automation_records::required_nullable")]
    pub status: Option<CredentialUpdateStatus>,
}
impl EffectiveCredentialUpdates {
    pub fn public_state(&self) -> CredentialUpdateState {
        CredentialUpdateState {
            execution_id: self.execution_id.clone(),
            revision: self.revision,
            generations: self
                .slots
                .iter()
                .map(|s| CredentialGenerationState {
                    credential_generation: s.credential_generation,
                    pending_generation: s.pending_generation,
                    generation_state: s.generation_state,
                    refresh_owner: s.route.refresh_owner,
                    status: s.latest_update.as_ref().map(|u| u.status),
                })
                .collect(),
        }
    }
}
/// Shared projection for ordinary profiles and automation; no fabricated mode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnedExecutionSnapshot {
    #[serde(deserialize_with = "crate::data::automation_records::required_nullable")]
    pub automation: Option<crate::data::automation_records::AutomationSnapshot>,
    pub credential_updates: Option<EffectiveCredentialUpdates>,
}

impl CredentialUpdateRoute {
    /// Projection of route evidence and current credential certainty.
    pub fn method(&self, certainty: GenerationState) -> CredentialUpdateMethod {
        if self.live_update == Some(true) && certainty == GenerationState::Confirmed {
            CredentialUpdateMethod::Live
        } else if self.restart_supported == Some(true) {
            CredentialUpdateMethod::Restart
        } else if self.live_update == Some(false) && self.restart_supported == Some(false) {
            CredentialUpdateMethod::Unavailable
        } else {
            CredentialUpdateMethod::Unknown
        }
    }
}
