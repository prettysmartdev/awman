# Work Item: Task

Title: grand architecture refactor — part 2/5 — Layer 1 engines (Container, Workflow, Ready, Init, Git, Overlay, Auth, Claws, Agent)
Issue: n/a — second of five work items implementing `aspec/architecture/2026-grand-architecture.md`

## Required reading before starting

This work item is the second of five executing the grand architecture refactor described in `aspec/architecture/2026-grand-architecture.md`. The implementing agent **MUST** read that document, the previous work item `0066-grand-architecture-foundation-and-layer-0-data.md`, and the current state of `src/data/` before writing any code.

The four tenets that govern this work item:

1. Layer 1 (engine) consumes Layer 0 (data) only. It MUST NOT call into Layer 2 (command), Layer 3 (frontend), or Layer 4 (binary). When the engines need user input or output, they accept a frontend trait *defined by Layer 1* — higher layers implement it.
2. Frontends contain no business logic. This affects Layer 1 because every engine's API surface must be expressed in a way that a frontend can satisfy by implementing a small trait, never by routing back through engine code.
3. Typed objects over `pub fn`. Builder/factory patterns over `run_X_with_Y(...)` mega-functions. The grand architecture document gives the canonical worked example — `ContainerRuntime::new_with_options(vec<options>) -> ContainerInstance` then `ContainerInstance::run_with_frontend(some_frontend_trait)` — and explicitly forbids the legacy `run_container_with_*` style.
4. When uncertain, ASK THE DEVELOPER. Do not write ambiguous "you could try this or this" code.

The companion work items are:

- `0066-grand-architecture-foundation-and-layer-0-data.md` (already merged)
- `0068-grand-architecture-layer-2-command-and-dispatch.md`
- `0069-grand-architecture-layer-3-frontends-and-binary.md`
- `0070-grand-architecture-finalize-and-remove-oldsrc.md`

## Summary:

- Before touching any engine code, add three missing Layer 0 modules to `src/data/`: `workflow_dag.rs` (DAG validation, cycle detection, ready-step computation), `workflow_state.rs` (`WorkflowState` + `StepState` serializable types), and `workflow_state_store.rs` (`WorkflowStateStore` I/O). These belong at Layer 0 because they are stateless functions over serializable types or thin filesystem I/O wrappers — not engine logic. If work item 0066 already created them, verify and move on; do not recreate.
- Build out `src/engine/` with seven engine modules: `container/`, `workflow/`, `ready/`, `init/`, `git/`, `overlay/`, `auth/`. Each is a typed object (or small set of typed objects) that owns its concern entirely.
- The `ContainerRuntime` is rewritten from scratch as a builder/factory: a small number of typed `ContainerOption` values feed `ContainerRuntime::build(...) -> ContainerInstance`, and `ContainerInstance::run_with_frontend(impl ContainerFrontend) -> ContainerExecution` is the only way to execute a container. The legacy `run_container_with_*` and `run_with_sink` style is forbidden.
- A new `ContainerExecution` type is introduced. It represents a "fully prepared, ready-to-run container handle" that Layer 2 can hand to `WorkflowEngine` without leaking the underlying frontend or runtime details.
- The `WorkflowEngine` is rewritten to hold all state, advancement logic, yolo/auto countdowns, agent/model resolution, exit-code handling, step persistence, and container lifecycle management per step. It understands how to re-use a running container (push a new prompt into it) versus launch a fresh container for the next step, and resolves the correct agent/model for each step. When a workflow uses multiple agents or models across steps, the engine enforces which advance actions are legal given the current configuration. It accepts a frontend trait at construction (e.g. `WorkflowFrontend` exposing `user_choose_next_action`, `confirm_resume`, `report_step_status`, etc.) and is forbidden from rendering anything itself or making any direct user-input syscalls.
- A new `ReadyEngine` (`src/engine/ready/`) is introduced to own all multi-phase logic for the `amux ready` command: preflight checks, legacy-layout detection and migration, Dockerfile.dev creation, Docker image build(s), local agent check, audit container run, and post-audit rebuild. A `ReadyPhase` state machine tracks execution; a `ReadyFrontend` trait exposes all Q&A and progress reporting to Layer 3.
- A new `InitEngine` (`src/engine/init/`) is introduced to own all multi-phase logic for the `amux init` command: git root resolution, aspec folder creation, Dockerfile.dev setup, config write, audit container run, image build, and work-items configuration. An `InitPhase` state machine tracks execution; an `InitFrontend` trait exposes all Q&A and progress reporting to Layer 3.
- The `GitEngine` consolidates every git operation amux performs (root resolution, dirty detection, worktree CRUD, merge, commit, future push/pull). The data layer's `GitRootResolver` trait is now satisfied by `GitEngine`.
- The `OverlayEngine` consolidates overlay construction and management — agent settings/config passthrough, user-defined directory overlays, env-var overlays, secret overlays, skill overlays. It consumes Layer 0's `OverlayPathResolver`.
- The `AuthEngine` consolidates host-side agent credential resolution and headless-server authentication. It consumes Layer 0's `AuthPathResolver` and `SqliteSessionStore`.
- A new `ClawsEngine` (`src/engine/claws/`) is introduced to own all multi-phase logic for `amux claws init` and related subcommands: repo clone, SSH/sudo permission check, nanoclaw image build, audit container run, controller configuration, and controller launch. A `ClawsPhase` state machine tracks execution; a `ClawsFrontend` trait exposes all Q&A and progress reporting to Layer 3.
- A new `AgentEngine` (`src/engine/agent/`) is introduced to consolidate the cross-cutting agent concerns called from five or more commands (`implement`, `chat`, `exec`, `ready`, `claws`): Dockerfile availability checking and download, agent image building, per-agent container option construction (entrypoint, model flag, autonomous flags, allowed-tools). Centralising these in `AgentEngine` prevents silent divergence as new agents and models are added.
- All engines have unit tests. `ContainerRuntime` and `WorkflowEngine` have additional integration tests using lightweight fakes that satisfy their frontend traits.

## User Stories

### User Story 1:
As a: future implementing agent picking up Layer 2

I want to:
find Layer 1 engines that expose builder/factory APIs and accept frontend traits

So I can:
wire commands by composing typed engine objects without ever needing to touch container, git, or workflow internals.

### User Story 2:
As a: maintainer reading `src/engine/container/`

I want to:
see a small number of `ContainerOption` variants and a single `ContainerRuntime::build` rather than a dozen `run_container_with_*` functions with overlapping parameter lists

So I can:
trust that adding a new container option is a small, local change rather than a sprawling refactor across every call site.

### User Story 3:
As a: maintainer reading `src/engine/workflow/`

I want to:
see all workflow execution logic, exit-code handling, yolo countdowns, and agent/model resolution in one place

So I can:
fix workflow bugs without sifting through TUI, CLI, and headless code paths that today re-implement parts of the same logic.

## Implementation Details:

### 0. Required reading and ground rules

- Read `aspec/architecture/2026-grand-architecture.md` end-to-end.
- Read `0066-grand-architecture-foundation-and-layer-0-data.md` and the resulting `src/data/` to understand the types Layer 1 consumes.
- For reference only (not to be edited or copied verbatim): `oldsrc/runtime/`, `oldsrc/workflow/`, `oldsrc/git.rs`, `oldsrc/overlays/`, `oldsrc/passthrough.rs`, and the auth bits in `oldsrc/commands/headless/auth.rs`. Use these to understand existing behavior; **do not** port the existing API surface verbatim, since the grand architecture explicitly mandates a redesign.
- When uncertain, ASK THE DEVELOPER.

### 0.5. Layer 0 additions required by this work item

The following three modules MUST exist in `src/data/` before `WorkflowEngine` is built. Check `src/data/` first — work item 0066 may have already created them. If they are present and correct, treat this section as a verification checklist and move on. If any module is absent or incomplete, add it here before touching `src/engine/workflow/`.

#### `src/data/workflow_dag.rs`

DAG data structures and pure algorithmic functions over a `Workflow`'s step graph. These are Layer 0 concerns because they are stateless functions over serializable types with no engine state dependencies.

```rust
/// Validated adjacency representation of a workflow's step graph.
pub struct WorkflowDag {
    // internal adjacency; not public — constructed via WorkflowDag::build
}

impl WorkflowDag {
    /// Build and validate a DAG from a slice of steps.
    /// Returns DataError if references are missing or a cycle is detected.
    pub fn build(steps: &[WorkflowStep]) -> Result<Self, DataError>;

    /// Steps that have no unmet dependencies given the completed set.
    pub fn ready_steps(&self, completed: &HashSet<String>) -> Vec<String>;

    /// Total ordering of steps (depth-first post-order), used for display.
    pub fn topological_order(&self) -> Vec<String>;
}

/// Referential integrity check — every `depends_on` entry names a real step.
pub fn validate_references(steps: &[WorkflowStep]) -> Result<(), DataError>;

/// Cycle detection — returns DataError if any cycle exists.
pub fn detect_cycle(steps: &[WorkflowStep]) -> Result<(), DataError>;
```

The logic mirrors `oldsrc/workflow/dag.rs` (231 lines) but is owned by `src/data/` and MUST NOT import from `src/engine/`.

#### `src/data/workflow_state.rs`

Fully serializable snapshot of workflow execution state.

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowState {
    pub schema_version: u32,
    pub workflow_name: String,
    pub workflow_hash: String,          // hash of parsed Workflow for resume validation
    pub step_states: HashMap<String, StepState>,
    pub completed_steps: HashSet<String>,
    pub current_step_index: Option<usize>,
    pub started_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum StepState {
    Pending,
    Running,
    Succeeded,
    Failed { exit_code: i32 },
    Cancelled,
}

impl WorkflowState {
    pub fn new(workflow_name: String, steps: &[WorkflowStep], hash: String) -> Self;
    pub fn schema_version() -> u32;   // current version constant
    pub fn is_complete(&self) -> bool;
    pub fn next_ready(&self, dag: &WorkflowDag) -> Vec<String>;
}
```

Both types MUST implement `serde::Serialize` + `serde::Deserialize`, `Clone`, and `Debug`. `WorkflowState::schema_version()` returns the current integer version so `WorkflowEngine` can reject stale persisted state.

#### `src/data/workflow_state_store.rs`

Thin I/O wrapper for reading and writing `WorkflowState` JSON to disk.

```rust
pub struct WorkflowStateStore {
    base_dir: PathBuf,   // e.g. $HOME/.amux/workflow-state/
}

impl WorkflowStateStore {
    pub fn new(session: &Session) -> Self;
    pub fn load(&self, workflow_name: &str) -> Result<Option<WorkflowState>, DataError>;
    pub fn save(&self, state: &WorkflowState) -> Result<(), DataError>;
    pub fn delete(&self, workflow_name: &str) -> Result<(), DataError>;
}
```

All three modules MUST be re-exported from `src/data/mod.rs`.

---

### 1. `src/engine/container/` — `ContainerRuntime`, `ContainerInstance`, `ContainerExecution`

#### 1a. Types

```rust
// src/engine/container/options.rs
pub enum ContainerOption {
    Image(ImageRef),
    Entrypoint(Entrypoint),
    Overlay(OverlaySpec),
    EnvPassthrough(EnvVar),
    SeededPrompt(String),
    Interactive(bool),
    AllowDocker(bool),
    MountSsh(bool),
    Yolo(YoloMode),
    Auto(AutoMode),
    Plan(PlanMode),
    WorkingDir(PathBuf),
    Name(ContainerName),
    Cpu(CpuLimit),
    Memory(MemoryLimit),
    AgentSettingsPassthrough(AgentSettings),
    // ...exhaustive list — every flag the legacy code spreads across
    // run_container_with_* parameters becomes one variant here
}
```

The variant set MUST cover *every* knob the legacy `oldsrc/runtime/{docker,apple,mod}.rs` exposes, plus anything new the grand architecture calls out (e.g. `AgentSettingsPassthrough`).

```rust
// src/engine/container/runtime.rs
pub struct ContainerRuntime {
    // Holds a Box<dyn ContainerBackend> internally. The concrete type (Docker or Apple)
    // is selected by ContainerRuntime::detect and is never exposed to callers.
    // Outside src/engine/container/, the backend variant is invisible.
}

impl ContainerRuntime {
    /// Inspect `global_config` and the host environment to select the correct
    /// backend (Docker or Apple Containers). The chosen backend is stored
    /// internally and MUST NOT be exposed via any public method or field.
    /// The backend is fixed for the lifetime of this `ContainerRuntime` instance.
    pub fn detect(global_config: &GlobalConfig) -> Result<Self, EngineError>;

    /// Build a fully configured `ContainerInstance` from the given options.
    /// Which backend runs the container is an opaque implementation detail.
    pub fn build(&self, options: impl IntoIterator<Item = ContainerOption>)
        -> Result<Box<dyn ContainerInstance>, EngineError>;

    pub fn list_running(&self, session: &Session) -> Result<Vec<ContainerHandle>, EngineError>;
    pub fn stats(&self, handle: &ContainerHandle) -> Result<ContainerStats, EngineError>;
    pub fn stop(&self, handle: &ContainerHandle) -> Result<(), EngineError>;
}

// src/engine/container/backend.rs — internal trait, NOT pub outside the module
trait ContainerBackend: Send + Sync {
    fn build(&self, options: &ResolvedContainerOptions) -> Result<Box<dyn ContainerInstance>, EngineError>;
    fn list_running(&self, session: &Session) -> Result<Vec<ContainerHandle>, EngineError>;
    fn stats(&self, handle: &ContainerHandle) -> Result<ContainerStats, EngineError>;
    fn stop(&self, handle: &ContainerHandle) -> Result<(), EngineError>;
}
```

The `Docker` and `Apple` backend structs implement `ContainerBackend` in `src/engine/container/docker.rs` and `src/engine/container/apple.rs` respectively. Both files are `pub(super)` — they MUST NOT be reachable by name from outside `src/engine/container/`. All callers go through `ContainerRuntime::build`.

```rust
// src/engine/container/instance.rs
pub trait ContainerInstance: Send + Sync {
    fn id(&self) -> &ContainerId;
    fn name(&self) -> &ContainerName;
    fn image(&self) -> &ImageRef;
    fn run_with_frontend(self: Box<Self>, frontend: Box<dyn ContainerFrontend>)
        -> Result<ContainerExecution, EngineError>;
}
```

```rust
// src/engine/container/execution.rs
pub struct ContainerExecution {
    // Owns the running container handle, the wired-up frontend, and exit-code futures.
    // Cannot be cloned. Cannot be inspected for frontend details by Layer 2 callers.
}

impl ContainerExecution {
    pub async fn wait(self) -> Result<ContainerExitInfo, EngineError>;
    pub fn handle(&self) -> &ContainerHandle;
    pub fn cancel(&self) -> Result<(), EngineError>;
}
```

```rust
// src/engine/container/frontend.rs — defined by Layer 1, implemented by Layer 3
pub trait ContainerFrontend: UserMessageSink + Send + Sync {
    fn write_stdout(&mut self, bytes: &[u8]) -> Result<(), EngineError>;
    fn write_stderr(&mut self, bytes: &[u8]) -> Result<(), EngineError>;
    fn read_stdin(&mut self, buf: &mut [u8]) -> Result<usize, EngineError>;   // 0 = EOF
    fn report_status(&mut self, status: ContainerStatus);
    fn report_progress(&mut self, progress: ContainerProgress);  // image pulls, build steps
    fn resize_pty(&mut self, cols: u16, rows: u16);
    // etc — must cover everything a TUI pty, CLI stdin/stdout binding, and a headless
    // SSE/WebSocket binding need. Define this trait once; implementations live in 0069.
}
```

#### 1b. What is forbidden in this module

- No `pub fn run_container_with_*`. Every previous "run with X" use case becomes one or more `ContainerOption` variants plus a frontend trait method.
- No exposure of the concrete backend type (Docker or Apple) to any caller outside `src/engine/container/`. The `docker.rs` and `apple.rs` files MUST be `pub(super)`. Any `match` on backend variant lives inside the module only.
- No direct PTY allocation. PTYs are a Layer 3 (frontend) concern; Layer 1 hands raw stdin/stdout bytes to the frontend trait and lets the frontend decide whether they go through a PTY (TUI), straight to fds (CLI), or over a socket (headless).
- No printing to stdout/stderr. All output goes through `ContainerFrontend::write_stdout`/`write_stderr`.
- No `tracing::info!` or similar to the user-facing console. Engine logs go to a `tracing` subscriber that the binary configures; they do not bypass the frontend.

### 2. `src/engine/workflow/` — `WorkflowEngine`

The legacy `oldsrc/workflow/mod.rs` (944 lines) and `oldsrc/workflow/parser.rs` (841 lines) and `oldsrc/workflow/dag.rs` (231 lines) collectively own workflow execution today, but workflow logic also leaks into `oldsrc/commands/implement.rs` (2087 lines), `oldsrc/commands/exec.rs`, and `oldsrc/tui/state.rs`. All of that logic consolidates here.

```rust
// src/engine/workflow/mod.rs
pub struct WorkflowEngine {
    workflow: Workflow,                // parsed workflow definition (Layer 0 data type)
    dag: WorkflowDag,                  // Layer 0 — built from workflow.steps at construction
    state: WorkflowState,              // Layer 0 — serializable execution snapshot
    state_store: WorkflowStateStore,   // Layer 0 — persists state on each step transition
    effective_config: EffectiveConfig, // Layer 0 — for agent/model resolution fallbacks
    frontend: Box<dyn WorkflowFrontend>,
    container_factory: Box<dyn ContainerExecutionFactory>,  // see below
    git_engine: Arc<GitEngine>,
    overlay_engine: Arc<OverlayEngine>,
}

impl WorkflowEngine {
    pub fn new(
        session: &Session,
        workflow: Workflow,
        frontend: Box<dyn WorkflowFrontend>,
        container_factory: Box<dyn ContainerExecutionFactory>,
        git_engine: Arc<GitEngine>,
        overlay_engine: Arc<OverlayEngine>,
    ) -> Result<Self, EngineError>;

    pub async fn run_to_completion(&mut self) -> Result<WorkflowOutcome, EngineError>;
    pub async fn step_once(&mut self) -> Result<StepOutcome, EngineError>;
    pub async fn pause(&mut self) -> Result<(), EngineError>;
    pub async fn resume(&mut self) -> Result<(), EngineError>;
    pub fn state(&self) -> &WorkflowState;
}
```

The `ContainerExecutionFactory` trait is the mechanism the grand architecture document calls out: Layer 2 builds a factory that, when invoked by the engine, returns a `ContainerExecution` for a given step. The engine never sees raw `ContainerOption` lists or frontend implementations; it only consumes already-prepared executions.

```rust
pub trait ContainerExecutionFactory: Send + Sync {
    /// Produce a fresh container execution for the given step.
    fn execution_for_step(
        &self,
        step: &WorkflowStep,
        session: &Session,
        runtime: &WorkflowRuntimeContext,
    ) -> Result<ContainerExecution, EngineError>;

    /// Inject an additional prompt into an already-running container rather than
    /// launching a new one. Returns None if the runtime backend does not support
    /// prompt injection (e.g. non-interactive containers), in which case the engine
    /// falls back to launching a fresh container.
    fn inject_prompt(
        &self,
        execution: &ContainerExecution,
        prompt: &str,
    ) -> Result<Option<()>, EngineError>;
}
```

#### 2b. Container lifecycle per step

For each workflow step, `WorkflowEngine` decides whether to launch a new container or reuse an existing one. This decision is driven by `NextAction` (returned by `WorkflowFrontend::user_choose_next_action`) and the step's configuration:

```rust
pub enum NextAction {
    /// Launch a fresh container for the next ready step.
    LaunchNext,
    /// Push an additional prompt into the container that just finished a step,
    /// keeping it alive for the next step. Only valid when the next step targets
    /// the same agent and the running container supports prompt injection.
    ContinueInCurrentContainer { prompt: String },
    /// Re-run the step that just completed, discarding its output and re-launching
    /// a fresh container for that same step. The step's `StepState` reverts to
    /// `Pending` before the new container is launched.
    RestartCurrentStep,
    /// Revert to the step immediately before the current one: mark the current step
    /// `Cancelled`, mark the previous step `Pending` again, and re-launch it.
    /// Only valid when there is a previous step in topological order.
    CancelToPreviousStep,
    /// Pause execution after the current step completes. Engine persists state.
    Pause,
    /// Abort the workflow entirely. Engine persists state with remaining steps Cancelled.
    Abort,
}
```

The engine enforces validity: `ContinueInCurrentContainer` is rejected (with `EngineError::InvalidAdvanceAction`) if:
- The current and next step specify different `agent` or `model` fields.
- The current running container has already exited.
- The factory's `inject_prompt` returns `None` for this backend.

`CancelToPreviousStep` is rejected if there is no previous step (i.e. the current step is the first in topological order).

When multiple agents or models appear within a single workflow, the engine computes the set of valid `NextAction` variants for each step transition and provides it to `WorkflowFrontend::user_choose_next_action` via `AvailableActions`. The frontend MUST render only the actions in that set; the engine rejects any action outside it.

```rust
pub struct AvailableActions {
    pub can_continue_in_current_container: bool,
    pub can_launch_next: bool,
    pub can_restart_current_step: bool,
    pub can_cancel_to_previous_step: bool,
    pub can_pause: bool,
    pub can_abort: bool,
    /// Human-readable explanation of why can_continue_in_current_container is false,
    /// so the frontend can show a tooltip rather than silently hiding the option.
    pub continue_unavailable_reason: Option<String>,
    /// Human-readable explanation of why can_cancel_to_previous_step is false
    /// (e.g. "this is the first step").
    pub cancel_to_previous_unavailable_reason: Option<String>,
}
```

#### 2c. Per-step agent and model resolution

`WorkflowEngine` resolves the effective agent and model for each step before invoking the factory. Resolution order (each level overrides the previous):

1. Workflow-level defaults (`workflow.agent`, `workflow.model`).
2. Step-level overrides (`step.agent`, `step.model`).
3. Session-level effective config (`EffectiveConfig` from Layer 0).

The resolved pair is passed to the factory via `WorkflowRuntimeContext`:

```rust
pub struct WorkflowRuntimeContext {
    pub step_agent: AgentName,
    pub step_model: ModelName,
    pub git_root: PathBuf,
    pub session_id: SessionId,
}
```

The engine MUST log (via `tracing`) which agent and model it resolved for each step, so users debugging unexpected agent selection have a structured trace. It MUST NOT print this to the user console directly.

The `WorkflowFrontend` trait covers every user-input concern the engine needs:

```rust
pub trait WorkflowFrontend: UserMessageSink + Send + Sync {
    /// Present the workflow control dialog after a step completes.
    /// `available` constrains which actions the frontend may offer.
    fn user_choose_next_action(
        &mut self,
        state: &WorkflowState,
        available: &AvailableActions,
    ) -> Result<NextAction, EngineError>;

    fn confirm_resume(&mut self, mismatch: &ResumeMismatch) -> Result<bool, EngineError>;
    fn report_step_status(&mut self, step: &WorkflowStep, status: WorkflowStepStatus);
    fn report_step_output(&mut self, step: &WorkflowStep, output: StepOutput);
    fn yolo_countdown_tick(&mut self, remaining: Duration) -> Result<YoloTickOutcome, EngineError>;
    fn report_workflow_completed(&mut self, outcome: &WorkflowOutcome);
    // ...exhaustively cover every prompt or report the legacy code performs
}
```

Workflow parsing (markdown, YAML, TOML — already supported per work item 0056) belongs to Layer 0 (`src/data/workflow_definition.rs` — created here if not already in 0066; ASK THE DEVELOPER if uncertain whether parsing belongs at Layer 0 or in `src/engine/workflow/parser.rs`. The grand architecture document is silent on this exact split; the strongest argument for Layer 0 is that parsed `Workflow` is a serializable data type, and parsers are filesystem concerns. The strongest argument for Layer 1 is that DAG validation is engine logic. **Decide with the developer.**)

#### What moves into `WorkflowEngine`

- Yolo-mode auto-advance (countdown timing + advance-on-stuck logic) — currently in `oldsrc/tui/state.rs` and `oldsrc/commands/implement.rs`.
- Agent and model resolution per step — currently scattered across `oldsrc/commands/implement.rs` and `oldsrc/commands/exec.rs`.
- Exit-code interpretation — currently in `oldsrc/commands/implement.rs` and `oldsrc/commands/exec.rs`.
- Resume compatibility validation — currently `oldsrc/workflow/mod.rs::validate_resume_compatibility`.
- Step persistence — currently `oldsrc/workflow/mod.rs::save_workflow_state`.

#### What is forbidden in `WorkflowEngine`

- No direct container construction. Engines never call `ContainerRuntime::build`; they receive prepared `ContainerExecution` from a factory.
- No rendering, no `eprintln!`, no `tracing` to the user console. Status flows through `WorkflowFrontend::report_*`.
- No `clap` or `crossterm` use. Those are Layer 3 concerns.
- No knowledge of which frontend (CLI vs TUI vs headless) is on the other side of the trait. The engine treats all three identically.
- **No worktree lifecycle management.** `WorkflowEngine` is handed a working directory (via `WorkflowRuntimeContext::git_root`) and runs steps in it. It does not know whether that directory is a git worktree or the main checkout, does not check for uncommitted files, does not create or remove worktrees, and does not prompt about merging or discarding branches after completion. That entire lifecycle belongs to the command layer's `WorktreeLifecycle` helper (see work item 0068).

### 3. `src/engine/git/` — `GitEngine`

Consolidates every git operation amux performs. Replaces the free `pub fn`s in `oldsrc/git.rs`.

```rust
pub struct GitEngine { /* probably stateless, but a struct enforces typed access */ }

impl GitEngine {
    pub fn new() -> Self;
    pub fn version_check(&self) -> Result<GitVersion, EngineError>;
    pub fn resolve_root(&self, working_dir: &Path) -> Result<PathBuf, EngineError>;
    pub fn is_clean(&self, path: &Path) -> Result<bool, EngineError>;
    pub fn uncommitted_files(&self, path: &Path) -> Result<Vec<PathBuf>, EngineError>;
    pub fn worktree_path(&self, git_root: &Path, work_item: u32) -> Result<PathBuf, EngineError>;
    pub fn worktree_path_named(&self, git_root: &Path, name: &str) -> Result<PathBuf, EngineError>;
    pub fn create_worktree(&self, git_root: &Path, worktree: &Path, branch: &str) -> Result<(), EngineError>;
    pub fn remove_worktree(&self, git_root: &Path, worktree: &Path) -> Result<(), EngineError>;
    pub fn merge_branch(&self, git_root: &Path, branch: &str) -> Result<(), EngineError>;
    pub fn commit_all(&self, path: &Path, message: &str) -> Result<(), EngineError>;
    pub fn delete_branch(&self, git_root: &Path, branch: &str) -> Result<(), EngineError>;
    pub fn branch_exists(&self, git_root: &Path, branch: &str) -> bool;
    pub fn is_detached_head(&self, git_root: &Path) -> bool;
}
```

`GitEngine` implements Layer 0's `GitRootResolver` trait (introduced in 0066) so `Session::open` can use it. Provide an explicit `impl GitRootResolver for GitEngine` in `src/engine/git/`.

### 4. `src/engine/overlay/` — `OverlayEngine`

Consolidates overlay construction and management. Replaces `oldsrc/overlays/` and the agent-settings-passthrough bits of `oldsrc/passthrough.rs`.

```rust
pub struct OverlayEngine {
    path_resolver: OverlayPathResolver,   // Layer 0
    auth_resolver: AuthPathResolver,      // Layer 0
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

`OverlayRequest` describes "I want overlays for command X with these flags"; `build_overlays` returns the resolved set, deduplicated and canonicalized. Layer 2 hands the result into `ContainerOption::Overlay` variants.

Auth-credential overlays for agents (Claude config, Codex config, OpenCode config, Crush config, etc. — currently sprinkled through `oldsrc/passthrough.rs`) move here. They are constructed via `OverlayEngine::agent_settings_overlays(agent)`.

### 5. `src/engine/auth/` — `AuthEngine`

Consolidates two distinct concerns the legacy code conflates:

- Resolving host-side agent credentials (read host paths to mount-as-overlays). This delegates to `OverlayEngine` for the overlay construction; `AuthEngine` only enumerates which credentials exist and are available.
- Headless server authentication (API key generation, hashing, comparison, persistence, refresh). This replaces `oldsrc/commands/headless/auth.rs`.

```rust
pub struct AuthEngine {
    auth_paths: AuthPathResolver,     // Layer 0
    headless_paths: HeadlessPaths,    // Layer 0
}

impl AuthEngine {
    pub fn new(session: &Session) -> Self;

    // Agent credential discovery
    pub fn list_agent_credentials(&self, agent: &AgentName) -> Result<AgentCredentialStatus, EngineError>;

    // Headless API-key lifecycle
    pub fn generate_api_key(&self) -> Result<ApiKey, EngineError>;
    pub fn write_api_key_hash(&self, hash: &ApiKeyHash) -> Result<(), EngineError>;
    pub fn read_api_key_hash(&self) -> Result<Option<ApiKeyHash>, EngineError>;
    pub fn verify_api_key(&self, presented: &ApiKey) -> Result<AuthOutcome, EngineError>;
    pub fn refresh_api_key(&self) -> Result<ApiKey, EngineError>;

    // TLS material (post-0065 feature)
    pub fn ensure_self_signed_tls(&self, bind_ip: IpAddr) -> Result<TlsMaterial, EngineError>;
    pub fn load_tls_from_paths(&self, cert: &Path, key: &Path) -> Result<TlsMaterial, EngineError>;
}
```

All cryptographic comparisons MUST use `subtle::ConstantTimeEq` exactly as `aspec/architecture/security.md` requires.

### 5a. `src/engine/claws/` — `ClawsEngine`

`claws init` is a multi-phase command with complexity matching `ReadyEngine` and `InitEngine`: it clones the nanoclaw repository, verifies SSH/sudo availability inside a probe container, builds the nanoclaw Docker image, runs an audit pass, writes per-user configuration, and launches the nanoclaw controller container. The legacy implementation (`oldsrc/commands/claws.rs`: 1327 lines) mixes all of this with TUI and CLI I/O. All of it moves into `ClawsEngine`. `claws ready` (ensure image built, start controller) and `claws chat` (attach to running controller or start one) are expressed as alternative entry modes on the same engine.

#### 5a.a State machine

```rust
// src/engine/claws/phase.rs
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClawsPhase {
    /// Runtime detection, git root, nanoclaw config load, existing-clone check.
    Preflight,
    /// An existing nanoclaw clone was found at the target path. Ask user whether to re-clone.
    AwaitingCloneDecision,
    /// Clone the nanoclaw repository.
    CloningRepo,
    /// Launch a probe container to verify SSH key availability and sudo permissions.
    CheckingPermissions,
    /// Build the nanoclaw Docker image from the cloned Dockerfile.
    BuildingImage,
    /// Ask user whether to run the audit container before configuring.
    AwaitingAuditDecision,
    /// Run the nanoclaw audit container.
    RunningAudit,
    /// Write per-user nanoclaw configuration.
    Configuring,
    /// Start the nanoclaw controller container.
    LaunchingController,
    /// All phases complete; controller is running.
    Complete,
    /// A phase failed.
    Failed(ClawsFailure),
}
```

`claws ready` and `claws chat` enter the state machine at `Preflight` but use a `ClawsMode` option to skip phases that are already satisfied (image already built, controller already running). The engine checks preconditions at `Preflight` and advances directly to the first unsatisfied phase.

#### 5a.b `ClawsEngine` struct and API

```rust
// src/engine/claws/mod.rs
pub struct ClawsEngine {
    session: Arc<Session>,
    git_engine: Arc<GitEngine>,
    overlay_engine: Arc<OverlayEngine>,
    container_runtime: Arc<ContainerRuntime>,
    options: ClawsEngineOptions,
    phase: ClawsPhase,
}

pub struct ClawsEngineOptions {
    pub mode: ClawsMode,
    pub nanoclaw_url: Option<String>,  // override for clone URL; defaults from config
    pub refresh: bool,                 // force re-clone and rebuild
    pub no_cache: bool,
}

pub enum ClawsMode {
    /// Full init: clone → permissions → build → audit → configure → launch.
    Init,
    /// Ensure ready and start controller (skip clone/build if image already exists).
    Ready,
    /// Attach to running controller or start one (skip everything if controller running).
    Chat,
}

impl ClawsEngine {
    pub fn new(
        session: Arc<Session>,
        git_engine: Arc<GitEngine>,
        overlay_engine: Arc<OverlayEngine>,
        container_runtime: Arc<ContainerRuntime>,
        options: ClawsEngineOptions,
    ) -> Self;

    pub fn phase(&self) -> &ClawsPhase;

    /// Advance exactly one phase. Calls appropriate `ClawsFrontend` methods for the
    /// current phase and then transitions to the next phase. Returns the new phase.
    pub async fn step(&mut self, frontend: &mut dyn ClawsFrontend) -> Result<ClawsPhase, EngineError>;

    /// Drive to completion: call `step` in a loop until phase is `Complete` or `Failed`.
    pub async fn run_to_completion(&mut self, frontend: &mut dyn ClawsFrontend) -> Result<ClawsSummary, EngineError>;

    pub fn summary(&self) -> ClawsSummary;
}
```

#### 5a.c `ClawsFrontend` trait (defined by Layer 1, implemented by Layer 3)

```rust
// src/engine/claws/frontend.rs
pub trait ClawsFrontend: UserMessageSink + Send + Sync {
    /// An existing nanoclaw clone exists at `path`. Return true to delete and re-clone.
    fn ask_replace_existing_clone(&mut self, path: &Path) -> Result<bool, EngineError>;

    /// Clone is present and permissions probe passed. Return true to run the audit container.
    fn ask_run_audit(&mut self) -> Result<bool, EngineError>;

    /// Report a phase transition (called at the start of each phase).
    fn report_phase(&mut self, phase: &ClawsPhase);

    /// Report a named step's status within the current phase.
    fn report_step_status(&mut self, step: &str, status: StepStatus);

    /// The engine is about to run a container. Returns the frontend for that container's I/O.
    fn container_frontend(&mut self) -> Box<dyn ContainerFrontend>;

    /// Report the final summary on both success and failure.
    fn report_summary(&mut self, summary: &ClawsSummary);
}
```

#### 5a.d `ClawsSummary`

```rust
// src/engine/claws/summary.rs
pub struct ClawsSummary {
    pub clone: StepStatus,
    pub permissions_check: StepStatus,
    pub image_build: StepStatus,
    pub audit: StepStatus,
    pub configure: StepStatus,
    pub controller: StepStatus,
}
```

#### 5a.e What is forbidden in `ClawsEngine`

- No direct I/O (`println!`, `eprintln!`, terminal escape codes). All output goes through `ClawsFrontend`.
- No `clap`, no `crossterm`, no `ratatui`.
- No knowledge of which frontend (CLI, TUI, headless) is on the other side of the trait.

---

### 5b. `src/engine/agent/` — `AgentEngine`

`ensure_agent_available`, `build_agent_image`, `prepare_agent_cli`, `append_model_flag`, and `append_autonomous_flags` are currently implemented in `oldsrc/commands/agent.rs` (1608 lines) and called from `implement`, `chat`, `exec`, `ready`, and `claws`. These are not a state machine but a set of shared agent-management concerns. Centralising them in `AgentEngine` ensures that adding a new agent type or changing model-flag injection is a single-file edit rather than a sprawling fix across every command.

#### 5b.a `AgentEngine` struct and API

```rust
// src/engine/agent/mod.rs
pub struct AgentEngine {
    overlay_engine: Arc<OverlayEngine>,
    container_runtime: Arc<ContainerRuntime>,
}

/// Options controlling how an agent container is invoked.
pub struct AgentRunOptions {
    pub yolo: Option<YoloMode>,
    pub auto: Option<AutoMode>,
    pub plan: Option<PlanMode>,
    pub allowed_tools: Vec<String>,
    pub initial_prompt: Option<String>,
    pub allow_docker: bool,
    /// When true, force the agent to run in print-only (non-interactive) mode.
    /// `AgentEngine::build_options` translates this into the agent-specific flag
    /// (e.g. `--print` for Claude Code). Sourced from the `--non-interactive` CLI flag
    /// or from `GlobalConfig::headless.alwaysNonInteractive`.
    pub non_interactive: bool,
}

impl AgentEngine {
    pub fn new(
        overlay_engine: Arc<OverlayEngine>,
        container_runtime: Arc<ContainerRuntime>,
    ) -> Self;

    /// Ensure the named agent is available: download the agent Dockerfile if it is absent,
    /// then build the agent image via `container_runtime`. Reports progress via `frontend`.
    /// Called once per command invocation, before `build_options`.
    pub async fn ensure_available(
        &self,
        agent: &AgentName,
        config: &EffectiveConfig,
        frontend: &mut dyn AgentFrontend,
    ) -> Result<(), EngineError>;

    /// Build the `ContainerOption` list for running an agent container.
    /// Resolves overlays, injects model flags, autonomous flags, and all
    /// agent-specific entrypoint options. The caller passes the result directly to
    /// `ContainerRuntime::build`.
    pub fn build_options(
        &self,
        agent: &AgentName,
        model: &ModelName,
        run_options: &AgentRunOptions,
        session: &Session,
    ) -> Result<Vec<ContainerOption>, EngineError>;
}
```

#### 5b.b `AgentFrontend` trait (defined by Layer 1, implemented by Layer 3)

```rust
// src/engine/agent/frontend.rs
pub trait AgentFrontend: UserMessageSink + Send + Sync {
    /// Report a named step's status (e.g. "Downloading Dockerfile", "Building image").
    fn report_step_status(&mut self, step: &str, status: StepStatus);

    /// The engine is about to build a Docker image. Returns the container frontend
    /// for streaming build output.
    fn container_frontend(&mut self) -> Box<dyn ContainerFrontend>;
}
```

#### 5b.c What is forbidden in `AgentEngine`

- No direct I/O or terminal output. All output goes through `AgentFrontend`.
- No knowledge of which frontend (CLI, TUI, headless) is on the other side of the trait.
- No duplication of `ensure_available` or `build_options` logic in any other module. All commands that launch agents MUST call `AgentEngine::ensure_available` and `AgentEngine::build_options`. The pattern `ContainerRuntime::build(agent_engine.build_options(...))` is the only sanctioned way to prepare an agent container outside of engine internals.

---

### 6. `src/engine/ready/` — `ReadyEngine`

`ready` is a multi-phase command — preflight checks, legacy-layout detection and migration, Dockerfile.dev creation, Docker image build(s), local agent check, audit container run, and post-audit rebuild. The legacy code (`oldsrc/commands/ready.rs`: 2239 lines, `oldsrc/commands/ready_flow.rs`: 726 lines) spreads this logic across command, TUI, and flow layers. All of it moves into `ReadyEngine`.

#### 6a. State machine

```rust
// src/engine/ready/phase.rs
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReadyPhase {
    /// Initial checks: runtime detection, git root, config load, env vars, legacy-layout detection.
    Preflight,
    /// Dockerfile.dev is missing or matches the unmodified project template; ask user.
    AwaitingDockerfileDecision,
    /// Create Dockerfile.dev from the project template.
    CreatingDockerfile,
    /// Legacy single-file layout detected; ask user whether to migrate.
    AwaitingLegacyMigrationDecision,
    /// Migrate the legacy layout to modular layout.
    MigratingLegacyLayout,
    /// Build or rebuild the project base Docker image.
    BuildingBaseImage,
    /// Build or rebuild the agent Docker image on top of the base image.
    BuildingAgentImage,
    /// Check local agent installation by sending a random greeting.
    CheckingLocalAgent,
    /// Launch the audit container to scan and update Dockerfile.dev.
    RunningAudit,
    /// Rebuild images after the audit modified Dockerfile.dev.
    RebuildingAfterAudit,
    /// All phases complete.
    Complete,
    /// A phase failed; holds the structured error.
    Failed(ReadyFailure),
}
```

The state machine advances forward only; there are no backward transitions. Each phase's work is performed inside `ReadyEngine::step`, which returns the new phase after performing exactly one phase's worth of work. The engine persists phase in memory only (not to disk); if the process is interrupted, the user re-runs `amux ready` from the beginning.

#### 6b. `ReadyEngine` struct and API

```rust
// src/engine/ready/mod.rs
pub struct ReadyEngine {
    session: Arc<Session>,
    git_engine: Arc<GitEngine>,
    overlay_engine: Arc<OverlayEngine>,
    container_runtime: Arc<ContainerRuntime>,
    agent_engine: Arc<AgentEngine>,
    options: ReadyEngineOptions,
    phase: ReadyPhase,
}

pub struct ReadyEngineOptions {
    pub agent: AgentName,
    pub refresh: bool,
    pub build: bool,
    pub no_cache: bool,
    pub allow_docker: bool,
}

impl ReadyEngine {
    pub fn new(
        session: Arc<Session>,
        git_engine: Arc<GitEngine>,
        overlay_engine: Arc<OverlayEngine>,
        container_runtime: Arc<ContainerRuntime>,
        agent_engine: Arc<AgentEngine>,
        options: ReadyEngineOptions,
    ) -> Self;

    pub fn phase(&self) -> &ReadyPhase;

    /// Advance exactly one phase. Calls the appropriate `ReadyFrontend` methods
    /// for the current phase (Q&A decisions, status reports, container frontends)
    /// and then transitions to the next phase. Returns the new phase.
    pub async fn step(&mut self, frontend: &mut dyn ReadyFrontend) -> Result<ReadyPhase, EngineError>;

    /// Drive to completion: call `step` in a loop until phase is `Complete` or `Failed`.
    pub async fn run_to_completion(&mut self, frontend: &mut dyn ReadyFrontend) -> Result<ReadySummary, EngineError>;

    pub fn summary(&self) -> ReadySummary;
}
```

#### 6c. `ReadyFrontend` trait (defined by Layer 1, implemented by Layer 3)

```rust
// src/engine/ready/frontend.rs
pub trait ReadyFrontend: UserMessageSink + Send + Sync {
    /// Dockerfile.dev is absent. Return true to create it from the project template and continue.
    fn ask_create_dockerfile(&mut self) -> Result<bool, EngineError>;

    /// Dockerfile.dev matches the unmodified project template. Return true to run the audit.
    fn ask_run_audit_on_template(&mut self) -> Result<bool, EngineError>;

    /// Legacy single-file layout detected. Return true to migrate to modular layout.
    fn ask_migrate_legacy_layout(&mut self, agent_name: &AgentName) -> Result<bool, EngineError>;

    /// Report a phase transition (called at the start of each phase).
    fn report_phase(&mut self, phase: &ReadyPhase);

    /// Report a named step's status within the current phase.
    fn report_step_status(&mut self, step: &str, status: StepStatus);

    /// The engine is about to run a container (image build or audit). Returns the
    /// frontend to use for that container's I/O. The engine owns the returned value
    /// for the duration of the container run.
    fn container_frontend(&mut self) -> Box<dyn ContainerFrontend>;

    /// Report the final summary on both success and failure.
    fn report_summary(&mut self, summary: &ReadySummary);
}
```

#### 6d. `ReadySummary`

```rust
// src/engine/ready/summary.rs
pub struct ReadySummary {
    pub runtime_name: String,
    pub base_image: StepStatus,
    pub agent_image: StepStatus,
    pub local_agent: StepStatus,
    pub audit: StepStatus,
    pub legacy_migration: StepStatus,
}
```

`StepStatus` is shared across `ReadyEngine` and `InitEngine`. Define it once in `src/engine/step_status.rs` and re-export from `src/engine/mod.rs`:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum StepStatus {
    Pending,
    Skipped,
    Running,
    Done,
    Failed(String),  // human-readable reason
}
```

#### 6e. What is forbidden in `ReadyEngine`

- No direct I/O (`println!`, `eprintln!`, terminal escape codes). All output goes through `ReadyFrontend`.
- No `clap`, no `crossterm`, no `ratatui`.
- No knowledge of which frontend (CLI, TUI, headless) is on the other side of the trait.

### 7. `src/engine/init/` — `InitEngine`

`init` sets up a new project: git root resolution, aspec folder creation, Dockerfile.dev template, `.amux.json` config write, optional audit container, image build, and work-items configuration. The legacy code (`oldsrc/commands/init.rs`: 54 lines, `oldsrc/commands/init_flow.rs`: 2648 lines) is likewise fragmented across command and flow layers. All of it moves into `InitEngine`.

#### 7a. State machine

```rust
// src/engine/init/phase.rs
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum InitPhase {
    /// Resolve git root and validate the environment.
    Preflight,
    /// Existing aspec folder found; ask user whether to replace it.
    AwaitingAspecDecision,
    /// Write the aspec template folder into the repo.
    CreatingAspecFolder,
    /// Create or confirm Dockerfile.dev from the template.
    SettingUpDockerfile,
    /// Write or update `.amux.json`.
    WritingConfig,
    /// Ask user whether to run the audit container.
    AwaitingAuditDecision,
    /// Build the base Docker image.
    BuildingImage,
    /// Run the audit container (agent scans and updates Dockerfile.dev).
    RunningAudit,
    /// Ask user whether to configure work items.
    AwaitingWorkItemsDecision,
    /// Write work-items config into `.amux.json`.
    WritingWorkItemsConfig,
    /// All phases complete.
    Complete,
    /// A phase failed.
    Failed(InitFailure),
}
```

As with `ReadyEngine`, the state machine is forward-only. If the process is interrupted, the user re-runs `amux init`.

#### 7b. `InitEngine` struct and API

```rust
// src/engine/init/mod.rs
pub struct InitEngine {
    session: Arc<Session>,
    git_engine: Arc<GitEngine>,
    overlay_engine: Arc<OverlayEngine>,
    container_runtime: Arc<ContainerRuntime>,
    options: InitEngineOptions,
    phase: InitPhase,
    summary: InitSummary,
}

pub struct InitEngineOptions {
    pub agent: AgentName,
    pub run_aspec_setup: bool,
    pub git_root: PathBuf,
}

impl InitEngine {
    pub fn new(
        session: Arc<Session>,
        git_engine: Arc<GitEngine>,
        overlay_engine: Arc<OverlayEngine>,
        container_runtime: Arc<ContainerRuntime>,
        options: InitEngineOptions,
    ) -> Self;

    pub fn phase(&self) -> &InitPhase;

    /// Advance exactly one phase.
    pub async fn step(&mut self, frontend: &mut dyn InitFrontend) -> Result<InitPhase, EngineError>;

    /// Drive to completion.
    pub async fn run_to_completion(&mut self, frontend: &mut dyn InitFrontend) -> Result<InitSummary, EngineError>;

    pub fn summary(&self) -> &InitSummary;
}
```

#### 7c. `InitFrontend` trait (defined by Layer 1, implemented by Layer 3)

```rust
// src/engine/init/frontend.rs
pub trait InitFrontend: UserMessageSink + Send + Sync {
    /// Existing aspec folder found. Return true to replace it; false to keep it.
    fn ask_replace_aspec(&mut self) -> Result<bool, EngineError>;

    /// Dockerfile.dev setup complete. Return true to run the audit container now.
    fn ask_run_audit(&mut self) -> Result<bool, EngineError>;

    /// Offer work-items configuration. Return Some(config) to enable; None to skip.
    fn ask_work_items_setup(&mut self) -> Result<Option<WorkItemsConfig>, EngineError>;

    /// Report a phase transition.
    fn report_phase(&mut self, phase: &InitPhase);

    /// Report a named step's status within the current phase.
    fn report_step_status(&mut self, step: &str, status: StepStatus);

    /// The engine is about to run a container. Returns the frontend for that container.
    fn container_frontend(&mut self) -> Box<dyn ContainerFrontend>;

    /// Report the final summary on both success and failure.
    fn report_summary(&mut self, summary: &InitSummary);
}
```

#### 7d. `InitSummary`

```rust
// src/engine/init/summary.rs
pub struct InitSummary {
    pub config: StepStatus,
    pub aspec_folder: StepStatus,
    pub dockerfile: StepStatus,
    pub audit: StepStatus,
    pub image_build: StepStatus,
    pub work_items_setup: StepStatus,
}
```

#### 7e. What is forbidden in `InitEngine`

- No direct I/O or terminal output.
- No `clap`, no `crossterm`, no `ratatui`.
- No knowledge of which frontend is on the other side of the trait.

### 8. `src/engine/message.rs` — `UserMessageSink`

All engines (and Layer 2 commands) need a way to write status messages to the user that are **not** container I/O. Examples: "Resolving agent credentials…", "Worktree created at /path/to/wt", "Step 1 of 3 completed in 47 s". These are distinct from container stdout/stderr (which flows through `ContainerFrontend`) and from per-engine structured reports (`WorkflowFrontend::report_step_status`, etc.).

The critical constraint is the **CLI queueing requirement**: when a PTY-bound container has the terminal, amux cannot write to it without corrupting the display. Messages written during that window must be queued and replayed after the container releases the terminal.

```rust
// src/engine/message.rs

#[derive(Debug, Clone)]
pub struct UserMessage {
    pub level: MessageLevel,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageLevel {
    Info,
    Warning,
    Error,
    Success,
}

/// A sink for amux-authored status messages that are displayed in the amux UI,
/// not in the container's terminal window. Defined by Layer 1; implemented by Layer 3.
///
/// # Queueing contract
///
/// Implementations MUST handle the case where the terminal is currently owned by a
/// running PTY-bound container (CLI mode). In that state, `write_message` queues
/// the message internally. `replay_queued` drains the queue to the output device
/// after the container releases the terminal.
///
/// TUI and headless implementations render messages live and SHOULD implement
/// `replay_queued` as a no-op.
pub trait UserMessageSink: Send + Sync {
    /// Write a message immediately if the output device is available, or queue it.
    fn write_message(&mut self, msg: UserMessage);

    /// Drain and emit all queued messages in insertion order.
    ///
    /// Called by the command layer immediately after a container exits and the
    /// terminal is available again. Idempotent — calling it twice is safe.
    fn replay_queued(&mut self);

    // Convenience helpers with default implementations:
    fn info(&mut self, text: impl Into<String>) {
        self.write_message(UserMessage { level: MessageLevel::Info, text: text.into() });
    }
    fn warning(&mut self, text: impl Into<String>) {
        self.write_message(UserMessage { level: MessageLevel::Warning, text: text.into() });
    }
    fn error_msg(&mut self, text: impl Into<String>) {
        self.write_message(UserMessage { level: MessageLevel::Error, text: text.into() });
    }
    fn success(&mut self, text: impl Into<String>) {
        self.write_message(UserMessage { level: MessageLevel::Success, text: text.into() });
    }
}
```

`UserMessageSink` is a **supertrait of every Layer 1 frontend trait** (`ContainerFrontend`, `WorkflowFrontend`, `ReadyFrontend`, `InitFrontend`). This means:
- Any type implementing `ContainerFrontend` also implements `UserMessageSink`.
- Engine code can call `frontend.info(...)` or `frontend.warning(...)` anywhere a frontend reference is held.
- Layer 3 implements `UserMessageSink` once per concrete frontend type and gets all the engine call sites for free.

The **CLI implementation** (`CliUserMessageSink`) holds a `Vec<UserMessage>` queue and a `bool pty_is_active` flag set by `CliContainerFrontend` before it hands the terminal to the container and cleared after the container exits. When `pty_is_active` is true, `write_message` pushes to the queue. When false, it writes immediately to stderr. `replay_queued` drains the queue to stderr and clears it; the CLI command layer calls this immediately after each `ContainerExecution::wait` returns.

The **TUI implementation** (`TuiUserMessageSink`) writes messages to the outer execution window behind the active container window. `replay_queued` is a no-op.

The **headless implementation** (`HeadlessUserMessageSink`) emits each message as an SSE event of type `amux-message` with `level` and `text` fields. `replay_queued` is a no-op.

### 9. Errors

`src/engine/error.rs` defines `EngineError` covering every failure mode in Layer 1. It wraps `DataError` for failures bubbling up from Layer 0. Higher layers wrap `EngineError` in their own error types; Layer 1 does not depend on higher-layer errors.

### 9a. Engine parity addenda — legacy behaviors that MUST be preserved

This section enumerates concrete behaviors observed in `oldsrc/` that the new engines MUST reproduce. Where the legacy behavior conflicts with a tenet, the tenet wins, but the conflict MUST be called out in the PR description and the developer MUST be consulted.

#### 9a.1 `AgentEngine` — per-agent matrix

`AgentEngine::build_options` MUST encode the following per-agent translation table. Any code that branches on agent name lives in `src/engine/agent/agent_matrix.rs` ONLY. Adding a new agent is a single-file edit.

The supported agent names — derived from the `Agent` enum in `oldsrc/cli.rs` — are:

`claude`, `codex`, `opencode`, `maki`, `gemini`, `copilot`, `crush`, `cline`.

For each agent, document and implement:

| Aspect | What MUST be encoded |
|---|---|
| Interactive entrypoint | The bare CLI command (e.g. `["claude"]` for Claude, `["copilot", "-i"]` for Copilot) |
| Non-interactive entrypoint | The print/run/exec form (e.g. `--print` / `-p` for Claude, `exec`/`run` for Codex, `run` for OpenCode/Crush, `task` for Cline). When `AgentRunOptions::non_interactive` is true. |
| Initial-prompt argument shape | Whether the prompt is a positional after the entrypoint or after a sub-flag |
| Plan-mode flag | Per-agent: `--permission-mode plan` (Claude), `--approval-mode plan` (Codex), `--approval-mode=plan` (Gemini), `--plan` (Copilot, Cline). OpenCode, Maki, Crush DO NOT support plan; supplying `PlanMode::Enabled` MUST yield `EngineError::PlanModeUnsupported { agent }`. |
| Yolo flag | Per-agent: `--dangerously-skip-permissions` (Claude family). For agents without a yolo flag, yolo silently equates to no permission flags but still applies the overlay-level `yoloDisallowedTools` (see below). |
| Auto flag | Per-agent: `--permission-mode auto` (Claude); other agents follow legacy mapping. |
| Allowed/disallowed tools | Per-agent flag name: `--disallowedTools` for Claude with the `yoloDisallowedTools` config list when in yolo mode. Other agents per their CLI docs. |
| Model flag | `--model NAME` for most; Claude additionally accepts `--model-claude-opus-4-6` shorthand the legacy code emits. The new code SHOULD prefer `--model NAME`. |
| Image tag convention | `<repo-hash>:<agent>:latest` (agent image), built `FROM <repo-hash>:latest` (project image). |
| Dockerfile path | `<git-root>/.amux/Dockerfile.<agent>`. The project Dockerfile is `<git-root>/Dockerfile.dev`. |
| Dockerfile download URL | Constructed against `download.rs` constants (GitHub raw, `qwibitai/amux/.../.amux/Dockerfile.<agent>`). The exact URL set is captured in a checked-in constant module — no string formatting from agent name + base URL elsewhere. |

`AgentEngine::ensure_available` MUST:

1. Check whether `<git-root>/.amux/Dockerfile.<agent>` exists. If not, download it (reporting `report_step_status("Downloading Dockerfile", Running)` then `Done`).
2. Check whether `<repo-hash>:<agent>:latest` exists locally. If not, build it (reporting `Building image`).
3. If the project base image is missing, fail with `EngineError::AgentRequiresProjectImage` — `AgentEngine` does NOT build the project image (that is `ReadyEngine`'s job). Cover with a unit test.
4. Be idempotent: if both Dockerfile and image exist, no `report_step_status` calls fire and no container_frontend is requested. Cover with a unit test (already listed; this addendum reinforces the test).

`AgentEngine` MUST NOT make agent-availability decisions — it reports state. The "offer to fall back to default agent if requested agent is unavailable" decision belongs to **Layer 2** (`ExecWorkflowCommand`, `ChatCommand`, etc.) via a new method on the per-command frontend trait. See WI 0068 §6.3b (`AgentSetupFrontend`) for the trait surface; `AgentEngine` only signals "image absent" via the return value or step status.

#### 9a.2 `OverlayEngine` — agent settings passthrough fidelity

`OverlayEngine::agent_settings_overlays` MUST replicate the legacy `HostSettings` machinery in `oldsrc/passthrough.rs`. Specifically:

- **Claude config sanitization**: when mounting `~/.claude.json`, strip the `oauthAccount` field before writing the container-side copy. Cover with a unit test that asserts the field is removed and other fields are preserved byte-for-byte.
- **Claude `.claude/` directory denylist filter**: copy `~/.claude/` into the prepared overlay with the legacy denylist applied — `projects`, `sessions`, `session-env`, `debug`, `file-history`, `history.jsonl`, `telemetry`, `downloads`, `ide`, `shell-snapshots`, `paste-cache`. The list lives in a single named constant `CLAUDE_DENYLIST` so adding a new entry is a one-line change. Cover with a unit test that verifies each denylisted entry is absent from the overlay output.
- **`apply_yolo_settings`**: when `AgentRunOptions::yolo` is `Some(YoloMode::Enabled)`, the prepared overlay's `settings.json` MUST contain `"skipDangerousModePermissionPrompt": true`. Cover with a unit test reading the produced file.
- **`disable_lsp_recommendations`**: every prepared Claude overlay sets `"hasShownLspRecommendation": true` (and removes the dead key the legacy code cleans up). Cover with a unit test.
- **`apply_dockerfile_user`**: when the agent's `Dockerfile.<agent>` ends with a non-root `USER` directive, the overlay's container path MUST be remapped from `/root/...` to the detected user home. Cover with a unit test using a synthetic Dockerfile.
- **`prepare_minimal` fallback**: when `~/.claude.json` does NOT exist on the host, `OverlayEngine` produces a minimal overlay containing a synthesized `claude.json` with `/workspace` project trust + LSP recommendation suppression. Cover with a unit test.
- **Non-Claude agents**: each non-Claude agent (Codex, OpenCode, Crush, etc.) maps to a single agent-config-dir overlay (legacy `new_agent_dir`). The host path and container path per agent live in the agent matrix from §9a.1.

These behaviors are **non-negotiable** — they govern whether agents in the new amux can authenticate and run successfully on first launch. Any deviation requires explicit developer sign-off in the PR description.

#### 9a.3 `AuthEngine` — keychain credential resolution

The legacy `oldsrc/commands/auth.rs::agent_keychain_credentials(agent)` and `resolve_auth(repo_dir, agent)` are NOT covered by §5's `AuthEngine` outline (which only covers headless API keys + TLS). They MUST be added to `AuthEngine`:

```rust
impl AuthEngine {
    /// Look up agent credentials in the host keychain (macOS Keychain, Linux libsecret, Windows credential manager).
    /// Returns the env-var pairs that should be injected into the agent container at launch.
    /// Returns an empty `AgentCredentials` when no credentials are configured for the agent —
    /// this is not an error.
    pub fn agent_keychain_credentials(&self, agent: &AgentName) -> Result<AgentCredentials, EngineError>;

    /// Composite resolver: returns keychain credentials for the agent, scoped to the per-repo config.
    /// Caller may consult `auto_agent_auth_accepted` from `EffectiveConfig` and prompt the user
    /// before injection (one-time consent flow handled at Layer 2).
    pub fn resolve_agent_auth(
        &self,
        session: &Session,
        agent: &AgentName,
    ) -> Result<AgentCredentials, EngineError>;
}

/// Env-var pairs to inject into an agent container.
pub struct AgentCredentials {
    pub env_vars: Vec<(String, String)>,
}
```

The keychain backend MUST use the `keyring` crate (or equivalent) and gate macOS/Linux/Windows behavior in a single module (`src/engine/auth/keychain.rs`). The per-agent env-var name set (e.g. `ANTHROPIC_API_KEY` for Claude) lives alongside the agent matrix from §9a.1.

The `auto_agent_auth_accepted` boolean in `Repo` config governs whether amux should offer to use keychain credentials silently (`true`) or prompt every time (`false`/unset). Reading and writing this flag is a Layer 2 / Layer 0 concern; `AuthEngine` only resolves the credentials.

#### 9a.4 `WorkflowEngine` — additional NextAction variants and dialog distinctions

The §2 `NextAction` enum is missing two legacy behaviors that the TUI exposes via the workflow control board:

```rust
pub enum NextAction {
    LaunchNext,
    ContinueInCurrentContainer { prompt: String },
    RestartCurrentStep,
    CancelToPreviousStep,
    /// Mark every remaining step as Skipped and the workflow as completed successfully.
    /// Only valid when the current step is the last in topological order; the engine rejects
    /// it otherwise with `EngineError::InvalidAdvanceAction`.
    /// Equivalent to the legacy TUI "Ctrl+Enter Finish" key on the WorkflowControlBoard.
    FinishWorkflow,
    Pause,
    Abort,
}
```

`AvailableActions` gains a corresponding `can_finish_workflow: bool` and `finish_workflow_unavailable_reason: Option<String>` field (true only on the last step).

The legacy "step failed → retry?" prompt is a distinct frontend interaction. Add a method to `WorkflowFrontend`:

```rust
pub trait WorkflowFrontend: UserMessageSink + Send + Sync {
    // …existing methods…

    /// Called immediately after a step transitions to `StepState::Failed`.
    /// Returns the user's choice. Default behaviors:
    ///   - `StepFailureChoice::Retry` → engine reverts the step to Pending and re-launches a fresh container.
    ///   - `StepFailureChoice::Pause` → engine persists state and returns from `step_once`.
    ///   - `StepFailureChoice::Abort` → engine marks remaining steps Cancelled and returns.
    fn user_choose_after_step_failure(
        &mut self,
        step: &WorkflowStep,
        exit: &ContainerExitInfo,
    ) -> Result<StepFailureChoice, EngineError>;
}

pub enum StepFailureChoice {
    Retry,
    Pause,
    Abort,
}
```

The legacy TUI also has a separate "cancel workflow" confirmation distinct from `Abort` — this is a Layer 3 dialog concern; the engine treats `NextAction::Abort` as the canonical "user wants out" signal.

#### 9a.5 `WorkflowEngine` — stuck-detection vs yolo-countdown distinction

The legacy code distinguishes two timers:

1. **Stuck timer** — agent has produced no PTY output for `agentStuckTimeout` seconds (default 30s, from `EffectiveConfig::agent_stuck_timeout`). Triggers a "the agent appears stuck" indicator (yellow tab + ⚠️ in the tab bar).
2. **Yolo countdown** — only when `--yolo` is set and a stuck timer has fired. Counts down `YOLO_COUNTDOWN_DURATION` seconds (60s) before the engine auto-advances via `NextAction::LaunchNext`.

`WorkflowEngine` MUST own both timers and surface them through distinct `WorkflowFrontend` methods:

```rust
pub trait WorkflowFrontend: UserMessageSink + Send + Sync {
    /// Called once when stuck-detection fires for the current step. The engine continues
    /// running the step; the frontend SHOULD render a stuck indicator.
    fn report_step_stuck(&mut self, step: &WorkflowStep);

    /// Called once when stuck-detection clears because the agent produced new output.
    fn report_step_unstuck(&mut self, step: &WorkflowStep);

    /// Called repeatedly while a yolo countdown is ticking down.
    /// Returns `YoloTickOutcome::Continue` to keep counting; `Cancel` to abandon (e.g. user
    /// pressed Esc); `AdvanceNow` to skip the rest of the countdown.
    fn yolo_countdown_tick(&mut self, remaining: Duration) -> Result<YoloTickOutcome, EngineError>;
}

pub enum YoloTickOutcome { Continue, Cancel, AdvanceNow }
```

The countdown MUST use `tokio::time::Instant` (monotonic). The stuck threshold MUST be sourced from `EffectiveConfig`, not hard-coded. The yolo countdown duration MUST be sourced from a named constant `YOLO_COUNTDOWN_DURATION` (60s) defined in `src/engine/workflow/timing.rs`.

When a yolo countdown is dismissed (`Cancel`), the engine MUST honor a backoff (`STUCK_DIALOG_BACKOFF`, 60s) before re-firing `report_step_stuck` for the same step — matching legacy behavior so dismissed dialogs don't re-pop instantly.

#### 9a.6 Workflow file parsing — Layer 0 ownership

ASK-THE-DEVELOPER question from §2 is resolved here: workflow file parsing (Markdown, TOML, YAML) MUST live in **Layer 0** under `src/data/workflow_definition.rs` (Markdown), `workflow_definition_toml.rs`, and `workflow_definition_yaml.rs`. Format detection (`detect_format(path)`) is a free `pub fn` in `workflow_definition.rs`. DAG validation (cycle detection, reference validation) is also Layer 0 (`workflow_dag.rs` from §0.5).

Prompt-template substitution — `{{work_item_number}}`, `{{work_item_content}}`, `{{work_item_section:[Name]}}` — is also Layer 0 (`src/data/workflow_prompt_template.rs`). The legacy `substitute_prompt` and `extract_section` semantics MUST be preserved exactly:

- `{{work_item_number}}` → zero-padded 4-digit work-item number.
- `{{work_item_content}}` → full text of the work-item file.
- `{{work_item_section:[Name]}}` → body of the named H1/H2 section, case-insensitive heading match, trailing `:` stripped from heading.
- When `work_item` is `None`, all `work_item_*` placeholders are replaced with empty strings AND a `UserMessage` is queued (level Warning) noting the missing work-item context. (The engine pulls this warning from Layer 0 and forwards via `UserMessageSink`.)

Cover each substitution rule with a Layer 0 unit test against synthetic markdown.

#### 9a.7 Workflow state persistence path

The legacy state path is `<git-root>/.amux/workflows/<hash8>-<work-item>-<name>.json` (or `<hash8>-<name>.json` when no work item). §0.5 of this work item proposes `<HOME>/.amux/workflow-state/`. **The legacy path is the source of truth** because users have in-flight workflow state on disk that must continue to load after upgrade. Update §0.5's `WorkflowStateStore::new(&Session)` to derive `base_dir` from `Session::git_root().join(".amux/workflows/")`.

`WorkflowStateStore` MUST also implement a one-time migration: on first `load(name)` call, scan a legacy fallback location (`<HOME>/.amux/workflow-state/`) and copy any matching files into `<git-root>/.amux/workflows/`. Cover with a Layer 0 integration test.

#### 9a.8 `ContainerRuntime` — naming, image tags, backend selection

- **Container name format**: `amux-<pid>-<nanos>` for ephemeral runs; stable names like `amux-claws-controller` for long-lived containers (set via `ContainerOption::Name`). The legacy `generate_container_name()` lives in `src/engine/container/naming.rs`.
- **Image tags**: project image is `<repo-hash>:latest`; agent image is `<repo-hash>:<agent>:latest`. Repo hash is the SHA256 prefix of the canonicalized git-root path (length and algorithm match the legacy `repo_hash` function — capture the legacy implementation in `src/engine/container/image_tag.rs` and add a unit test against a known fixture path).
- **Backend selection** (resolves §5b "ASK THE DEVELOPER"): on macOS, accept `runtime` config values `docker` (default) and `apple-containers`. On non-macOS, accept only `docker`; an `apple-containers` value yields `EngineError::BackendUnsupportedOnPlatform { backend, platform }`. Empty/missing config defaults to `docker`. An unknown value defaults to `docker` and emits a `UserMessage::warning`. Cover all four cases (Docker on Linux, Apple on macOS, Apple on Linux → error, unknown → warn+default) with unit tests.
- **Apple Containers stats parsing**: legacy uses `container stats --format json` and derives CPU% from two time-spaced samples. The Apple backend MUST replicate this in `src/engine/container/apple.rs::stats` and produce the same `ContainerStats { cpu_percent, memory }` shape as the Docker backend.

#### 9a.9 `GitEngine` — naming conventions and merge strategy

`GitEngine` MUST encode the legacy naming and merge conventions:

- **Worktree path**: `<HOME>/.amux/worktrees/<repo-name>/<NNNN>/` for work-item runs and `<HOME>/.amux/worktrees/<repo-name>/wf-<workflow-name>/` for named-workflow runs. `<repo-name>` is the basename of `git_root`. `<NNNN>` is the zero-padded 4-digit work-item number.
- **Branch name**: `amux/work-item-<NNNN>` and `amux/workflow-<workflow-name>`.
- **Merge strategy**: `git merge --squash <branch>` followed by `git commit -m "Implement <branch>"`. The commit message format is preserved verbatim.
- **Detached HEAD detection**: `is_detached_head` calls `git symbolic-ref --quiet HEAD` and returns `true` only when the command exits non-zero. Cover with a hermetic temp-repo unit test.
- **Worktree path / branch helper methods**: `worktree_path` (work-item form) and `worktree_path_named` (workflow form) MUST encode the conventions above; document them in rustdoc with concrete examples.

#### 9a.10 Backend-aware `ContainerOption` ergonomics

Legacy `oldsrc/runtime/` exposes one option-bag struct that flattens many concerns. The new `ContainerOption` enum MUST exhaustively cover at minimum:

`Image`, `Entrypoint`, `Overlay`, `EnvPassthrough`, `EnvLiteral { key, value }`, `SeededPrompt`, `Interactive`, `AllowDocker`, `MountSsh { source }`, `Yolo`, `Auto`, `Plan`, `WorkingDir`, `Name`, `AgentSettingsPassthrough`, `AgentCredentials { env_vars }` (from §9a.3), `DisallowedTools(Vec<String>)`, `Model { flag_form, value }`, `NonInteractivePrintFlag`, `DockerfileUser` (resolved by §9a.2's `apply_dockerfile_user`).

If a `ContainerOption` is irrelevant to the chosen backend (e.g. `MountSsh { source }` on a backend without bind-mount support — currently nonexistent), the backend MAY ignore it but MUST NOT silently drop a security-relevant option. Surfacing via `EngineError::OptionNotSupportedByBackend` is the safer default. Cover with a unit test using a fake backend.

### 10. What must NOT happen in this work item

- No changes to `oldsrc/`. The user-visible `amux` binary continues to ship from `oldsrc/`.
- No direct user-facing I/O from any engine. All message output goes through `UserMessageSink::write_message`. No `println!`, `eprintln!`, or `print!` anywhere in `src/engine/`.
- No work in `src/command/` or `src/frontend/` beyond ensuring they compile as empty modules.
- No `pub fn run_container_with_*` style APIs. Hard-fail any review that introduces them.
- No exposure of the Docker or Apple backend type outside `src/engine/container/`. If a reviewer can name the concrete backend type from a call site outside that module, it is a violation.
- No PTY/crossterm code in `src/engine/`. PTYs are Layer 3.
- No `clap` references in `src/engine/`. Clap is Layer 4 / Layer 3 (CLI).
- No DAG logic, workflow state types, or state persistence code inside `src/engine/workflow/`. Those live in `src/data/` (section 0.5). If the implementing agent finds themselves writing DAG traversal or JSON serialization inside `src/engine/workflow/`, they must move that code to `src/data/` instead.
- No multi-phase command logic (phase loops, decision prompts, image build sequences) inside `src/command/` or `src/frontend/`. `ReadyEngine`, `InitEngine`, and `ClawsEngine` own all of that; Layer 2 and Layer 3 only construct those engines and implement their frontend traits.
- No duplication of agent availability or container option construction outside `AgentEngine`. Any code in `src/engine/` or `src/command/` that launches an agent container MUST call `AgentEngine::ensure_available` and `AgentEngine::build_options`. Duplicating `append_model_flag` or `append_autonomous_flags` logic elsewhere is a violation.
- No "just do it like the legacy code did" decisions. If the grand architecture's tenets disagree with the legacy approach, follow the tenets and ASK THE DEVELOPER if the cost looks high.

## Edge Case Considerations:

- **Backend encapsulation in `ContainerRuntime`**: `ContainerRuntime::detect` selects the backend once at construction. The chosen variant (Docker or Apple) is stored as `Box<dyn ContainerBackend>` inside `ContainerRuntime` and MUST NOT leak through any public method, error message, or `Debug` output that reaches callers outside `src/engine/container/`. If a caller somehow needs to know the backend name for display (e.g. `amux status`), expose a `runtime_name() -> &'static str` method on `ContainerRuntime` — not the concrete type.
- **Backend is process-wide**: the global config field that selects Docker vs Apple is read once at startup. If the user has Docker in one tab and Apple in another, that is a user error — the process picks one. ASK THE DEVELOPER whether to error on ambiguous config or to always prefer one over the other.
- **Container lifetime exceeding `ContainerExecution`**: today some commands intentionally leave a container running (e.g. headless background mode). The `ContainerExecution::wait` API forces a join; provide an alternative `ContainerExecution::detach() -> ContainerHandle` that hands ownership of the running container back to the caller without joining.
- **Workflow resume across amux versions**: `WorkflowState` is persisted by Layer 0, but the *interpretation* of state lives in `WorkflowEngine`. The engine must reject (with a structured error, not a panic) any workflow state whose `schema_version` is newer than the engine understands.
- **Workflow container reuse across steps**: when `NextAction::ContinueInCurrentContainer` is chosen, the engine must confirm that the step transition is valid (same agent, same model, container still running, factory supports injection) before calling `inject_prompt`. If any check fails, the engine returns `EngineError::InvalidAdvanceAction` with a structured reason — it does not silently fall back to a new container.
- **Multi-agent workflow advance constraints**: when step N specifies `agent: claude` and step N+1 specifies `agent: codex`, `ContinueInCurrentContainer` MUST be absent from `AvailableActions`. The engine computes this set before calling `user_choose_next_action`; the frontend renders only the available options. Cover with a unit test using a two-step workflow with different agents.
- **Yolo countdown precision**: today the countdown uses wallclock; prefer `tokio::time::Instant` (monotonic) so suspending the process or system clock skew does not accelerate or skip the countdown. ASK THE DEVELOPER if they prefer wallclock for any user-facing reason.
- **`ReadyEngine` phase interruption**: if the process is killed mid-phase (e.g. during `BuildingAgentImage`), the engine has no checkpoint. On the next `amux ready` invocation, a fresh `ReadyEngine` begins from `Preflight`. The engine MUST NOT leave behind partial artifacts that cause the next run to fail; specifically, a partially-built Docker image (if the build was cancelled) must not prevent a clean re-run. Cover `Preflight` → `BuildingBaseImage` → process kill → clean re-run in a unit test using a fake container runtime.
- **`InitEngine` aspec folder idempotency**: if `AwaitingAspecDecision` asks the user and the user declines to replace, the engine skips `CreatingAspecFolder` but continues through the remaining phases. Cover with a unit test that declines the aspec replacement and asserts the summary shows `aspec_folder: StepStatus::Skipped`.
- **`OverlayEngine` deduplication keys**: today's dedup uses canonicalized paths. Re-use `OverlayPathResolver::canonicalize` (Layer 0) — do not re-implement.
- **`AuthEngine::verify_api_key` timing**: every comparison MUST be constant-time even when no hash exists on disk (compare against a fixed-length sentinel). This avoids leaking "is the server running with auth disabled" via timing.
- **`GitEngine::resolve_root` failure on a directory that *is* a git root**: `git rev-parse --show-toplevel` already returns the input dir if it is itself a git root; cover this in a unit test.
- **`ContainerFrontend::read_stdin` blocking semantics**: define explicitly whether `read_stdin` may block, and how cancellation works. The frontend trait MUST be usable from both async (TUI, headless) and sync (CLI) contexts. ASK THE DEVELOPER whether to make the trait `async_trait` or to keep it sync with `tokio::task::spawn_blocking` adapters at frontend implementation sites.
- **PTY size changes mid-execution**: `ContainerFrontend::resize_pty` is called by Layer 3; the engine forwards to the underlying Docker/Apple resize syscall. Cover with an integration test that resizes mid-stream and confirms the container sees the new size.

## Test Considerations:

### Test philosophy (read first)

Tests for Layer 1 are **designed and written from scratch** alongside the new engines. **Do not port tests from `oldsrc/tests/*` or from `oldsrc/runtime/**/#[cfg(test)] mod tests`, `oldsrc/workflow/**`, `oldsrc/git.rs`, `oldsrc/overlays/**`, or `oldsrc/passthrough.rs` test blocks.** Those tests assume the legacy `run_container_with_*` API surface, the legacy workflow/CLI flow that conflated business logic with frontend output, and the legacy free-function helpers. Carrying them forward defeats the refactor's purpose.

The narrow exception is a test that satisfies **all** of the following:

1. Asserts a precise behavioral invariant the new engine MUST preserve (e.g. exit-code semantics, container name format, branch-naming convention, overlay dedup rules, constant-time auth verification).
2. Compiles unchanged or with mechanical edits against the new engine surfaces.
3. Exercises only Layer 0 + Layer 1 — no upward calls, no legacy-runtime types.

If any old test is brought forward under this exception, the PR description MUST list it with a one-sentence justification. The default answer is "rewrite from scratch."

This work item produces **only Layer 1 unit tests** using fakes that satisfy the engine-defined frontend traits. **No real Docker, no real network, no real PTY, no real HTTP, and no end-to-end multi-engine scenarios** in this work item. Those are 0070's responsibility, against a freshly rebuilt `tests/` directory.

### Unit tests (colocated `#[cfg(test)] mod tests`)

All tests use either fully synthetic inputs or hermetic temp-directories. Container tests use a `FakeContainerInstance` that the test module owns, satisfying `ContainerInstance` by recording calls without invoking Docker.

- **`UserMessageSink`** (colocated in `src/engine/message.rs`):
  - `write_message` when `replay_queued` has not been called: message is queued.
  - `replay_queued` drains the queue in insertion order; a second call is a no-op.
  - Convenience methods (`info`, `warning`, `error_msg`, `success`) write the correct level.
  - Implement a `RecordingMessageSink` (used in all engine unit tests in this work item) that records written messages and exposes them for assertion.
- **Layer 0 additions (`WorkflowDag`, `WorkflowState`, `WorkflowStateStore`)**:
  - `WorkflowDag::build` rejects a step graph with a missing dependency reference (`DataError::MissingDependency`).
  - `WorkflowDag::build` rejects a step graph with a cycle (`DataError::CyclicDependency`).
  - `ready_steps` returns only steps whose all dependencies are in `completed`; returns the root steps when `completed` is empty.
  - `topological_order` is stable across calls on the same DAG (deterministic ordering).
  - `WorkflowState` round-trips through `serde_json::to_string` / `from_str` without loss.
  - `WorkflowState::schema_version()` increments are tested via a checked-in JSON fixture representing the prior version (validates backward-compat parsing).
  - `WorkflowStateStore::save` then `load` round-trip in a `tempfile::TempDir`.
  - `WorkflowStateStore::delete` on a nonexistent workflow is a no-op (not an error).
- **`ContainerRuntime`**:
  - For each `ContainerOption` variant, a focused test asserts the option lands in the resulting `ContainerInstance`'s recorded config.
  - Conflicting options (e.g. `Yolo(true)` + `Auto(true)` if mutually exclusive) produce a structured `EngineError::ConflictingOptions` rather than a panic.
  - `ContainerRuntime::detect` with a `GlobalConfig` that selects Docker returns a runtime whose `runtime_name()` is `"docker"`; same for Apple.
  - The concrete backend type is NOT accessible from the test — only `runtime_name()` and `build(...)` are exercised. No `downcast` or `Any` usage.
- **`ContainerInstance` (via `FakeContainerInstance`)**:
  - `run_with_frontend` drives the recording frontend through the expected lifecycle (open → write_stdout chunks → status updates → exit).
  - PTY resize calls forwarded through `ContainerFrontend::resize_pty`.
- **`ContainerExecution`**:
  - `wait` returns a structured `ContainerExitInfo` that includes exit code, signal (if applicable), and start/end timestamps.
  - `cancel` on an already-finished execution is a no-op (does not panic).
  - `detach` transfers ownership of the handle without joining.
- **`WorkflowEngine`** (against a `FakeContainerExecutionFactory` and `FakeWorkflowFrontend`):
  - `step_once` advances exactly one step and persists state via the injected `WorkflowStateStore` (Layer 0).
  - `run_to_completion` runs every step when the frontend returns `NextAction::LaunchNext`.
  - `pause` then `resume` (with no schema drift) returns to the same step.
  - Resume against a workflow whose persisted hash differs invokes `confirm_resume`; engine respects the return value.
  - Yolo mode invokes `WorkflowFrontend::yolo_countdown_tick` at the configured cadence under a `tokio::time::pause()` clock.
  - Exit-code interpretation: non-zero → `WorkflowStepStatus::Failed`; zero → `Succeeded`; cancelled → `Cancelled`.
  - `ContinueInCurrentContainer` against a two-step workflow where both steps use the same agent calls `inject_prompt` on the factory rather than `execution_for_step`.
  - `ContinueInCurrentContainer` against a two-step workflow where steps use different agents returns `EngineError::InvalidAdvanceAction` and the `AvailableActions` computed for that step has `can_continue_in_current_container: false`.
  - `RestartCurrentStep` re-runs the current step: `StepState` reverts to `Pending`, `execution_for_step` is called again (not `inject_prompt`), and the step re-runs from scratch. The factory's recorded call count increases by one.
  - `CancelToPreviousStep` on a two-step workflow where step 2 just completed: step 2 is marked `Cancelled`, step 1 reverts to `Pending`, and `execution_for_step` is called for step 1 again.
  - `CancelToPreviousStep` on the first step of a workflow: engine returns `EngineError::InvalidAdvanceAction` and `AvailableActions::can_cancel_to_previous_step` is `false` with a non-empty `cancel_to_previous_unavailable_reason`.
  - Per-step agent/model resolution: step-level override supersedes workflow-level default; workflow-level default supersedes `EffectiveConfig` fallback. Cover all three resolution levels with a data-table test.
- **`ReadyEngine`** (against a `FakeReadyFrontend` and `FakeContainerRuntime`):
  - `run_to_completion` with a fresh repo advances through all phases in order and returns a `ReadySummary` with all fields `Done`.
  - `AwaitingDockerfileDecision` → frontend returns `false` (abort) → engine phase transitions to `Failed` without calling any further container methods.
  - `AwaitingLegacyMigrationDecision` → frontend returns `false` → engine continues in legacy mode (does not call migration functions).
  - Each phase is independently reachable via `step` calls; no phase is skipped invisibly.
- **`InitEngine`** (against a `FakeInitFrontend` and `FakeContainerRuntime`):
  - `run_to_completion` with an empty repo advances through all phases and returns an `InitSummary` with all non-skipped fields `Done`.
  - `AwaitingAspecDecision` → frontend returns `false` → `aspec_folder` field in summary is `Skipped`; remaining phases continue.
  - `AwaitingWorkItemsDecision` → frontend returns `None` → `work_items_setup` field in summary is `Skipped`.
  - Each phase independently reachable via `step`.
- **`ClawsEngine`** (against a `FakeClawsFrontend` and `FakeContainerRuntime`):
  - `run_to_completion` with `ClawsMode::Init` and no existing clone advances through all phases in order and returns a `ClawsSummary` with all fields `Done`.
  - `AwaitingCloneDecision` → frontend returns `false` (keep existing clone) → engine skips `CloningRepo` and continues from `CheckingPermissions`; `clone` field in summary is `Skipped`.
  - `AwaitingAuditDecision` → frontend returns `false` → engine skips `RunningAudit` and continues to `Configuring`; `audit` field in summary is `Skipped`.
  - `ClawsMode::Ready` with image already built → `Preflight` skips directly to `LaunchingController`; `clone`, `permissions_check`, `image_build`, `audit`, `configure` fields are all `Skipped`.
  - `ClawsMode::Chat` with controller already running → `Preflight` transitions directly to `Complete` without calling any container methods; no `container_frontend` call is recorded.
  - Each phase is independently reachable via `step`; no phase is silently skipped without a corresponding `Skipped` summary field.
- **`AgentEngine`** (against a `FakeAgentFrontend` and `FakeContainerRuntime`):
  - `ensure_available` when Dockerfile is absent: download step is recorded, then image build step is recorded; `report_step_status` is called for both in order.
  - `ensure_available` when Dockerfile already exists but image is absent: download step is skipped (`report_step_status` not called for it); only image build step is recorded.
  - `ensure_available` when both Dockerfile and image exist: no steps are executed; `container_frontend` is not called.
  - `build_options` for each supported `AgentName`: the returned `Vec<ContainerOption>` contains the expected `Image`, `Entrypoint`, and model/autonomous flag options. Cover with a data-table test for at least two distinct agent names.
  - `build_options` with `AgentRunOptions { yolo: Some(...) }`: the `Yolo` option is present in the returned list.
  - `build_options` with `AgentRunOptions { allowed_tools: vec!["Bash", "Read"] }`: an `AllowedTools` option (or equivalent variant) is present with the correct tool list.
  - `build_options` with `AgentRunOptions { plan: Some(...) }` and `AgentRunOptions { yolo: Some(...) }` simultaneously: returns `EngineError::ConflictingOptions` if they are mutually exclusive per the agent's constraints.
  - `build_options` with `AgentRunOptions { non_interactive: true }`: the returned options contain the agent-specific print/non-interactive flag (e.g. `--print` for Claude Code). Cover for at least two agent types to confirm the per-agent translation is correct.
  - `build_options` with `AgentRunOptions { non_interactive: false }`: the print/non-interactive flag is absent from the returned options.
- **`GitEngine`**:
  - Each method runs against a per-test `tempfile::TempDir` with `git init`. These are *unit tests in form* (one method, one assertion) but use real `git` because git is the system under test.
  - `resolve_root` returns the input dir when the input *is* the root.
  - `create_worktree` then `remove_worktree` is idempotent against the same name.
  - `branch_exists` / `is_detached_head` against synthetic states.
- **`OverlayEngine::build_overlays`**:
  - Dedupes overlapping host paths after canonicalization.
  - `agent_settings_overlays` returns empty when no credentials exist on disk; emits the right overlay set when they do.
  - User-supplied overlay specs are validated and rejected with structured errors when malformed.
- **`AuthEngine`**:
  - `generate_api_key` → `write_api_key_hash` → `read_api_key_hash` → `verify_api_key` round-trip.
  - `verify_api_key` on a missing hash file is constant-time vs. `verify_api_key` on a present hash with a wrong key (use `criterion`'s `black_box` + a relaxed timing assertion, or simply assert that the code path performs a sentinel comparison rather than short-circuits).
  - `ensure_self_signed_tls` writes cert + key with `0o600` on Unix and produces a stable fingerprint on idempotent reruns within the validity window.

### What does NOT belong in this work item

- Real-Docker container startup, image pulls, network calls, or PTY interactions. These are 0070.
- Multi-engine scenarios that combine `WorkflowEngine` + real `ContainerRuntime` + real `GitEngine`. These are 0070.
- Any test in the top-level `tests/` directory. Leave `tests/` alone in this work item; 0070 rebuilds it.
- Parity tests against pre-refactor behavior of any kind.
- TUI, CLI, or headless surface tests — those layers don't exist yet.

### Build & CI

- `cargo build --bin amux` (still from `oldsrc/`) succeeds — the user-facing CLI is unchanged.
- `cargo build --bin amux-next` succeeds — Layer 0 + Layer 1 compile cleanly together.
- `cargo test` passes including the new engine unit tests.


## Codebase Integration:

- Follow `aspec/architecture/2026-grand-architecture.md` as the source of truth.
- Follow established conventions, best practices, testing, and architecture patterns from the project's `aspec/`.
- Do not edit `oldsrc/`. Do not delete `oldsrc/`. Both are in 0070's scope.
- Do not introduce upward calls from Layer 1 to Layer 2/3/4. Use traits owned by Layer 1.
- Do not introduce free `pub fn` for stateful engine concerns. Prefer struct + methods.
- The PR description MUST link to `aspec/architecture/2026-grand-architecture.md` and to this work item, MUST list any developer-clarification questions raised and how they were resolved, and MUST explicitly call out any place a legacy `oldsrc` API was *not* preserved verbatim (with rationale).
- After this work item lands, the next agent picks up `0068-grand-architecture-layer-2-command-and-dispatch.md`.
