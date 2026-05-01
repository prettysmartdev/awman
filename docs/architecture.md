# amux Architecture

## Overview

amux has two coexisting source trees:

- **`src/`** — the new five-layer architecture (in progress; Layer 0 complete). The `amux-next` binary is built from here.
- **`oldsrc/`** — the frozen pre-refactor source. The user-facing `amux` binary continues to build from here until the refactor is complete in work item 0070.

The `oldsrc/` tree is frozen: no edits are allowed. It will be deleted when `amux-next` reaches full parity in work item 0070. The rest of this document covers both trees: the new layered architecture first, then the legacy architecture that currently ships to users.

---

## Grand Architecture Refactor

### Purpose

amux grew into three execution modes (CLI, TUI, headless) that share the same core functionality but implement it separately, producing subtle behavioural drift and making parity across modes hard to guarantee. The grand architecture refactor replaces this with a strict five-layer system where every frontend is a thin presentation shell over a shared, tested core.

### Tenets

1. **No upward calls.** Lower layers never call functions or use types from higher layers. If a lower layer needs to delegate upward, it defines a trait that a higher layer implements.
2. **Frontends are dumb.** No frontend (CLI, TUI, headless) may implement business logic. All logic lives in Layer 2 (`command`) or below.
3. **Typed objects over free functions.** Every significant abstraction is a struct with methods. Free `pub fn` is acceptable only for stateless helpers, constructors, and one-off utilities.

### Layers

```
Layer 4: binary    main.rs — sets up frontends, delegates everything
Layer 3: frontend  CLI, TUI, Headless — input/output only
Layer 2: command   Dispatch, per-command business logic
Layer 1: engine    ContainerRuntime, WorkflowEngine, GitEngine, OverlayEngine, AuthEngine
Layer 0: data      Session, config, filesystem, database, typed data
```

**Layer 0 (data)** owns every data definition, config concern, filesystem access, and database interaction. No business logic, no container calls, no git operations, no workflow execution. See [Layer 0 reference](#layer-0-data-srcdata) below.

**Layer 1 (engine)** owns core runtime primitives: container lifecycle, workflow execution, git operations, overlay construction, authentication, agent management, and the multi-phase `ready`/`init`/`claws` engines. See [Layer 1 reference](#layer-1-engine-srcengine) below.

**Layer 2 (command)** owns higher-level business logic: the `Dispatch` type that routes input to typed command objects, and command-specific types (`ChatCommand`, `InitCommand`, etc.). Implemented in work item 0068.

**Layer 3 (frontend)** contains the CLI, TUI, and headless server. Each is a presentation layer only: it translates user input into `Dispatch` calls and renders command output. Implemented in work item 0069.

**Layer 4 (binary)** is `src/main.rs` — currently a stub. It becomes the real entrypoint in work item 0069.

### Current Status

| Layer | Location | Status |
|-------|----------|--------|
| 0 — data | `src/data/` | Complete (work item 0066) |
| 1 — engine | `src/engine/` | Complete (work item 0067) |
| 2 — command | `src/command/` | Stub — populated in 0068 |
| 3 — frontend | `src/frontend/` | Stub — populated in 0069 |
| 4 — binary | `src/main.rs` | Stub — wired in 0069 |
| Legacy binary | `oldsrc/` | Frozen, ships to users |

---

## New Source Tree (`src/`)

```
src/
  main.rs                 Layer 4 stub (amux-next binary)
  lib.rs                  Re-exports the four layers
  data/                   Layer 0 — fully implemented
    mod.rs
    session.rs            Session, SessionState, SessionId, AgentName, …
    session_manager.rs    SessionManager, SessionStore, InMemorySessionStore
    error.rs              DataError
    workflow_dag.rs       WorkflowDag, validate_references, detect_cycle
    workflow_definition.rs  Workflow, WorkflowStep, format detection
    workflow_state.rs     WorkflowState, StepState, WORKFLOW_STATE_SCHEMA_VERSION
    workflow_state_store.rs WorkflowStateStore (git-root-scoped persistence)
    workflow_prompt_template.rs  Prompt-template substitution
    worktree_paths.rs     WorktreePaths, worktree_branch_name helpers
    config/
      mod.rs
      repo.rs             RepoConfig and related types
      global.rs           GlobalConfig
      env.rs              EnvSnapshot, Env, env var constants
      flags.rs            FlagConfig
      effective.rs        EffectiveConfig (merged view)
    fs/
      mod.rs
      headless_db.rs      SqliteSessionStore, SessionRecord, CommandRecord
      headless_paths.rs   HeadlessPaths
      workflow_state.rs   WorkflowStateStore (legacy alias kept for compat)
      skill_dirs.rs       SkillDirs
      workflow_dirs.rs    WorkflowDirs
      overlay_paths.rs    OverlayPathResolver
      auth_paths.rs       AuthPathResolver, AgentAuthPaths
  engine/                 Layer 1 — fully implemented
    mod.rs                Re-exports: EngineError, UserMessage*, StepStatus
    error.rs              EngineError
    message.rs            UserMessage, MessageLevel, UserMessageSink, RecordingMessageSink
    step_status.rs        StepStatus (shared by ReadyEngine, InitEngine, ClawsEngine)
    container/
      mod.rs              Re-exports: ContainerRuntime, ContainerOption*, ContainerFrontend, …
      runtime.rs          ContainerRuntime::detect / build / list_running / stats / stop
      options.rs          ContainerOption enum + surrounding types (ImageRef, Entrypoint, …)
      instance.rs         ContainerInstance trait, ContainerExecution, ContainerExitInfo
      frontend.rs         ContainerFrontend trait (defined by Layer 1, implemented by Layer 3)
      backend.rs          ContainerBackend trait (pub(super) — opaque to callers)
      docker.rs           DockerBackend (pub(super))
      apple.rs            AppleBackend (pub(super); macOS only)
      naming.rs           generate_container_name()
    workflow/
      mod.rs              WorkflowEngine struct + all public methods
      actions.rs          NextAction, AvailableActions, WorkflowOutcome, StepOutcome, …
      factory.rs          ContainerExecutionFactory trait, WorkflowRuntimeContext
      frontend.rs         WorkflowFrontend trait
      timing.rs           YOLO_COUNTDOWN_DURATION, STUCK_DIALOG_BACKOFF constants
    git/
      mod.rs              GitEngine; impl GitRootResolver for GitEngine
    overlay/
      mod.rs              OverlayEngine, OverlayRequest, DirectorySpec, CLAUDE_DENYLIST
    auth/
      mod.rs              AuthEngine (headless API keys, TLS, keychain credentials)
      keychain.rs         Per-OS keychain backend (keyring crate)
    agent/
      mod.rs              AgentEngine, AgentRunOptions
      agent_matrix.rs     Per-agent entrypoint/flag translation table
      frontend.rs         AgentFrontend trait
      download.rs         Dockerfile download URL constants
    ready/
      mod.rs              ReadyEngine, ReadyEngineOptions
      phase.rs            ReadyPhase state machine, ReadyFailure
      frontend.rs         ReadyFrontend trait
      summary.rs          ReadySummary
    init/
      mod.rs              InitEngine, InitEngineOptions
      phase.rs            InitPhase state machine, InitFailure
      frontend.rs         InitFrontend trait
      summary.rs          InitSummary
    claws/
      mod.rs              ClawsEngine, ClawsEngineOptions, ClawsMode
      phase.rs            ClawsPhase state machine, ClawsFailure
      frontend.rs         ClawsFrontend trait
      summary.rs          ClawsSummary
  command/
    mod.rs                (stub — populated in 0068)
  frontend/
    mod.rs                (stub — populated in 0069)
```

---

## Layer 0: Data (`src/data/`)

Layer 0 is the foundation every other layer builds on. It owns:

- The `Session` ruling type and its runtime state
- The `SessionManager` collection and persistence interface
- All configuration loading, saving, and merging
- All filesystem and database interactions
- The typed `DataError` error enum

Nothing in `src/data/` ever spawns a process, opens a network socket, calls `git`, or manages a container. Those are Layer 1 concerns.

---

### Session (`src/data/session.rs`)

`Session` is the ruling type for every amux operation. It ties together a working directory, a resolved git root, loaded configurations, and the in-flight runtime state. Every command and workflow invocation starts with a `Session`.

- The **CLI** creates one `Session` per invocation.
- The **TUI** creates one `Session` per tab.
- The **headless server** creates one `Session` per API session.

#### `SessionId`

```rust
pub struct SessionId(Uuid);
```

Newtype over `uuid::Uuid`. Implements `Display` (UUID string format), `Hash`, and `Eq`. `SessionId::new()` generates a random v4 UUID; `SessionId::from_uuid(uuid)` wraps an existing one for persistence round-trips.

#### `AgentName`

```rust
pub struct AgentName(String);
```

Newtype over `String` with validation: ASCII alphanumerics, hyphens, and underscores; 1–64 characters. `AgentName::new("claude")` returns `Result<AgentName, DataError>`. `as_str()` and `Display` give the inner string.

#### `ContainerHandle`

```rust
pub struct ContainerHandle {
    pub id: String,
    pub image_tag: String,
    pub name: String,
    pub started_at: chrono::DateTime<chrono::Utc>,
}
```

The persistable identity of a running container. Layer 0 holds only the identity; the runtime object that controls a container (start/stop/wait) is Layer 1.

#### `SessionState`

```rust
pub struct SessionState {
    pub current_command: Option<CommandInvocation>,
    pub current_workflow: Option<WorkflowInvocation>,
    pub current_container: Option<ContainerHandle>,
    pub errors: Vec<SessionLogEntry>,
    pub notes: Vec<SessionLogEntry>,
}
```

Mutable runtime state owned by a `Session`. `record_error(msg)` and `record_note(kind, msg)` append to the respective logs. `SessionLogEntry` carries a UTC timestamp, a `SessionLogKind` (Info / Warning / Error / Diagnostic), and a message string.

#### `CommandInvocation` and `WorkflowInvocation`

`CommandInvocation` is the persistable record of a single in-flight command (subcommand name, args, status, exit code, timestamps). `WorkflowInvocation` is the persistable record of a running workflow (workflow name and hash, work item, step records, paused/yolo/auto flags, current step index).

Both are serializable via serde and stored in `SessionState` for persistence by the headless server.

#### `GitRootResolver` trait

```rust
pub trait GitRootResolver: Send + Sync {
    fn resolve(&self, working_dir: &Path) -> Result<PathBuf, DataError>;
}
```

Layer 0 never calls `git rev-parse` directly. Instead, `Session::open` accepts a `&dyn GitRootResolver` and delegates resolution to Layer 1's `GitEngine` (wired in work item 0067). `StaticGitRootResolver` is the test-only implementation that returns a fixed path.

#### `Session` constructors and accessors

```rust
impl Session {
    pub fn open(
        working_dir: PathBuf,
        resolver: &dyn GitRootResolver,
        opts: SessionOpenOptions,
    ) -> Result<Self, DataError>;

    pub fn open_at_git_root(
        working_dir: PathBuf,
        git_root: PathBuf,
        opts: SessionOpenOptions,
    ) -> Result<Self, DataError>;

    // Read accessors
    pub fn id(&self) -> SessionId;
    pub fn working_dir(&self) -> &Path;
    pub fn git_root(&self) -> &Path;
    pub fn repo_config(&self) -> &RepoConfig;
    pub fn global_config(&self) -> &GlobalConfig;
    pub fn env(&self) -> &EnvSnapshot;
    pub fn flags(&self) -> &FlagConfig;
    pub fn default_agent(&self) -> Option<&AgentName>;
    pub fn available_agents(&self) -> &[AgentName];
    pub fn state(&self) -> &SessionState;
    pub fn created_at(&self) -> SystemTime;
    pub fn last_active_at(&self) -> SystemTime;
    pub fn uptime(&self) -> Duration;

    // Mutation
    pub fn state_mut(&mut self) -> &mut SessionState;
    pub fn touch(&mut self);
    pub fn set_flags(&mut self, flags: FlagConfig);
    pub fn set_env(&mut self, env: EnvSnapshot);
    pub fn set_available_agents(&mut self, agents: Vec<AgentName>);

    // Merged config view
    pub fn effective_config(&self) -> EffectiveConfig;
}
```

`Session::open` resolves the git root via the resolver, loads `RepoConfig` and `GlobalConfig` from disk, resolves the default agent using the precedence order (flags > repo config > global config), and records creation timestamps. It returns `DataError::GitRootNotFound` if the resolver fails.

`SessionOpenOptions` carries optional `FlagConfig`, an optional `EnvSnapshot`, and an optional `Vec<AgentName>` for available agents.

---

### SessionManager (`src/data/session_manager.rs`)

```rust
pub struct SessionManager { … }
```

A concurrency-safe collection of `Session` values backed by a `tokio::sync::RwLock`. All methods are `async`.

```rust
impl SessionManager {
    pub fn in_memory() -> Self;
    pub fn with_persistence(store: Arc<dyn SessionStore>) -> Self;

    pub async fn create(&self, session: Session) -> Result<SessionId, DataError>;
    pub async fn get(&self, id: SessionId) -> Result<Session, DataError>;
    pub async fn update<F, T>(&self, id: SessionId, f: F) -> Result<T, DataError>
    where F: FnOnce(&mut Session) -> T;
    pub async fn list(&self) -> Vec<Session>;
    pub async fn len(&self) -> usize;
    pub async fn is_empty(&self) -> bool;
    pub async fn remove(&self, id: SessionId) -> Result<(), DataError>;
    pub fn has_persistence(&self) -> bool;
}
```

`SessionManager::in_memory()` creates a manager with no persistence backend (used by the CLI for its single session and by the TUI for per-tab sessions). `SessionManager::with_persistence(store)` attaches a `SessionStore` backend that receives an `upsert` call on every `create` or `update` and a `remove` call on every `remove`. The headless server uses this variant with `SqliteSessionStore`.

`update` takes a closure instead of returning `&mut Session` to avoid exposing an unguarded mutable reference across an `await` point.

`create` returns `DataError::SessionIdCollision` (instead of panicking) in the astronomically unlikely event of a UUID v4 collision.

#### `SessionStore` trait

```rust
pub trait SessionStore: Send + Sync {
    fn upsert(&self, session: &Session) -> Result<(), DataError>;
    fn remove(&self, id: SessionId) -> Result<(), DataError>;
}
```

The persistence interface implemented by Layer 0's `SqliteSessionStore` (and by `InMemorySessionStore` for tests).

---

### Configuration (`src/data/config/`)

All configuration concerns live in `src/data/config/`. Four source layers are merged in a fixed priority order:

```
flags  >  env  >  repo config  >  global config  >  built-in default
```

The merge is enforced by `EffectiveConfig` and is never duplicated elsewhere.

#### `RepoConfig` (`config/repo.rs`)

Per-repository configuration stored at `<git_root>/.amux/config.json`.

```rust
pub struct RepoConfig {
    pub agent: Option<String>,
    pub auto_agent_auth_accepted: Option<bool>,
    pub terminal_scrollback_lines: Option<usize>,
    pub yolo_disallowed_tools: Option<Vec<String>>,  // "yoloDisallowedTools" in JSON
    pub env_passthrough: Option<Vec<String>>,          // "envPassthrough" in JSON
    pub work_items: Option<WorkItemsConfig>,           // "workItems" in JSON
    pub overlays: Option<OverlaysConfig>,
    pub agent_stuck_timeout_secs: Option<u64>,        // "agentStuckTimeout" in JSON
}
```

Key methods:

| Method | Description |
|--------|-------------|
| `RepoConfig::path(git_root)` | Returns `<git_root>/.amux/config.json` |
| `RepoConfig::legacy_path(git_root)` | Returns `<git_root>/aspec/.amux.json` (pre-migration path) |
| `RepoConfig::load(git_root)` | Loads from disk; returns `default()` when absent, `DataError::ConfigParse` on malformed JSON |
| `RepoConfig::save(&self, git_root)` | Persists to disk, creating parent dirs as needed |
| `RepoConfig::migrate_legacy(git_root)` | Moves `aspec/.amux.json` → `.amux/config.json` if and only if legacy exists and new path does not; returns `true` when migration occurred |
| `RepoConfig::work_items_dir(git_root)` | Resolves configured work items directory |
| `RepoConfig::work_items_template(git_root)` | Resolves configured work item template path |

Nested types: `WorkItemsConfig` (dir, template), `OverlaysConfig` (directories list), `DirectoryOverlayConfig` (host, container, permission), `HeadlessConfig` (workDirs, alwaysNonInteractive), `RemoteConfig` (defaultAddr, savedDirs, defaultAPIKey).

#### `GlobalConfig` (`config/global.rs`)

Global configuration stored at `$HOME/.amux/config.json`. The path is overridden by the `AMUX_CONFIG_HOME` environment variable (used by tests to isolate state).

```rust
pub struct GlobalConfig {
    pub default_agent: Option<String>,
    pub terminal_scrollback_lines: Option<usize>,
    pub runtime: Option<String>,
    pub yolo_disallowed_tools: Option<Vec<String>>,
    pub env_passthrough: Option<Vec<String>>,
    pub headless: Option<HeadlessConfig>,
    pub remote: Option<RemoteConfig>,
    pub overlays: Option<OverlaysConfig>,
    pub agent_stuck_timeout_secs: Option<u64>,
}
```

Key methods:

| Method | Description |
|--------|-------------|
| `GlobalConfig::home_dir()` | Resolves `$AMUX_CONFIG_HOME` or `$HOME/.amux` |
| `GlobalConfig::home_dir_with(env)` | Same, reading from an `EnvSnapshot` |
| `GlobalConfig::path()` / `path_with(env)` | Resolves the full config file path |
| `GlobalConfig::load()` / `load_with(env)` | Loads from disk; returns `default()` when absent |
| `GlobalConfig::save()` / `save_with(env)` | Persists to disk |

#### `EnvSnapshot` and `Env` (`config/env.rs`)

`EnvSnapshot` is a frozen snapshot of every environment variable amux reads. No scattered `std::env::var()` calls appear elsewhere in Layer 0.

```rust
pub struct EnvSnapshot { … }

impl EnvSnapshot {
    pub fn empty() -> Self;
    pub fn with_overrides<I, K, V>(entries: I) -> Self;
    pub fn get(&self, key: &str) -> Option<&str>;

    // Typed accessors for known vars
    pub fn config_home(&self) -> Option<PathBuf>;    // AMUX_CONFIG_HOME
    pub fn headless_root(&self) -> Option<PathBuf>;  // AMUX_HEADLESS_ROOT
    pub fn overlays(&self) -> Option<&str>;          // AMUX_OVERLAYS
    pub fn remote_addr(&self) -> Option<&str>;       // AMUX_REMOTE_ADDR
    pub fn remote_session(&self) -> Option<&str>;    // AMUX_REMOTE_SESSION
    pub fn api_key(&self) -> Option<&str>;           // AMUX_API_KEY
}
```

`Env` is a stateless namespace used to read from the real process environment at startup. Tests use `EnvSnapshot::with_overrides([("AMUX_CONFIG_HOME", tmp_path)])` to avoid touching the filesystem.

Defined constants for every env var amux reads:

| Constant | Variable | Purpose |
|----------|----------|---------|
| `AMUX_CONFIG_HOME` | `AMUX_CONFIG_HOME` | Override global config home dir |
| `AMUX_HEADLESS_ROOT` | `AMUX_HEADLESS_ROOT` | Override headless storage root |
| `AMUX_OVERLAYS` | `AMUX_OVERLAYS` | Comma-separated overlay specs |
| `AMUX_REMOTE_ADDR` | `AMUX_REMOTE_ADDR` | Override remote server address |
| `AMUX_REMOTE_SESSION` | `AMUX_REMOTE_SESSION` | Sticky session id for remote ops |
| `AMUX_API_KEY` | `AMUX_API_KEY` | API key for headless server |

#### `FlagConfig` (`config/flags.rs`)

Typed struct carrying the flag values parsed by a frontend. Frontends (CLI via clap, TUI via the flag parser) populate a `FlagConfig` and pass it into `SessionOpenOptions`. The config layer itself never parses command-line strings.

Key fields: `agent`, `terminal_scrollback_lines`, `agent_stuck_timeout`, `non_interactive`, `env_passthrough`, `yolo_disallowed_tools`, `remote_addr`, `remote_session`, `api_key`.

#### `EffectiveConfig` (`config/effective.rs`)

The merged view of all four config sources. `Session::effective_config()` returns a fresh `EffectiveConfig` on demand; it is not cached on the session because flags can be updated via `Session::set_flags`.

```rust
pub struct EffectiveConfig {
    flags: FlagConfig,
    env: EnvSnapshot,
    repo: RepoConfig,
    global: GlobalConfig,
}

impl EffectiveConfig {
    pub fn new(flags, env, repo, global) -> Self;

    // Raw source access
    pub fn flags(&self) -> &FlagConfig;
    pub fn env(&self) -> &EnvSnapshot;
    pub fn repo(&self) -> &RepoConfig;
    pub fn global(&self) -> &GlobalConfig;

    // Merged accessors (precedence enforced internally)
    pub fn agent(&self) -> Option<String>;           // flag > repo > global
    pub fn env_passthrough(&self) -> Vec<String>;    // flag > repo > global > []
    pub fn yolo_disallowed_tools(&self) -> Vec<String>; // flag > repo > global > []
    pub fn scrollback_lines(&self) -> usize;         // flag > repo > global > 10_000
    pub fn agent_stuck_timeout(&self) -> Duration;   // flag > repo > global > 30s
    pub fn headless_work_dirs(&self) -> Vec<String>; // global only
    pub fn always_non_interactive(&self) -> bool;    // flag > global > false
    pub fn remote_default_addr(&self) -> Option<String>;  // flag > env > global
    pub fn remote_default_api_key(&self) -> Option<String>; // flag > env > global
    pub fn remote_saved_dirs(&self) -> Vec<String>;  // global only
    pub fn remote_session(&self) -> Option<String>;  // flag > env
    pub fn runtime(&self) -> Option<String>;         // global only
}
```

Built-in defaults: `scrollback_lines` = 10,000 lines; `agent_stuck_timeout` = 30 seconds.

---

### Filesystem Stores (`src/data/fs/`)

Every direct filesystem or database interaction in Layer 0 is encapsulated in a typed object in this module. Higher layers consume these objects; they never call `std::fs::*` or `rusqlite::*` directly.

#### `SqliteSessionStore` (`fs/headless_db.rs`)

Sqlite-backed persistence for headless-mode session and command metadata. Schema is compatible with `oldsrc/commands/headless/db.rs` so that existing on-disk databases written by earlier amux releases remain readable.

```rust
pub struct SqliteSessionStore { conn: Mutex<Connection> }

impl SqliteSessionStore {
    pub fn open(root: &Path) -> Result<Self, DataError>;
    pub fn open_from_paths(paths: &HeadlessPaths) -> Result<Self, DataError>;

    pub fn insert_session(&self, id, workdir, created_at) -> Result<(), DataError>;
    pub fn close_session(&self, id, closed_at) -> Result<(), DataError>;
    pub fn list_sessions(&self) -> Result<Vec<SessionRecord>, DataError>;
    pub fn get_session(&self, id) -> Result<Option<SessionRecord>, DataError>;

    pub fn insert_command(&self, id, session_id, subcommand, args, log_path) -> Result<(), DataError>;
    pub fn update_command_status(&self, id, status, exit_code, finished_at) -> Result<(), DataError>;
    pub fn list_commands(&self, session_id) -> Result<Vec<CommandRecord>, DataError>;
    pub fn get_command(&self, id) -> Result<Option<CommandRecord>, DataError>;
}
```

`SqliteSessionStore::open(root)` creates the database at `<root>/amux.db`, enables WAL mode, and runs schema migrations idempotently. The schema has two tables: `sessions` and `commands`.

`SessionRecord` and `CommandRecord` are plain structs (no Arc, no async) that carry the persisted metadata fields.

#### `HeadlessPaths` (`fs/headless_paths.rs`)

Typed accessors for every path used by the headless server. Replaces ad-hoc `dirs::data_dir().join("amux/headless/…")` calls scattered through the legacy code.

```rust
pub struct HeadlessPaths { root: PathBuf }

impl HeadlessPaths {
    pub fn from_env(env: &EnvSnapshot) -> Result<Self, DataError>;
    pub fn root(&self) -> &Path;
    pub fn db_path(&self) -> PathBuf;          // <root>/amux.db
    pub fn log_path(&self) -> PathBuf;         // <root>/amux.log
    pub fn pid_path(&self) -> PathBuf;         // <root>/amux.pid
    pub fn tls_dir(&self) -> PathBuf;          // <root>/tls/
    pub fn sessions_dir(&self) -> PathBuf;     // <root>/sessions/
    pub fn session_dir(&self, id) -> PathBuf;  // <root>/sessions/<id>/
    pub fn command_dir(&self, session_id, command_id) -> PathBuf;
    pub fn stdout_log(&self, session_id, command_id) -> PathBuf;
    pub fn stderr_log(&self, session_id, command_id) -> PathBuf;
}
```

`HeadlessPaths::from_env` reads `AMUX_HEADLESS_ROOT` from the snapshot; if unset, uses `$HOME/.amux/headless`.

#### `WorkflowStateStore` (`fs/workflow_state.rs`)

Persists and retrieves `WorkflowInvocation` to/from disk. Replaces the free `pub fn` helpers `workflow_state_path`, `save_workflow_state`, `load_workflow_state`, and `validate_resume_compatibility` in the legacy code.

```rust
pub struct WorkflowStateStore { base_dir: PathBuf }

impl WorkflowStateStore {
    pub fn new(base_dir: PathBuf) -> Self;
    pub fn for_session(session: &Session) -> Self;

    pub fn state_path(&self, workflow_name: &str) -> PathBuf;
    pub fn save(&self, invocation: &WorkflowInvocation) -> Result<(), DataError>;
    pub fn load(&self, workflow_name: &str) -> Result<Option<WorkflowInvocation>, DataError>;
    pub fn validate_resume(&self, invocation: &WorkflowInvocation) -> Result<(), DataError>;
    pub fn remove(&self, workflow_name: &str) -> Result<(), DataError>;
}
```

Workflow state is stored as JSON at `<base_dir>/workflow-state/<workflow_name>.json`. `validate_resume` checks that the workflow hash in the stored invocation matches the hash of the workflow file on disk, returning `DataError::WorkflowResumeIncompatible` if they differ.

#### `SkillDirs` (`fs/skill_dirs.rs`)

Typed access to global and per-repo skill directories.

```rust
pub struct SkillDirs {
    global_dir: Option<PathBuf>,
    repo_dir: Option<PathBuf>,
}

impl SkillDirs {
    pub fn resolve(session: &Session) -> Self;
    pub fn global_dir(&self) -> Option<&Path>;
    pub fn repo_dir(&self) -> Option<&Path>;
    pub fn all_dirs(&self) -> Vec<&Path>;
}
```

Global skills live at `$HOME/.amux/skills/` (or `$AMUX_CONFIG_HOME/skills/`). Per-repo skills live at `<git_root>/.amux/skills/`.

#### `WorkflowDirs` (`fs/workflow_dirs.rs`)

Typed access to global and per-repo workflow directories. Same structure as `SkillDirs`: global at `$HOME/.amux/workflows/`, per-repo at `<git_root>/.amux/workflows/`.

#### `OverlayPathResolver` (`fs/overlay_paths.rs`)

Resolves overlay host paths from raw user input. Path *mounting* into containers is Layer 1; path *resolution* is Layer 0.

```rust
pub struct OverlayPathResolver;

impl OverlayPathResolver {
    pub fn new() -> Self;
    pub fn expand_tilde(path: &str) -> PathBuf;
    pub fn make_absolute_with_cwd(path: &str, cwd: &Path) -> PathBuf;
    pub fn make_absolute(path: &str) -> PathBuf;
    pub fn canonicalize_lossy(path: &Path) -> PathBuf;
}
```

`canonicalize_lossy` handles the common case of overlay paths that don't exist yet: it walks up to the nearest existing ancestor, canonicalises that, and re-appends the missing trailing components. This mirrors the behaviour of `oldsrc/overlays/make_host_path_canonical` from work item 0065.

#### `AuthPathResolver` (`fs/auth_paths.rs`)

Resolves host-side credential and settings paths for each supported agent. The *passthrough* of those paths into containers (file copying, scrubbing, bind-mount construction) is a Layer 1 concern.

```rust
pub struct AuthPathResolver { home: PathBuf }

impl AuthPathResolver {
    pub fn at_home(home: impl Into<PathBuf>) -> Self;
    pub fn from_process_env() -> Result<Self, DataError>;
    pub fn home(&self) -> &Path;
    pub fn resolve(&self, agent: &str) -> AgentAuthPaths;
}

pub struct AgentAuthPaths {
    pub agent: String,
    pub config_file: Option<PathBuf>,
    pub settings_dir: Option<PathBuf>,
}
```

`resolve("claude")` returns `config_file = Some(~/.claude.json)`, `settings_dir = Some(~/.claude)`. Each supported agent maps to its own file locations.

---

### Error Types (`src/data/error.rs`)

All Layer 0 errors are variants of `DataError`. Higher layers wrap `DataError` in their own error enums.

```rust
#[derive(Debug, Error)]
pub enum DataError {
    GitRootNotFound { working_dir: PathBuf },
    GitRootResolution { working_dir: PathBuf, message: String },
    SessionNotFound { id: Uuid },
    SessionIdCollision { id: Uuid },
    InvalidAgentName { name: String, reason: String },
    ConfigParse { path: PathBuf, source: serde_json::Error },
    ConfigSerialize { source: serde_json::Error },
    Io { path: PathBuf, source: std::io::Error },
    HomeNotFound,
    Sqlite(rusqlite::Error),
    WorkflowState(String),
    WorkflowResumeIncompatible(String),
    InvalidPath { path: PathBuf, reason: String },
}
```

`DataError::io(path, err)` and `DataError::config_parse(path, err)` are convenience constructors. `DataError` uses `thiserror` for `Display` and `Error::source` implementations.

---

## Layer 1: Engine (`src/engine/`)

Layer 1 is the engine layer: typed objects that own every runtime concern Layer 2 commands need to compose. It is built on top of Layer 0 and never calls into Layer 2, 3, or 4. When an engine needs user input or output it accepts a **frontend trait** defined by Layer 1 — higher layers implement that trait and pass it in at construction.

Three rules govern every engine in this layer:

1. **No direct I/O.** No `println!`, `eprintln!`, `tracing::info!` to user-facing output. All user-visible output flows through `UserMessageSink::write_message` or the appropriate frontend trait.
2. **No PTY, no `clap`, no `crossterm`, no `ratatui`.** Those are Layer 3 concerns.
3. **Typed objects over free functions.** Every significant abstraction is a struct with methods.

---

### `UserMessageSink` and `UserMessage` (`src/engine/message.rs`)

`UserMessageSink` is a supertrait of every frontend trait in Layer 1. Any type that implements `ContainerFrontend`, `WorkflowFrontend`, `ReadyFrontend`, `InitFrontend`, `ClawsFrontend`, or `AgentFrontend` also implements `UserMessageSink`, so engine code can call `frontend.info(…)`, `frontend.warning(…)`, etc. anywhere a frontend reference is held.

```rust
pub struct UserMessage {
    pub level: MessageLevel,   // Info | Warning | Error | Success
    pub text: String,
}

pub trait UserMessageSink: Send + Sync {
    fn write_message(&mut self, msg: UserMessage);
    fn replay_queued(&mut self);

    // Convenience defaults:
    fn info(&mut self, text: impl Into<String>);
    fn warning(&mut self, text: impl Into<String>);
    fn error_msg(&mut self, text: impl Into<String>);
    fn success(&mut self, text: impl Into<String>);
}
```

**CLI queueing contract**: when a PTY-bound container owns the terminal, `write_message` queues the message instead of writing. `replay_queued` drains the queue after the container releases the terminal. TUI and headless implementations render live and treat `replay_queued` as a no-op.

`RecordingMessageSink` (also in `message.rs`) records every message passed to it and is used by all engine unit tests.

---

### `EngineError` (`src/engine/error.rs`)

All Layer 1 failures are variants of `EngineError`. It wraps `DataError` for failures from Layer 0; higher layers wrap `EngineError` in their own error types.

Key variants:

| Variant | Meaning |
|---------|---------|
| `Data(DataError)` | Propagated from Layer 0 |
| `Git(String)` | Git subprocess failure |
| `Container(String)` | Backend container operation failure |
| `ConflictingOptions(String)` | Mutually exclusive `ContainerOption`s |
| `OptionNotSupportedByBackend { option, backend }` | Option irrelevant to chosen backend |
| `BackendUnsupportedOnPlatform { backend, platform }` | e.g. Apple Containers on Linux |
| `InvalidAdvanceAction(String)` | `NextAction` rejected by `WorkflowEngine` |
| `UnsupportedWorkflowSchemaVersion { found, supported }` | Persisted state is too new |
| `WorkflowResumeIncompatible(String)` | User declined drift-resume |
| `PlanModeUnsupported { agent }` | Agent does not support `--plan` |
| `AgentRequiresProjectImage { tag }` | Base image not built yet |

---

### `StepStatus` (`src/engine/step_status.rs`)

Shared across `ReadyEngine`, `InitEngine`, and `ClawsEngine` for their summary structs.

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum StepStatus {
    Pending,
    Skipped,
    Running,
    Done,
    Failed(String),   // human-readable reason
}
```

---

### Container Engine (`src/engine/container/`)

The container engine provides a single typed factory for building and running containers. The concrete backend (Docker or Apple Containers) is selected once at construction and never exposed to callers outside the module.

#### `ContainerRuntime`

```rust
pub struct ContainerRuntime { /* holds Box<dyn ContainerBackend> — opaque */ }

impl ContainerRuntime {
    /// Inspect global_config to pick Docker (default) or Apple Containers.
    /// Returns BackendUnsupportedOnPlatform if Apple Containers is requested on non-macOS.
    /// Unknown runtime values default to Docker and emit a warning.
    pub fn detect(global_config: &GlobalConfig) -> Result<Self, EngineError>;

    /// Name of the chosen backend ("docker" or "apple-containers"). Safe for display.
    pub fn runtime_name(&self) -> &'static str;

    /// Build a fully-configured ContainerInstance from the given options.
    pub fn build(&self, options: impl IntoIterator<Item = ContainerOption>)
        -> Result<Box<dyn ContainerInstance>, EngineError>;

    pub fn list_running(&self, session: &Session) -> Result<Vec<ContainerHandle>, EngineError>;
    pub fn stats(&self, handle: &ContainerHandle) -> Result<ContainerStats, EngineError>;
    pub fn stop(&self, handle: &ContainerHandle) -> Result<(), EngineError>;
}
```

Backend selection rules: `"docker"` or absent → Docker; `"apple-containers"` on macOS → Apple; `"apple-containers"` on non-macOS → `EngineError::BackendUnsupportedOnPlatform`; unknown value → warn + Docker.

#### `ContainerOption`

Every knob a container invocation accepts. Adding a new option is one new variant plus one branch in `ResolvedContainerOptions::ingest` — no changes to call sites needed.

```rust
pub enum ContainerOption {
    Image(ImageRef),
    Entrypoint(Entrypoint),
    Overlay(OverlaySpec),
    EnvPassthrough(EnvVar),
    EnvLiteral(EnvLiteral),
    SeededPrompt(String),
    Interactive(bool),
    AllowDocker(bool),
    MountSsh { source: PathBuf },
    Yolo(YoloMode),
    Auto(AutoMode),
    Plan(PlanMode),
    WorkingDir(PathBuf),
    Name(ContainerName),
    Cpu(CpuLimit),
    Memory(MemoryLimit),
    AgentSettingsPassthrough(AgentSettings),
    AgentCredentials { env_vars: Vec<(String, String)> },
    DisallowedTools(Vec<String>),
    AllowedTools(Vec<String>),
    Model { flag: ModelFlagForm },
    NonInteractivePrintFlag(String),
    DockerfileUser(String),
}
```

`ModelFlagForm` distinguishes `--model NAME` (Argument) from standalone shorthands like `--model-claude-opus-4-6` (Shorthand).

#### `ContainerInstance` and `ContainerExecution`

```rust
pub trait ContainerInstance: Send + Sync {
    fn id(&self) -> &ContainerId;
    fn name(&self) -> &ContainerName;
    fn image(&self) -> &ImageRef;
    fn run_with_frontend(self: Box<Self>, frontend: Box<dyn ContainerFrontend>)
        -> Result<ContainerExecution, EngineError>;
}

pub struct ContainerExecution { /* owns running handle + exit futures */ }

impl ContainerExecution {
    pub async fn wait(self) -> Result<ContainerExitInfo, EngineError>;
    pub fn handle(&self) -> &ContainerHandle;
    pub fn cancel(&self) -> Result<(), EngineError>;
    /// Hand ownership of the running container back to the caller without joining.
    pub fn detach(self) -> ContainerHandle;
}
```

`ContainerExitInfo` carries `exit_code`, `signal` (if applicable), `started_at`, and `ended_at`.

#### `ContainerFrontend` trait

Defined by Layer 1, implemented by Layer 3. Governs all I/O the container runtime needs from the outside world.

```rust
pub trait ContainerFrontend: UserMessageSink + Send + Sync {
    fn write_stdout(&mut self, bytes: &[u8]) -> Result<(), EngineError>;
    fn write_stderr(&mut self, bytes: &[u8]) -> Result<(), EngineError>;
    fn read_stdin(&mut self, buf: &mut [u8]) -> Result<usize, EngineError>;
    fn report_status(&mut self, status: ContainerStatus);
    fn report_progress(&mut self, progress: ContainerProgress);
    fn resize_pty(&mut self, cols: u16, rows: u16);
}
```

PTY allocation is a Layer 3 concern. Layer 1 passes raw bytes to the frontend and lets it decide whether they route through a PTY (TUI), straight to fds (CLI), or over a socket (headless).

#### What is forbidden in `src/engine/container/`

- No `pub fn run_container_with_*` style APIs.
- `docker.rs` and `apple.rs` are `pub(super)` — no caller outside the module can name the backend type.
- No direct PTY allocation or `crossterm` use.
- No `println!` / `eprintln!`. All output goes through `ContainerFrontend`.

---

### Workflow Engine (`src/engine/workflow/`)

`WorkflowEngine` owns every workflow execution concern: step ordering, state advancement, yolo/auto countdowns, stuck detection, per-step agent and model resolution, exit-code interpretation, step persistence, and container lifecycle management per step.

```rust
pub struct WorkflowEngine {
    session: Session,
    workflow: Workflow,                    // parsed definition (Layer 0)
    dag: WorkflowDag,                      // Layer 0 — cycle-free adjacency
    state: WorkflowState,                  // Layer 0 — serializable snapshot
    state_store: WorkflowStateStore,       // Layer 0 — git-root-scoped I/O
    effective_config: EffectiveConfig,     // Layer 0 — for agent/model fallbacks
    frontend: Box<dyn WorkflowFrontend>,
    container_factory: Box<dyn ContainerExecutionFactory>,
    git_engine: Arc<GitEngine>,
    overlay_engine: Arc<OverlayEngine>,
    // … current_execution, current_step tracking fields …
}

impl WorkflowEngine {
    pub fn new(session, workflow, frontend, factory, git_engine, overlay_engine)
        -> Result<Self, EngineError>;
    pub async fn resume(session, workflow, frontend, factory, git_engine, overlay_engine)
        -> Result<Self, EngineError>;

    pub async fn run_to_completion(&mut self) -> Result<WorkflowOutcome, EngineError>;
    pub async fn step_once(&mut self) -> Result<StepOutcome, EngineError>;
    pub fn compute_available_actions(&self) -> Result<AvailableActions, EngineError>;
    pub fn state(&self) -> &WorkflowState;
}
```

#### Per-step agent and model resolution

Resolution order (each level overrides the previous):
1. Step-level `agent`/`model` fields.
2. Workflow-level `agent`/`model` defaults.
3. `EffectiveConfig` fallback (flags > env > repo > global).

The resolved pair is logged via `tracing` and passed to the factory via `WorkflowRuntimeContext { step_agent, step_model, git_root, session_id }`.

#### `NextAction` and `AvailableActions`

After each step completes, `WorkflowEngine` asks the frontend which action to take:

```rust
pub enum NextAction {
    LaunchNext,
    ContinueInCurrentContainer { prompt: String },
    RestartCurrentStep,
    CancelToPreviousStep,
    FinishWorkflow,    // mark remaining steps Skipped; only valid on last step
    Pause,
    Abort,
}
```

The engine computes `AvailableActions` before calling `user_choose_next_action`, encoding which actions are legal given the current step configuration. The frontend renders only the available set.

`ContinueInCurrentContainer` is unavailable when: the next step targets a different agent or model; the running container has already exited; or the factory's `inject_prompt` returns `None`. `CancelToPreviousStep` is unavailable on the first step.

#### `ContainerExecutionFactory` trait

Layer 2 builds a factory that `WorkflowEngine` calls per step. The engine never sees raw `ContainerOption` lists or frontend implementations.

```rust
pub trait ContainerExecutionFactory: Send + Sync {
    fn execution_for_step(&self, step, session, runtime) -> Result<ContainerExecution, EngineError>;
    fn inject_prompt(&self, execution, prompt) -> Result<Option<()>, EngineError>;
}
```

#### `WorkflowFrontend` trait

```rust
pub trait WorkflowFrontend: UserMessageSink + Send + Sync {
    fn user_choose_next_action(&mut self, state, available) -> Result<NextAction, EngineError>;
    fn confirm_resume(&mut self, mismatch: &ResumeMismatch) -> Result<bool, EngineError>;
    fn user_choose_after_step_failure(&mut self, step, exit) -> Result<StepFailureChoice, EngineError>;
    fn report_step_status(&mut self, step, status: WorkflowStepStatus);
    fn report_step_output(&mut self, step, output: StepOutput);
    fn report_step_stuck(&mut self, step);
    fn report_step_unstuck(&mut self, step);
    fn yolo_countdown_tick(&mut self, remaining: Duration) -> Result<YoloTickOutcome, EngineError>;
    fn report_workflow_completed(&mut self, outcome: &WorkflowOutcome);
}
```

#### Stuck detection and yolo countdown

`WorkflowEngine` owns two timers:

1. **Stuck timer** — fires when the agent produces no PTY output for `EffectiveConfig::agent_stuck_timeout` (default 30 s). Triggers `report_step_stuck`.
2. **Yolo countdown** — only when `--yolo` is set and the stuck timer has fired. Counts down `YOLO_COUNTDOWN_DURATION` (60 s, defined in `timing.rs`) before auto-advancing via `NextAction::LaunchNext`. Backoff: `STUCK_DIALOG_BACKOFF` (60 s) prevents re-firing immediately after a dismissed countdown.

#### Workflow state persistence

State is persisted to `<git-root>/.amux/workflows/<hash8>-<name>.json` after every step transition. On resume, the engine checks `schema_version`; if the persisted version is newer than `WORKFLOW_STATE_SCHEMA_VERSION`, it returns `EngineError::UnsupportedWorkflowSchemaVersion`. If the workflow hash has drifted, it calls `confirm_resume`; if declined, it returns `WorkflowResumeIncompatible`.

#### What is forbidden in `WorkflowEngine`

- No direct container construction. Containers arrive pre-built via `ContainerExecutionFactory`.
- No rendering, no `eprintln!`, no user-console `tracing`. Status flows through the frontend.
- No worktree lifecycle management. The engine operates on a given `git_root` and is unaware of whether it is a worktree. Worktree creation/removal is a Layer 2 concern.
- No `clap`, no `crossterm`, no `ratatui`.
- No DAG logic or state persistence code — those live in `src/data/`.

---

### Git Engine (`src/engine/git/`)

`GitEngine` consolidates every git operation amux performs. It is a stateless struct whose methods are the only public surface. It implements Layer 0's `GitRootResolver` trait so `Session::open` can use it.

```rust
pub struct GitEngine;

impl GitEngine {
    pub fn new() -> Self;
    pub fn version_check(&self) -> Result<GitVersion, EngineError>;
    pub fn resolve_root(&self, working_dir: &Path) -> Result<PathBuf, EngineError>;
    pub fn is_clean(&self, path: &Path) -> Result<bool, EngineError>;
    pub fn uncommitted_files(&self, path: &Path) -> Result<Vec<String>, EngineError>;

    // Worktree paths (convention: ~/.amux/worktrees/<repo>/<NNNN>/ or wf-<name>/)
    pub fn worktree_path(&self, git_root: &Path, work_item: u32) -> Result<PathBuf, EngineError>;
    pub fn worktree_path_named(&self, git_root: &Path, name: &str) -> Result<PathBuf, EngineError>;
    pub fn branch_name_for_work_item(&self, work_item: u32) -> String;  // amux/work-item-NNNN
    pub fn branch_name_for_workflow(&self, name: &str) -> String;       // amux/workflow-<name>

    pub fn create_worktree(&self, git_root, worktree_path, branch) -> Result<(), EngineError>;
    pub fn remove_worktree(&self, git_root, worktree_path) -> Result<(), EngineError>;

    // Merge strategy: git merge --squash <branch> + git commit -m "Implement <branch>"
    pub fn merge_branch(&self, git_root: &Path, branch: &str) -> Result<(), EngineError>;
    pub fn commit_all(&self, path: &Path, message: &str) -> Result<(), EngineError>;
    pub fn delete_branch(&self, git_root: &Path, branch: &str) -> Result<(), EngineError>;
    pub fn branch_exists(&self, git_root: &Path, branch: &str) -> bool;
    pub fn is_detached_head(&self, git_root: &Path) -> bool;
}
```

Naming conventions enforced by `GitEngine`:
- Worktree path (work-item): `$HOME/.amux/worktrees/<repo-name>/<NNNN>/`
- Worktree path (workflow): `$HOME/.amux/worktrees/<repo-name>/wf-<workflow-name>/`
- Branch (work-item): `amux/work-item-<NNNN>` (zero-padded 4 digits)
- Branch (workflow): `amux/workflow-<workflow-name>`
- Merge commit: `"Implement <branch>"` (verbatim format preserved)

---

### Overlay Engine (`src/engine/overlay/`)

`OverlayEngine` consolidates overlay construction and management. Layer 0 resolves host paths; Layer 1 builds `OverlaySpec` values that `ContainerOption::Overlay` accepts.

```rust
pub struct OverlayEngine {
    path_resolver: OverlayPathResolver,
    auth_resolver: AuthPathResolver,
}

impl OverlayEngine {
    pub fn new(session: &Session) -> Result<Self, EngineError>;

    pub fn build_overlays(
        &self,
        session: &Session,
        request: &OverlayRequest,
    ) -> Result<Vec<OverlaySpec>, EngineError>;

    pub fn resolve_user_overlay(&self, spec: &str) -> Result<DirectoryOverlay, EngineError>;
    pub fn agent_settings_overlays(&self, agent: &AgentName) -> Result<Vec<OverlaySpec>, EngineError>;
}
```

`OverlayRequest` describes the desired overlays for a given invocation. `build_overlays` returns the resolved, deduplicated, canonicalized set; callers pass each item as `ContainerOption::Overlay`.

Per-agent settings handling (`agent_settings_overlays`) replicates the legacy `HostSettings` machinery:

- **Claude**: mounts `~/.claude.json` (with `oauthAccount` field stripped), mounts `~/.claude/` (applying `CLAUDE_DENYLIST` to exclude telemetry/history entries), sets `skipDangerousModePermissionPrompt: true` in `settings.json` when yolo mode is active, and always sets `hasShownLspRecommendation: true`.
- **Minimal fallback**: when `~/.claude.json` is absent, produces a synthesized overlay with `/workspace` project trust and LSP suppression.
- **Non-Claude agents**: each maps to a single agent-config-dir overlay (host path + container path per the agent matrix).

`CLAUDE_DENYLIST` is a named constant — adding a new excluded entry is a one-line change.

---

### Auth Engine (`src/engine/auth/`)

`AuthEngine` consolidates two previously-separate concerns:

1. **Host-side agent credential discovery** — resolving credentials from the OS keychain to inject into agent containers.
2. **Headless server authentication** — API key generation, hashing, comparison, persistence, and TLS material.

```rust
pub struct AuthEngine {
    auth_paths: AuthPathResolver,
    headless_paths: HeadlessPaths,
}

impl AuthEngine {
    pub fn new(session: &Session) -> Self;

    // Keychain credentials
    pub fn agent_keychain_credentials(&self, agent: &AgentName) -> Result<AgentCredentials, EngineError>;
    pub fn resolve_agent_auth(&self, session: &Session, agent: &AgentName)
        -> Result<AgentCredentials, EngineError>;

    // Headless API-key lifecycle
    pub fn generate_api_key(&self) -> Result<ApiKey, EngineError>;
    pub fn write_api_key_hash(&self, hash: &ApiKeyHash) -> Result<(), EngineError>;
    pub fn read_api_key_hash(&self) -> Result<Option<ApiKeyHash>, EngineError>;
    pub fn verify_api_key(&self, presented: &ApiKey) -> Result<AuthOutcome, EngineError>;
    pub fn refresh_api_key(&self) -> Result<ApiKey, EngineError>;

    // TLS material
    pub fn ensure_self_signed_tls(&self, bind_ip: IpAddr) -> Result<TlsMaterial, EngineError>;
    pub fn load_tls_from_paths(&self, cert: &Path, key: &Path) -> Result<TlsMaterial, EngineError>;
}
```

All cryptographic comparisons in `verify_api_key` use `subtle::ConstantTimeEq`, including the case where no hash file exists (compared against a fixed-length sentinel to prevent timing-based "is auth disabled?" leaks).

`keychain.rs` provides the per-OS keychain backend (macOS Keychain, Linux libsecret, Windows credential manager) via the `keyring` crate. The per-agent env-var name set (e.g. `ANTHROPIC_API_KEY` for Claude) is co-located with the agent matrix.

`AuthEngine` only resolves credentials; the "offer to use keychain credentials silently vs. prompt every time" decision is a Layer 2 concern driven by `EffectiveConfig::auto_agent_auth_accepted`.

---

### Agent Engine (`src/engine/agent/`)

`AgentEngine` centralises the cross-cutting agent concerns called from multiple commands (`implement`, `chat`, `exec`, `ready`, `claws`): ensuring the agent is available (Dockerfile + image), and building the `ContainerOption` list for a given invocation. Centralising here ensures adding a new agent type or changing model-flag injection is a single-file edit.

```rust
pub struct AgentEngine {
    overlay_engine: Arc<OverlayEngine>,
    container_runtime: Arc<ContainerRuntime>,
}

pub struct AgentRunOptions {
    pub yolo: Option<YoloMode>,
    pub auto: Option<AutoMode>,
    pub plan: Option<PlanMode>,
    pub allowed_tools: Vec<String>,
    pub initial_prompt: Option<String>,
    pub allow_docker: bool,
    pub non_interactive: bool,
}

impl AgentEngine {
    pub fn new(overlay_engine, container_runtime) -> Self;

    /// Ensure the agent Dockerfile and image exist; download/build if absent.
    /// Idempotent: no steps fire and no container_frontend is requested when
    /// both already exist.
    pub async fn ensure_available(
        &self, agent, config, frontend: &mut dyn AgentFrontend,
    ) -> Result<(), EngineError>;

    /// Build the ContainerOption list for running an agent container.
    /// Resolves overlays, injects model flags, autonomous flags, and all
    /// agent-specific entrypoint options. Pass the result to ContainerRuntime::build.
    pub fn build_options(
        &self, agent, model, run_options, session,
    ) -> Result<Vec<ContainerOption>, EngineError>;
}
```

`ensure_available` steps:
1. Check for `<git-root>/.amux/Dockerfile.<agent>`; download if absent.
2. Check for `<repo-hash>:<agent>:latest` locally; build if absent.
3. If the project base image (`<repo-hash>:latest`) is missing, fail with `EngineError::AgentRequiresProjectImage` — `AgentEngine` does not build the project image (`ReadyEngine`'s job).

#### Agent matrix (`agent_matrix.rs`)

All per-agent branching — entrypoints, non-interactive flags, plan-mode flags, yolo flags, model flags, image tags, Dockerfile paths, and download URLs — lives exclusively in `agent_matrix.rs`. Adding a new agent is a single-file edit.

Supported agents: `claude`, `codex`, `opencode`, `maki`, `gemini`, `copilot`, `crush`, `cline`.

Key per-agent distinctions:

| Agent | Interactive entrypoint | Non-interactive flag | Plan-mode flag |
|-------|------------------------|----------------------|----------------|
| `claude` | `claude` | `--print` / `-p` | `--permission-mode plan` |
| `codex` | `codex` | `exec`/`run` subcommand | `--approval-mode plan` |
| `opencode` | `opencode` | `run` subcommand | (unsupported — error) |
| `gemini` | `gemini` | varies | `--approval-mode=plan` |
| `copilot` | `copilot -i` | varies | `--plan` |
| `cline` | `cline` | `task` subcommand | `--plan` |
| `crush` | `crush` | `run` subcommand | (unsupported — error) |
| `maki` | `maki` | varies | (unsupported — error) |

`AgentEngine::build_options` with `PlanMode::Enabled` for an agent that does not support plan returns `EngineError::PlanModeUnsupported { agent }`.

#### `AgentFrontend` trait

```rust
pub trait AgentFrontend: UserMessageSink + Send + Sync {
    fn report_step_status(&mut self, step: &str, status: StepStatus);
    fn container_frontend(&mut self) -> Box<dyn ContainerFrontend>;
}
```

#### Canonical usage pattern

```rust
// The only sanctioned way to prepare and run an agent container:
agent_engine.ensure_available(&agent, &config, &mut frontend).await?;
let opts = agent_engine.build_options(&agent, &model, &run_options, &session)?;
let instance = container_runtime.build(opts)?;
let execution = instance.run_with_frontend(Box::new(container_frontend))?;
let exit = execution.wait().await?;
```

Duplicating `ensure_available` or `build_options` logic in any other module is a violation.

---

### Ready Engine (`src/engine/ready/`)

`ReadyEngine` owns all multi-phase logic for `amux ready`: preflight checks, legacy-layout detection and migration, Dockerfile.dev creation, Docker image builds, local agent check, audit container run, and post-audit rebuild. The legacy code (`oldsrc/commands/ready.rs`: 2239 lines, `oldsrc/commands/ready_flow.rs`: 726 lines) is replaced entirely.

#### Phase state machine

```rust
pub enum ReadyPhase {
    Preflight,                    // runtime detection, git root, config, env vars, legacy detection
    AwaitingDockerfileDecision,   // Dockerfile.dev absent or unmodified template
    CreatingDockerfile,           // write Dockerfile.dev from project template
    AwaitingLegacyMigrationDecision,  // legacy single-file layout detected
    MigratingLegacyLayout,        // migrate to modular layout
    BuildingBaseImage,            // build/rebuild project Docker image
    BuildingAgentImage,           // build/rebuild agent Docker image
    CheckingLocalAgent,           // send random greeting to local agent
    RunningAudit,                 // audit container scans/updates Dockerfile.dev
    RebuildingAfterAudit,         // rebuild after audit modifies Dockerfile.dev
    Complete,
    Failed(ReadyFailure),
}
```

The state machine is forward-only. If the process is interrupted the user re-runs `amux ready` from the beginning; no partial checkpoint is written.

#### `ReadyEngine` API

```rust
pub struct ReadyEngine { /* session, engines, options, phase */ }

pub struct ReadyEngineOptions {
    pub agent: AgentName,
    pub refresh: bool,
    pub build: bool,
    pub no_cache: bool,
    pub allow_docker: bool,
}

impl ReadyEngine {
    pub fn new(session, git_engine, overlay_engine, container_runtime, agent_engine, options) -> Self;
    pub fn phase(&self) -> &ReadyPhase;

    /// Advance exactly one phase, calling appropriate ReadyFrontend methods. Returns new phase.
    pub async fn step(&mut self, frontend: &mut dyn ReadyFrontend) -> Result<ReadyPhase, EngineError>;

    /// Drive to completion (calls step in a loop). Returns ReadySummary.
    pub async fn run_to_completion(&mut self, frontend: &mut dyn ReadyFrontend) -> Result<ReadySummary, EngineError>;

    pub fn summary(&self) -> ReadySummary;
}
```

#### `ReadyFrontend` trait

```rust
pub trait ReadyFrontend: UserMessageSink + Send + Sync {
    fn ask_create_dockerfile(&mut self) -> Result<bool, EngineError>;
    fn ask_run_audit_on_template(&mut self) -> Result<bool, EngineError>;
    fn ask_migrate_legacy_layout(&mut self, agent_name: &AgentName) -> Result<bool, EngineError>;
    fn report_phase(&mut self, phase: &ReadyPhase);
    fn report_step_status(&mut self, step: &str, status: StepStatus);
    fn container_frontend(&mut self) -> Box<dyn ContainerFrontend>;
    fn report_summary(&mut self, summary: &ReadySummary);
}
```

#### `ReadySummary`

```rust
pub struct ReadySummary {
    pub runtime_name: String,
    pub base_image: StepStatus,
    pub agent_image: StepStatus,
    pub local_agent: StepStatus,
    pub audit: StepStatus,
    pub legacy_migration: StepStatus,
}
```

---

### Init Engine (`src/engine/init/`)

`InitEngine` owns all multi-phase logic for `amux init`: git root resolution, aspec folder creation, Dockerfile.dev setup, `.amux.json` config write, optional audit container, image build, and work-items configuration. Replaces `oldsrc/commands/init.rs` + `oldsrc/commands/init_flow.rs` (2702 lines combined).

#### Phase state machine

```rust
pub enum InitPhase {
    Preflight,                  // resolve git root, validate environment
    AwaitingAspecDecision,      // existing aspec folder found
    CreatingAspecFolder,        // write aspec template into repo
    SettingUpDockerfile,        // create/confirm Dockerfile.dev
    WritingConfig,              // write or update .amux.json
    AwaitingAuditDecision,      // ask whether to run audit
    BuildingImage,              // build base Docker image
    RunningAudit,               // agent scans and updates Dockerfile.dev
    AwaitingWorkItemsDecision,  // ask whether to configure work items
    WritingWorkItemsConfig,     // write work-items config into .amux.json
    Complete,
    Failed(InitFailure),
}
```

Forward-only. If the user declines `AwaitingAspecDecision`, `aspec_folder` is `StepStatus::Skipped` and remaining phases continue.

#### `InitEngine` API

```rust
pub struct InitEngineOptions {
    pub agent: AgentName,
    pub run_aspec_setup: bool,
    pub git_root: PathBuf,
}

impl InitEngine {
    pub fn new(session, git_engine, overlay_engine, container_runtime, options) -> Self;
    pub fn phase(&self) -> &InitPhase;
    pub async fn step(&mut self, frontend: &mut dyn InitFrontend) -> Result<InitPhase, EngineError>;
    pub async fn run_to_completion(&mut self, frontend: &mut dyn InitFrontend) -> Result<InitSummary, EngineError>;
    pub fn summary(&self) -> &InitSummary;
}
```

#### `InitFrontend` trait

```rust
pub trait InitFrontend: UserMessageSink + Send + Sync {
    fn ask_replace_aspec(&mut self) -> Result<bool, EngineError>;
    fn ask_run_audit(&mut self) -> Result<bool, EngineError>;
    fn ask_work_items_setup(&mut self) -> Result<Option<WorkItemsConfig>, EngineError>;
    fn report_phase(&mut self, phase: &InitPhase);
    fn report_step_status(&mut self, step: &str, status: StepStatus);
    fn container_frontend(&mut self) -> Box<dyn ContainerFrontend>;
    fn report_summary(&mut self, summary: &InitSummary);
}
```

#### `InitSummary`

```rust
pub struct InitSummary {
    pub config: StepStatus,
    pub aspec_folder: StepStatus,
    pub dockerfile: StepStatus,
    pub audit: StepStatus,
    pub image_build: StepStatus,
    pub work_items_setup: StepStatus,
}
```

---

### Claws Engine (`src/engine/claws/`)

`ClawsEngine` owns all multi-phase logic for `amux claws init` and related subcommands: repo clone, SSH/sudo permission check, nanoclaw image build, audit container run, per-user configuration, and controller launch. Replaces `oldsrc/commands/claws.rs` (1327 lines).

#### Phase state machine

```rust
pub enum ClawsPhase {
    Preflight,                // runtime detection, git root, config load, existing-clone check
    AwaitingCloneDecision,    // existing clone found at target path
    CloningRepo,              // clone the nanoclaw repository
    CheckingPermissions,      // probe container verifies SSH key + sudo
    BuildingImage,            // build nanoclaw Docker image
    AwaitingAuditDecision,    // ask whether to run audit before configuring
    RunningAudit,             // nanoclaw audit container
    Configuring,              // write per-user nanoclaw configuration
    LaunchingController,      // start nanoclaw controller container
    Complete,
    Failed(ClawsFailure),
}
```

`claws ready` and `claws chat` enter the state machine at `Preflight` with a `ClawsMode` that skips satisfied phases:
- `ClawsMode::Ready`: skips to `LaunchingController` when image already exists.
- `ClawsMode::Chat`: transitions directly to `Complete` when controller is already running.

#### `ClawsEngine` API

```rust
pub struct ClawsEngineOptions {
    pub mode: ClawsMode,          // Init | Ready | Chat
    pub nanoclaw_url: Option<String>,
    pub refresh: bool,
    pub no_cache: bool,
}

impl ClawsEngine {
    pub fn new(session, git_engine, overlay_engine, container_runtime, options) -> Self;
    pub fn phase(&self) -> &ClawsPhase;
    pub async fn step(&mut self, frontend: &mut dyn ClawsFrontend) -> Result<ClawsPhase, EngineError>;
    pub async fn run_to_completion(&mut self, frontend: &mut dyn ClawsFrontend) -> Result<ClawsSummary, EngineError>;
    pub fn summary(&self) -> ClawsSummary;
}
```

#### `ClawsFrontend` trait

```rust
pub trait ClawsFrontend: UserMessageSink + Send + Sync {
    fn ask_replace_existing_clone(&mut self, path: &Path) -> Result<bool, EngineError>;
    fn ask_run_audit(&mut self) -> Result<bool, EngineError>;
    fn report_phase(&mut self, phase: &ClawsPhase);
    fn report_step_status(&mut self, step: &str, status: StepStatus);
    fn container_frontend(&mut self) -> Box<dyn ContainerFrontend>;
    fn report_summary(&mut self, summary: &ClawsSummary);
}
```

#### `ClawsSummary`

```rust
pub struct ClawsSummary {
    pub clone: StepStatus,
    pub permissions_check: StepStatus,
    pub image_build: StepStatus,
    pub audit: StepStatus,
    pub configure: StepStatus,
    pub controller: StepStatus,
}
```

---

### Layer 0 additions required by Layer 1 (`src/data/`)

Three modules were added to Layer 0 as part of work item 0067 because they are stateless functions over serializable types — not engine logic.

#### `WorkflowDag` (`src/data/workflow_dag.rs`)

```rust
pub struct WorkflowDag { /* adjacency; constructed via WorkflowDag::build */ }

impl WorkflowDag {
    pub fn build(steps: &[WorkflowStep]) -> Result<Self, DataError>;
    pub fn ready_steps(&self, completed: &HashSet<String>) -> Vec<String>;
    pub fn topological_order(&self) -> Vec<String>;
}

pub fn validate_references(steps: &[WorkflowStep]) -> Result<(), DataError>;
pub fn detect_cycle(steps: &[WorkflowStep]) -> Result<(), DataError>;
```

`build` returns `DataError::MissingDependency` for unknown `depends_on` entries and `DataError::CyclicDependency` for cycles. `topological_order` is deterministic across calls.

#### `WorkflowState` and `StepState` (`src/data/workflow_state.rs`)

Fully serializable snapshot of workflow execution state. Stored per-workflow at `<git-root>/.amux/workflows/`.

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowState {
    pub schema_version: u32,
    pub workflow_name: String,
    pub workflow_hash: String,
    pub step_states: HashMap<String, StepState>,
    pub completed_steps: HashSet<String>,
    pub current_step_index: Option<usize>,
    pub started_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

pub enum StepState {
    Pending,
    Running,
    Succeeded,
    Failed { exit_code: i32, error_message: Option<String> },
    Skipped,
    Cancelled,
}
```

`WorkflowEngine` rejects state whose `schema_version` exceeds `WORKFLOW_STATE_SCHEMA_VERSION` with `EngineError::UnsupportedWorkflowSchemaVersion`.

#### `WorkflowStateStore` (`src/data/workflow_state_store.rs`)

```rust
pub struct WorkflowStateStore { base_dir: PathBuf }

impl WorkflowStateStore {
    pub fn new(session: &Session) -> Self;       // base_dir = <git-root>/.amux/workflows/
    pub fn at_git_root(git_root: &Path) -> Self; // convenience for tests
    pub fn load(&self, workflow_name: &str) -> Result<Option<WorkflowState>, DataError>;
    pub fn save(&self, state: &WorkflowState) -> Result<(), DataError>;
    pub fn delete(&self, workflow_name: &str) -> Result<(), DataError>;
}
```

---

## Legacy Architecture (`oldsrc/`)

The following describes the user-facing `amux` binary, which continues to build from `oldsrc/` until work item 0070. The `oldsrc/` tree is frozen — no edits are allowed.

### High-level Overview

```
User
 │
 ▼
amux binary ──► command mode  ──► commands/{init,ready,implement,chat,new}
     │                                       │
     ├──────► interactive mode (TUI)         │
     │              │                        ▼
     │        tui/{mod,state,          runtime: AgentRuntime (Arc<dyn>)
     │         input,render,pty}             │
     │              │              ┌──────────┴──────────┐
     │              │         DockerRuntime       AppleContainersRuntime
     │              │              │                     │ (macOS 26+)
     │              ▼              ▼                     ▼
     │        Container Runtime ──────────────► Managed Container
     │          (Docker or                      (agent runs here)
     │       Apple Containers)
     │
     └──────► headless mode ──► commands/headless/{mod,server,db,process,logging}
                    │                        │
                    ▼                        ▼
             HTTP server (axum)      SQLite DB + log files
               localhost:<port>       ~/.amux/headless/
```

---

### Source Layout

```
oldsrc/
  main.rs                  Entry point: dispatch TUI or command mode
  lib.rs                   Re-exports public API for integration tests
  cli.rs                   clap CLI: Cli, Command, Agent enums
  config/
    mod.rs                 RepoConfig, GlobalConfig, HeadlessConfig, load/save helpers,
                           DEFAULT_SCROLLBACK_LINES, effective_scrollback_lines(),
                           effective_headless_work_dirs(), effective_always_non_interactive()
  commands/
    mod.rs                 Public run() dispatcher
    spec.rs                CommandSpec + FlagSpec tables: canonical single source of truth
                           for all subcommand flags. Imported by cli.rs, tui/mod.rs, and
                           tui/input.rs. Never imports from those modules (leaf node).
    output.rs              OutputSink: routes output to stdout or TUI channel
    auth.rs                Agent credential path resolution, auth prompts
    agent.rs               Shared agent launching: run_agent_with_sink()
                           Used by both implement and chat
    download.rs            GitHub downloads: Dockerfile templates (raw files)
                           and aspec folder (tarball extraction)
    init_flow.rs           Canonical `init` engine (mode-agnostic). Owns all business logic:
                           InitFlow::execute(): sequential stage runner
                           InitQa trait: ask_replace_aspec(), ask_run_audit(), ask_work_items_setup()
                           InitContainerLauncher trait: build_image(), run_audit()
                           InitParams, InitSummary, and per-stage StepStatus
                           All helpers: write_project_dockerfile(), write_agent_dockerfile(),
                             download_or_fallback_dockerfile(), print_init_summary(), print_whats_next()
    init.rs                Thin CLI shim: constructs CliInitQa (stdin-backed) and
                           CliContainerLauncher (synchronous blocking), then delegates to
                           init_flow::execute(). Contains no business logic.
    new.rs                 `amux new` (work item creation) — run() + run_with_sink()
                           WorkItemKind, slugify, apply_template,
                           find_template, next_work_item_number
                           Auto-downloads aspec/ if template is missing
    new_cmd.rs             `amux new` top-level dispatcher (spec/workflow/skill)
                           Routes NewAction variants to the appropriate module;
                           new spec delegates to specs::run_new()
    new_workflow.rs        `amux new workflow` — run_new_workflow() + run_new_workflow_with_sink()
                           WorkflowInput, WorkflowStepInput, WorkflowFormat
                           validate_artefact_name(), resolve_workflow_dest()
                           write_workflow_file(), serialize_workflow() (TOML / YAML / Markdown)
                           skeleton_workflow() for --interview mode
                           workflow_interview_agent_entrypoint() + non-interactive variant
    new_skill.rs           `amux new skill` — run_new_skill() + run_new_skill_with_sink()
                           SkillInput, resolve_skill_dest()
                           render_skill_file(), render_skill_skeleton(), write_skill_file()
                           skill_interview_agent_entrypoint() + non-interactive variant
    ready.rs               `amux ready` — run() + run_with_sink()
                           ReadyOptions, ReadyContext, ReadySummary, AuditSetup
                           StepStatus, print_summary, print_interactive_notice,
                           audit_entrypoint, audit_entrypoint_non_interactive
                           Engine functions (called identically from CLI and TUI):
                             compute_ready_build_flag(refresh, build)
                             is_legacy_layout(git_root, agent_name)
                             perform_legacy_migration(git_root)
                             gather_ready_env_vars(git_root, agent_name)
                             create_ready_host_settings(agent_name)
                             apply_ready_user_directive(host_settings, ctx)
                             check_allow_docker(out, allow_docker, runtime)
                             build_audit_setup(ctx, non_interactive)
                           run_pre_audit(), run_post_audit()
    implement.rs           `amux implement` — run() + run_with_sink()
                           agent_entrypoint, agent_entrypoint_non_interactive
    chat.rs                `amux chat` — run() + run_with_sink()
                           chat_entrypoint, chat_entrypoint_non_interactive
    exec.rs                `amux exec` — run_prompt(), run_workflow()
                           Thin dispatch layer: delegates to agent::run_agent_with_sink
                           (for prompt) and implement::run_workflow (for workflow);
                           agent_entrypoint_with_prompt helper
    headless/
      mod.rs               Top-level dispatch: run_start, run_kill, run_logs, run_status
      server.rs            axum HTTP router + handlers; shared AppState (sessions, allowlist,
                           in-memory busy-session mutex); request/response types
      db.rs                SQLite schema setup (sessions + commands tables);
                           all data-access functions; session/command CRUD;
                           AMUX_HEADLESS_ROOT env override for test isolation
      process.rs           OS process manager integration: systemd-run (Linux),
                           launchd plist (macOS), double-fork fallback;
                           PID file write/read/delete; live-process detection
      logging.rs           tracing-subscriber setup: human-readable to stdout
                           (foreground) or JSON/appending to amux.log (background);
                           periodic heartbeat log every 60 s
  runtime/
    mod.rs                 AgentRuntime trait (all container operations);
                           resolve_runtime() factory (reads GlobalConfig);
                           HostSettings (sanitized config mount, shared by all runtimes);
                           ContainerStats; free utilities: generate_container_name,
                           project_image_tag, agent_image_tag, parse_cpu_percent,
                           parse_memory_mb, format_build_cmd, format_run_cmd
    docker.rs              DockerRuntime — implements AgentRuntime via the
                           `docker` CLI; replaces src/docker/mod.rs
    apple.rs               AppleContainersRuntime — implements AgentRuntime via
                           the `container` CLI; #[cfg(target_os = "macos")]
  tui/
    mod.rs                 run() entry point; event loop; action dispatcher;
                           ClipboardWriter trait; copy_selection_to_clipboard();
                           capture_vt100_snapshot(); extract_selection_text()
    state.rs               App struct; Focus/ExecutionPhase/Dialog enums;
                           PendingCommand (Ready/Implement/Chat with flags,
                             including agent: Option<String> on Chat and Implement);
                           TuiInitAnswers: pre-collected init Q&A answers for TuiInitQa;
                           ContainerWindowState, ContainerInfo,
                           LastContainerSummary; terminal selection state fields;
                           terminal_scrollback_lines; container_inner_area;
                           Tab.ready_summary: Option<ReadySummary> (stores
                           pre-audit summary for handoff to post-audit phase)
    input.rs               handle_key(); Action enum (incl. CopyToClipboard);
                           autocomplete (flag_suggestions_for() generated from
                             CommandSpec — no manual hint lists);
                           key→bytes; Ctrl+Y copy keybinding
    flag_parser.rs         parse_flags(): generic TUI flag parser driven by CommandSpec.
                           Handles both --flag value and --flag=value forms.
                           flag_bool() / flag_string() convenience helpers.
                           Replaces the deleted parse_chat_flags(),
                           parse_implement_flags(), and parse_agent_flag() functions.
    render.rs              draw(); draw_exec_window/command_box/dialog etc.;
                           render_vt100_screen/no_cursor (selection highlight);
                           cell_in_selection(); scrollback depth probe + indicator
    pty.rs                 PtySession; PtyEvent; spawn_text_command helper
templates/
  Dockerfile.project       Project base template: FROM debian:bookworm-slim;
                           installs git, curl, make, ca-certificates; no USER directive.
                           Written to GITROOT/Dockerfile.dev on init.
  Dockerfile.claude        Agent template: FROM {{AMUX_BASE_IMAGE}}; installs Claude Code;
                           creates non-root amux user. Written to .amux/Dockerfile.claude.
                           Bundled fallback via include_str!; primary source downloaded
                           from github.com/prettysmartdev/aspec-cli
  Dockerfile.codex         Agent template (same pattern as claude)
  Dockerfile.opencode      Agent template (same pattern as claude)
  Dockerfile.maki          Agent template (same pattern as claude)
  Dockerfile.gemini        Agent template (same pattern as claude)
  Dockerfile.nanoclaw      Nanoclaw persistent-agent template (see docs/06-nanoclaw.md)
tests/
  cli_integration.rs       Binary-level integration tests
  command_tui_parity.rs    Verifies command/TUI mode share the same logic
  dockerfile_build.rs      Builds each agent template Dockerfile to verify validity
  download_integration.rs  GitHub download tests: templates, aspec folder, fallback
  memory_bounds.rs         vt100 scrollback cap, tab cleanup, memory-per-tab bounds
  terminal_selection.rs    Text selection, clipboard (MockClipboard), scrollback depth,
                           coordinate mapping, resize-clears-selection integration tests
```

---

### The `OutputSink` Abstraction

Every command function (`init::run_with_sink`, `ready::run_with_sink`, etc.) accepts
an `OutputSink` instead of calling `println!` directly:

```rust
pub enum OutputSink {
    Stdout,                               // command mode
    Channel(UnboundedSender<String>),     // TUI mode
}
```

`OutputSink` implements `Clone`, allowing it to be passed to streaming callbacks
like `runtime.build_image_streaming()`.

This is the core mechanism that allows zero code duplication between the two
execution modes. The command logic is identical — only the destination of output differs.

In command mode, `run()` wraps `run_with_sink(…, &OutputSink::Stdout)`.
In TUI mode, `execute_command()` passes `OutputSink::Channel(app.output_tx.clone())`.

---

### The `AgentRuntime` Abstraction

All container operations go through a single `AgentRuntime` trait defined in
`oldsrc/runtime/mod.rs`. This decouples the agent-launching logic from any
specific container technology.

```rust
pub trait AgentRuntime: Send + Sync {
    fn is_available(&self) -> bool;
    fn name(&self) -> &'static str;
    fn cli_binary(&self) -> &'static str;

    // Image lifecycle
    fn build_image(&self, tag: &str, dockerfile: &Path, context: &Path, no_cache: bool) -> Result<String>;
    fn build_image_streaming<F>(&self, ...) -> Result<String>;
    fn image_exists(&self, tag: &str) -> bool;

    // Container run variants
    fn run_container(&self, ...) -> Result<()>;
    fn run_container_captured(&self, ...) -> Result<(String, String)>;
    fn run_container_detached(&self, ...) -> Result<String>;
    // … additional run_container_at_path variants …

    // Container lifecycle
    fn start_container(&self, id: &str) -> Result<()>;
    fn stop_container(&self, id: &str) -> Result<()>;
    fn remove_container(&self, id: &str) -> Result<()>;
    fn is_container_running(&self, id: &str) -> bool;

    // Discovery & stats
    fn list_running_containers_by_prefix(&self, prefix: &str) -> Vec<String>;
    fn query_container_stats(&self, name: &str) -> Option<ContainerStats>;

    // PTY argument builders (for TUI interactive sessions)
    fn build_run_args_pty(&self, ...) -> Vec<String>;
    fn build_exec_args_pty(&self, ...) -> Vec<String>;
}
```

The runtime is resolved once at startup via `resolve_runtime(&GlobalConfig)`,
which reads the `runtime` config field and returns an `Arc<dyn AgentRuntime>`.

### Runtime implementations

| Struct | File | Notes |
|--------|------|-------|
| `DockerRuntime` | `oldsrc/runtime/docker.rs` | Wraps the `docker` CLI |
| `AppleContainersRuntime` | `oldsrc/runtime/apple.rs` | Wraps the `container` CLI; `#[cfg(target_os = "macos")]` |

---

### Working Directory Contract

All `run_with_sink` functions accept an explicit `cwd: &Path` parameter that
determines where the Git root is searched from. This ensures correctness for
both execution modes:

| Mode | `cwd` value | Behaviour |
|------|-------------|-----------|
| CLI (command mode) | `std::env::current_dir()` | Uses the directory where `amux` was launched |
| TUI (interactive mode) | `app.active_tab().cwd` | Uses the tab's working directory |

**Rule:** No command implementation may call `find_git_root()` (which reads the
process CWD). All callers must use `find_git_root_from(cwd)` with an explicitly
provided `cwd`.

---

### TUI State Machine

The TUI state is split across three orthogonal enums plus the `App` struct:

#### `Focus`

```
CommandBox  ←──── Esc ────── ExecutionWindow
    │                                ▲
    └─────── ↑ arrow / running ──────┘
```

#### `ExecutionPhase`

```
Idle ──[Submit]──► Running ──[exit 0]──► Done
                      │
                      └──[exit ≠ 0]──► Error
```

#### `Dialog`

```
None ──[q / Ctrl+C]──────────────────────────► QuitConfirm      ──[y]──► quit
     ──[ready|implement|chat, cwd ≠ root]──► MountScope        ──[r/c]──► resume
     ──[new]───────────────────────────────► NewKindSelect      ──[1/2/3]──► NewTitleInput ──[Enter]──► create
     ──[init, --aspec + aspec/ exists]─────► InitReplaceAspec   ──[y/n]─┐
     ──[init, all other cases]────────────────────────────────────────►  InitAuditConfirm ──[y/n]──► InitWorkItemsSetup ──[y/n]──► launch_init()
```

---

### CLI/TUI Flag Unification

`spec.rs` is the leaf module that all three sites import from. It defines every
flag for every subcommand as static data:

```rust
pub struct FlagSpec {
    pub name: &'static str,
    pub takes_value: bool,
    pub value_name: &'static str,
    pub hint: &'static str,
}

pub struct CommandSpec {
    pub name: &'static str,
    pub flags: &'static [FlagSpec],
}

pub static ALL_COMMANDS: &[CommandSpec] = &[
    CommandSpec { name: "chat",      flags: CHAT_FLAGS      },
    CommandSpec { name: "implement", flags: IMPLEMENT_FLAGS },
    // … all subcommands
];
```

`parse_flags(parts, spec)` in `tui/flag_parser.rs` replaces all ad-hoc `parse_*_flags()` functions and drives both TUI parsing and autocomplete from the same `CommandSpec`.

### Agent override resolution order

1. **Flag** — `--agent <name>` passed on the command line (CLI or TUI)
2. **Repo config** — `agent` field in `.amux/config.json`
3. **Global config** — `default_agent` field in `~/.amux/config.json`
4. **Built-in default** — `claude`

---

### Ready Command

The `ready` command has two modes based on the `--refresh` flag:

**Without `--refresh`** (default): check runtime, Dockerfile.dev, and images; print summary.

**With `--refresh`**: check runtime → launch agent to audit Dockerfile.dev → rebuild images → print summary.

All business logic for `ready` lives in `oldsrc/commands/ready.rs`. The TUI and CLI call the same engine functions; the only difference is how user input is collected and how the audit container is executed.

---

### Init Command

All business logic lives in `oldsrc/commands/init_flow.rs`, called identically from the CLI (`init.rs`) and TUI adapters. The two differ only in `InitQa` (stdin vs. pre-collected TUI answers) and `InitContainerLauncher` (synchronous vs. background task).

---

### Docker Build Streaming

`docker::build_image_streaming()` spawns `docker build` and reads stdout and stderr concurrently in separate background threads, forwarding lines through a shared `mpsc` channel to the `on_line` callback as they arrive.

---

### PTY Architecture

```
App::pty (PtySession)
    │
    ├── master (Box<dyn MasterPty>)       ← held for resize()
    └── input_tx (SyncSender<Vec<u8>>)    ← TUI keypresses → writer thread
                                                           → PTY master
                                                           → container stdin

PtyEvent channel (std::sync::mpsc)
    ├── reader thread → Data(Vec<u8>)     ← PTY master → strip ANSI → output_lines
    └── wait thread   → Exit(i32)         ← child.wait() → finish_command()
```

---

### Container Window

```
Hidden ──[start_container()]──► Maximized ──[Esc]──► Minimized ──['c']──► Maximized
                                     │                    │
                                     └────[finish]────────┘──► Hidden + Summary bar
```

When maximized, the container window covers 95% of the outer execution window. When minimized, a 1-line green-bordered bar shows the agent name and live stats.

Container stats are polled every 5 seconds via a tokio task that calls `docker stats --no-stream`.

---

### Host Settings Injection

`HostSettings` encapsulates the preparation and lifetime of the sanitized agent configuration mounted into every container:

```
~/.claude.json   ──sanitize──► temp/claude.json      (oauthAccount removed,
~/.claude/       ──filter──►   temp/dot-claude/        /workspace trust added,
                                   settings.json        LSP suppression applied)
```

The denylist excludes `projects/`, `sessions/`, `history.jsonl`, `telemetry/`, and similar host-only artefacts.

---

### Agent Auth Flow

```
ready/implement/chat submitted
        │
        ▼
   read_keychain_raw() → extract OAuth JSON → CLAUDE_CODE_OAUTH_TOKEN env var
```

Credentials are sourced from the macOS system keychain and passed as an environment variable — never mounted as files.

---

### Performance Characteristics

**Render loop:** `terminal.draw()` is called unconditionally on every loop iteration (~60 Hz). Ratatui double-buffering means terminal I/O is proportional to changed cells, not screen size.

**Output buffer:** `TabState` holds an `output_lines: Vec<String>`. A 10,000-line cap (configurable) applies to the vt100 container parser. The outer text buffer is bounded by a VecDeque cap (see work item 0035).

**Docker interaction:** all Docker operations spawn a new `std::process::Command` child. Stats are polled every 5 seconds per active container.

**Scalability target:** 20 concurrent tabs.

---

### Headless Mode

The headless server runs as a third execution mode alongside command mode and the TUI.

```
HTTP client
     │
     ▼
axum router (server.rs)
     │
     ├── POST /v1/sessions ──► db::create_session() ──► SQLite
     │
     └── POST /v1/commands ──► validate session (DB)
                                     │
                                     └── tokio::spawn ──► commands::run() dispatch
                                                               │
                                                               ▼
                                                         Docker container
                                                         stdout/stderr → log files
                                                         status → db::update_command()
```

`AppState` holds the allowlist, a `Mutex<Connection>`, and a per-session mutex map. The `AMUX_HEADLESS_ROOT` env var overrides the storage root for test isolation.

Background daemonization: systemd-run on Linux, launchd plist on macOS, double-fork fallback elsewhere.

---

### Testing Strategy

| Layer | Location | What is tested |
|-------|----------|----------------|
| Layer 0 unit | `src/data/**/#[cfg(test)]` | Session, SessionManager, all config types, all fs stores |
| Unit — per module | `oldsrc/**/#[cfg(test)]` | Individual functions, data structures |
| Unit — border colors | `oldsrc/tui::state::tests` | All 6 combinations of phase × focus |
| Unit — PTY data | `oldsrc/tui::state::tests` | `\r`/`\n`/`\r\n` processing, live-line updates |
| Unit — container window | `oldsrc/tui::state::tests` | Container state transitions, PTY routing, summary generation |
| Unit — CLI/spec parity | `oldsrc/cli::tests` | Every clap flag for each subcommand is present in `spec::*_FLAGS` and vice versa |
| Unit — flag parser | `oldsrc/tui::flag_parser::tests` | `parse_flags()` with every flag in both forms |
| Unit — init flow | `oldsrc/commands::init_flow::tests` | Each stage via mock InitQa + InitContainerLauncher |
| Unit — headless db | `oldsrc/commands::headless::db::tests` | Schema creation, session/command CRUD |
| Integration — CLI | `tests/cli_integration.rs` | Binary-level: help, version, flags, work items |
| Integration — parity | `tests/command_tui_parity.rs` | Shared logic between command/TUI modes |
| Integration — headless HTTP | `oldsrc/commands::headless::server::tests` | Full session + command lifecycle |
| End-to-end — headless | `tests/headless_integration.rs` | `amux headless start` subprocess; HTTP requests via reqwest |

---

[← Headless Mode](08-headless-mode.md) · [Contents](contents.md)
