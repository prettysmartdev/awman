//! `Session` and `SessionState` — the ruling Layer 0 types for awman operations.
//!
//! A `Session` ties together a working directory, a git root, the loaded
//! configurations, and the in-flight runtime state. The CLI runs a single
//! session per invocation; the TUI runs one per tab; the API server runs
//! one per API session.

use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::data::config::effective::EffectiveConfig;
use crate::data::config::env::EnvSnapshot;
use crate::data::config::flags::FlagConfig;
use crate::data::config::global::GlobalConfig;
use crate::data::config::repo::RepoConfig;
use crate::data::error::DataError;

/// Newtype around the underlying session UUID.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionId(Uuid);

impl SessionId {
    /// Generate a fresh random session id (v4).
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// Wrap an existing UUID (round-trips through persistence).
    pub fn from_uuid(uuid: Uuid) -> Self {
        Self(uuid)
    }

    /// Underlying UUID.
    pub fn as_uuid(&self) -> Uuid {
        self.0
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl Default for SessionId {
    fn default() -> Self {
        Self::new()
    }
}

/// Newtype wrapper around an agent name.
///
/// Validation matches the legacy `cli::validate_agent_name`: ASCII alphanumerics,
/// hyphens, and underscores, length 1..=64.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AgentName(String);

impl AgentName {
    /// Construct an agent name, validating its shape.
    pub fn new(name: impl Into<String>) -> Result<Self, DataError> {
        let name = name.into();
        if name.is_empty() {
            return Err(DataError::InvalidAgentName {
                name,
                reason: "must not be empty".to_string(),
            });
        }
        if name.len() > 64 {
            return Err(DataError::InvalidAgentName {
                name,
                reason: "must be 64 characters or fewer".to_string(),
            });
        }
        if !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err(DataError::InvalidAgentName {
                name,
                reason: "only ASCII alphanumerics, '-', and '_' are allowed".to_string(),
            });
        }
        Ok(Self(name))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }
}

impl std::fmt::Display for AgentName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Persistable identity of a running container.
///
/// Layer 0 holds only the persistable identity. The runtime object that
/// controls a container (start/stop/wait) is a Layer 1 concern.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContainerHandle {
    pub id: String,
    pub image_tag: String,
    pub name: String,
    pub started_at: chrono::DateTime<chrono::Utc>,
}

/// Lifecycle state of a single command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CommandStatus {
    Pending,
    Running,
    Done,
    Error(String),
}

/// Persistable record of a single in-flight command invocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandInvocation {
    pub id: Uuid,
    pub subcommand: String,
    pub args: Vec<String>,
    pub status: CommandStatus,
    pub exit_code: Option<i32>,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub finished_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Lifecycle state of a single workflow step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum StepStatus {
    Pending,
    Running,
    Done,
    Error(String),
}

/// Persistable record of one step in a workflow invocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowStepRecord {
    pub name: String,
    pub depends_on: Vec<String>,
    pub prompt_template: String,
    pub status: StepStatus,
    pub container_id: Option<String>,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
}

/// Persistable state of a workflow invocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowInvocation {
    pub id: Uuid,
    pub title: Option<String>,
    pub workflow_name: String,
    pub workflow_hash: String,
    #[serde(default)]
    pub work_item: Option<u32>,
    pub steps: Vec<WorkflowStepRecord>,
    /// User-controlled flags persisted alongside the run.
    #[serde(default)]
    pub paused: bool,
    #[serde(default)]
    pub yolo: bool,
    #[serde(default)]
    pub auto: bool,
    /// Index of the step the workflow is currently processing, if any.
    #[serde(default)]
    pub current_step: Option<usize>,
}

/// Severity of a session log entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionLogKind {
    Info,
    Warning,
    Error,
    Diagnostic,
}

/// A structured note or error attached to a session for later display.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionLogEntry {
    pub at: chrono::DateTime<chrono::Utc>,
    pub kind: SessionLogKind,
    pub message: String,
}

impl SessionLogEntry {
    pub fn now(kind: SessionLogKind, message: impl Into<String>) -> Self {
        Self {
            at: chrono::Utc::now(),
            kind,
            message: message.into(),
        }
    }
}

/// Mutable runtime state belonging to a session.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionState {
    pub current_command: Option<CommandInvocation>,
    pub current_workflow: Option<WorkflowInvocation>,
    pub current_container: Option<ContainerHandle>,
    pub errors: Vec<SessionLogEntry>,
    pub notes: Vec<SessionLogEntry>,
}

impl SessionState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record_error(&mut self, message: impl Into<String>) {
        self.errors
            .push(SessionLogEntry::now(SessionLogKind::Error, message));
    }

    pub fn record_note(&mut self, kind: SessionLogKind, message: impl Into<String>) {
        self.notes.push(SessionLogEntry::now(kind, message));
    }
}

/// Trait used by Layer 0 to delegate git-root resolution to Layer 1.
///
/// Layer 0 must never invoke `git rev-parse` directly; it accepts a resolver
/// at construction time and the real implementation lives in `GitEngine`
/// (Layer 1).
pub trait GitRootResolver: Send + Sync {
    fn resolve(&self, working_dir: &Path) -> Result<PathBuf, DataError>;
}

/// Resolver that always returns the same git root regardless of input.
/// Used by Layer-0-internal tests and the API server's session restore.
#[derive(Debug, Clone)]
pub struct StaticGitRootResolver {
    root: PathBuf,
}

impl StaticGitRootResolver {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
}

impl GitRootResolver for StaticGitRootResolver {
    fn resolve(&self, _working_dir: &Path) -> Result<PathBuf, DataError> {
        Ok(self.root.clone())
    }
}

/// Whether this session targets a local working directory or a remote
/// repository that was cloned automatically.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionType {
    Local {
        workdir: PathBuf,
    },
    Remote {
        repo_url: String,
        branch: String,
        cloned_path: PathBuf,
    },
}

impl SessionType {
    pub fn is_remote(&self) -> bool {
        matches!(self, SessionType::Remote { .. })
    }

    pub fn cloned_path(&self) -> Option<&Path> {
        match self {
            SessionType::Remote { cloned_path, .. } => Some(cloned_path),
            SessionType::Local { .. } => None,
        }
    }

    pub fn working_dir(&self) -> &Path {
        match self {
            SessionType::Local { workdir } => workdir,
            SessionType::Remote { cloned_path, .. } => cloned_path,
        }
    }
}

/// The ruling Layer 0 type that every command and workflow invocation hangs off.
#[derive(Debug, Clone)]
pub struct Session {
    id: SessionId,
    session_type: SessionType,
    git_root: PathBuf,
    repo_config: RepoConfig,
    global_config: GlobalConfig,
    env: EnvSnapshot,
    flags: FlagConfig,
    default_agent: Option<AgentName>,
    available_agents: Vec<AgentName>,
    state: SessionState,
    created_at: SystemTime,
    last_active_at: SystemTime,
    created_at_instant: Instant,
}

/// Builder-style options for constructing a `Session`.
#[derive(Debug, Default, Clone)]
pub struct SessionOpenOptions {
    pub flags: FlagConfig,
    pub env: Option<EnvSnapshot>,
    pub available_agents: Option<Vec<AgentName>>,
}

impl Session {
    /// Open a session at the supplied working directory, resolving the git
    /// root via `resolver` and loading repo + global config from disk.
    pub fn open(
        working_dir: PathBuf,
        resolver: &dyn GitRootResolver,
        opts: SessionOpenOptions,
    ) -> Result<Self, DataError> {
        let git_root = resolver.resolve(&working_dir).map_err(|e| match e {
            DataError::GitRootNotFound { working_dir } => {
                DataError::GitRootNotFound { working_dir }
            }
            other => DataError::GitRootResolution {
                working_dir: working_dir.clone(),
                message: other.to_string(),
            },
        })?;
        Self::open_at_git_root(working_dir, git_root, opts)
    }

    /// Open a session, falling back to using the working directory as the git
    /// root when git resolution fails. Valid for non-git directories.
    pub fn open_or_workdir_fallback(
        working_dir: PathBuf,
        resolver: &dyn GitRootResolver,
        opts: SessionOpenOptions,
    ) -> Result<Self, DataError> {
        match Self::open(working_dir.clone(), resolver, opts.clone()) {
            Ok(session) => Ok(session),
            Err(DataError::GitRootNotFound { .. }) => {
                Self::open_at_git_root(working_dir.clone(), working_dir, opts)
            }
            Err(other) => Err(other),
        }
    }

    /// Open a session with an explicit, pre-resolved git root.
    pub fn open_at_git_root(
        working_dir: PathBuf,
        git_root: PathBuf,
        opts: SessionOpenOptions,
    ) -> Result<Self, DataError> {
        let env = opts.env.unwrap_or_else(EnvSnapshot::empty);
        let repo_config = RepoConfig::load(&git_root)?;
        let global_config = GlobalConfig::load_with(&env)?;

        let default_agent = resolve_default_agent(&opts.flags, &repo_config, &global_config)?;
        let available_agents = opts.available_agents.unwrap_or_default();

        let now = SystemTime::now();

        Ok(Self {
            id: SessionId::new(),
            session_type: SessionType::Local {
                workdir: working_dir,
            },
            git_root,
            repo_config,
            global_config,
            env,
            flags: opts.flags,
            default_agent,
            available_agents,
            state: SessionState::new(),
            created_at: now,
            last_active_at: now,
            created_at_instant: Instant::now(),
        })
    }

    pub fn id(&self) -> SessionId {
        self.id
    }

    pub fn working_dir(&self) -> &Path {
        self.session_type.working_dir()
    }

    pub fn session_type(&self) -> &SessionType {
        &self.session_type
    }

    pub fn set_session_type(&mut self, session_type: SessionType) {
        self.session_type = session_type;
    }

    pub fn git_root(&self) -> &Path {
        &self.git_root
    }

    pub fn repo_config(&self) -> &RepoConfig {
        &self.repo_config
    }

    pub fn global_config(&self) -> &GlobalConfig {
        &self.global_config
    }

    pub fn env(&self) -> &EnvSnapshot {
        &self.env
    }

    pub fn flags(&self) -> &FlagConfig {
        &self.flags
    }

    pub fn default_agent(&self) -> Option<&AgentName> {
        self.default_agent.as_ref()
    }

    pub fn available_agents(&self) -> &[AgentName] {
        &self.available_agents
    }

    pub fn state(&self) -> &SessionState {
        &self.state
    }

    pub fn state_mut(&mut self) -> &mut SessionState {
        &mut self.state
    }

    pub fn created_at(&self) -> SystemTime {
        self.created_at
    }

    pub fn last_active_at(&self) -> SystemTime {
        self.last_active_at
    }

    pub fn uptime(&self) -> std::time::Duration {
        self.created_at_instant.elapsed()
    }

    /// Mark the session as active *now*; intended to be called whenever the
    /// session services any user-visible operation.
    pub fn touch(&mut self) {
        self.last_active_at = SystemTime::now();
    }

    /// Replace the captured flag set (e.g. when the frontend reparses input).
    pub fn set_flags(&mut self, flags: FlagConfig) {
        self.flags = flags;
    }

    /// Replace the captured env snapshot.
    pub fn set_env(&mut self, env: EnvSnapshot) {
        self.env = env;
    }

    /// Replace the available agents list (typically derived by Layer 1 when
    /// scanning Dockerfile.* templates).
    pub fn set_available_agents(&mut self, agents: Vec<AgentName>) {
        self.available_agents = agents;
    }

    /// Return a freshly-merged `EffectiveConfig` view.
    pub fn effective_config(&self) -> EffectiveConfig {
        EffectiveConfig::new(
            self.flags.clone(),
            self.env.clone(),
            self.repo_config.clone(),
            self.global_config.clone(),
        )
    }
}

fn resolve_default_agent(
    flags: &FlagConfig,
    repo: &RepoConfig,
    global: &GlobalConfig,
) -> Result<Option<AgentName>, DataError> {
    if let Some(name) = flags.agent.as_deref() {
        return Ok(Some(AgentName::new(name)?));
    }
    if let Some(name) = repo.agent.as_deref() {
        return Ok(Some(AgentName::new(name)?));
    }
    if let Some(name) = global.default_agent.as_deref() {
        return Ok(Some(AgentName::new(name)?));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::config::env::{EnvSnapshot, AWMAN_CONFIG_HOME};
    use crate::data::config::repo::REPO_CONFIG_SUBDIR;

    // ─── helpers ─────────────────────────────────────────────────────────────

    struct IsolatedSetup {
        git_root: tempfile::TempDir,
        home_dir: tempfile::TempDir,
    }

    impl IsolatedSetup {
        fn new() -> Self {
            Self {
                git_root: tempfile::tempdir().unwrap(),
                home_dir: tempfile::tempdir().unwrap(),
            }
        }

        fn env(&self) -> EnvSnapshot {
            EnvSnapshot::with_overrides([(
                AWMAN_CONFIG_HOME,
                self.home_dir.path().to_str().unwrap(),
            )])
        }

        fn open_session(&self) -> Session {
            self.open_session_with_opts(Default::default())
        }

        fn open_session_with_opts(&self, flags: FlagConfig) -> Session {
            let resolver = StaticGitRootResolver::new(self.git_root.path());
            let opts = SessionOpenOptions {
                flags,
                env: Some(self.env()),
                available_agents: None,
            };
            Session::open(self.git_root.path().to_path_buf(), &resolver, opts).unwrap()
        }
    }

    struct FailingGitRootResolver;
    impl GitRootResolver for FailingGitRootResolver {
        fn resolve(&self, working_dir: &Path) -> Result<PathBuf, DataError> {
            Err(DataError::GitRootNotFound {
                working_dir: working_dir.to_path_buf(),
            })
        }
    }

    // ─── AgentName tests ─────────────────────────────────────────────────────

    #[test]
    fn agent_name_valid_ascii_alphanum_hyphen_underscore() {
        assert!(AgentName::new("claude").is_ok());
        assert!(AgentName::new("claude-3-5").is_ok());
        assert!(AgentName::new("my_agent_v2").is_ok());
        assert!(AgentName::new("a").is_ok());
        assert!(AgentName::new("A1_B-C").is_ok());
    }

    #[test]
    fn agent_name_empty_returns_invalid_agent_name_error() {
        let err = AgentName::new("").unwrap_err();
        assert!(matches!(err, DataError::InvalidAgentName { .. }));
    }

    #[test]
    fn agent_name_too_long_returns_error() {
        let long = "a".repeat(65);
        let err = AgentName::new(long).unwrap_err();
        assert!(matches!(err, DataError::InvalidAgentName { .. }));
    }

    #[test]
    fn agent_name_exactly_64_chars_is_valid() {
        let exactly_64 = "a".repeat(64);
        assert!(AgentName::new(exactly_64).is_ok());
    }

    #[test]
    fn agent_name_invalid_char_space_returns_error() {
        let err = AgentName::new("my agent").unwrap_err();
        assert!(matches!(err, DataError::InvalidAgentName { .. }));
    }

    #[test]
    fn agent_name_invalid_char_dot_returns_error() {
        let err = AgentName::new("my.agent").unwrap_err();
        assert!(matches!(err, DataError::InvalidAgentName { .. }));
    }

    #[test]
    fn agent_name_display_matches_inner_string() {
        let name = AgentName::new("my-agent").unwrap();
        assert_eq!(name.to_string(), "my-agent");
        assert_eq!(name.as_str(), "my-agent");
    }

    // ─── SessionId tests ──────────────────────────────────────────────────────

    #[test]
    fn session_id_new_generates_unique_values() {
        let id1 = SessionId::new();
        let id2 = SessionId::new();
        assert_ne!(id1, id2);
    }

    #[test]
    fn session_id_from_uuid_round_trips() {
        let uuid = uuid::Uuid::new_v4();
        let id = SessionId::from_uuid(uuid);
        assert_eq!(id.as_uuid(), uuid);
    }

    #[test]
    fn session_id_display_is_uuid_format() {
        let id = SessionId::new();
        let s = id.to_string();
        assert_eq!(s.len(), 36); // standard UUID: 8-4-4-4-12 with hyphens
        assert!(s.chars().all(|c| c.is_ascii_hexdigit() || c == '-'));
    }

    // ─── Session::open tests ─────────────────────────────────────────────────

    #[test]
    fn session_open_with_static_resolver_returns_expected_fields() {
        let setup = IsolatedSetup::new();
        let session = setup.open_session();
        assert_eq!(session.git_root(), setup.git_root.path());
        assert_eq!(session.working_dir(), setup.git_root.path());
        // Each call produces a fresh session with a new ID.
        let session2 = setup.open_session();
        assert_ne!(session.id(), session2.id());
    }

    #[test]
    fn session_open_propagates_git_root_not_found() {
        let setup = IsolatedSetup::new();
        let resolver = FailingGitRootResolver;
        let opts = SessionOpenOptions {
            env: Some(setup.env()),
            ..Default::default()
        };
        let err = Session::open(setup.git_root.path().to_path_buf(), &resolver, opts).unwrap_err();
        assert!(
            matches!(err, DataError::GitRootNotFound { .. }),
            "expected GitRootNotFound, got {err:?}"
        );
    }

    #[test]
    fn session_state_mut_permits_mutation_visible_via_state() {
        let setup = IsolatedSetup::new();
        let mut session = setup.open_session();
        assert!(session.state().errors.is_empty());

        session.state_mut().record_error("something went wrong");

        assert_eq!(session.state().errors.len(), 1);
        assert_eq!(session.state().errors[0].message, "something went wrong");
    }

    #[test]
    fn session_state_is_read_only_accessor() {
        let setup = IsolatedSetup::new();
        let session = setup.open_session();
        // `state()` returns `&SessionState` — verify it's accessible without mut.
        let _state: &SessionState = session.state();
    }

    #[test]
    fn session_with_malformed_repo_config_returns_config_parse_error() {
        let setup = IsolatedSetup::new();
        // Write broken JSON to the repo config file.
        let awman_dir = setup.git_root.path().join(REPO_CONFIG_SUBDIR);
        std::fs::create_dir_all(&awman_dir).unwrap();
        std::fs::write(awman_dir.join("config.json"), b"{this is not json}").unwrap();

        let resolver = StaticGitRootResolver::new(setup.git_root.path());
        let opts = SessionOpenOptions {
            env: Some(setup.env()),
            ..Default::default()
        };
        let err = Session::open(setup.git_root.path().to_path_buf(), &resolver, opts).unwrap_err();
        assert!(
            matches!(err, DataError::ConfigParse { .. }),
            "expected ConfigParse, got {err:?}"
        );
    }

    #[test]
    fn session_flags_override_default_agent() {
        let setup = IsolatedSetup::new();
        let flags = FlagConfig {
            agent: Some("flag-agent".to_string()),
            ..Default::default()
        };
        let session = setup.open_session_with_opts(flags);
        assert_eq!(
            session.default_agent().map(|a| a.as_str()),
            Some("flag-agent")
        );
    }

    // ─── Layer-0-internal integration: Config + Session round-trip ───────────

    #[test]
    fn session_open_merges_repo_and_global_config_correctly() {
        let git_tmp = tempfile::tempdir().unwrap();
        let home_tmp = tempfile::tempdir().unwrap();

        // Write repo config: sets agent and scrollback.
        let awman_dir = git_tmp.path().join(REPO_CONFIG_SUBDIR);
        std::fs::create_dir_all(&awman_dir).unwrap();
        std::fs::write(
            awman_dir.join("config.json"),
            r#"{"agent":"codex","terminal_scrollback_lines":7777}"#,
        )
        .unwrap();

        // Write global config: sets a different agent (should lose to repo) and scrollback.
        std::fs::write(
            home_tmp.path().join("config.json"),
            r#"{"default_agent":"claude","terminal_scrollback_lines":2000}"#,
        )
        .unwrap();

        let env =
            EnvSnapshot::with_overrides([(AWMAN_CONFIG_HOME, home_tmp.path().to_str().unwrap())]);
        let resolver = StaticGitRootResolver::new(git_tmp.path());
        let opts = SessionOpenOptions {
            env: Some(env),
            ..Default::default()
        };
        let session = Session::open(git_tmp.path().to_path_buf(), &resolver, opts).unwrap();

        // Repo agent wins over global.
        assert_eq!(session.default_agent().map(|a| a.as_str()), Some("codex"));
        // EffectiveConfig reflects repo scrollback win.
        let ec = session.effective_config();
        assert_eq!(ec.scrollback_lines(), 7777);
        // Both raw configs are accessible.
        assert_eq!(session.repo_config().agent.as_deref(), Some("codex"));
        assert_eq!(
            session.global_config().default_agent.as_deref(),
            Some("claude")
        );
    }
}
