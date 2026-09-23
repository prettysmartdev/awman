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
use crate::data::fs::SquadPaths;
use crate::data::workflow_state::WorkflowSummary;

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
pub struct AgentHandle {
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
///
/// Decision Q3 makes this the ruling in-flight state: commands write
/// `current_command` / `current_workflow` / `current_container` through the
/// [`Session`] they own, and a frontend view — the TUI's tab — derives from it
/// rather than keeping a parallel copy (WI 0114 F-30/F-22).
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionState {
    pub current_command: Option<CommandInvocation>,
    /// The workflow running in this session right now, summarised.
    ///
    /// A projection of the `WorkflowState` the engine owns, refreshed
    /// whenever the engine persists. The run's authoritative state is never
    /// here: this exists so a session view can render without loading a state
    /// file, which is what the deleted `WorkflowInvocation` — a second,
    /// never-written workflow model — was trying and failing to be.
    pub current_workflow: Option<WorkflowSummary>,
    pub current_container: Option<AgentHandle>,
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

    // ── In-flight transitions ───────────────────────────────────────────────
    //
    // Decision Q3 makes this state the ruling record of what a session is
    // doing. Every write goes through one of the methods below so the rules —
    // what a status means, what a terminal command keeps, when the container
    // handle is dropped — are stated once, in Layer 0, and a frontend deriving
    // a view from this state can never disagree with the command that wrote it
    // (WI 0114 F-22).

    /// Record that `subcommand` has started running in this session.
    ///
    /// Idempotent for the same command: the TUI records the start
    /// synchronously so the tab never renders an idle frame between spawning a
    /// command and the command thread reaching `Dispatch::run_command`, which
    /// records it again.
    pub fn begin_command(&mut self, subcommand: impl Into<String>, args: Vec<String>) {
        let subcommand = subcommand.into();
        if let Some(existing) = self.current_command.as_mut() {
            if existing.status == CommandStatus::Running
                && existing.subcommand == subcommand
                && existing.args == args
            {
                return;
            }
        }
        self.current_command = Some(CommandInvocation {
            id: Uuid::new_v4(),
            subcommand,
            args,
            status: CommandStatus::Running,
            exit_code: None,
            started_at: chrono::Utc::now(),
            finished_at: None,
        });
    }

    /// Record that the running command finished with `exit_code`.
    ///
    /// The invocation is kept rather than cleared: a frontend shows what just
    /// ran and how it ended until the next command replaces it. Does nothing
    /// when no command is recorded.
    pub fn finish_command(&mut self, exit_code: i32) {
        if let Some(cmd) = self.current_command.as_mut() {
            cmd.status = CommandStatus::Done;
            cmd.exit_code = Some(exit_code);
            cmd.finished_at = Some(chrono::Utc::now());
        }
        self.current_container = None;
    }

    /// Record that the running command failed before producing an exit code.
    pub fn fail_command(&mut self, message: impl Into<String>) {
        let message = message.into();
        if let Some(cmd) = self.current_command.as_mut() {
            cmd.status = CommandStatus::Error(message);
            cmd.finished_at = Some(chrono::Utc::now());
        }
        self.current_container = None;
    }

    /// Mirror the live workflow run's summary. `None` clears it when no
    /// workflow is running.
    pub fn set_current_workflow(&mut self, summary: Option<WorkflowSummary>) {
        self.current_workflow = summary;
    }

    /// Record the agent container this session is currently running.
    pub fn set_current_container(&mut self, container: Option<AgentHandle>) {
        self.current_container = container;
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

/// Which kind of session this is, with no payload.
///
/// The discriminant of [`SessionType`], usable where only the kind matters:
/// the `type` field on the API's create-session body, the `session_type`
/// column, and the catalogue's `--type` flag, all of which carried it as a
/// free-form `String` matched with `as_str()` in three places (WI 0114 F-48).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionKind {
    Local,
    Remote,
}

impl SessionKind {
    /// Every kind, in the order the `--type` flag lists them.
    pub const ALL: &'static [SessionKind] = &[SessionKind::Local, SessionKind::Remote];

    /// The serialised spelling — the wire value and the database value.
    pub fn as_str(self) -> &'static str {
        match self {
            SessionKind::Local => "local",
            SessionKind::Remote => "remote",
        }
    }
}

impl std::fmt::Display for SessionKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for SessionKind {
    type Err = DataError;

    /// Case-insensitive, matching what the API accepted when this was a
    /// lowercased `String` comparison.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "local" => Ok(SessionKind::Local),
            "remote" => Ok(SessionKind::Remote),
            other => Err(DataError::Other(format!(
                "unknown session type '{other}'; expected one of {}",
                SessionKind::ALL
                    .iter()
                    .map(|k| k.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))),
        }
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
    /// This session's kind, without its payload.
    pub fn kind(&self) -> SessionKind {
        match self {
            SessionType::Local { .. } => SessionKind::Local,
            SessionType::Remote { .. } => SessionKind::Remote,
        }
    }

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
    /// Whether `git_root` is a real repository root, as the `GitRootResolver`
    /// answered at open time.
    ///
    /// `false` only for a session opened over a directory git does not know
    /// about (`open_or_workdir_fallback`) or over the squad storage root.
    /// Recorded here rather than probed for later: the resolver is the one
    /// place that decides, a `.git` probe is wrong for a worktree or a
    /// submodule, and a frontend must not be the layer asking.
    is_git_repo: bool,
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
        Self::open_resolved(working_dir, git_root, opts, true)
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
                Self::open_resolved(working_dir.clone(), working_dir, opts, false)
            }
            Err(other) => Err(other),
        }
    }

    /// Open the session the squad view is rooted at.
    ///
    /// The squad daemon owns tasks, not repositories, so there is no git root
    /// to resolve: the session exists to satisfy the frontends' `Session` API
    /// and is rooted at the squad storage root, which is created if it is not
    /// there yet. Nothing a squad view renders derives from it.
    pub fn open_squad_root(env: &EnvSnapshot) -> Result<Self, DataError> {
        let root = SquadPaths::from_env(env)?.root().to_path_buf();
        std::fs::create_dir_all(&root).map_err(|source| DataError::io(&root, source))?;
        Self::open_resolved(
            root.clone(),
            root,
            SessionOpenOptions {
                env: Some(env.clone()),
                ..Default::default()
            },
            false,
        )
    }

    /// Open a session with an explicit, pre-resolved git root.
    ///
    /// The caller supplying a git root asserts that it is one; the two callers
    /// that know otherwise ([`Session::open_or_workdir_fallback`]'s fallback
    /// arm and [`Session::open_squad_root`]) go through `open_resolved`.
    pub fn open_at_git_root(
        working_dir: PathBuf,
        git_root: PathBuf,
        opts: SessionOpenOptions,
    ) -> Result<Self, DataError> {
        Self::open_resolved(working_dir, git_root, opts, true)
    }

    /// The one constructor. `is_git_repo` records what the caller's resolution
    /// actually established, so nothing downstream has to probe for it.
    fn open_resolved(
        working_dir: PathBuf,
        git_root: PathBuf,
        opts: SessionOpenOptions,
        is_git_repo: bool,
    ) -> Result<Self, DataError> {
        let env = opts.env.unwrap_or_else(EnvSnapshot::empty);
        let repo_config = RepoConfig::load(&git_root)?;
        let global_config = GlobalConfig::load_with(&env)?;

        // One agent-precedence rule, and it is `EffectiveConfig`'s
        // (flag > repo.agent > global.default_agent). This used to be
        // re-encoded here as `resolve_default_agent`, which is exactly the
        // drift Layer 0 is supposed to prevent (WI 0114 F-30).
        let default_agent = EffectiveConfig::new(
            opts.flags.clone(),
            env.clone(),
            repo_config.clone(),
            global_config.clone(),
        )
        .agent()
        .map(|name| AgentName::new(&name))
        .transpose()?;
        let available_agents = opts.available_agents.unwrap_or_default();

        let now = SystemTime::now();

        Ok(Self {
            id: SessionId::new(),
            session_type: SessionType::Local {
                workdir: working_dir,
            },
            git_root,
            is_git_repo,
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

    /// Whether this session is rooted at a real git repository.
    ///
    /// Answered from the `GitRootResolver` outcome captured when the session
    /// was opened. Frontends used to probe `git_root().join(".git").exists()`
    /// to decide which command to start a tab with (WI 0114 F-21); that probe
    /// is both a Layer 3 decision and wrong for a linked worktree, whose
    /// `.git` is a file, and for a bare or submodule checkout.
    pub fn is_git_repo(&self) -> bool {
        self.is_git_repo
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

/// The one way a unit test builds a `Session` (WI 0114 F-52).
///
/// Twenty-three copies of a private `make_session` used to stand here, one per
/// test module, and they had drifted: some pinned `AWMAN_CONFIG_HOME` at a
/// temp dir and some did not, so whether a test could fall through to the
/// developer's real `~/.awman/config.json` depended on which module it lived
/// in. These three constructors cover every shape those copies had between
/// them; a test that needs something else builds `SessionOpenOptions` itself
/// and says why.
///
/// `#[cfg(test)]` because the equivalent for an *integration* test is
/// `tests/helpers/mod.rs` — `IsolatedEnv::open_session` and `session_at`.
#[cfg(test)]
impl Session {
    /// A session whose working directory and Git root are both `root`.
    ///
    /// The env snapshot is the process's, so a test that reads configuration
    /// wants [`Session::for_tests_isolated`] instead.
    pub(crate) fn for_tests(root: &Path) -> Self {
        Self::for_tests_with_options(root, SessionOpenOptions::default())
    }

    /// As [`Session::for_tests`], but with `AWMAN_CONFIG_HOME` pinned at
    /// `config_home` so the session cannot read the developer's real global
    /// config. Any test asserting on a config *source* must use this.
    pub(crate) fn for_tests_isolated(root: &Path, config_home: &Path) -> Self {
        Self::for_tests_with_env(
            root,
            EnvSnapshot::with_overrides([(
                crate::data::config::env::AWMAN_CONFIG_HOME,
                config_home.to_str().expect("fixture path must be UTF-8"),
            )]),
        )
    }

    /// As [`Session::for_tests`], with a caller-supplied env snapshot.
    pub(crate) fn for_tests_with_env(root: &Path, env: EnvSnapshot) -> Self {
        Self::for_tests_with_options(
            root,
            SessionOpenOptions {
                env: Some(env),
                ..Default::default()
            },
        )
    }

    /// The general case, for a fixture none of the three above fits.
    pub(crate) fn for_tests_with_options(root: &Path, opts: SessionOpenOptions) -> Self {
        Self::open_at_git_root(root.to_path_buf(), root.to_path_buf(), opts)
            .expect("Session::for_tests must open at a fixture root")
    }
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

    /// F-30's regression guard: `Session::default_agent` and
    /// `EffectiveConfig::agent` are the same rule, not two copies of it. If a
    /// second precedence chain is ever reintroduced in `Session::open`, one of
    /// these rows disagrees.
    #[test]
    fn session_default_agent_never_diverges_from_effective_config() {
        let cases: [(Option<&str>, Option<&str>, Option<&str>); 6] = [
            // (flag, repo, global)
            (Some("flag"), Some("repo"), Some("global")),
            (None, Some("repo"), Some("global")),
            (None, None, Some("global")),
            (None, None, None),
            (Some("flag"), None, None),
            (None, Some("repo"), None),
        ];

        for (flag, repo_agent, global_agent) in cases {
            let git_tmp = tempfile::tempdir().unwrap();
            let home_tmp = tempfile::tempdir().unwrap();

            if let Some(a) = repo_agent {
                let awman_dir = git_tmp.path().join(REPO_CONFIG_SUBDIR);
                std::fs::create_dir_all(&awman_dir).unwrap();
                std::fs::write(
                    awman_dir.join("config.json"),
                    format!(r#"{{"agent":"{a}"}}"#),
                )
                .unwrap();
            }
            if let Some(a) = global_agent {
                std::fs::write(
                    home_tmp.path().join("config.json"),
                    format!(r#"{{"default_agent":"{a}"}}"#),
                )
                .unwrap();
            }

            let env = EnvSnapshot::with_overrides([(
                AWMAN_CONFIG_HOME,
                home_tmp.path().to_str().unwrap(),
            )]);
            let resolver = StaticGitRootResolver::new(git_tmp.path());
            let opts = SessionOpenOptions {
                env: Some(env),
                flags: FlagConfig {
                    agent: flag.map(str::to_string),
                    ..Default::default()
                },
                ..Default::default()
            };
            let session = Session::open(git_tmp.path().to_path_buf(), &resolver, opts).unwrap();

            assert_eq!(
                session.default_agent().map(|a| a.as_str().to_string()),
                session.effective_config().agent(),
                "flag={flag:?} repo={repo_agent:?} global={global_agent:?}"
            );
        }
    }
}

#[cfg(test)]
mod session_state_command_tests {
    use super::*;

    /// Finishing a command drops the container handle: nothing is running any
    /// more, so a view must not keep drawing one.
    #[test]
    fn finishing_a_command_clears_the_current_container() {
        let mut state = SessionState::new();
        state.begin_command("chat", vec![]);
        state.set_current_container(Some(AgentHandle {
            id: "abc123".into(),
            image_tag: "awman-claude:latest".into(),
            name: "awman-chat".into(),
            started_at: chrono::Utc::now(),
        }));
        assert!(state.current_container.is_some());

        state.finish_command(0);
        assert!(state.current_container.is_none());
    }

    /// The synchronous start the TUI records and the one
    /// `Dispatch::run_command` records on the command thread must not produce
    /// two invocations.
    #[test]
    fn begin_command_is_idempotent_for_the_same_command() {
        let mut state = SessionState::new();
        state.begin_command("chat", vec!["--agent".into(), "claude".into()]);
        let first_id = state.current_command.as_ref().unwrap().id;

        state.begin_command("chat", vec!["--agent".into(), "claude".into()]);
        assert_eq!(
            state.current_command.as_ref().unwrap().id,
            first_id,
            "re-recording the same running command must not start a new invocation"
        );

        state.begin_command("ready", vec![]);
        assert_ne!(
            state.current_command.as_ref().unwrap().id,
            first_id,
            "a different command does start a new invocation"
        );
    }
}
