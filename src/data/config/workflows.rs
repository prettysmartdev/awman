//! `workflows` section: concurrency, prerequisite (credential) policy and
//! dynamic-workflow settings.

use crate::data::config::units::DurationStr;
use crate::data::error::DataError;
use crate::data::workflow_prerequisites::AcquireScope;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

pub const PREREQUISITES_DEFAULT_ENV_VARS: &[&str] = &["GH_TOKEN", "GITHUB_TOKEN"];
pub const ACQUIRE_TIMEOUT_DEFAULT_SECS: u64 = 30;
pub const ACQUIRE_TIMEOUT_MAX_SECS: u64 = 300;
pub const PREREQUISITE_POLICY_VERSION: u32 = 1;

/// Maximum number of `workflows.dynamic.guidance` entries after merging every
/// layer. Caps prompt bloat.
pub const GUIDANCE_MAX_ENTRIES: usize = 50;

/// Maximum length of a single `workflows.dynamic.guidance` entry, in chars.
pub const GUIDANCE_MAX_ENTRY_LEN: usize = 1000;

/// The `workflows` block.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkflowsConfig {
    /// Cap on concurrently running workflow steps (`None` = unlimited).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_concurrent_steps: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prerequisites: Option<PrerequisitesConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dynamic: Option<DynamicConfig>,
}

/// `workflows.dynamic`: which agents/models a dynamic workflow's leader may
/// schedule, the default leader, and project guidance for the leader.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DynamicConfig {
    /// Default leader spec in `agent::model` form; `--leader` overrides it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_leader: Option<String>,
    /// Agent name → model names available for that agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agents_to_models: Option<BTreeMap<String, Vec<String>>>,
    /// Instructions the leader follows, rendered as a bullet list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guidance: Option<Vec<String>>,
}

impl DynamicConfig {
    /// Semantic checks the key table cannot express. `field_path` prefixes
    /// every message (`workflows.dynamic`).
    pub fn validate(&self, field_path: &str) -> Result<(), DataError> {
        if let Some(leader) = &self.default_leader {
            validate_leader_spec(field_path, leader)?;
        }
        for (agent, models) in self.agents_to_models.iter().flatten() {
            if models.is_empty() {
                return Err(DataError::Other(format!(
                    "{field_path}.agentsToModels.{agent} has an empty model list. \
                     Provide at least one model name."
                )));
            }
            if models.iter().any(|m| m.trim().is_empty()) {
                return Err(DataError::Other(format!(
                    "{field_path}.agentsToModels.{agent} contains an empty model name."
                )));
            }
        }
        if let Some(entries) = &self.guidance {
            validate_guidance(field_path, entries)?;
        }
        Ok(())
    }

    /// Whether every field is unset or an empty collection.
    pub fn is_empty(&self) -> bool {
        self.default_leader.is_none()
            && self
                .agents_to_models
                .as_ref()
                .is_none_or(BTreeMap::is_empty)
            && self.guidance.as_ref().is_none_or(Vec::is_empty)
    }
}

/// `workflows.dynamic.defaultLeader` syntax: `agent::model`, two non-empty
/// components without surrounding whitespace, and a valid agent name.
pub(crate) fn validate_leader_spec(field_path: &str, raw: &str) -> Result<(), DataError> {
    let parts: Vec<&str> = raw.split("::").collect();
    if parts.len() != 2 || parts[0].is_empty() || parts[1].is_empty() {
        return Err(DataError::Other(format!(
            "{field_path}.defaultLeader {raw:?} is invalid; expected agent::model \
             (e.g. claude::claude-opus-4-8)"
        )));
    }
    let (agent, model) = (parts[0], parts[1]);
    if agent != agent.trim() || model != model.trim() {
        return Err(DataError::Other(format!(
            "{field_path}.defaultLeader {raw:?} must not have leading or trailing whitespace \
             in the agent or model component"
        )));
    }
    if crate::data::session::AgentName::new(agent).is_err() {
        return Err(DataError::Other(format!(
            "{field_path}.defaultLeader agent component {agent:?} is not a valid agent name: \
             only ASCII alphanumerics, '-', and '_' are allowed, length 1..=64"
        )));
    }
    Ok(())
}

/// Per-entry guidance checks (non-empty, bounded length). The 50-entry limit
/// is checked after merging every layer.
pub(crate) fn validate_guidance(field_path: &str, entries: &[String]) -> Result<(), DataError> {
    if entries.len() > GUIDANCE_MAX_ENTRIES {
        return Err(DataError::Other(format!(
            "{field_path}.guidance has {} entries; the maximum is {GUIDANCE_MAX_ENTRIES}.",
            entries.len()
        )));
    }
    for (i, entry) in entries.iter().enumerate() {
        if entry.trim().is_empty() {
            return Err(DataError::Other(format!(
                "{field_path}.guidance[{i}] is empty or whitespace-only. \
                 Remove it or provide a non-empty instruction."
            )));
        }
        if entry.chars().count() > GUIDANCE_MAX_ENTRY_LEN {
            return Err(DataError::Other(format!(
                "{field_path}.guidance[{i}] is {} characters; the maximum is \
                 {GUIDANCE_MAX_ENTRY_LEN}.",
                entry.chars().count()
            )));
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PreflightMode {
    #[default]
    Warn,
    Off,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UnresolvedMode {
    Warn,
    Fail,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CredentialUse {
    #[default]
    Ask,
    Allow,
    Deny,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcquireRefresh {
    #[default]
    Start,
    BeforeTeardown,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GitIdentityConfig {
    #[serde(rename = "use", default, skip_serializing_if = "Option::is_none")]
    pub use_: Option<CredentialUse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SshConfig {
    #[serde(rename = "use", default, skip_serializing_if = "Option::is_none")]
    pub use_: Option<CredentialUse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AcquireCommandConfig {
    /// The command; absent means no acquire command is configured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub argv: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<DurationStr>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh: Option<AcquireRefresh>,
}

impl AcquireCommandConfig {
    /// Configured timeout in whole seconds (rounded up), or the default.
    pub fn timeout_secs(&self) -> u64 {
        self.timeout
            .as_ref()
            .map_or(ACQUIRE_TIMEOUT_DEFAULT_SECS, |d| {
                let d = d.to_duration();
                d.as_secs() + u64::from(d.subsec_nanos() > 0)
            })
    }
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GithubTokenConfig {
    #[serde(rename = "use", default, skip_serializing_if = "Option::is_none")]
    pub use_: Option<CredentialUse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env_vars: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acquire: Option<AcquireCommandConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verify: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PrerequisitesConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preflight: Option<PreflightMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unresolved: Option<UnresolvedMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_identity: Option<GitIdentityConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh: Option<SshConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub github_token: Option<GithubTokenConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_api_client_grants: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigScope {
    Global,
    Repo,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicySource {
    Default,
    Global,
    SquadGlobal,
    Repo,
    Task,
    RemoteTask,
    Flag,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FixedIdentity {
    pub name: String,
    pub email: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdentityPolicy {
    #[serde(rename = "use")]
    pub use_: CredentialUse,
    pub fixed: Option<FixedIdentity>,
    pub source: PolicySource,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SshPolicy {
    #[serde(rename = "use")]
    pub use_: CredentialUse,
    /// Unvalidated path as written; `SshKeyInventory::validate_key_path` validates before any mount.
    pub key: Option<PathBuf>,
    /// true when the key came from repo config (pre-selected in the prompt, never a grant).
    pub key_is_suggestion: bool,
    /// `--prerequisite-ssh all`: mount the whole ~/.ssh (today's `ssh()`).
    pub whole_dir: bool,
    pub source: PolicySource,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcquirePolicy {
    pub argv: Vec<String>,
    pub timeout_secs: u64,
    pub refresh: AcquireRefresh,
    pub scope: AcquireScope,
    pub sha256: String, /* sha256 of serde_json(argv) */
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenPolicy {
    #[serde(rename = "use")]
    pub use_: CredentialUse,
    pub env_vars: Vec<String>,
    pub acquire: Option<AcquirePolicy>,
    pub verify: bool,
    pub source: PolicySource,
}

/// The resolved, serialisable policy (stored on squad `Task`; never holds a secret).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrerequisitePolicy {
    pub version: u32,
    pub preflight: PreflightMode,
    /// `None` ⇒ the frontend class default (interactive: warn; headless: `HeadlessDefaults::prerequisite_unresolved`).
    pub unresolved: Option<UnresolvedMode>,
    pub git_identity: IdentityPolicy,
    pub ssh: SshPolicy,
    pub github_token: TokenPolicy,
    pub allow_api_client_grants: bool,
    /// Merge warnings (repo `allow` ignored, …) — emitted once by Layer 2, shown in the report notes.
    pub warnings: Vec<String>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialKind {
    Identity,
    Ssh,
    Token,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct PrerequisiteResolveContext {
    pub remote_session: bool,
}

pub struct PrerequisitePolicyLayers<'a> {
    pub global: Option<&'a PrerequisitesConfig>,
    /// A squad task file's `workflows.prerequisites` block. Task files are
    /// agent-writable, so it applies with repo semantics: it may narrow, never grant.
    pub task_file: Option<&'a PrerequisitesConfig>,
    pub repo: Option<&'a PrerequisitesConfig>,
    pub task: Option<&'a PrerequisitePolicy>,
    pub flags: &'a crate::data::config::flags::PrerequisiteFlags,
    pub context: PrerequisiteResolveContext,
}

impl PrerequisitesConfig {
    pub fn validate(&self, scope: ConfigScope) -> Result<Vec<String>, DataError> {
        let invalid = |key: &str, why: &str| {
            DataError::Other(format!("workflows.prerequisites.{key}: {why}"))
        };
        if let Some(identity) = &self.git_identity {
            if identity
                .name
                .as_ref()
                .is_some_and(|v| v.trim().is_empty() || v.chars().any(char::is_control))
            {
                return Err(invalid(
                    "gitIdentity.name",
                    "must be nonempty and contain no control characters",
                ));
            }
            if identity
                .email
                .as_ref()
                .is_some_and(|v| !v.contains('@') || v.chars().any(char::is_control))
            {
                return Err(invalid(
                    "gitIdentity.email",
                    "must contain @ and no control characters",
                ));
            }
        }
        if let Some(token) = &self.github_token {
            if token
                .env_vars
                .as_ref()
                .is_some_and(|vars| vars.iter().any(|v| !valid_env_name(v)))
            {
                return Err(invalid(
                    "githubToken.envVars",
                    "names must match [A-Z_][A-Z0-9_]*",
                ));
            }
            if let Some(acquire) = &token.acquire {
                if acquire.argv.as_ref().is_some_and(|argv| {
                    argv.is_empty()
                        || argv[0].trim().is_empty()
                        || argv.iter().any(|a| a.chars().any(char::is_control))
                }) {
                    return Err(invalid(
                        "githubToken.acquire.argv",
                        "requires a command and arguments without control characters",
                    ));
                }
                if acquire.timeout.as_ref().is_some_and(|t| {
                    let d = t.to_duration();
                    d.is_zero() || d > std::time::Duration::from_secs(ACQUIRE_TIMEOUT_MAX_SECS)
                }) {
                    return Err(invalid(
                        "githubToken.acquire.timeout",
                        "must be greater than zero and at most 300s",
                    ));
                }
            }
        }
        let mut warnings = Vec::new();
        if scope == ConfigScope::Repo && self.allow_api_client_grants.is_some() {
            warnings.push(
                "workflows.prerequisites.allowApiClientGrants is global-only; repo value ignored"
                    .into(),
            );
        }
        Ok(warnings)
    }
}

pub(crate) fn valid_env_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|b| b == b'_' || b.is_ascii_uppercase())
        && bytes.all(|b| b == b'_' || b.is_ascii_uppercase() || b.is_ascii_digit())
}

impl Default for PrerequisitePolicy {
    fn default() -> Self {
        Self {
            version: PREREQUISITE_POLICY_VERSION,
            preflight: PreflightMode::Warn,
            unresolved: None,
            git_identity: IdentityPolicy {
                use_: CredentialUse::Ask,
                fixed: None,
                source: PolicySource::Default,
            },
            ssh: SshPolicy {
                use_: CredentialUse::Ask,
                key: None,
                key_is_suggestion: false,
                whole_dir: false,
                source: PolicySource::Default,
            },
            github_token: TokenPolicy {
                use_: CredentialUse::Ask,
                env_vars: PREREQUISITES_DEFAULT_ENV_VARS
                    .iter()
                    .map(|v| (*v).into())
                    .collect(),
                acquire: None,
                verify: true,
                source: PolicySource::Default,
            },
            allow_api_client_grants: false,
            warnings: vec![],
        }
    }
}

impl AcquirePolicy {
    fn from_config(config: &AcquireCommandConfig, scope: AcquireScope) -> Self {
        Self {
            argv: config.argv.clone().unwrap_or_default(),
            timeout_secs: config.timeout_secs(),
            refresh: config.refresh.unwrap_or_default(),
            scope,
            sha256: crate::data::fs::hash::sha256_hex(
                serde_json::to_vec(&config.argv.clone().unwrap_or_default())
                    .expect("argv serializes"),
            ),
        }
    }
    pub fn timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.timeout_secs)
    }
    pub fn needs_approval_by_hash(&self) -> bool {
        self.scope == AcquireScope::Repo
    }
}

impl PrerequisitePolicy {
    pub fn resolve(layers: PrerequisitePolicyLayers<'_>) -> Self {
        let mut policy = Self::default();
        if let Some(global) = layers.global {
            policy.apply_config(global, PolicySource::Global, false);
            policy.allow_api_client_grants = global.allow_api_client_grants.unwrap_or(false);
        }
        if let Some(repo) = layers.repo {
            policy.apply_config(repo, PolicySource::Repo, true);
        }
        if let Some(task) = layers.task_file {
            policy.apply_config(task, PolicySource::Task, true);
        }
        if let Some(task) = layers.task {
            policy.apply_task(task);
        }
        let global_ssh_allow = layers
            .global
            .is_some_and(|c| c.ssh.as_ref().and_then(|s| s.use_) == Some(CredentialUse::Allow));
        if layers.context.remote_session && !global_ssh_allow {
            policy.ssh.use_ = CredentialUse::Deny;
        }
        policy.apply_flags(layers.flags);
        policy
    }
    /// Default provenance is an absent task override, not an explicit ask.
    /// A task may select the operator's acquire command, never replace its argv.
    pub fn apply_task(&mut self, task: &Self) {
        if task.git_identity.source != PolicySource::Default {
            self.git_identity = task.git_identity.clone();
            self.git_identity.source = if task.git_identity.source == PolicySource::RemoteTask {
                PolicySource::RemoteTask
            } else {
                PolicySource::Task
            };
        }
        if task.ssh.source != PolicySource::Default {
            self.ssh = task.ssh.clone();
            self.ssh.source = if task.ssh.source == PolicySource::RemoteTask {
                PolicySource::RemoteTask
            } else {
                PolicySource::Task
            };
        }
        if task.github_token.source != PolicySource::Default {
            let acquire = self.github_token.acquire.clone();
            self.github_token = task.github_token.clone();
            self.github_token.acquire = acquire.filter(|a| a.scope == AcquireScope::Global);
            self.github_token.source = if task.github_token.source == PolicySource::RemoteTask {
                PolicySource::RemoteTask
            } else {
                PolicySource::Task
            };
        }
        if task.unresolved.is_some() {
            self.unresolved = task.unresolved;
        }
    }

    pub fn edited_task(&self, edit: &Self) -> Self {
        let mut merged = self.clone();
        merged.apply_task(edit);
        merged
    }

    pub fn from_flags(flags: &crate::data::config::flags::PrerequisiteFlags) -> Self {
        Self::resolve(PrerequisitePolicyLayers {
            global: None,
            task_file: None,
            repo: None,
            task: None,
            flags,
            context: PrerequisiteResolveContext::default(),
        })
    }
    pub fn unresolved_or(&self, default: UnresolvedMode) -> UnresolvedMode {
        self.unresolved.unwrap_or(default)
    }
    pub fn uses_credential(&self, kind: CredentialKind) -> CredentialUse {
        match kind {
            CredentialKind::Identity => self.git_identity.use_,
            CredentialKind::Ssh => self.ssh.use_,
            CredentialKind::Token => self.github_token.use_,
        }
    }
    fn repo_warning(&mut self, key: &str) {
        self.warnings.push(format!("workflows.prerequisites.{key}: repo config cannot grant credentials; set this in ~/.awman/config.json or answer the prompt"));
    }
    fn apply_config(&mut self, config: &PrerequisitesConfig, source: PolicySource, repo: bool) {
        if let Some(mode) = config.preflight {
            if repo && mode == PreflightMode::Off {
                self.warnings
                    .push("workflows.prerequisites.preflight: repo off ignored".into());
            } else {
                self.preflight = mode;
            }
        }
        if let Some(mode) = config.unresolved {
            if repo && mode == UnresolvedMode::Warn {
                self.warnings.push(
                    "workflows.prerequisites.unresolved: repo warn cannot loosen fail".into(),
                );
            } else {
                self.unresolved = Some(mode);
            }
        }
        if let Some(identity) = &config.git_identity {
            let fixed = identity
                .name
                .as_ref()
                .zip(identity.email.as_ref())
                .map(|(name, email)| FixedIdentity {
                    name: name.clone(),
                    email: email.clone(),
                });
            if let Some(use_) = identity.use_ {
                if repo && use_ == CredentialUse::Allow && fixed.is_none() {
                    self.repo_warning("gitIdentity.use");
                } else {
                    self.git_identity.use_ = if repo && fixed.is_none() {
                        narrow_use(self.git_identity.use_, use_)
                    } else {
                        use_
                    };
                    self.git_identity.source = source;
                }
            }
            if let Some(fixed) = fixed {
                self.git_identity.fixed = Some(fixed);
                self.git_identity.source = source;
            }
        }
        if let Some(ssh) = &config.ssh {
            if let Some(use_) = ssh.use_ {
                if repo && use_ == CredentialUse::Allow {
                    self.repo_warning("ssh.use");
                } else {
                    self.ssh.use_ = if repo {
                        narrow_use(self.ssh.use_, use_)
                    } else {
                        use_
                    };
                    self.ssh.source = source;
                }
            }
            if let Some(key) = &ssh.key {
                self.ssh.key = Some(key.into());
                self.ssh.key_is_suggestion = repo;
                self.ssh.whole_dir = false;
                self.ssh.source = source;
            }
        }
        if let Some(token) = &config.github_token {
            if let Some(use_) = token.use_ {
                if repo && use_ == CredentialUse::Allow {
                    self.repo_warning("githubToken.use");
                } else {
                    self.github_token.use_ = if repo {
                        narrow_use(self.github_token.use_, use_)
                    } else {
                        use_
                    };
                    self.github_token.source = source;
                }
            }
            if let Some(vars) = &token.env_vars {
                if repo
                    && !vars
                        .iter()
                        .all(|var| self.github_token.env_vars.contains(var))
                {
                    self.warnings.push("workflows.prerequisites.githubToken.envVars: repo names must be a subset of global names; ignored".into());
                } else {
                    self.github_token.env_vars = vars.clone();
                    self.github_token.source = source;
                }
            }
            if let Some(acquire) = token.acquire.as_ref().filter(|a| a.argv.is_some()) {
                if repo
                    && self
                        .github_token
                        .acquire
                        .as_ref()
                        .is_some_and(|a| a.scope == AcquireScope::Global)
                {
                    self.warnings
                        .push("repository acquire ignored: trusted global command retained".into());
                } else {
                    self.github_token.acquire = Some(AcquirePolicy::from_config(
                        acquire,
                        if repo {
                            AcquireScope::Repo
                        } else {
                            AcquireScope::Global
                        },
                    ));
                    self.github_token.source = source;
                }
            }
            if let Some(verify) = token.verify {
                if repo && self.github_token.verify && !verify {
                    self.warnings.push("workflows.prerequisites.githubToken.verify: repo cannot disable verification; ignored".into());
                } else {
                    self.github_token.verify = verify;
                }
            }
        }
        if repo && config.allow_api_client_grants.is_some() {
            self.warnings.push(
                "workflows.prerequisites.allowApiClientGrants is global-only; repo value ignored"
                    .into(),
            );
        }
    }
    fn apply_flags(&mut self, flags: &crate::data::config::flags::PrerequisiteFlags) {
        use crate::data::config::flags::{IdentityFlag, SshFlag, TokenFlag};
        if let Some(flag) = &flags.identity {
            self.git_identity.source = PolicySource::Flag;
            self.git_identity.fixed = match flag {
                IdentityFlag::Fixed { name, email } => Some(FixedIdentity {
                    name: name.clone(),
                    email: email.clone(),
                }),
                _ => None,
            };
            self.git_identity.use_ = if matches!(flag, IdentityFlag::Deny) {
                CredentialUse::Deny
            } else {
                CredentialUse::Allow
            };
        }
        if let Some(flag) = &flags.ssh {
            self.ssh.source = PolicySource::Flag;
            self.ssh.key_is_suggestion = false;
            self.ssh.key = match flag {
                SshFlag::Key(path) => Some(path.clone()),
                _ => None,
            };
            self.ssh.whole_dir = matches!(flag, SshFlag::All);
            self.ssh.use_ = if matches!(flag, SshFlag::Deny) {
                CredentialUse::Deny
            } else {
                CredentialUse::Allow
            };
        }
        if let Some(flag) = &flags.token {
            self.github_token.source = PolicySource::Flag;
            self.github_token.use_ = if matches!(flag, TokenFlag::Deny) {
                CredentialUse::Deny
            } else {
                CredentialUse::Allow
            };
            if let TokenFlag::Env(var) = flag {
                self.github_token.env_vars = vec![var.clone()];
                self.github_token.acquire = None;
            }
            if matches!(flag, TokenFlag::Acquire | TokenFlag::Secret(_)) {
                self.github_token.env_vars.clear();
            }
            if matches!(flag, TokenFlag::Secret(_)) {
                self.github_token.acquire = None;
            }
        }
        if let Some(mode) = flags.unresolved {
            self.unresolved = Some(mode);
        }
    }
}

fn narrow_use(current: CredentialUse, requested: CredentialUse) -> CredentialUse {
    match (current, requested) {
        (CredentialUse::Deny, _) | (_, CredentialUse::Deny) => CredentialUse::Deny,
        (CredentialUse::Ask, _) | (_, CredentialUse::Ask) => CredentialUse::Ask,
        _ => CredentialUse::Allow,
    }
}
