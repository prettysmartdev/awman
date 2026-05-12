# Work Item: Task

Title: grand architecture refactor — part 1/5 — foundation, oldsrc move, and Layer 0 (data)
Issue: n/a — first of five work items implementing `aspec/architecture/2026-grand-architecture.md`

## Required reading before starting

This work item is the first of five that together execute the grand architecture refactor described in `aspec/architecture/2026-grand-architecture.md`. The implementing agent **MUST** read that document in full before writing any code, internalize the layering tenets (no upward calls, frontends have no business logic, typed objects over `pub fn`), and treat it as the single source of truth for every design decision. When this work item is silent or ambiguous, defer to the grand architecture document. When the grand architecture document is silent or ambiguous, **STOP and ASK THE DEVELOPER** rather than guess.

The companion work items are:

- `0067-grand-architecture-layer-1-engines.md` — Layer 1 (engine: container, workflow, git, overlay, auth)
- `0068-grand-architecture-layer-2-command-and-dispatch.md` — Layer 2 (command + dispatch)
- `0069-grand-architecture-layer-3-frontends-and-binary.md` — Layer 3 (CLI, TUI, headless) + Layer 4 (binary)
- `0070-grand-architecture-finalize-and-remove-oldsrc.md` — final parity validation, oldsrc removal, docs

## Summary:

- Rename the existing `src/` tree to `oldsrc/` and rewire `Cargo.toml` so the existing `amux` binary continues to build and run from `oldsrc/` for the duration of the refactor. **No legacy code may be edited inside `oldsrc/` after this work item completes** — it is frozen reference material.
- Scaffold a new, empty `src/` tree organized strictly by the five-layer architecture (`src/data/`, `src/engine/`, `src/command/`, `src/frontend/`, plus `src/main.rs` for Layer 4) and add a second binary target `amux-next` (or equivalent) that compiles from `src/` so each layer can be exercised in isolation while `oldsrc/` keeps shipping.
- Fully implement Layer 0 (`src/data/`) per the grand architecture: the new `Session` and `SessionState` types, `SessionManager`, all configuration concerns (repo config, global config, env vars, flag-value reconciliation), and every filesystem and database concern (sqlite for headless, headless directories, workflow state persistence, global workflow/skill directories, container overlay & agent settings filepath resolution).
- No business logic, no container interaction, no git interaction, no workflow execution logic, no command logic, no frontend code is permitted in `src/data/`. Layer 0 only describes data, persists it, and resolves filesystem paths.
- Every public surface in `src/data/` must be expressed as a typed object (struct + methods) rather than a free `pub fn`, except for clearly stateless helpers (e.g. a single hash, a path-join helper) and constructors. This tenet is non-negotiable per the grand architecture document.
- Comprehensive unit tests for every Layer 0 type, including round-tripping config files, env-var precedence, sqlite open/migrate/close, and `SessionManager` concurrency-safety.

## User Stories

### User Story 1:
As a: maintainer of amux

I want to:
freeze the existing `src/` tree as `oldsrc/` and start a clean, layered `src/` tree from scratch

So I can:
build the grand architecture without legacy patterns leaking in, and so the existing `amux` binary keeps building and shipping for users while the refactor is in flight.

### User Story 2:
As a: future implementing agent picking up Layer 1, 2, 3, or 4

I want to:
find a fully realized Layer 0 with `Session`, `SessionState`, `SessionManager`, and every config + filesystem concern already implemented, tested, and documented

So I can:
build each subsequent layer on a solid foundation without having to revisit data definitions or filesystem concerns.

### User Story 3:
As a: maintainer reading `src/data/`

I want to:
see typed objects (e.g. `RepoConfig::load(git_root)`, `GlobalConfig::load()`, `Session::new(...)`, `SessionManager::insert(...)`, `WorkflowStateStore::save(...)`) rather than a sprawl of free `pub fn` calls

So I can:
trust that the data layer is encapsulated, easy to mock in higher layers, and impossible to misuse by accident.

## Implementation Details:

### 0. Required reading and ground rules

The implementing agent **MUST**:

1. Read `aspec/architecture/2026-grand-architecture.md` end-to-end before writing any code.
2. Read `aspec/foundation.md`, `aspec/architecture/design.md`, `aspec/architecture/security.md`, and `aspec/devops/localdev.md` for project-wide constraints.
3. Treat the grand architecture's tenets as binding:
   - Lower layers MUST NOT call functions or use types from higher layers. Layer 0 calls nothing above itself; if it ever needs an upward concern, it defines a trait that a higher layer implements.
   - Frontends are forbidden from holding business logic — irrelevant to this work item but informs how Layer 0's API is shaped (no frontend-specific types here).
   - Prefer typed objects over `pub fn`. Construct structs that own their state and expose methods. Free `pub fn` is only acceptable for stateless helpers, constructors, and small one-off utilities.
4. When uncertain about layer placement or naming, **ASK THE DEVELOPER** — do not guess.

### 1. Move existing `src/` to `oldsrc/`

Rename the entire `src/` directory to `oldsrc/` with `git mv`. Do not edit any file inside `oldsrc/`. Update `Cargo.toml`:

```toml
[[bin]]
name = "amux"
path = "oldsrc/main.rs"

[lib]
name = "amux"
path = "oldsrc/lib.rs"
```

Add a second binary target that compiles from the new tree (this is what subsequent work items grow into the real `amux`):

```toml
[[bin]]
name = "amux-next"
path = "src/main.rs"
```

The `amux-next` binary in this work item is a stub that prints `amux-next: Layer 0 only — see aspec/architecture/2026-grand-architecture.md` and exits 0. Its job in this work item is to give CI a way to exercise the Layer 0 crate. The user-facing `amux` binary remains identical to today and continues to be built from `oldsrc/`.

Update `Makefile` so `make all`, `make install`, and `make test` continue to do exactly what they did before (build/install/test the `amux` binary). Add `make test-next` that runs `cargo test --bin amux-next` and `cargo test -p amux --test '*'` filtered to the new tree only — but only if straightforward; otherwise put the new tests under the same `cargo test` invocation and ensure they pass alongside legacy tests.

Add a top-of-file comment to every file under `oldsrc/` (or, more practically, a single `oldsrc/README.md`) stating:

> **FROZEN.** This tree is the pre-refactor amux source. Do not edit. The new architecture lives under `src/`. See `aspec/architecture/2026-grand-architecture.md`. This tree will be deleted in work item 0070.

### 2. Scaffold new `src/` tree

Create the following directory structure:

```
src/
  main.rs                  # Layer 4 stub (becomes the real entrypoint in 0069)
  lib.rs                   # re-exports the four layers
  data/                    # Layer 0
    mod.rs
    session.rs
    session_manager.rs
    config/
      mod.rs
      repo.rs
      global.rs
      env.rs
      flags.rs
    fs/
      mod.rs
      headless_db.rs
      headless_paths.rs
      workflow_state.rs
      skill_dirs.rs
      workflow_dirs.rs
      overlay_paths.rs
      auth_paths.rs
    error.rs
  engine/                  # Layer 1 — empty in 0066, filled in 0067
    mod.rs                 # `// populated in work item 0067`
  command/                 # Layer 2 — empty in 0066, filled in 0068
    mod.rs                 # `// populated in work item 0068`
  frontend/                # Layer 3 — empty in 0066, filled in 0069
    mod.rs                 # `// populated in work item 0069`
```

`src/lib.rs`:

```rust
pub mod data;
pub mod engine;     // empty until 0067
pub mod command;    // empty until 0068
pub mod frontend;   // empty until 0069
```

`src/main.rs` is a 5-line stub (described above).

### 3. Implement Layer 0 (`src/data/`)

The grand architecture explicitly enumerates what belongs in Layer 0. Every item below MUST be implemented in this work item.

**PARITY MANDATE**: Layer 0 sits at the storage boundary. Anything users have on disk today (config files, sqlite db, workflow state files, api-key hash, PID file) MUST remain readable and writable by the new code without any user-visible migration. The "Compatibility Inventory" subsections below pin the exact schemas, field names, file paths, env-var names, and on-disk formats that MUST be preserved byte-for-byte. **These are NOT design suggestions — they are contracts with existing user data.**

#### 3a. `Session` and `SessionState` (`src/data/session.rs`)

`Session` is the new ruling type for all amux operations. It replaces:

- `oldsrc/tui/state.rs::TabState` (TUI tabs become a frontend representation of a `Session`).
- The ad-hoc session struct currently inside `oldsrc/commands/headless/server.rs`.
- The implicit "current working directory + git root" state that today's CLI infers from `std::env::current_dir`.

A `Session` MUST own:

- `id: SessionId` — newtype wrapping `uuid::Uuid` (ULID is also acceptable; ASK THE DEVELOPER if unsure which).
- `working_dir: PathBuf` — the directory the session was created from.
- `git_root: PathBuf` — resolved once at session construction; sessions cannot exist without a git root.
- `repo_config: RepoConfig` — fully loaded and merged at construction time.
- `global_config: GlobalConfig` — fully loaded and merged at construction time.
- `default_agent: AgentName` — newtype around `String`, not free strings.
- `available_agents: Vec<AgentName>` — derived from config + filesystem at construction.
- `state: SessionState` — see below.
- `created_at`, `last_active_at` (monotonic + wallclock).

`SessionState` MUST own:

- `current_command: Option<CommandInvocation>` — the in-flight command (defined as a Layer 0 data struct, not a Layer 2 type — Layer 2 builds on this).
- `current_workflow: Option<WorkflowInvocation>` — workflow id, work item, current step index, paused/yolo/auto flags, etc. Persistable.
- `current_container: Option<ContainerHandle>` — Layer 0 holds *only* the persistable identity (container id, image tag, name, started_at). The runtime object that controls a container is Layer 1 and is **not** stored here.
- `errors: Vec<SessionError>` — structured error log.
- `notes: Vec<SessionNote>` — anything the engine/command layers want to surface to a frontend (used in 0067/0068).

Constructors:

```rust
impl Session {
    pub fn open(working_dir: PathBuf) -> Result<Self, SessionError>;       // resolves git root, loads configs
    pub fn open_at_git_root(git_root: PathBuf) -> Result<Self, SessionError>;
    pub fn id(&self) -> SessionId;
    pub fn git_root(&self) -> &Path;
    pub fn repo_config(&self) -> &RepoConfig;
    pub fn global_config(&self) -> &GlobalConfig;
    pub fn state(&self) -> &SessionState;
    pub fn state_mut(&mut self) -> &mut SessionState;
    // and so on — every field accessor as a typed method
}
```

Layer 0 MUST NOT call git commands directly — `Session::open` resolves git root via a `GitRootResolver` trait that is implemented in Layer 1 (`GitEngine`) and passed in. **However**, since Layer 1 does not yet exist in this work item, expose a small temporary trait:

```rust
pub trait GitRootResolver {
    fn resolve(&self, working_dir: &Path) -> Result<PathBuf, SessionError>;
}
```

…and implement a single test-only `static_resolver` that returns a fixed path. The real implementation lands in 0067. **Do not** invoke `git rev-parse` from `src/data/` — that is a Layer 1 concern.

If this dependency-direction is awkward (a `Session` cannot fully open without git root resolution and Layer 1 doesn't exist yet), **ASK THE DEVELOPER** whether to (a) accept the resolver as a constructor argument, (b) split `Session::open` into `Session::open(git_root)` (taking pre-resolved git root) with the resolver invocation moving to Layer 2 entirely, or (c) something else.

#### 3b. `SessionManager` (`src/data/session_manager.rs`)

`SessionManager` owns a collection of `Session` and:

- Provides CRUD: `create`, `get`, `get_mut`, `list`, `remove`.
- Is concurrency-safe — internal locking is `tokio::sync::RwLock`;
- Issues unique `SessionId` values.
- For headless mode: persists session metadata to the sqlite database (see §3d) on mutation. Persistence is opt-in: `SessionManager::with_persistence(store: impl SessionStore)` vs `SessionManager::in_memory()`.
- The CLI uses `SessionManager::in_memory()` and creates exactly one session per invocation. The TUI uses `SessionManager::in_memory()` and creates one session per tab. The headless server uses `SessionManager::with_persistence(...)` and one session per API session.

`SessionStore` is a Layer 0 trait implemented by Layer 0's `SqliteSessionStore` — note this does *not* violate the layering rule because Layer 0 is implementing its own trait. Higher layers consume `SessionManager`.

#### 3c. Config (`src/data/config/`)

Move every config concern out of `oldsrc/config/mod.rs` (1636 lines) into structured modules:

- `repo.rs` — `RepoConfig`, `OverlaysConfig`, `DirectoryOverlayConfig`, `WorkItemsConfig`, `RemoteConfig`, `HeadlessConfig`. Methods: `RepoConfig::load(git_root)`, `RepoConfig::save(&self, git_root)`, `RepoConfig::path(git_root)`, `RepoConfig::legacy_path(git_root)`, `RepoConfig::migrate_legacy(git_root)`.
- `global.rs` — `GlobalConfig` with methods `GlobalConfig::load()`, `GlobalConfig::save(&self)`, `GlobalConfig::path()`.
- `env.rs` — typed reads of every env var amux honors. Each var is a constant + a typed read method on a `Env` struct or namespace, never a scattered `std::env::var("AMUX_…")` call.
- `flags.rs` — typed flag values that survive across the layers. Frontends parse user input into these structs and pass them down. (Concrete `clap` definitions still live in Layer 2's Dispatch in 0068; *this* file just defines the typed flag value structs.)

Define a single `EffectiveConfig` type that owns the merged view (repo + global + env + flags) and exposes typed accessors that today exist as scattered free `pub fn` calls in `oldsrc/config/mod.rs` (`effective_env_passthrough`, `effective_yolo_disallowed_tools`, `effective_scrollback_lines`, `effective_agent_stuck_timeout`, `effective_headless_work_dirs`, `effective_remote_default_addr`, `effective_remote_default_api_key`, `effective_remote_saved_dirs`). Each becomes a method on `EffectiveConfig`. Note: `effective_always_non_interactive` is dropped — the top-level global config field it read was never used. Non-interactive mode for agent containers is now controlled exclusively by `GlobalConfig::headless.alwaysNonInteractive` (read directly by `Dispatch`) and the `--non-interactive` CLI flag.

`Session` owns an `EffectiveConfig` (or constructs one on demand).

##### Compatibility Inventory — JSON schema (parity-critical)

**Repository config** — `<git_root>/.amux/config.json`. Field-by-field schema; the JSON keys are load-bearing — mixed snake_case + camelCase is the contract:

| Rust field | JSON key | Type | Default | Notes |
|---|---|---|---|---|
| `agent` | `agent` (snake) | `Option<String>` | `None` (inherits global `default_agent`) | Validates against canonical agent enum |
| `auto_agent_auth_accepted` | `auto_agent_auth_accepted` (snake) | `Option<bool>` | `None` | **Read-only via CLI**; set by auto-auth flow only — `EffectiveConfig` exposes a getter, **no setter via `amux config set`** |
| `terminal_scrollback_lines` | `terminal_scrollback_lines` (snake) | `Option<usize>` | `10_000` | Must be `> 0` |
| `yolo_disallowed_tools` | `yoloDisallowedTools` (camel) | `Option<Vec<String>>` | `None` | Empty string clears (becomes `Some(vec![])`) |
| `env_passthrough` | `envPassthrough` (camel) | `Option<Vec<String>>` | `None` | Empty string explicitly clears |
| `work_items` | `workItems` (camel) | `Option<WorkItemsConfig>` | `None` | Nested |
| `work_items.dir` | `dir` (snake within nested) | `Option<String>` | `None` | Path-escape validated via `validate_path_within_git_root` |
| `work_items.template` | `template` (snake within nested) | `Option<String>` | `None` | Same path-escape validation |
| `overlays` | `overlays` (snake) | `Option<OverlaysConfig>` | `None` | Nested |
| `overlays.directories[]` | `directories` (snake) | `Option<Vec<DirectoryOverlayConfig>>` | `None` | |
| `overlays.directories[].host` | `host` | `String` | required | Tilde-expanded, made absolute |
| `overlays.directories[].container` | `container` | `String` | required | Must be absolute path |
| `overlays.directories[].permission` | `permission` | `Option<String>` | `"ro"` when absent | Must be `"ro"` or `"rw"` |
| `agent_stuck_timeout_secs` | `agentStuckTimeout` (camel — note: NO `Secs` suffix in JSON) | `Option<u64>` | `30` | Must be `> 0` |

**Global config** — `$HOME/.amux/config.json`:

| Rust field | JSON key | Type | Default | Notes |
|---|---|---|---|---|
| `default_agent` | `default_agent` (snake) | `Option<String>` | `"claude"` | |
| `terminal_scrollback_lines` | `terminal_scrollback_lines` (snake) | `Option<usize>` | `10_000` | |
| `runtime` | `runtime` (snake) | `Option<String>` | `"docker"` | Must be `"docker"` or `"apple-containers"`; `apple-containers` falls back to `docker` on non-mac with a warning |
| `yolo_disallowed_tools` | `yoloDisallowedTools` (camel) | `Option<Vec<String>>` | `None` | |
| `env_passthrough` | `envPassthrough` (camel) | `Option<Vec<String>>` | `None` | |
| `headless` | `headless` (snake) | `Option<HeadlessConfig>` | `None` | Nested |
| `headless.work_dirs` | `workDirs` (camel) | `Option<Vec<String>>` | `None` | Absolute paths, canonicalized at server start |
| `headless.always_non_interactive` | `alwaysNonInteractive` (camel) | `Option<bool>` | `false` | Drives `--non-interactive` injection for `chat`, `ready`, `exec prompt`, `exec workflow`, `specs amend` |
| `remote` | `remote` (snake) | `Option<RemoteConfig>` | `None` | Nested |
| `remote.default_addr` | `defaultAddr` (camel) | `Option<String>` | `None` | URL, e.g. `http://1.2.3.4:9876` |
| `remote.saved_dirs` | `savedDirs` (camel) | `Option<Vec<String>>` | `None` | Pre-saved working dirs for `remote session start` |
| `remote.default_api_key` | `defaultAPIKey` (CAMEL — uppercase `API`, NOT `Api`) | `Option<String>` | `None` | Masked on display via `first4…last4` |
| `overlays` | `overlays` (snake) | `Option<OverlaysConfig>` | `None` | Same shape as repo |
| `agent_stuck_timeout_secs` | `agentStuckTimeout` (camel) | `Option<u64>` | `30` | |

**Renames are load-bearing.** `serde(rename = "...")` MUST be applied per the table. "Normalizing" `defaultAPIKey` to `defaultApiKey` or `agentStuckTimeout` to `agentStuckTimeoutSecs` will silently drop user data.

**Legacy keys MUST be silently ignored on load (NOT deserialized into the new fields).** The pre-WI-0058 flat keys `headlessWorkDirs` and `remoteDefaultAddr` at the top level of global config MUST remain rejected (preserve the breaking change documented by the older work item). Tests must assert this.

##### Compatibility Inventory — `EffectiveConfig` merge rules (per-field, NOT uniform)

The merge rule differs by field. The 0067-spec phrase "flag > env > repo > global > built-in default" is correct in spirit but the actual rules are:

| Field | Rule |
|---|---|
| `terminal_scrollback_lines` | repo > global > 10000 |
| `yolo_disallowed_tools` | repo > global > empty (**NO additive merge** — repo `[]` wins entirely over global list) |
| `env_passthrough` | repo > global > empty (**NO additive merge** — repo `[]` wins entirely) |
| `agent_stuck_timeout_secs` | repo > global > 30 |
| `agent` (effective) | repo.agent > global.default_agent > "claude" |
| `runtime` | global only > "docker" |
| `default_agent` | global only > "claude" |
| `headless.workDirs` | global only |
| `headless.alwaysNonInteractive` | global only |
| `remote.defaultAddr` | env (`AMUX_REMOTE_ADDR`) > global only |
| `remote.savedDirs` | global only |
| `remote.defaultAPIKey` | env (`AMUX_API_KEY`) > global only — but `defaultAPIKey` only forwarded when target addr matches `defaultAddr` exactly (after case-insensitive + trailing-slash normalization). **Cross-host forwarding MUST be prevented.** |
| `work_items.dir/template` | repo only |
| `auto_agent_auth_accepted` | repo only, read-only |
| `overlays` | **additive 4-source merge** — see below |

**Vec list scope replacement is critical.** `envPassthrough` and `yoloDisallowedTools` are NOT additively merged. A repo config setting them to `[]` MUST result in `EffectiveConfig::env_passthrough() == &[]`, NOT the global list. Required test: `effective_env_passthrough_repo_empty_array_wins_over_global` (preserve the legacy test by name).

**Overlay merge** — 4 sources, additive, by `conflict_key`:
- Priority 0: global config `overlays.directories`
- Priority 1: repo config `overlays.directories`
- Priority 2: `AMUX_OVERLAYS` env var (parsed)
- Priority 3: CLI `--overlay` flags

Dedup by `conflict_key()` (canonicalized host path; falls back to raw path string if canonicalize fails). Higher-priority `container_path` wins. **Most restrictive `permission` wins regardless of priority** (`ro` beats `rw`). Missing host paths are non-fatal (`tracing::warn!` + drop). Malformed values are FATAL for both env var and CLI flags. Container-path collisions across distinct host paths emit a warning.

The merge logic MUST live in Layer 0 as a method on `EffectiveConfig` (e.g. `EffectiveConfig::overlays(&Env, &Flags) -> Vec<DirectoryOverlay>`) or in a dedicated `OverlayResolver` data type. **The parsing of `AMUX_OVERLAYS` env-var grammar is also Layer 0.** The mounting (i.e. translating `Vec<DirectoryOverlay>` into container `-v` flags) is Layer 1's `OverlayEngine`.

**`AMUX_OVERLAYS` grammar** (preserved from `oldsrc/overlays/mod.rs`):

```
overlay-list   := overlay-expr ("," overlay-expr)*
overlay-expr   := type-tag "(" overlay-args ")"
type-tag       := "dir"           # currently the only type
overlay-args   := host-path ":" container-path [ ":" permission ]
permission     := "ro" | "rw"
```

Outer commas split via `split_top_level_commas` (parens nest). `~` and `~/...` expand to `$HOME`. Relative paths resolve against `cwd`. Whitespace around tokens trimmed. Spaces in paths permitted literally (no quoting). Unknown type tag → fatal error listing supported tags.

##### Compatibility Inventory — `ConfigFieldDef` master list

`oldsrc/commands/config.rs::ALL_FIELDS` is the canonical metadata source for the `amux config show/get/set` CLI surface. Reproduce it as a Layer 0 typed object (e.g. `pub static CONFIG_FIELDS: &[ConfigFieldDef]`) exposed for Layer 2's CLI to consume. Without this, the new `amux config show/get/set` cannot match. Each entry carries: dotted key, scope (global/repo), Rust type, default, description, settable flag, validation. `auto_agent_auth_accepted` MUST have `settable=false`; expose only a typed marker token method (e.g. `RepoConfig::record_auth_decision(...)`) for the auto-auth flow.

##### Compatibility Inventory — Built-in defaults

Built-in defaults (returned by `EffectiveConfig` accessors when no scope sets them):

- `default_agent = "claude"`
- `runtime = "docker"`
- `terminal_scrollback_lines = 10_000` (constant `DEFAULT_SCROLLBACK_LINES`)
- `agent_stuck_timeout_secs = 30` (constant `DEFAULT_STUCK_TIMEOUT_SECS`)
- `headless.alwaysNonInteractive = false`

All other fields default to `None` / empty.

##### Compatibility Inventory — Environment variables

Every env var amux honors (each becomes a constant + typed accessor on the `Env` struct, never a scattered `std::env::var(...)`):

| Env var | Effect | Layer 0 home |
|---|---|---|
| `AMUX_CONFIG_HOME` | Override `$HOME/.amux/` location (test/CI override; affects `global_config_path`, `global_workflows_dir`, `global_skills_dir`) | `Env::config_home()` |
| `AMUX_HEADLESS_ROOT` | Override `~/.amux/headless/` (used in tests + power users) | `Env::headless_root()` |
| `AMUX_OVERLAYS` | Comma-separated overlay spec; priority 2 in overlay merge | `Env::overlays()` |
| `AMUX_API_KEY` | API key for remote headless; takes precedence over `remote.defaultAPIKey` config | `Env::api_key()` |
| `AMUX_REMOTE_ADDR` | Remote server addr; takes precedence over `remote.defaultAddr` | `Env::remote_addr()` |
| `AMUX_REMOTE_SESSION` | Resumable remote session ID for `remote run` | `Env::remote_session()` |
| `RUST_LOG` | Tracing log filter (consumed by Layer 3 logging init) | `Env::rust_log()` |
| `TERM_PROGRAM` | VS Code detection for `amux new` UX | `Env::term_program()` |
| `HOME` | Standard | `Env::home()` |

Plus the dynamic set named in `envPassthrough` config — those are read at agent-launch time from the host process env and forwarded into containers as `-e NAME=value`.

#### 3d. Filesystem (`src/data/fs/`)

Move every direct filesystem and database concern out of the old code into typed objects:

- `headless_db.rs` — `SqliteSessionStore` (replaces the loose helpers in `oldsrc/commands/headless/db.rs`). Owns the sqlite connection pool, schema migrations, CRUD. Consumes `Session` and persists relevant fields.
- `headless_paths.rs` — `HeadlessPaths` struct: typed accessors for the headless root and its child files/dirs.
- `workflow_state.rs` — `WorkflowStateStore`: persists `WorkflowInvocation` to disk. Replaces the free `pub fn`s `workflow_state_path`, `save_workflow_state`, `load_workflow_state`, `validate_resume_compatibility` in `oldsrc/workflow/mod.rs`.
- `workflow_definition.rs` — parses workflow files (`.md`, `.toml`, `.yml`/`.yaml`) into `Workflow` + `WorkflowStep` structs; provides `substitute_prompt` for template variables. See "Workflow definition format" below.
- `worktree_paths.rs` — `WorktreePaths`: resolves `~/.amux/worktrees/<repo-name>/<NNNN>/` and `~/.amux/worktrees/<repo-name>/wf-<name>/`. The git operations themselves are Layer 1, but path resolution is Layer 0.
- `repo_dockerfile_paths.rs` — `RepoDockerfilePaths`: resolves `<git_root>/.amux/Dockerfile.dev` and `<git_root>/.amux/Dockerfile.<agent>` per agent.
- `skill_dirs.rs` — `SkillDirs`: typed access to global (`$HOME/.amux/skills/`) + per-repo (`<git_root>/.claude/skills/`) skill directories.
- `workflow_dirs.rs` — `WorkflowDirs`: typed access to global (`$HOME/.amux/workflows/`) + per-repo (`<git_root>/aspec/workflows/`) workflow directories.
- `overlay_paths.rs` — `OverlayPathResolver`: resolves host paths (canonicalize, expand `~`, dedup keys). The grand architecture explicitly states this filesystem-resolution concern lives in Layer 0; the *mounting* of overlays into containers is Layer 1.
- `auth_paths.rs` — `AuthPathResolver`: resolves host-side credential file locations for each agent. Same rationale: filepath resolution is Layer 0; the *passthrough into containers* is Layer 1. Per-agent paths and exclusion lists are pinned below.
- `image_tags.rs` — pure helper functions `project_image_tag(git_root) -> "amux-{folder}:latest"` and `agent_image_tag(git_root, agent) -> "amux-{folder}-{agent}:latest"`. Used by both `AgentEngine` and `ContainerRuntime`; Layer 0 to avoid duplication.

Every type above is a struct with methods. No free `pub fn`s except small stateless helpers (the image-tag and parse helpers in `image_tags.rs` are the explicit exception).

##### Compatibility Inventory — `HeadlessPaths` exact layout

Resolve the headless root via `Env::headless_root()` (defaults to `$HOME/.amux/headless/`). The new struct MUST expose accessors for these paths verbatim (the legacy code calls these out by name; preserving them is a contract with existing user installs):

```
<root>                                    # HeadlessPaths::root()
<root>/amux.db                            # HeadlessPaths::db_path()
<root>/amux.pid                           # HeadlessPaths::pid_file()
<root>/amux.log                           # HeadlessPaths::log_file()
<root>/api_key.hash                       # HeadlessPaths::api_key_hash_file()  -- mode 0o600 on Unix
<root>/sessions/                          # HeadlessPaths::sessions_dir()
<root>/sessions/<sid>/                    # HeadlessPaths::session_dir(sid)
<root>/sessions/<sid>/commands/<cid>/     # HeadlessPaths::command_dir(sid, cid)
<root>/sessions/<sid>/commands/<cid>/output.log         # HeadlessPaths::command_log_path(sid, cid)
<root>/sessions/<sid>/commands/<cid>/metadata.json      # HeadlessPaths::command_metadata_path(sid, cid)
<root>/sessions/<sid>/commands/<cid>/workflow.state.json # HeadlessPaths::command_workflow_state_path(sid, cid)
<root>/sessions/<sid>/worktree/           # HeadlessPaths::session_worktree_dir(sid)
<root>/sessions/<sid>/agent-settings/     # HeadlessPaths::session_agent_settings_dir(sid)
```

There is **no TLS dir today** in legacy. If TLS is added, document it separately (the 0067 `AuthEngine` section introduces self-signed TLS material; coordinate path naming there).

**macOS launchd plist** path: `$HOME/Library/LaunchAgents/io.amux.headless.plist` — Label `io.amux.headless`. Resolution belongs in Layer 0 (`HeadlessPaths::launchd_plist_path()`); writing/loading the plist is a Layer 1 daemonization concern.

##### Compatibility Inventory — sqlite schema (verbatim)

`SqliteSessionStore::open` MUST produce this exact schema on a fresh DB and MUST NOT alter it on an existing DB. The schema is created via `CREATE TABLE IF NOT EXISTS` (no version table, no migration framework). `PRAGMA journal_mode=WAL` MUST be set on every open:

```sql
PRAGMA journal_mode = WAL;

CREATE TABLE IF NOT EXISTS sessions (
    id         TEXT PRIMARY KEY,             -- uuid v4 string
    workdir    TEXT NOT NULL,                -- canonicalized absolute path
    created_at TEXT NOT NULL,                -- RFC3339 string
    status     TEXT NOT NULL DEFAULT 'active', -- 'active' | 'closed'
    closed_at  TEXT                          -- RFC3339 or NULL
);

CREATE TABLE IF NOT EXISTS commands (
    id          TEXT PRIMARY KEY,            -- uuid v4 string
    session_id  TEXT NOT NULL REFERENCES sessions(id),  -- NO ON DELETE CASCADE (manual cascade in delete_closed_sessions_older_than)
    subcommand  TEXT NOT NULL,
    args        TEXT NOT NULL,               -- JSON-encoded array of strings
    status      TEXT NOT NULL DEFAULT 'pending', -- 'pending' | 'running' | 'done' | 'error'
    exit_code   INTEGER,                     -- nullable
    started_at  TEXT,                        -- RFC3339 nullable
    finished_at TEXT,                        -- RFC3339 nullable
    log_path    TEXT NOT NULL                -- absolute filesystem path to output.log
);
```

**No indexes today** (lookups by `session_id` and `status` are full table scans on small datasets). Adding indexes is safe (sqlite creates them on next open) but removing the `WAL` mode or adding `ON DELETE CASCADE` is a behavior change that breaks user-data assumptions.

**Required `SqliteSessionStore` methods** (legacy CRUD that downstream code depends on):

- `insert_session(session)`, `get_session(id)`, `list_sessions()`, `list_sessions_by_status(Option<status>)`, `close_session(id)`, `count_active_sessions()`
- `delete_closed_sessions_older_than(hours)` — manual cascade: deletes `commands` rows for matching sessions first, then `sessions` rows. Returns `Vec<(SessionId, command_count)>`. Boundary is exclusive (`closed_at < cutoff`).
- `insert_command(command)`, `get_command(id)`, `update_command_started(id)`, `update_command_finished(id, exit_code, status)`
- `count_running_commands()`, `has_running_command_for_session(session_id)` — counts `status IN ('pending','running')`. **Crash-recovery semantics**: catches commands left orphaned by a prior server crash. MUST be preserved.

**Status enum strings are stable.** `sessions.status` ∈ `{'active', 'closed'}`. `commands.status` ∈ `{'pending', 'running', 'done', 'error'}`. Renaming requires migration.

##### Compatibility Inventory — `WorkflowStateStore` filename and JSON shape

Persistence path: `<git_root>/.amux/workflows/<repohash8>-[<NNNN>-]<workflow_name>.json` (per-repo, NOT under `$HOME`).

- `repohash8` = `sha256_hex(git_root.to_string_lossy())[..8]`. The hash function MUST stay byte-stable to preserve in-flight workflow resumption.
- With work item: `<repohash8>-<NNNN>-<name>.json` (NNNN zero-padded 4-digit).
- Without: `<repohash8>-<name>.json`.

The JSON shape (every field required for resume):

```json
{
  "title": "optional",
  "steps": [
    {
      "name": "step-name",
      "depends_on": ["other-step"],
      "prompt_template": "raw template with {{vars}}",
      "status": "Pending" | "Running" | "Done" | {"Error": "msg"} | {"Failed": {"exit_code": 1, "error_message": "..."}},
      "container_id": "abc123" | null,
      "agent": "codex" | null,
      "model": "claude-..." | null
    }
  ],
  "workflow_hash": "<sha256-hex of source file>",
  "work_item": 27,
  "workflow_name": "filename-stem",
  "schema_version": 1
}
```

**`agent`, `model`, `work_item`, `schema_version` MUST use `#[serde(default)]`** so older state files (no `schema_version` field, no per-step `agent`/`model`) load as v0 and migrate-on-load.

**`StepStatus` MUST preserve the legacy human-readable error message.** The legacy `Error(String)` carries a free-form failure description. The new representation MUST keep that string addressable (e.g. `Failed { exit_code: i32, error_message: Option<String> }`) — a bare `exit_code: i32` is insufficient, build/launch/container-side errors do not always come from a process exit.

`WorkflowState` methods that downstream layers depend on:

- `next_ready()`, `completed_set()`, `all_done()`, `set_status(name, status)`, `set_container_id(name, id)`, `get_step(name)`, `interrupted_running_steps()`, `is_terminal()`, `parallel_group_for(step_name)` (returns the set of steps sharing the same `depends_on` set; used by TUI parallel-group rendering).

`WorkflowStateStore::validate_resume_compatibility(saved, new_steps)`: checks step count match + per-step `name` and `depends_on` equality. **`prompt_template` is permitted to differ silently** — preserve the legacy test `resume_compat_same_steps_ok`.

##### Compatibility Inventory — Workflow definition format (`workflow_definition.rs`)

Three formats by extension via `detect_format(path)`: `.md` → Markdown, `.toml` → TOML, `.yml`/`.yaml` → YAML. `.json` is **explicitly rejected**.

- TOML/YAML: `#[serde(deny_unknown_fields)]` everywhere — typos error.
- TOML uses key `[[step]]` (array-of-table), mapped via `#[serde(rename = "step")]` from the `steps: Vec<RawStep>` Rust field.
- UTF-8 BOM stripped before parsing TOML/YAML.
- Markdown grammar: optional `# Title`, then `## Step: <name>` blocks containing `Depends-on:`, `Agent:`, `Model:` directive lines (only before `Prompt:`), and a `Prompt:` body that consumes everything until the next `## ` heading. **`Agent:` / `Model:` lines AFTER `Prompt:` are captured as part of the prompt body, NOT as directives.** Empty file → error "no steps". Comma-separated `Depends-on` parsed and trimmed.
- Validation: `validate_references` (every `depends_on` names a real step), `detect_cycle` (DFS with self-loop detection), `validate_agent_name`.

**Prompt template substitution** — `substitute_prompt(template, work_item, work_item_content) -> String` MUST support these tokens (legacy `oldsrc/workflow/mod.rs::substitute_prompt`):

- `{{work_item_number}}` → zero-padded 4-digit (`0042`)
- `{{work_item_content}}` → full work-item file body
- `{{work_item_section:[name]}}` → case-insensitive section match, optional trailing colon, supports H1 + H2 sections; iterative loop on section tokens
- `{{work_item}}` → numeric (no padding, e.g. `42`)

Emits a stderr warning (TODO: route through `UserMessageSink::warning` from the engine layer) if work-item vars used without `--work-item`.

##### Compatibility Inventory — `AuthPathResolver` per-agent paths

Per-agent host paths and exclusion lists (preserve verbatim from `oldsrc/passthrough.rs`):

| Agent | Host path | Container target | Exclusions |
|---|---|---|---|
| `claude` | `~/.claude.json` (file) + `~/.claude/` (dir) | `<container_home>/.claude.json` + `<container_home>/.claude/` | denylist: `projects, sessions, session-env, debug, file-history, history.jsonl, telemetry, downloads, ide, shell-snapshots, paste-cache` |
| `claude` | macOS keychain `Claude Code-credentials` → env var `CLAUDE_CODE_OAUTH_TOKEN` | (env var) | JSON path `claudeAiOauth.accessToken` extracted |
| `codex` | `~/.codex/` | `/root/.codex` (rw) | denylist: `logs` |
| `gemini` | `~/.gemini/` (or empty dir) | `/root/.gemini` (rw) | denylist: `logs` |
| `opencode` | `~/.config/opencode/` | `/root/.config/opencode` (rw) | denylist: `logs` |
| `crush` | `~/.config/crush/` (or empty dir) | `/root/.config/crush` (rw) — remapped if container is non-root | denylist: `logs` |
| `cline` | `~/.cline/data/` (or empty dir) | `/root/.cline/data` (rw) | denylist: `tasks, workspace` |
| `copilot` | (none on host) | (none) — env var `COPILOT_OFFLINE=true` injected | n/a |
| `maki` | (none) | (none) | n/a |

The `<container_home>` placeholder is `/root` by default and remapped to `/home/<user>` when the agent's Dockerfile declares a non-root `USER`. The remap logic (legacy `apply_dockerfile_user`) is Layer 1's responsibility (it parses Dockerfiles); Layer 0 only resolves the host paths.

**`~/.claude.json` sanitization** before mounting: strip `oauthAccount` field, add `/workspace` project trust entry. Legacy lives in `oldsrc/passthrough.rs::ClaudePassthrough`. Layer 0 owns the JSON manipulation; Layer 1 schedules the temp-dir copy + mount.

#### 3e. Errors (`src/data/error.rs`)

Define a typed error enum `DataError` covering every failure mode in Layer 0 (config parse error, fs error, sqlite error, git-root-not-found, session-not-found, etc.). Use `thiserror`. Higher layers will wrap this in their own error enums; Layer 0 does not depend on higher layers' errors.

### 4. What must NOT happen in this work item

To keep the work bounded and to enforce the layering tenets:

- **Do not** implement any container, workflow, git, overlay, or auth *behavior* in `src/`. Trait shapes and types that Layer 1 will need are fine, but no behavior. Behavior lands in 0067.
- **Do not** modify `oldsrc/` after the rename + `oldsrc/README.md` write. If a bug is discovered in `oldsrc/` during this work, file it as a bug; do not fix it in `oldsrc/` (fix it in the new tree once the relevant layer exists).
- **Do not** delete any oldsrc files. Removal happens in 0070.
- **Do not** wire `oldsrc/` to consume anything from `src/data/`. The two trees are completely independent until 0069 swaps the binary entrypoint.
- **Do not** add any `pub fn` to `src/data/` that could just as well be a method on a struct.

## Edge Case Considerations:

- **Git root cannot be resolved**: `Session::open` must return a structured `DataError::GitRootNotFound { working_dir }`. The CLI frontend in 0069 will translate that into the user-facing error. Layer 0 itself prints nothing.
- **Two Cargo bins with the same crate**: A workspace member with `[lib]` and two `[[bin]]` entries (`amux` from `oldsrc/main.rs`, `amux-next` from `src/main.rs`) requires both to compile against the same library. Since the library `path` points at `oldsrc/lib.rs`, `src/main.rs` cannot trivially import `amux::data::*`. Two viable approaches: (a) split the crate into a Cargo workspace with `oldsrc/` as one member crate and a new `amux-next` workspace member with its own `Cargo.toml`, (b) make `amux-next` use `path = "src/lib.rs"` via a separate `[lib]` block (not directly possible — would need a workspace). **ASK THE DEVELOPER** which approach they prefer; the grand architecture document does not prescribe the Cargo layout.
- **`oldsrc/lib.rs` vs `oldsrc/main.rs` divergence**: confirm both compile after the rename — `cargo build --bin amux` and `cargo build --bin amux-next` must both succeed at the end of this work item.
- **Sqlite schema migration**: the existing headless db schema in `oldsrc/commands/headless/db.rs` will be re-implemented by `SqliteSessionStore`. Since Layer 0 is not yet wired into anything, the migration must be schema-compatible with the existing on-disk databases users already have; otherwise users will lose state at 0069's swap. write a schema-compat test that opens an existing db file and confirms `SqliteSessionStore` can read it.
- **Concurrent `SessionManager` mutation**: covered by tests; due to `tokio::sync::RwLock`, every `SessionManager` method is `async`;
- **`SessionId` collision**: the chance is astronomically low for UUIDv4/ULID, but `SessionManager::insert` must still surface a `DataError::SessionIdCollision` rather than panic.
- **Config file partially missing**: `RepoConfig::load` must distinguish "no config file" (return defaults) from "config file present but malformed" (return structured error). Same for `GlobalConfig`.
- **Env var precedence**: the merge order is flag > env > repo config > global config > built-in default. This precedence MUST be encoded in `EffectiveConfig` and have unit tests covering every combination.
- **Path canonicalization on non-existent paths**: `OverlayPathResolver` must handle the same edge case `oldsrc/overlays/mod.rs::make_host_path_canonical` handles after work item 0065 — walk up to the nearest existing ancestor. Reuse the algorithm but encapsulated as a method on the resolver.
- **Vec list scope replacement (CRITICAL)**: when `RepoConfig.envPassthrough` is `Some(vec![])`, `EffectiveConfig::env_passthrough()` MUST return `[]`, NOT the global list. Same for `yoloDisallowedTools`, `headless.workDirs`, `remote.savedDirs`. Tests must include the empty-array-clears-global case (preserve legacy test `effective_env_passthrough_repo_empty_array_wins_over_global`).
- **Path-escape validation for `work_items.dir/template`**: must be validated against repo-root-escape using path normalization (legacy `validate_path_within_git_root`). Layer 0 owns this validator; Layer 2 calls it before save. Reject paths containing `..` segments that escape `git_root` after canonicalization.
- **`api_key.hash` file mode**: MUST be created with mode `0o600` on Unix via `OpenOptions::mode(0o600).create(true).truncate(true)` (atomic create-with-mode, NOT `chmod` after create — the latter has a TOCTOU window). Legacy uses this pattern; preserve it.
- **Sqlite `WAL` journal mode required**: legacy DBs are WAL on disk. Switching to default (delete) journal mode would force-rewrite every user's DB. `SqliteSessionStore::open` MUST `PRAGMA journal_mode=WAL;` on every open.
- **Sqlite manual cascade**: `commands.session_id REFERENCES sessions(id)` has **no `ON DELETE CASCADE`**. `delete_closed_sessions_older_than(hours)` deletes commands first then sessions, returning per-session `(SessionId, deleted_command_count)`. Cleanup at server-startup is one-shot (24-hour cutoff hard-coded in legacy); preserve the function shape so 0067/0068 can decide on the trigger schedule.
- **Status string stability**: changing `'active'` → `'open'` or `'pending'` → `'queued'` etc. invalidates user data. Pin the four command statuses and two session statuses verbatim.
- **Workflow state forward-compat**: load JSON without `schema_version` as v0 via `#[serde(default)]` and migrate on save. `agent`, `model`, `work_item` per-step fields also `#[serde(default)]` for legacy load.
- **`StepStatus::Failed` error string must round-trip**: a v0 file written as `{"Error": "msg"}` MUST load and re-save losslessly under the new representation that preserves `error_message`.
- **macOS-only keychain**: `agent_keychain_credentials(agent)` is macOS-only via `security find-generic-password`. On Linux/Windows it returns no credentials; agents auth via env vars in `envPassthrough`. Layer 0 just exposes the path/service name table; Layer 1 invokes the platform tool.
- **Per-agent denylists are pinned constants**: `CLAUDE_DIR_DENYLIST`, `OPENCODE_DIR_DENYLIST`, `CODEX_DIR_DENYLIST`, `GEMINI_DIR_DENYLIST`, `CRUSH_CONFIG_DENYLIST`, `CLINE_DATA_DENYLIST` move from `oldsrc/passthrough.rs` to Layer 0 as `pub const &[&str]` arrays. Layer 1's `OverlayEngine` reads them when copying credential dirs into temp dirs for mounting.

## Test Considerations:

### Test philosophy (read first)

Tests for Layer 0 are **designed and written from scratch** alongside the new types. **Do not port tests from `oldsrc/tests/*` or from `oldsrc/**/#[cfg(test)] mod tests` blocks.** Those tests were written against the pre-refactor architecture and carry forward assumptions that the layered design explicitly invalidates (mode-specific behavior, untyped flag handling, ad-hoc filesystem helpers, etc.). Copying them over reintroduces the cruft this refactor exists to remove.

The narrow exception is a test that satisfies **all** of the following:

1. Asserts a user-visible or on-disk behavior the new architecture must preserve byte-for-byte (e.g. `config.json` schema compatibility, sqlite db schema readability for users upgrading from a prior install).
2. Compiles unchanged (or with only mechanical import-path changes) against the new Layer 0 types.
3. Exercises only Layer 0 surfaces. Anything that pokes a Layer 1 concern, a frontend, or the CLI binary is out of scope.

If any old test is brought forward under this exception, the PR description MUST list it explicitly with a one-sentence justification. The default answer is "rewrite from scratch."

This work item produces **only Layer 0 unit tests** (and a small number of Layer-0-internal integration tests, defined below). All cross-layer integration tests, end-to-end tests, real-Docker tests, real-network tests, parity tests, and full-binary smoke tests are consolidated in work item 0070 against a freshly rebuilt `tests/` directory. **Do not add anything to the top-level `tests/` directory in this work item.**

### Unit tests (`src/data/**/*` — colocated `#[cfg(test)] mod tests` blocks)

- **Session**:
  - `Session::open` with a static `GitRootResolver` returns a session with the expected git root, working dir, and merged config.
  - `Session::open` propagates `DataError::GitRootNotFound` from the resolver.
  - `Session::state_mut` permits mutation; `Session::state` is read-only.
  - Constructing a `Session` with malformed `RepoConfig` on disk returns `DataError::ConfigParse`, never panics.
- **SessionManager**:
  - `create`, `get`, `get_mut`, `list`, `remove` happy paths.
  - `remove(non_existent_id)` returns `DataError::SessionNotFound`.
  - Concurrent `create` from N tasks produces N distinct sessions (`tokio::test` with `spawn`, or `parking_lot` + `std::thread::scope`).
  - `with_persistence(store)` writes to the supplied `SessionStore` on every mutation; `in_memory()` does not touch disk.
- **RepoConfig / GlobalConfig**:
  - Load → save → load round-trip is byte-stable for representative configs.
  - `migrate_legacy` reads a legacy on-disk path, writes the new path, and removes the legacy file (or whatever the chosen migration policy is — confirm with developer).
  - Malformed JSON returns `DataError::ConfigParse { … }` with line/column when serde provides them.
- **EffectiveConfig**:
  - Precedence (flag > env > repo > global > built-in default) — one targeted unit test per adjacent pair, plus one full-stack test that sets a value at every level and asserts the highest-priority value wins.
  - Every accessor that replaces an `oldsrc::config::effective_*` free function has a focused unit test against synthetic inputs — **not** against the legacy function. The new behavior is the source of truth.
- **Filesystem stores**:
  - `SqliteSessionStore::open` runs migrations on a fresh DB and is idempotent on a populated DB.
  - `SqliteSessionStore` schema readability against a checked-in fixture DB written by the prior amux release (covers the user-upgrade path; see Edge Case Considerations).
  - `SqliteSessionStore::open` introspection: `PRAGMA table_info('sessions')` and `PRAGMA table_info('commands')` produce the EXACT column list (name, type, nullability, default) documented in the Compatibility Inventory. Catches accidental column reorder/rename.
  - `SqliteSessionStore::open` sets `PRAGMA journal_mode=WAL`. Verify via `PRAGMA journal_mode;`.
  - `delete_closed_sessions_older_than` cascades commands manually and returns `Vec<(SessionId, command_count)>`.
  - `has_running_command_for_session` returns true when status is `pending` OR `running` (crash-recovery semantics).
  - `WorkflowStateStore::save` then `load` round-trips a representative `WorkflowInvocation`.
  - `WorkflowStateStore::load` of a legacy fixture (`tests/fixtures/workflow_state/v0_no_schema_version.json`) succeeds and returns `schema_version == 0`. `save` afterwards rewrites with `schema_version == 1`.
  - `WorkflowStateStore::load` round-trips `StepStatus::Error("legacy message")` losslessly into the new representation that preserves `error_message`.
  - `WorkflowStateStore::save` filename for `(git_root, work_item=42, name="impl")` matches `<sha256_hex(git_root_lossy)[..8]>-0042-impl.json`. The hash function MUST be byte-stable.
  - `validate_resume_compatibility(saved, new_steps)` accepts identical step names + `depends_on` even when `prompt_template` differs (preserve legacy `resume_compat_same_steps_ok`).
  - `OverlayPathResolver::canonicalize("/foo/baz/../bar")` returns `/foo/bar` even when the leaf does not exist.
  - `AuthPathResolver` resolves the right host-side credential path per agent on Linux, macOS, and (best-effort, behind `cfg(windows)`) Windows.
  - `RepoConfig` round-trip preserves the exact JSON keys (`yoloDisallowedTools` camel, `agentStuckTimeout` camel-no-suffix, `workItems` camel, `defaultAPIKey` uppercase API). A "normalize all to camelCase" round-trip MUST FAIL the test.
  - `GlobalConfig` legacy-key rejection: top-level `headlessWorkDirs` and `remoteDefaultAddr` MUST be silently ignored on load (preserve WI-0058 breaking change).
  - `ConfigFieldDef` master list (the new `CONFIG_FIELDS` const) covers every entry from `oldsrc/commands/config.rs::ALL_FIELDS` (data-table assertion: `(key, scope, settable, default)` rows).
  - `auto_agent_auth_accepted` has `settable=false` in `CONFIG_FIELDS`; the `record_auth_decision` typed setter is the only path to mutate it.
  - `validate_path_within_git_root("/git/root", "../../escape")` returns `Err`; same for `"foo/../../escape"`. `("/git/root", "subdir")` returns `Ok`.
  - `EffectiveConfig::overlays(env, flags)`: 4-source merge with priority order, container-path-by-priority, permission-by-most-restrictive (`ro` beats `rw`), missing-host-path warn-and-drop, malformed-value fatal.
  - `Env::overlays()` parses every legal grammar form (`dir(/h:/c)`, `dir(/h:/c:ro)`, `~/h`, `dir(/spaces in path:/c)`, multi-comma) and rejects unknown type tags.

### Layer-0-internal integration tests (colocated, not in top-level `tests/`)

A small number of Layer-0-internal multi-component tests are acceptable as `#[cfg(test)] mod` blocks, since they exercise only Layer 0:

- **Config + Session round-trip** (`src/data/session.rs`): construct a temp dir with a sample `.amux.json`, override `HOME` to point at a temp `~/.amux/config.json`, open a `Session` via a stub `GitRootResolver`, assert `EffectiveConfig` reflects both files merged correctly.
- **SessionManager + SqliteSessionStore round-trip** (`src/data/session_manager.rs`): create N sessions through `SessionManager::with_persistence`, drop the manager, reopen the store, list sessions, assert all N are present and equal (modulo `last_active_at`).

### What does NOT belong in this work item

- Tests touching real Docker daemons, real container runtimes, real PTYs, real HTTP servers, or the real `amux` CLI binary.
- Tests asserting cross-layer behavior (Layer 0 + Layer 1, etc.). Layer 1 doesn't exist yet.
- Tests in the top-level `tests/` directory. Leave it untouched in this work item; 0070 rebuilds it.
- Any port of `oldsrc/tests/*.rs` — those tests stay in place, run against `oldsrc/` only, and are deleted in 0070 along with the rest of `oldsrc/`.

### Build & CI

- `cargo build --bin amux` succeeds (compiles from `oldsrc/`).
- `cargo build --bin amux-next` succeeds (compiles from `src/`, prints stub message at runtime).
- `cargo test` runs both the new Layer 0 tests and the surviving `oldsrc/` test tree; both pass.
- `make all`, `make install`, `make test` continue to work (the user-visible CLI experience is identical to pre-refactor).

### Manual smoke test

- Run the existing `amux` binary against a real repo. Confirm `amux ready`, `amux init`, `amux status`, `amux chat`, etc. behave exactly as before. (They are still legacy code — this work item changes nothing user-visible.)

## Codebase Integration:

- Follow established conventions, best practices, testing, and architecture patterns from the project's `aspec/`. The grand architecture document (`aspec/architecture/2026-grand-architecture.md`) is the single source of truth for design decisions in this and the four follow-up work items.
- The existing tenets in `aspec/architecture/design.md` and `aspec/architecture/security.md` continue to apply unchanged.
- All Rust code MUST be `#![forbid(unsafe_code)]` at the crate root; if any layer needs `unsafe`, ASK THE DEVELOPER first.
- Use existing project dependencies wherever possible (`serde`, `tokio`, `anyhow`/`thiserror`, `uuid`, `rusqlite`/`sqlx`, etc.). Adding a new dependency requires justification in the PR description.
- Do not edit anything under `oldsrc/`. The only allowed write into `oldsrc/` during this work item is the initial `oldsrc/README.md` freeze notice.
- Do not delete `oldsrc/`. That happens in work item 0070.
- The TUI, CLI, and headless surfaces visible to users MUST be byte-for-byte identical to pre-refactor at the end of this work item, because the user is still running `oldsrc` code.
- The PR description MUST link to `aspec/architecture/2026-grand-architecture.md` and to this work item, and MUST list any developer-clarification questions that came up and how they were resolved.
- After this work item lands, the next agent picks up `0067-grand-architecture-layer-1-engines.md`.
