//! JSON messages shared by work servers and workers.
use crate::secret::{CommandSecrets, CommandSecretsInvalid, SecretString};
use crate::{Affinity, ContextChanges, ItemContext, ItemWorkflowContext, WorkflowContextResult};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;

pub use crate::wire::List;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiError {
    pub error: String,
    pub code: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkStart {
    pub user_code: String,
    pub verification_uri: String,
    pub link_id: String,
    pub interval_ms: u64,
}

/// A pending 202 response may increase the polling interval.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkPending {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval_ms: Option<u64>,
}
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkToken {
    pub refresh_token: String,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccessToken {
    pub access_token: String,
    #[serde(default)]
    #[serde(with = "crate::wire::opt_timestamp")]
    pub expires_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisterRequest {
    pub name: String,
    pub labels: BTreeMap<String, String>,
    pub capacity: usize,
    pub features: Vec<String>,
}

impl RegisterRequest {
    /// The `awman/` namespace is reported by awman, never an owner config.
    pub fn with_labels(
        name: String,
        reported: BTreeMap<String, String>,
        configured: BTreeMap<String, String>,
        capacity: usize,
        features: Vec<String>,
    ) -> Result<Self, &'static str> {
        let labels = crate::labels::WorkerLabels::new(reported, configured)
            .map_err(|_| "invalid worker labels")?
            .merged();
        Ok(Self {
            name,
            labels,
            capacity,
            features,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisterResponse {
    pub worker_id: String,
    #[serde(default)]
    pub server_labels: BTreeMap<String, String>,
    #[serde(default = "default_heartbeat_ms")]
    pub heartbeat_ms: u64,
    #[serde(default = "default_poll_ms")]
    pub poll_ms: u64,
}

const fn default_heartbeat_ms() -> u64 {
    15_000
}
const fn default_poll_ms() -> u64 {
    1_000
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueItem {
    /// Optional Markdown preview; contains no repository credentials.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<Source>,
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub summary: Option<String>,
    pub kind: WorkKind,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    #[serde(default)]
    pub priority: i32,
    #[serde(with = "crate::wire::timestamp")]
    pub enqueued_at: DateTime<Utc>,
    #[serde(default)]
    pub assigned_to_me: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkKind {
    Workflow,
    Prompt,
}

/// Work items are generic: servers fill what they need and may add fields of
/// their own, which a worker ignores. Only what a worker must act on,
/// `exec`, repository `credentials`, `outputs` and `affinity`, is decoded
/// strictly, so a requirement it does not know is refused rather than skipped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkItem {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<ItemContext>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow_context: Option<ItemWorkflowContext>,
    #[serde(default)]
    pub summary: Option<String>,
    pub id: String,
    pub title: String,
    pub kind: WorkKind,
    pub body: String,
    #[serde(default)]
    pub source: Option<Source>,
    pub repo: Repo,
    pub exec: Exec,
    #[serde(default)]
    pub push: Option<Push>,
    #[serde(default)]
    pub outputs: Option<Outputs>,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    #[serde(default)]
    pub priority: i32,
    #[serde(default)]
    pub affinity: Affinity,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Source {
    pub url: String,
}

impl WorkItem {
    /// The branch to publish to, when the item asks for a push. `push.branch`
    /// defaults to the branch the work is done on.
    pub fn push_branch(&self) -> Option<&str> {
        let push = self.push.as_ref().filter(|push| push.enabled)?;
        Some(if push.branch.is_empty() {
            &self.repo.branch
        } else {
            &push.branch
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Repo {
    pub url: String,
    pub base_ref: String,
    pub branch: String,
    #[serde(default)]
    pub merge_refs: Vec<String>,
    #[serde(default)]
    pub credentials: Option<Credentials>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialKind {
    Bearer,
    Basic,
    None,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Credentials {
    pub kind: CredentialKind,
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    #[serde(with = "crate::wire::opt_timestamp")]
    pub expires_at: Option<DateTime<Utc>>,
}

impl std::fmt::Debug for LinkToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LinkToken")
            .field("refresh_token", &"[redacted]")
            .finish()
    }
}
impl std::fmt::Debug for AccessToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccessToken")
            .field("access_token", &"[redacted]")
            .field("expires_at", &self.expires_at)
            .finish()
    }
}
impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("kind", &self.kind)
            .field("token", &"[redacted]")
            .field("username", &"[redacted]")
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Exec {
    #[serde(default)]
    pub dynamic: bool,
    #[serde(default)]
    pub leader: Option<String>,
    #[serde(default)]
    pub workflow_toml: Option<String>,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub read_only: bool,
    #[serde(default)]
    pub overlays: Vec<String>,
    #[serde(default)]
    pub interaction: Option<Interaction>,
    #[serde(default)]
    pub yolo: bool,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    #[serde(default, skip_serializing)]
    pub secrets: ExecSecrets,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prerequisites: Option<ExecPrerequisites>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflows_dynamic: Option<ExecWorkflowsDynamic>,
}

/// In-memory lease secrets. Values are accepted on input and only emitted by
/// the explicit `secret::server_wire` adapter.
#[derive(Clone, Default)]
pub struct ExecSecrets(Option<Arc<CommandSecrets>>);

impl ExecSecrets {
    pub fn new(
        entries: impl IntoIterator<Item = (String, SecretString)>,
    ) -> Result<Self, CommandSecretsInvalid> {
        let secrets = CommandSecrets::new(entries)?;
        Ok(if secrets.is_empty() {
            Self::default()
        } else {
            Self(Some(Arc::new(secrets)))
        })
    }
    pub fn is_empty(&self) -> bool {
        self.0.as_ref().is_none_or(|s| s.is_empty())
    }
    pub fn names(&self) -> Vec<String> {
        self.0
            .as_ref()
            .map(|s| s.names().map(str::to_owned).collect())
            .unwrap_or_default()
    }
    pub fn get(&self, name: &str) -> Option<&SecretString> {
        self.0.as_ref()?.get(name)
    }
    pub fn shared(&self) -> Option<Arc<CommandSecrets>> {
        self.0.clone()
    }
    pub fn take(&mut self) -> Self {
        std::mem::take(self)
    }
}

impl std::fmt::Debug for ExecSecrets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.0 {
            Some(secrets) => std::fmt::Debug::fmt(secrets, f),
            None => f.write_str("CommandSecrets([])"),
        }
    }
}
impl PartialEq for ExecSecrets {
    fn eq(&self, other: &Self) -> bool {
        match (&self.0, &other.0) {
            (None, None) => true,
            (Some(a), Some(b)) => a
                .iter()
                .map(|(n, v)| (n, v.expose()))
                .eq(b.iter().map(|(n, v)| (n, v.expose()))),
            (Some(a), None) => a.is_empty(),
            (None, Some(b)) => b.is_empty(),
        }
    }
}
impl Eq for ExecSecrets {}

impl<'de> Deserialize<'de> for ExecSecrets {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::{MapAccess, Visitor};
        struct SecretVisitor;
        impl<'de> Visitor<'de> for SecretVisitor {
            type Value = ExecSecrets;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("an object mapping secret names to strings")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut entries: Vec<(String, SecretString)> = Vec::new();
                while let Some(name) = map.next_key::<String>()? {
                    if entries.len() >= crate::secret::MAX_COMMAND_SECRETS {
                        return Err(serde::de::Error::custom(CommandSecretsInvalid::TooMany {
                            count: entries.len() + 1,
                        }));
                    }
                    if entries.iter().any(|(seen, _)| seen == &name) {
                        return Err(serde::de::Error::custom(CommandSecretsInvalid::Duplicate {
                            name,
                        }));
                    }
                    let value = map.next_value::<String>()?;
                    entries.push((name, SecretString::new(value)));
                }
                ExecSecrets::new(entries).map_err(serde::de::Error::custom)
            }
        }
        deserializer.deserialize_map(SecretVisitor)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecPrerequisites {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_identity: Option<ExecGitIdentity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh: Option<ExecCredentialChoice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub github_token: Option<ExecCredentialChoice>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecGitIdentity {
    pub name: String,
    pub email: String,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecCredentialChoice {
    Deny,
    Allow,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecWorkflowsDynamic {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agents_to_models: Option<BTreeMap<String, Vec<String>>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_leader: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guidance: Option<Vec<String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Interaction {
    Interactive,
    Headless,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Push {
    pub enabled: bool,
    /// Empty means `repo.branch`; see [`WorkItem::push_branch`].
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub branch: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Outputs {
    #[serde(default = "Outputs::default_max_bytes")]
    pub max_bytes: u64,
}

impl Outputs {
    /// What `"outputs": {}` allows: 1 MiB.
    pub const fn default_max_bytes() -> u64 {
        1024 * 1024
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lease {
    pub id: String,
    pub epoch: u64,
    #[serde(with = "crate::wire::timestamp")]
    pub expires_at: DateTime<Utc>,
    pub item: WorkItem,
    #[serde(default)]
    pub terminal: bool,
    #[serde(default)]
    pub remote_input: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimResponse {
    pub lease: Lease,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HeartbeatState {
    Preparing,
    Running,
    AwaitingInput,
    Pushing,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HeartbeatRequest {
    pub state: HeartbeatState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub step: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeartbeatResponse {
    #[serde(with = "crate::wire::timestamp")]
    pub expires_at: DateTime<Utc>,
    pub cancel: bool,
    #[serde(default)]
    pub cancel_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventBatch<E> {
    pub events: Vec<E>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LeaseResult<R> {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_changes: Option<ContextChanges>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow_context: Option<WorkflowContextResult>,
    pub result: R,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outputs: Option<BTreeMap<String, String>>,
    pub outputs_mounted: bool,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReleaseReason {
    UserDeclined,
    Ineligible,
    Shutdown,
    Error,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReleaseRequest<R> {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_changes: Option<ContextChanges>,
    pub reason: ReleaseReason,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<R>,
}

pub const WSP_VERSION: &str = "1";
pub const VERSION_HEADER: &str = "x-wsp-version";
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenRequest {
    pub refresh_token: String,
}
impl std::fmt::Debug for TokenRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenRequest")
            .field("refresh_token", &"[redacted]")
            .finish()
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventEnvelope {
    pub attempt: u32,
    pub sequence: u64,
    #[serde(flatten)]
    pub rest: serde_json::Value,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventsResponse {
    pub accepted: usize,
}
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::large_enum_variant)]
pub enum ClaimOutcome {
    Leased(Lease),
    Empty,
}
impl ApiError {
    pub fn status(&self) -> u16 {
        match self.code.as_str() {
            "taken" | "lease_lost" | "ineligible" => 409,
            "unauthorized" => 401,
            "not_found" | "worker_not_found" => 404,
            "expired" => 410,
            "context_too_large" => 413,
            "access_denied" => 403,
            _ => 400,
        }
    }
}
pub mod codes {
    pub const CONTEXT_TOO_LARGE: &str = "context_too_large";
    pub const CONTEXT_INVALID: &str = "context_invalid";
    pub const TAKEN: &str = "taken";
    pub const LEASE_LOST: &str = "lease_lost";
    pub const INELIGIBLE: &str = "ineligible";
    pub const NOT_FOUND: &str = "not_found";
    pub const UNAUTHORIZED: &str = "unauthorized";
    pub const INVALID_EVENT: &str = "invalid_event";
    pub const LINK_PENDING: &str = "authorization_pending";
    pub const LINK_DENIED: &str = "access_denied";
    pub const PREREQUISITE_WIDENING_REFUSED: &str = "prerequisite_widening_refused";
    pub const CREDENTIALS_LOST: &str = "credentials_lost";
}
pub mod features {
    pub const LEASE_CONTEXT: &str = "lease_context";
    pub const LEASE_WORKFLOW_CONTEXT: &str = "lease_workflow_context";
    pub const TERMINAL: &str = "terminal";
    pub const INTERACTIVE: &str = "interactive";
    pub const PROMPT: &str = "prompt";
    pub const WORKFLOW_DYNAMIC: &str = "workflow_dynamic";
    pub const WORKFLOW_FILE: &str = "workflow_file";
    pub const OUTPUTS: &str = "outputs";
    pub const OUTPUTS_MOUNT_STATE: &str = "outputs_mount_state";
    pub const WORKFLOW_STATE_EVENTS: &str = "workflow_state_events";
    pub const LEASE_CREDENTIALS: &str = "lease_credentials";
    pub const LEASE_INLINE_WORK_ITEM: &str = "lease_inline_work_item";
    pub const LEASE_READ_ONLY: &str = "lease_read_only";
    pub const LEASE_GIT_AUTH_FAILURE: &str = "lease_git_auth_failure";
    pub const LEASE_PUSH_RETRY: &str = "lease_push_retry";
    pub const LEASE_SECRETS: &str = "lease_secrets";
    pub const LEASE_SKILLS: &str = "lease_skills";
    pub const LEASE_PREREQUISITES: &str = "lease_prerequisites";
    pub const LEASE_WORKFLOWS_DYNAMIC: &str = "lease_workflows_dynamic";
    pub const ALL: &[&str] = &[
        LEASE_CONTEXT,
        LEASE_WORKFLOW_CONTEXT,
        TERMINAL,
        INTERACTIVE,
        PROMPT,
        WORKFLOW_DYNAMIC,
        WORKFLOW_FILE,
        OUTPUTS,
        OUTPUTS_MOUNT_STATE,
        WORKFLOW_STATE_EVENTS,
        LEASE_CREDENTIALS,
        LEASE_INLINE_WORK_ITEM,
        LEASE_READ_ONLY,
        LEASE_GIT_AUTH_FAILURE,
        LEASE_PUSH_RETRY,
        LEASE_SECRETS,
        LEASE_SKILLS,
        LEASE_PREREQUISITES,
        LEASE_WORKFLOWS_DYNAMIC,
    ];
}

#[cfg(test)]
mod exec_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn exec_secrets_decode_validate_and_never_serialize() {
        let source = json!({"secrets":{"TOKEN":"needle-secret"}});
        let exec: Exec = serde_json::from_value(source).unwrap();
        assert_eq!(exec.secrets.names(), ["TOKEN"]);
        assert!(!format!("{exec:?}").contains("needle-secret"));
        assert!(!serde_json::to_string(&exec)
            .unwrap()
            .contains("needle-secret"));
        let item = crate::conformance::work_item("secret-wire");
        let mut item = item;
        item.exec.secrets = exec.secrets.clone();
        assert!(!format!("{item:?}").contains("needle-secret"));
        assert!(!serde_json::to_string(&item)
            .unwrap()
            .contains("needle-secret"));
        let server_value = crate::secret::server_wire::work_item(&item);
        assert_eq!(server_value["exec"]["secrets"]["TOKEN"], "needle-secret");
        let lease = Lease {
            id: "lease".into(),
            epoch: 1,
            expires_at: chrono::Utc::now(),
            item: item.clone(),
            terminal: false,
            remote_input: false,
        };
        assert!(!format!("{lease:?}").contains("needle-secret"));
        assert!(!serde_json::to_string(&lease)
            .unwrap()
            .contains("needle-secret"));
        assert!(serde_json::from_value::<Exec>(json!({"unknown":true})).is_err());
        for bad in [
            json!({"secrets":{"bad":"x"}}),
            json!({"secrets":{"TOKEN":7}}),
            json!({"secrets":{"TOKEN":""}}),
            json!({"secrets":{"TOKEN":"x\u{0000}y"}}),
            json!({"secrets":{"TOKEN":"x"},"prerequisites":{"unknown":true}}),
            json!({"workflows_dynamic":{"unknown":true}}),
        ] {
            assert!(serde_json::from_value::<Exec>(bad).is_err());
        }
        let too_large = "x".repeat(8193);
        let error = serde_json::from_value::<Exec>(json!({"secrets":{"TOKEN":too_large}}))
            .unwrap_err()
            .to_string();
        assert!(!error.contains(&"x".repeat(100)));
        let mut many = serde_json::Map::new();
        for i in 0..33 {
            many.insert(format!("KEY_{i}"), json!("x"));
        }
        assert!(serde_json::from_value::<Exec>(json!({"secrets":many})).is_err());
        assert!(serde_json::from_str::<Exec>(r#"{"secrets":{"TOKEN":"a","TOKEN":"b"}}"#).is_err());
    }

    #[test]
    fn new_exec_fields_round_trip_and_omit_defaults() {
        let defaults = Exec {
            dynamic: false,
            leader: None,
            workflow_toml: None,
            agent: None,
            model: None,
            read_only: false,
            overlays: vec![],
            interaction: None,
            yolo: false,
            timeout_ms: None,
            secrets: ExecSecrets::default(),
            prerequisites: None,
            workflows_dynamic: None,
        };
        let value = serde_json::to_value(&defaults).unwrap();
        assert!(
            value.get("secrets").is_none()
                && value.get("prerequisites").is_none()
                && value.get("workflows_dynamic").is_none()
        );
        let mut exec = defaults;
        exec.prerequisites = Some(ExecPrerequisites {
            git_identity: Some(ExecGitIdentity {
                name: "A".into(),
                email: "a@b".into(),
            }),
            ssh: Some(ExecCredentialChoice::Allow),
            github_token: Some(ExecCredentialChoice::Deny),
        });
        exec.workflows_dynamic = Some(ExecWorkflowsDynamic {
            agents_to_models: Some(BTreeMap::from([("claude".into(), vec!["m".into()])])),
            default_leader: Some("claude:m".into()),
            guidance: Some(vec!["g".into()]),
        });
        exec.overlays = vec!["env(TOKEN)".into(), "skill(foo)".into(), "ssh()".into()];
        let json = serde_json::to_value(&exec).unwrap();
        let decoded: Exec = serde_json::from_value(json).unwrap();
        assert_eq!(exec, decoded);
        let item = crate::conformance::work_item("secret-debug");
        let encoded = serde_json::to_string(&item).unwrap();
        assert!(!encoded.contains("needle-secret"));
    }
}
