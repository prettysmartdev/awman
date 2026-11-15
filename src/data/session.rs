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

use crate::data::config::effective::{env_overrides, ConfigLayers, EffectiveConfig};
use crate::data::config::env::EnvSnapshot;
use crate::data::config::file::ConfigFile;
use crate::data::config::flags::FlagConfig;
use crate::data::config::overrides::ConfigOverrides;
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

/// Lifecycle state of one in-memory [`CommandInvocation`]. API command rows use
/// [`crate::data::command_status::CommandStatus`] instead.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum InvocationStatus {
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
    pub status: InvocationStatus,
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
    #[serde(default)]
    pub workflow_prerequisites:
        Option<crate::data::workflow_prerequisites::WorkflowPrerequisiteReport>,
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
    pub fn record_workflow_prerequisites(
        &mut self,
        report: crate::data::workflow_prerequisites::WorkflowPrerequisiteReport,
    ) {
        self.workflow_prerequisites = Some(report);
    }
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
            if existing.status == InvocationStatus::Running
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
            status: InvocationStatus::Running,
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
            cmd.status = InvocationStatus::Done;
            cmd.exit_code = Some(exit_code);
            cmd.finished_at = Some(chrono::Utc::now());
        }
        self.current_container = None;
    }

    /// Record that the running command failed before producing an exit code.
    pub fn fail_command(&mut self, message: impl Into<String>) {
        let message = message.into();
        if let Some(cmd) = self.current_command.as_mut() {
            cmd.status = InvocationStatus::Error(message);
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
    repo_context: RepoContextBinding,
    id: SessionId,
    inline_work_item: Option<PathBuf>,
    /// The typed form of a pushed API inline work item (title and stable
    /// source id); its body is `inline_work_item`. `None` for the path-only
    /// pulled-lease input.
    staged_work_item: Option<crate::data::work_item::StagedInlineWorkItem>,
    session_type: SessionType,
    git_root: PathBuf,
    definition_root: PathBuf,
    /// Whether `git_root` is a real repository root, as the `GitRootResolver`
    /// answered at open time.
    ///
    /// `false` for a session opened over a plain directory or the squad
    /// storage root.
    /// Recorded here rather than probed for later: the resolver is the one
    /// place that decides, a `.git` probe is wrong for a worktree or a
    /// submodule, and a frontend must not be the layer asking.
    is_git_repo: bool,
    /// The merged config (global → repo → env → `--set`), resolved and
    /// validated at open and before each command. [`Session::effective_config`]
    /// pairs it with the current flags and env snapshot.
    config: EffectiveConfig,
    env: EnvSnapshot,
    flags: FlagConfig,
    default_agent: Option<AgentName>,
    available_agents: Vec<AgentName>,
    state: SessionState,
    created_at: SystemTime,
    last_active_at: SystemTime,
    created_at_instant: Instant,
}

/// The agent a session runs when no flag or config names one.
pub const FALLBACK_AGENT: &str = "claude";

/// Builder-style options for constructing a `Session`.
#[derive(Debug, Default, Clone)]
pub struct SessionOpenOptions {
    pub flags: FlagConfig,
    pub env: Option<EnvSnapshot>,
    pub available_agents: Option<Vec<AgentName>>,
    /// `--set key=value` overrides, already resolved against the key table.
    pub overrides: ConfigOverrides,
}

impl Session {
    pub fn prerequisite_policy(&self) -> crate::data::config::workflows::PrerequisitePolicy {
        self.effective_config().prerequisite_policy(
            crate::data::config::workflows::PrerequisiteResolveContext {
                remote_session: self.session_type().is_remote(),
            },
        )
    }
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
    /// The caller supplying a git root asserts that it is one. Plain
    /// directories use [`Session::open_at_directory_root`] instead.
    pub fn open_at_git_root(
        working_dir: PathBuf,
        git_root: PathBuf,
        opts: SessionOpenOptions,
    ) -> Result<Self, DataError> {
        Self::open_resolved(working_dir, git_root, opts, true)
    }

    /// Open a session rooted at a plain directory with no Git repository.
    pub fn open_at_directory_root(
        root: PathBuf,
        opts: SessionOpenOptions,
    ) -> Result<Self, DataError> {
        Self::open_resolved(root.clone(), root, opts, false)
    }

    /// Open a session re-rooted at a workflow worktree on behalf of `invoker`.
    ///
    /// The worktree checkout is cut from a commit, so its `.awman/config.json`
    /// lacks the invoker's uncommitted edits ("use last commit", or a resumed
    /// older worktree, make that common). Some settings must still be the
    /// invoker's:
    /// - the repo `builtin` block, whose network, vCPU and memory narrowing is
    ///   a security control;
    /// - the repo `agent`, which decides the agent a step runs when it names
    ///   none (and so which agent `--agent-flag` targets);
    /// - the repo `workItems` block, so references, providers and the draft
    ///   template resolve exactly as they did for the invoking checkout;
    /// - the env, flags and global config, so a relocated `AWMAN_HOME`
    ///   is honoured exactly as the invoker honoured it.
    /// - the definition root, so uncommitted agent definitions and their
    ///   approvals follow every step into the worktree.
    ///
    /// Everything else comes from the worktree checkout.
    pub fn open_worktree_for(
        working_dir: PathBuf,
        git_root: PathBuf,
        invoker: &Session,
    ) -> Result<Self, DataError> {
        let mut session = Self::open_at_git_root(
            working_dir,
            git_root,
            SessionOpenOptions {
                flags: invoker.flags.clone(),
                env: Some(invoker.env.clone()),
                available_agents: Some(invoker.available_agents.clone()),
                overrides: invoker.config.layers().set.clone(),
            },
        )?;
        // The invoker's repo `runtime.builtin` block and `agents.default`, and
        // its global file and overrides, replace the checkout's.
        let mut layers = session.config.layers().clone();
        let invoker_layers = invoker.config.layers();
        let invoker_builtin = invoker_layers
            .repo
            .runtime
            .as_ref()
            .and_then(|r| r.builtin.clone());
        if invoker_builtin.is_some() || layers.repo.runtime.is_some() {
            layers
                .repo
                .runtime
                .get_or_insert_with(Default::default)
                .builtin = invoker_builtin;
        }
        let invoker_agent = invoker_layers
            .repo
            .agents
            .as_ref()
            .and_then(|a| a.default.clone());
        if invoker_agent.is_some() || layers.repo.agents.is_some() {
            layers
                .repo
                .agents
                .get_or_insert_with(Default::default)
                .default = invoker_agent;
        }
        // Work item settings (source, providers, dir, template) are the
        // invoker's: a branch checkout must not redirect where drafts are
        // published or which tracker a step's references resolve against.
        layers.repo.work_items = invoker_layers.repo.work_items.clone();
        layers.global = invoker_layers.global.clone();
        layers.env = invoker_layers.env.clone();
        session.config =
            EffectiveConfig::resolve(layers, session.flags.clone(), session.env.clone())?;
        session.definition_root = invoker.definition_root.clone();
        session.repo_context = invoker.repo_context.clone();
        session.default_agent = invoker.default_agent.clone();
        Ok(session)
    }

    /// Re-root a validated session without reading configuration from the target.
    ///
    /// Branch/PR checkouts can contain untrusted repo configuration. Preserve
    /// the complete invoker snapshot and definition provenance, with a fresh
    /// identity and execution state. Workflow sessions retain their separate
    /// `open_worktree_for` behavior.
    pub fn open_worktree_with_invoker_config(
        working_dir: PathBuf,
        git_root: PathBuf,
        invoker: &Session,
    ) -> Result<Self, DataError> {
        let now = SystemTime::now();
        Ok(Self {
            id: SessionId::new(),
            inline_work_item: invoker.inline_work_item.clone(),
            staged_work_item: invoker.staged_work_item.clone(),
            repo_context: invoker.repo_context.clone(),
            session_type: SessionType::Local {
                workdir: working_dir,
            },
            git_root,
            definition_root: invoker.definition_root.clone(),
            is_git_repo: true,
            config: invoker.config.clone(),
            env: invoker.env.clone(),
            flags: invoker.flags.clone(),
            default_agent: invoker.default_agent.clone(),
            available_agents: invoker.available_agents.clone(),
            state: SessionState::new(),
            created_at: now,
            last_active_at: now,
            created_at_instant: Instant::now(),
        })
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
        let layers = ConfigLayers::load(&env, Some(&git_root), opts.overrides.clone())?;
        let config = EffectiveConfig::resolve(layers, opts.flags.clone(), env.clone())?;

        // One agent-precedence rule, and it is `EffectiveConfig`'s
        // (flag > repo.agent > global.default_agent). This used to be
        // re-encoded here as `resolve_default_agent`, which is exactly the
        // drift Layer 0 is supposed to prevent (WI 0114 F-30).
        let default_agent = config
            .agent()
            .map(|name| AgentName::new(&name))
            .transpose()?;
        let available_agents = opts.available_agents.unwrap_or_default();

        let now = SystemTime::now();

        Ok(Self {
            id: SessionId::new(),
            inline_work_item: None,
            staged_work_item: None,
            repo_context: RepoContextBinding::Inferred,
            session_type: SessionType::Local {
                workdir: working_dir,
            },
            definition_root: git_root.clone(),
            git_root,
            is_git_repo,
            config,
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

    pub fn set_inline_work_item(&mut self, path: PathBuf) {
        self.inline_work_item = Some(path);
    }
    /// Attach a pushed inline work item with its caller-supplied title and
    /// stable source id, so the resolver never has to synthesize them.
    pub fn set_staged_work_item(&mut self, staged: crate::data::work_item::StagedInlineWorkItem) {
        self.inline_work_item = Some(staged.body_path.clone());
        self.staged_work_item = Some(staged);
    }
    pub fn staged_work_item(&self) -> Option<&crate::data::work_item::StagedInlineWorkItem> {
        self.staged_work_item.as_ref()
    }
    pub fn inline_work_item(&self) -> Option<&Path> {
        self.inline_work_item.as_deref()
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

    /// Repository whose agent definitions and approvals govern this session.
    pub fn definition_root(&self) -> &Path {
        &self.definition_root
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

    /// The merged config file (defaults applied).
    pub fn config(&self) -> &ConfigFile {
        self.config.config()
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

    /// The agent this session runs when nothing names one: its configured
    /// default agent (flag > repo > global), else `claude`. `ready` prepares
    /// this agent's image as the required one; every other agent's image is
    /// optional and may not reuse a single-image default source.
    pub fn configured_agent(&self) -> AgentName {
        self.default_agent.clone().unwrap_or_else(|| {
            AgentName::new(FALLBACK_AGENT).expect("fallback agent name is valid")
        })
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

    /// Reload file-backed config and resolve the complete layer stack for
    /// the next command. Keep invocation overrides and an installed task
    /// layer; replace the snapshot only after every file and the merge pass
    /// validation, so a bad edit cannot partially update this session.
    pub fn reload_config(&mut self) -> Result<(), DataError> {
        let previous = self.config.layers();
        let mut layers = ConfigLayers::load(&self.env, Some(&self.git_root), previous.set.clone())?;
        layers.task = previous.task.clone();
        if let Some(task) = &layers.task {
            layers.env = env_overrides(&self.env, &[&layers.global, &layers.repo, task])?;
        }
        let config = EffectiveConfig::resolve(layers, self.flags.clone(), self.env.clone())?;
        self.replace_config(config)
    }

    fn replace_config(&mut self, config: EffectiveConfig) -> Result<(), DataError> {
        let default_agent = config
            .agent()
            .map(|name| AgentName::new(&name))
            .transpose()?;
        self.config = config;
        self.default_agent = default_agent;
        Ok(())
    }

    /// Add `--set`-layer overrides (applied after any already present) and
    /// re-resolve the merged config.
    pub fn add_overrides(&mut self, overrides: ConfigOverrides) -> Result<(), DataError> {
        if overrides.is_empty() {
            return Ok(());
        }
        let mut layers = self.config.layers().clone();
        layers.set.0.extend(overrides.0);
        self.replace_config(EffectiveConfig::resolve(
            layers,
            self.flags.clone(),
            self.env.clone(),
        )?)
    }

    /// Install a squad task's config file as the task layer (above global
    /// and repo, below env and `--set`) and re-resolve. Without this a task's
    /// restrictions, overlays, concurrency and prerequisite denials are
    /// accepted by validation but never reach the session that runs it.
    pub fn set_task_config(
        &mut self,
        task: crate::data::config::ConfigFile,
    ) -> Result<(), DataError> {
        let mut layers = self.config.layers().clone();
        layers.task = Some(task);
        self.replace_config(EffectiveConfig::resolve(
            layers,
            self.flags.clone(),
            self.env.clone(),
        )?)
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
        self.config
            .clone()
            .with_flags(self.flags.clone())
            .with_env(self.env.clone())
    }
}

/// The repository context selected for this session across all frontends.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum RepoContextBinding {
    #[default]
    Inferred,
    Keyed(crate::data::context::ContextKey),
    Staged(std::path::PathBuf),
}
impl Session {
    pub fn repo_context(&self) -> &RepoContextBinding {
        &self.repo_context
    }
    pub fn set_repo_context(&mut self, binding: RepoContextBinding) {
        self.repo_context = binding;
    }
}
