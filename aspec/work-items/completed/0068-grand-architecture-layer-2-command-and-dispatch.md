# Work Item: Task

Title: grand architecture refactor — part 3/5 — Layer 2 (Command + Dispatch)
Issue: n/a — third of five work items implementing `aspec/architecture/2026-grand-architecture.md`

## Required reading before starting

This work item is the third of five executing the grand architecture refactor described in `aspec/architecture/2026-grand-architecture.md`. The implementing agent **MUST** read that document, the previous two work items (`0066-…` and `0067-…`), and the current state of `src/data/` and `src/engine/` before writing any code.

The four tenets are restated for emphasis:

1. Layer 2 (command) consumes Layer 0 (data) and Layer 1 (engine) only. It MUST NOT call into Layer 3 (frontend) or Layer 4 (binary). When commands need user input or output, they accept frontend traits *defined by Layer 2* — Layer 3 implements them.
2. Frontends contain no business logic. Every command knob — every flag, every prompt, every dialog selection — flows through Layer 2's `Dispatch` system or through a per-command frontend trait. Frontends do not parse, validate, or interpret command strings; they hand raw input to Dispatch and render whatever Dispatch hands back.
3. Typed objects over `pub fn`. Each amux command becomes a `*Command` struct that implements a `Command` trait and exposes `run_with_frontend(frontend) -> CommandOutcome`. No `pub async fn run(args)` style.
4. **The full list of available commands and flags lives ONLY in `Dispatch`. NEVER in any frontend.** Frontends ask Dispatch for projections (clap definitions, TUI hint strings, headless OpenAPI/JSON schemas). This is the single most important guarantee against mode-drift.

The companion work items are:

- `0066-grand-architecture-foundation-and-layer-0-data.md` (already merged)
- `0067-grand-architecture-layer-1-engines.md` (already merged)
- `0069-grand-architecture-layer-3-frontends-and-binary.md`
- `0070-grand-architecture-finalize-and-remove-oldsrc.md`

## Summary:

- Build `src/command/` with two halves: a `dispatch/` module that holds the canonical command catalogue and per-frontend projections, and a `commands/` module that holds one struct per amux command (`init`, `ready`, `chat`, `exec prompt`, `exec workflow`, `claws`, `status`, `specs amend`, `config`, `headless`, `remote`, `new`, plus subcommands).
- Define a single `CommandCatalogue` data structure that enumerates every command, subcommand, flag, argument, and value type *exactly once*. Every projection (clap commands, TUI hints, headless schema) is generated from this catalogue. Adding a new flag is one edit in one file.
- Define a `Dispatch` type that frontends construct with a frontend-specific trait object (`CliCommandFrontend`, `TuiCommandFrontend`, `HeadlessCommandFrontend`). Dispatch uses the trait to pull flag values and then constructs and returns the appropriate `*Command` struct, instantiated with all engines, configs, and per-command frontend traits it needs.
- Define a `Command` trait: `async fn run_with_frontend(self, frontend: Self::Frontend) -> Result<CommandOutcome, CommandError>`. Each command has its own associated `Frontend` trait describing exactly the user-input methods that command requires.
- Move every command's business logic out of `oldsrc/commands/` (12k+ lines) into the appropriate `*Command::new` constructor + `run_with_frontend` body. No business logic remains anywhere else.
- Comprehensive unit tests for Dispatch projection consistency (clap ↔ TUI hints ↔ headless schema agree on every flag) plus per-command tests using fake engines and fake command frontends.

## User Stories

### User Story 1:
As a: future implementing agent picking up Layer 3

I want to:
construct a CLI, TUI, or headless frontend by handing Dispatch a frontend trait and rendering whatever it returns

So I can:
build a frontend in hundreds of lines instead of thousands, with zero risk of accidentally diverging from the canonical command list.

### User Story 2:
As a: maintainer adding a new flag to `amux exec workflow`

I want to:
edit the command catalogue once and have the flag appear in CLI help, TUI hints, headless API schema, and the `*Command::new` signature simultaneously

So I can:
trust that mode parity is maintained by construction.

### User Story 3:
As a: maintainer reading `src/command/commands/exec_workflow.rs`

I want to:
see the entire `amux exec workflow` business logic — flag interpretation, agent/model resolution, container option assembly, workflow construction, exit-code reporting — in one place, with all I/O routed through frontend traits

So I can:
fix bugs without sifting through CLI, TUI, and headless code paths.

## Implementation Details:

### 0. Required reading and ground rules

- Read `aspec/architecture/2026-grand-architecture.md` end-to-end.
- Read `aspec/uxui/cli.md` to understand the canonical CLI command surface that must be preserved (no changes to user-visible CLI behavior in this work item).
- Read `0066-…` and `0067-…` and the current state of `src/data/` and `src/engine/`.
- For reference only: `oldsrc/cli.rs` (the legacy clap definitions, 2496 lines) and `oldsrc/commands/*.rs` (12k+ lines of business logic). Use these to understand existing behavior; **do not** port them verbatim — restructure into `*Command` types.
- When uncertain, ASK THE DEVELOPER.

### 1. `src/command/dispatch/` — the canonical catalogue and projections

#### 1a. `CommandCatalogue` (`src/command/dispatch/catalogue.rs`)

`CommandCatalogue` is a single static (or `OnceLock`-built) data structure listing every command. Each entry contains:

```rust
pub struct CommandSpec {
    pub name: &'static str,                     // "exec"
    pub aliases: &'static [&'static str],
    pub help: &'static str,                     // shown in clap, in TUI hint, in OpenAPI desc
    pub long_help: Option<&'static str>,
    pub arguments: &'static [ArgumentSpec],
    pub flags: &'static [FlagSpec],
    pub subcommands: &'static [&'static CommandSpec],
}

pub struct FlagSpec {
    pub long: &'static str,                     // "yolo"
    pub short: Option<char>,                    // None for --yolo
    pub help: &'static str,
    pub kind: FlagKind,                         // Bool, String, OptionalString, Enum(&'static [&'static str]), VecString, Path, etc.
    pub default: FlagDefault,
    pub frontends: FrontendVisibility,          // CLI-only? TUI-only? all three?
}

pub struct ArgumentSpec { /* analogous */ }
```

`CommandCatalogue` exposes:

```rust
impl CommandCatalogue {
    pub fn get() -> &'static CommandCatalogue;
    pub fn root() -> &'static CommandSpec;
    pub fn lookup(path: &[&str]) -> Option<&'static CommandSpec>;  // ["exec", "prompt"]
}
```

The catalogue MUST enumerate every command currently defined in `oldsrc/cli.rs`:

- `init`, `ready`, `chat`, `exec prompt`, `exec workflow`, `claws *`, `status`, `specs amend`, `config *`, `headless *`, `remote *`, `new *`.

If the catalogue and `oldsrc/cli.rs` ever disagree on an existing command's name, alias, flag, or default, the catalogue is wrong and must be fixed in this work item — there is to be zero user-visible drift.

#### 1b. Projections (`src/command/dispatch/projections/`)

```rust
// src/command/dispatch/projections/clap.rs
impl CommandCatalogue {
    pub fn build_clap_command(&self) -> clap::Command;
}

// src/command/dispatch/projections/tui_hints.rs
impl CommandCatalogue {
    pub fn tui_hint_for(&self, path: &[&str]) -> Option<TuiHint>;     // hint shown above the TUI command box
    pub fn tui_completions(&self, partial: &str) -> Vec<TuiCompletion>;
}

// src/command/dispatch/projections/headless_schema.rs
impl CommandCatalogue {
    pub fn openapi_schema(&self) -> serde_json::Value;
    pub fn rest_route_table(&self) -> Vec<RestRoute>;
}
```

Frontends call only these projection methods; they MUST NEVER hard-code a command name, flag name, or default value. A unit test enforces this — see Test Considerations.

#### 1c. `Dispatch` (`src/command/dispatch/mod.rs`)

```rust
pub struct Dispatch<F: CommandFrontend> {
    catalogue: &'static CommandCatalogue,
    frontend: F,
    session: Arc<RwLock<Session>>,
    runtime: Arc<ContainerRuntime>,
    git_engine: Arc<GitEngine>,
    overlay_engine: Arc<OverlayEngine>,
    auth_engine: Arc<AuthEngine>,
    agent_engine: Arc<AgentEngine>,
    workflow_state_store: Arc<WorkflowStateStore>,
}

// NOTE: ReadyEngine, InitEngine, and ClawsEngine are NOT pre-constructed on Dispatch.
// Their constructors accept per-invocation options (flags, mode) that only exist at
// call time. Each command constructs a fresh engine instance from the component
// references above (git_engine, overlay_engine, runtime, agent_engine) plus the
// flag values resolved from the CommandFrontend.


impl<F: CommandFrontend> Dispatch<F> {
    pub fn new(
        frontend: F,
        session: Arc<RwLock<Session>>,
        engines: Engines,
    ) -> Self;

    pub async fn run_command(self, path: &[&str]) -> Result<CommandOutcome, CommandError>;
    pub fn build_command(self, path: &[&str]) -> Result<BuiltCommand, CommandError>;
}
```

`CommandFrontend` is the catch-all trait that frontends implement to *supply* flag values to Dispatch. It also extends `UserMessageSink` so that Layer 2 commands can write status messages through the same frontend object without needing a separate argument:

```rust
pub trait CommandFrontend: UserMessageSink + Send + Sync {
    fn flag_bool(&self, command_path: &[&str], flag: &str) -> Result<Option<bool>, CommandError>;
    fn flag_string(&self, command_path: &[&str], flag: &str) -> Result<Option<String>, CommandError>;
    fn flag_strings(&self, command_path: &[&str], flag: &str) -> Result<Vec<String>, CommandError>;
    fn flag_path(&self, command_path: &[&str], flag: &str) -> Result<Option<PathBuf>, CommandError>;
    fn flag_enum(&self, command_path: &[&str], flag: &str) -> Result<Option<String>, CommandError>;
    fn argument(&self, command_path: &[&str], name: &str) -> Result<Option<String>, CommandError>;
    // …complete the surface so every FlagKind has a corresponding method
}
```

Three concrete `CommandFrontend` implementations live in Layer 3 (built in 0069):

- `CliCommandFrontend` — wraps `clap::ArgMatches`.
- `TuiCommandFrontend` — wraps the parsed TUI command-box input.
- `HeadlessCommandFrontend` — wraps an HTTP request body + query parameters.

Dispatch validates flag types and required-vs-optional based on the catalogue and surfaces structured errors back to the frontend (`CommandError::MissingRequiredFlag`, etc.). **Validation lives only here**; Layer 3 never validates user input.

Dispatch also exposes a `parse_command_box_input(raw: &str) -> Result<ParsedCommandBoxInput, CommandError>` helper used by the TUI's command-box widget. The TUI submits the raw user string; Dispatch tokenizes it against the catalogue, returns a typed `ParsedCommandBoxInput { path, flags, arguments }`, and the TUI feeds that back through a `TuiCommandFrontend` to invoke `Dispatch::run_command`. **All command-string interpretation lives here**, never in the TUI.

`Dispatch::run_command(["exec", "workflow"])` looks up the spec, asks the frontend for every flag, instantiates `ExecWorkflowCommand::new(...)`, and calls its `run_with_frontend`. The per-command frontend trait (e.g. `ExecWorkflowCommandFrontend`) is *requested from* the outer `CommandFrontend` via a method like:

```rust
pub trait CommandFrontend: Send + Sync {
    // ...flag methods...
    fn exec_workflow_frontend(&self) -> Box<dyn ExecWorkflowCommandFrontend>;
    fn ready_frontend(&self) -> Box<dyn ReadyCommandFrontend>;
    fn chat_frontend(&self) -> Box<dyn ChatCommandFrontend>;
    // …one per command that needs a per-command frontend
}
```

ASK THE DEVELOPER if you find a cleaner pattern (e.g. associated types, trait objects keyed by `TypeId`); the grand architecture document calls out the trait-per-command pattern explicitly so default to that.

### 2. `src/command/commands/` — one struct per command

For each command in the catalogue, create a module under `src/command/commands/` containing:

- The `*Command` struct, owning every flag value, every engine reference, and every Layer 0 type it needs.
- The `*CommandFrontend` trait, listing exactly the user-input methods that command needs.
- The `impl Command for *Command` block with `run_with_frontend(frontend) -> CommandOutcome`.
- Unit tests against fake engines and a fake frontend.

#### 2a. `src/command/commands/worktree_lifecycle.rs` — shared pre/post worktree helper

**Architectural ruling**: all worktree lifecycle logic (pre-creation checks, post-completion merge/discard/keep) is a **command-layer concern**, not a `WorkflowEngine` concern. `WorkflowEngine` is handed a working directory and runs steps in it; it does not know whether that directory is a git worktree or the main checkout. This helper is used by `ExecWorkflowCommand`.

##### Decision types

```rust
// src/command/commands/worktree_lifecycle.rs

/// Result of the pre-creation uncommitted-files dialog.
pub enum PreWorktreeDecision {
    /// Commit all currently uncommitted files with this message, then create the worktree.
    Commit { message: String },
    /// Proceed using the last commit; uncommitted files will NOT be in the worktree.
    UseLastCommit,
    /// Abort the command entirely.
    Abort,
}

/// Result of the "worktree already exists" dialog.
pub enum ExistingWorktreeDecision {
    /// Reuse the existing worktree as-is (resume).
    Resume,
    /// Delete and recreate the worktree from HEAD.
    Recreate,
}

/// Result of the post-workflow merge-or-discard prompt.
pub enum PostWorkflowWorktreeAction {
    /// Merge the worktree branch into the current branch (squash-merge).
    Merge,
    /// Delete the worktree and branch, discarding all changes.
    Discard,
    /// Leave the worktree and branch in place without merging.
    Keep,
}
```

##### `WorktreeLifecycleFrontend` trait (defined by Layer 2, implemented by Layer 3)

```rust
pub trait WorktreeLifecycleFrontend: UserMessageSink + Send + Sync {
    // ─── Pre-creation ───────────────────────────────────────────────────────

    /// The main branch has uncommitted files that will NOT be in the new worktree.
    /// `files` is a list of `git status --porcelain` lines. Return the user's decision.
    fn ask_pre_worktree_uncommitted_files(
        &mut self,
        files: &[String],
    ) -> Result<PreWorktreeDecision, CommandError>;

    /// A worktree already exists at `path` on `branch`.
    /// Return whether to resume it or recreate it from HEAD.
    fn ask_existing_worktree(
        &mut self,
        path: &Path,
        branch: &str,
    ) -> Result<ExistingWorktreeDecision, CommandError>;

    /// Report that the worktree has been created (or reused) at `path` on `branch`.
    fn report_worktree_created(&mut self, path: &Path, branch: &str);

    // ─── Post-completion ─────────────────────────────────────────────────────

    /// The command completed (with or without error). The worktree branch is ready.
    /// `had_error` is true when the container or workflow exited non-zero.
    /// Return what to do with the worktree.
    fn ask_post_workflow_action(
        &mut self,
        branch: &str,
        had_error: bool,
    ) -> Result<PostWorkflowWorktreeAction, CommandError>;

    /// The worktree branch has uncommitted files that must be committed before the
    /// merge can proceed cleanly. Return a commit message to commit them, or None
    /// to skip the commit (and proceed to merge with those files uncommitted, which
    /// may fail — that is the user's choice).
    fn ask_worktree_commit_before_merge(
        &mut self,
        branch: &str,
        files: &[String],
    ) -> Result<Option<String>, CommandError>;

    /// Confirm squash-merge of `branch` into the current HEAD.
    fn confirm_squash_merge(&mut self, branch: &str) -> Result<bool, CommandError>;

    /// After a successful merge: confirm deletion of the worktree directory and branch.
    fn confirm_worktree_cleanup(&mut self, branch: &str, path: &Path) -> Result<bool, CommandError>;

    /// Report that a merge conflict prevented automatic merging. Instructs the user
    /// how to resolve manually.
    fn report_merge_conflict(
        &mut self,
        branch: &str,
        worktree_path: &Path,
        git_root: &Path,
    );

    /// Report that the worktree was discarded (branch and directory deleted).
    fn report_worktree_discarded(&mut self, branch: &str);

    /// Report that the worktree was kept in place (branch and directory preserved).
    fn report_worktree_kept(&mut self, path: &Path, branch: &str);
}
```

##### `WorktreeLifecycle` struct

```rust
pub struct WorktreeLifecycle {
    git_engine: Arc<GitEngine>,
    git_root: PathBuf,
    worktree_path: PathBuf,
    branch: String,
}

impl WorktreeLifecycle {
    /// Build lifecycle for a named workflow (branch name: `amux/<workflow-slug>`).
    pub fn for_workflow(
        git_engine: Arc<GitEngine>,
        git_root: PathBuf,
        workflow_name: &str,
    ) -> Self;

    pub fn worktree_path(&self) -> &Path;
    pub fn branch(&self) -> &str;

    /// Run pre-creation checks and create (or reuse) the worktree.
    ///
    /// Steps:
    /// 1. If a worktree already exists at `worktree_path`:
    ///    call `frontend.ask_existing_worktree` → Recreate removes it; Resume skips creation.
    /// 2. If no worktree exists: check for uncommitted files on the main branch.
    ///    If files exist: call `frontend.ask_pre_worktree_uncommitted_files`.
    ///    - Commit { message } → `git_engine.commit_all(git_root, message)`.
    ///    - UseLastCommit → proceed.
    ///    - Abort → return `CommandError::Aborted`.
    /// 3. `git_engine.create_worktree(git_root, worktree_path, branch)`.
    /// 4. Call `frontend.report_worktree_created`.
    /// 5. Return the worktree path (= the mount path for the container/workflow).
    pub async fn prepare(
        &self,
        frontend: &mut dyn WorktreeLifecycleFrontend,
    ) -> Result<PathBuf, CommandError>;

    /// Run post-completion flow.
    ///
    /// Steps:
    /// 1. Call `frontend.ask_post_workflow_action(branch, had_error)`.
    /// 2. PostWorkflowWorktreeAction::Merge:
    ///    a. Check uncommitted files in the worktree.
    ///    b. If files exist: call `frontend.ask_worktree_commit_before_merge`.
    ///       If Some(msg): `git_engine.commit_all(worktree_path, msg)`.
    ///    c. Call `frontend.confirm_squash_merge(branch)`.
    ///       If false: skip the merge, fall through to Keep.
    ///    d. `git_engine.merge_branch(git_root, branch)`.
    ///       On success: call `frontend.confirm_worktree_cleanup(branch, worktree_path)`.
    ///         If true: `git_engine.remove_worktree` + `git_engine.delete_branch`.
    ///         If false: frontend.report_worktree_kept.
    ///       On conflict: call `frontend.report_merge_conflict`.
    /// 3. PostWorkflowWorktreeAction::Discard:
    ///    `git_engine.remove_worktree` + `git_engine.delete_branch`.
    ///    Call `frontend.report_worktree_discarded`.
    /// 4. PostWorkflowWorktreeAction::Keep:
    ///    Call `frontend.report_worktree_kept`.
    pub async fn finalize(
        &self,
        frontend: &mut dyn WorktreeLifecycleFrontend,
        had_error: bool,
    ) -> Result<(), CommandError>;
}
```

This module is **not** exported from `src/command/mod.rs` — it is `pub(super)` within `src/command/commands/` and referenced only by `ExecWorkflowCommand`.

#### 2b. `src/command/remote_client.rs` — `RemoteClient`

`oldsrc/commands/remote.rs` (1183 lines) embeds ~300 lines of HTTP infrastructure — client construction, request building, error mapping, API key resolution, and SSE stream handling — directly in the command file. In the new architecture this infrastructure becomes a typed Layer 2 helper, not a full engine (it has no state machine or frontend trait). `RemoteCommand` constructs one per invocation; no other command uses it.

```rust
// src/command/remote_client.rs

/// Typed HTTP client for communicating with a remote amux headless server.
/// Constructed fresh per `RemoteCommand` invocation from CLI/TUI/headless flags.
pub struct RemoteClient {
    base_url: Url,
    http: reqwest::Client,
}

impl RemoteClient {
    /// Construct from a base URL and an API key.
    /// The key is sent as `Authorization: Bearer <key>` on every request.
    pub fn new(base_url: Url, api_key: &ApiKey) -> Result<Self, CommandError>;

    /// Resolve the API key to use: `explicit` argument (from `--api-key` flag)
    /// > `AMUX_API_KEY` env var > `~/.amux/api-key` file.
    pub fn resolve_api_key(
        session: &Session,
        explicit: Option<&str>,
    ) -> Result<ApiKey, CommandError>;

    /// Send a command to the remote server and collect the full JSON response.
    pub async fn send_command(
        &self,
        path: &[&str],
        flags: &[(&str, serde_json::Value)],
    ) -> Result<RemoteResponse, CommandError>;

    /// Send a command and stream SSE events to `sink` until the server closes the stream.
    pub async fn stream_command(
        &self,
        path: &[&str],
        flags: &[(&str, serde_json::Value)],
        sink: &mut dyn RemoteEventSink,
    ) -> Result<(), CommandError>;

    fn map_reqwest_error(e: reqwest::Error) -> CommandError;
}

/// Sink for SSE events streamed from a remote amux server.
/// Defined by Layer 2; implemented by Layer 3 (CLI, TUI, headless).
pub trait RemoteEventSink: Send + Sync {
    fn on_event(&mut self, event_type: &str, data: &str);
    fn on_done(&mut self);
}
```

`RemoteClient` is `pub(super)` within `src/command/commands/` — not re-exported from `src/command/mod.rs` and not visible to Layer 3 except through `RemoteCommand`'s frontend trait. All HTTP error variants (timeout, TLS failure, non-2xx status, malformed SSE) map to specific `CommandError` variants via `map_reqwest_error`.

#### 2c. Example skeletons

`src/command/commands/exec_workflow.rs`:

```rust
pub struct ExecWorkflowCommand {
    workflow_name: String,
    flags: ExecWorkflowFlags,
    session: Arc<RwLock<Session>>,
    runtime: Arc<ContainerRuntime>,
    git: Arc<GitEngine>,
    overlay: Arc<OverlayEngine>,
    agent: Arc<AgentEngine>,
    workflow_store: Arc<WorkflowStateStore>,
    workflow: Option<Workflow>,    // resolved from --workflow flag
}

pub trait ExecWorkflowCommandFrontend:
    ContainerFrontend
    + WorkflowFrontend
    + WorktreeLifecycleFrontend
    + Send
{
    fn report_workflow_summary(&mut self, summary: &WorkflowSummary);
    // ...anything not already covered by the supertrait bounds
}

impl Command for ExecWorkflowCommand {
    type Frontend = Box<dyn ExecWorkflowCommandFrontend>;
    type Outcome = ExecWorkflowOutcome;

    async fn run_with_frontend(self, frontend: Self::Frontend) -> Result<Self::Outcome, CommandError> {
        // 1. Resolve agent + model (via Layer 0 EffectiveConfig + Layer 1 OverlayEngine).
        // 2. Call agent.ensure_available(agent, config, frontend).await?
        // 3. Call agent.build_options(agent, model, run_options, session) → Vec<ContainerOption>.
        // 4. If --worktree (or implied by --yolo/--auto):
        //    a. Construct WorktreeLifecycle::for_workflow(git, git_root, workflow_name).
        //    b. lifecycle.prepare(frontend).await? → mount_path (worktree directory).
        // 5. Construct WorkflowEngine with mount_path and run it.
        // 6. If worktree was used: lifecycle.finalize(frontend, had_error).await?.
        // 7. Wrap the exit info in ExecWorkflowOutcome and return.
    }
}
```

`src/command/commands/ready.rs`, `chat.rs`, `init.rs`, `exec_prompt.rs`, `exec_workflow.rs`, `claws.rs`, `status.rs`, `specs_amend.rs`, `config.rs`, `headless_*.rs`, `remote.rs`, `new_workflow.rs`, `new_skill.rs`, `parity.rs`, `download.rs`, `output.rs`, `auth.rs` — every command currently in `oldsrc/commands/` (except `agent.rs`, which becomes `engine/agent/`) becomes one of these structs.

`ReadyCommand`, `InitCommand`, and `ClawsCommand` are intentionally thin: their `run_with_frontend` bodies construct the corresponding engine from the pre-wired engine references on `Dispatch`, then call `engine.run_to_completion(frontend)`. All multi-phase logic lives in the engine (Layer 1); the command struct owns only the flag values and engine references. The command frontend traits MUST satisfy the engine-level frontend traits as supertrait bounds:

```rust
pub trait ReadyCommandFrontend: ReadyFrontend + Send { /* no additional methods needed */ }
pub trait InitCommandFrontend: InitFrontend + Send { /* no additional methods needed */ }
pub trait ClawsCommandFrontend: ClawsFrontend + Send { /* no additional methods needed */ }
```

If a command-layer concern genuinely cannot be expressed through the engine frontend traits (e.g. a Layer 2 lifecycle event before or after the engine runs), add a dedicated method to the command frontend trait — but ASK THE DEVELOPER before adding such methods, since the default answer is that the engine trait is sufficient.

#### What moves into `*Command::run_with_frontend`

- All flag interpretation, all option construction, all engine invocation, all output assembly.
- Any prompts to the user — moved to per-command frontend trait methods.
- Any reporting of progress — moved to frontend trait methods like `report_*`.
- Any exit-code interpretation — turned into typed `*Outcome` values.

#### What is forbidden

- No `eprintln!`, no `println!`, no direct user-facing I/O. All status messages go through `UserMessageSink::write_message` on the frontend; all structured output goes through per-command `report_*` frontend trait methods.
- No `clap::ArgMatches` references inside `*Command` bodies. Flag values arrive as typed fields populated by Dispatch.
- No `crossterm`, no `ratatui`, no `axum`. Those are Layer 3.
- No "if this is the CLI vs TUI vs headless" checks. The command never knows which frontend is on the other side.
- No worktree lifecycle logic outside `WorktreeLifecycle`. Commands MUST NOT call `git_engine.create_worktree`, `git_engine.merge_branch`, or `git_engine.remove_worktree` directly; all git worktree operations flow through `WorktreeLifecycle::prepare` and `WorktreeLifecycle::finalize`.

### 3. Errors

`src/command/error.rs` defines `CommandError` covering every failure mode in Layer 2. It wraps `EngineError` and `DataError` from below. Layer 3 wraps `CommandError` in its own user-facing presentation; Layer 2 does not depend on Layer 3 errors.

### 4. Migration of legacy command modules

Every file under `oldsrc/commands/` has a Layer 2 destination:

| oldsrc                           | Layer 2 destination                              |
|----------------------------------|--------------------------------------------------|
| `commands/agent.rs`              | `engine/agent/` (`AgentEngine`) — absorbed entirely into Layer 1; not a user-facing command |
| `commands/auth.rs`               | `command/commands/auth.rs` if it is a user command, else absorbed into `engine/auth/` |
| `commands/chat.rs`               | `command/commands/chat.rs`                       |
| `commands/claws.rs`              | `command/commands/claws.rs` (thin wrapper over `ClawsEngine`) |
| `commands/config.rs`             | `command/commands/config.rs`                     |
| `commands/download.rs`           | `command/commands/download.rs`                   |
| `commands/exec.rs`               | `command/commands/exec_prompt.rs` + `exec_workflow.rs` |
| `commands/headless/*`            | `command/commands/headless/*` (start/stop/status/etc) |
| `commands/implement.rs`          | `command/commands/implement.rs` (thin wrapper over `ExecWorkflowCommand` + Layer 1 engines — **MUST REMAIN A TOP-LEVEL COMMAND**, see §6 below) |
| `commands/init.rs` + `init_flow.rs` | `command/commands/init.rs` (thin wrapper over `InitEngine`) |
| `commands/new.rs` + `new_cmd.rs` + `new_workflow.rs` + `new_skill.rs` | `command/commands/new/*` |
| `commands/output.rs`             | Layer 3 helper: presentation only. Move to `src/frontend/cli/output.rs` (CLI color/TTY decisions) and a `src/frontend/headless/output.rs` (JSON serialization). NOT a command. |
| `commands/parity.rs`             | DROPPED. The legacy `parity.rs` is a compile-time assertion mechanism over the `CommandId` enum — its purpose is replaced by `CommandCatalogue` consistency tests (see §1b/§1c projection tests). Confirm with developer; no migration needed. |
| `commands/ready.rs` + `ready_flow.rs` | `command/commands/ready.rs` (thin wrapper over `ReadyEngine`) |
| `commands/remote.rs`             | `command/commands/remote.rs`                     |
| `commands/spec.rs` + `specs.rs`  | `command/commands/specs/amend.rs` AND `command/commands/specs/new.rs`. `specs new` MUST be preserved as an alias for `new spec` (see §6.1 below) — do NOT drop the `specs new` invocation form. |
| `commands/status.rs`             | `command/commands/status.rs`                     |
| `commands/auth.rs`               | Split: keychain credential resolution moves to `engine/auth/` (`AuthEngine::agent_keychain_credentials` and `AuthEngine::resolve_agent_auth`, per WI 0067 §9a.3). The `auto_agent_auth_accepted` per-repo consent flag is read/written by `command/commands/auth.rs` (a small user-facing accept/decline command), or by Layer 2 commands that launch agents. Confirm split with developer. |
| `commands/download.rs`           | INTERNAL helper consumed by `engine/agent/`. Move to `src/engine/agent/dockerfile_downloads.rs`. NOT a user-facing command. The user-visible `amux download` command (if any survives) is documented as a thin wrapper; ASK THE DEVELOPER whether to retain a top-level `amux download` command at all. |

Anything in this table that is "actually a helper, not a command" should be flagged with the developer and moved into Layer 1 instead.

### 5. What must NOT happen in this work item

- No changes to `oldsrc/`. The user-visible binary still ships from `oldsrc/`.
- No work in `src/frontend/` beyond ensuring it compiles. The CLI/TUI/headless rebuild is 0069.
- No `pub fn run(args)` style command entry points. Every command is a struct + trait impl.
- No frontend-specific code in `src/command/`. Dispatch projects to clap/TUI/headless via methods on `CommandCatalogue`; it does not host frontend logic.
- No swap of the binary entrypoint. `amux` still runs from `oldsrc/`.
- **No silent removal of any user-facing command, flag, or alias** that exists in `oldsrc/cli.rs`. Any legacy CLI surface that is dropped MUST be explicitly listed in the PR description with an explanation. Default: preserve.

### 6. Command parity addenda — legacy CLI surface that MUST be preserved

The catalogue (§1a) is the single source of truth post-refactor, but the catalogue MUST cover every command and flag currently in `oldsrc/cli.rs`. Below is the explicit list, captured from the legacy `Command`, `NewAction`, `WorkflowFormat`, `ConfigAction`, `SpecsAction`, `ExecAction`, `HeadlessAction`, `RemoteAction`, `RemoteSessionAction`, and `ClawsAction` enums. Names below match the legacy clap names exactly.

#### 6.1 Top-level commands

`init`, `ready`, `implement`, `chat`, `specs`, `claws`, `status`, `config`, `exec`, `headless`, `remote`, `new`.

**`implement` is preserved as a top-level command.** Despite the introduction of `exec workflow`, the legacy `amux implement WORK_ITEM` invocation form is the most-used user surface today. `ImplementCommand` MUST exist in `src/command/commands/implement.rs` and implement the same flag set as `oldsrc/cli.rs::Command::Implement`. Internally, `ImplementCommand::run_with_frontend` SHOULD delegate to `ExecWorkflowCommand` (constructing a synthetic single-step workflow when `--workflow` is absent), but the CLI surface is preserved.

**`specs new` is preserved as an alias** for `new spec`. Both forms MUST work and MUST produce identical behavior. Encode this in the catalogue as a `aliases: &[&["specs", "new"]]` entry on the `["new", "spec"]` `CommandSpec`

#### 6.2 Per-command flag tables

For each command below, the catalogue MUST contain every listed flag with the listed name, kind, and default. Cover with a data-table test (`catalogue_legacy_parity_NNNN`).

**`init`**: `--agent <claude|codex|opencode|maki|gemini|copilot|crush|cline>` (default `claude`), `--aspec` (bool, default false).

**`ready`**: `--refresh`, `--build`, `--no-cache`, `-n/--non-interactive`, `--allow-docker`, `--json`. The `--json` flag implies `--non-interactive` (Dispatch enforces this in `build_command` after reading flags, before constructing `ReadyCommand`). Document the implication in `FlagSpec::implies` so projections render the implication rule.

**`implement`** (positional `<WORK_ITEM>` required): `-n/--non-interactive`, `--plan`, `--allow-docker`, `--workflow <PATH>`, `--worktree`, `--mount-ssh`, `--yolo`, `--auto`, `--agent <NAME>`, `--model <NAME>`, `--overlay <SPEC>` (repeatable). The implication rules are: `--yolo` or `--auto` combined with `--workflow` implies `--worktree`. Without `--workflow`, `--yolo`/`--auto` do NOT imply `--worktree`. Cover both implication branches with catalogue + dispatch unit tests.

**`chat`**: `-n/--non-interactive`, `--plan`, `--allow-docker`, `--mount-ssh`, `--yolo`, `--auto`, `--agent <NAME>`, `--model <NAME>`, `--overlay <SPEC>` (repeatable).

**`specs new`**: `--interview`. **`specs amend` (positional `<WORK_ITEM>` required)**: `-n/--non-interactive`, `--allow-docker`.

**`claws init`**: no flags. **`claws ready`**: no flags. **`claws chat`**: no flags.

**`status`**: `--watch` (continuous refresh every 3s).

**`config show`**: no flags. **`config get` (positional `<FIELD>` required)**: no flags. **`config set` (positional `<FIELD> <VALUE>` required)**: `--global` (write to global config; default scope is repo).

**`exec prompt` (positional `<PROMPT>` required, non-empty validated)**: `-n/--non-interactive`, `--plan`, `--allow-docker`, `--mount-ssh`, `--yolo`, `--auto`, `--agent <NAME>`, `--model <NAME>`, `--overlay <SPEC>` (repeatable).

**`exec workflow` (alias `wf`) (positional `<WORKFLOW>` PATH required)**: `--work-item <NUM>` (optional), `-n/--non-interactive`, `--plan`, `--allow-docker`, `--worktree`, `--mount-ssh`, `--yolo`, `--auto`, `--agent <NAME>`, `--model <NAME>`, `--overlay <SPEC>` (repeatable). Implication: `--yolo` or `--auto` implies `--worktree` (already documented in §561).

**`headless start`**: `--port <u16>` (default `9876`), `--workdirs <DIR>` (repeatable), `--background`, `--refresh-key`, `--dangerously-skip-auth`. **`headless kill`**, **`headless logs`**, **`headless status`**: no flags.

**`remote run` (positional `<COMMAND>...` required, with `trailing_var_arg = true, allow_hyphen_values = true`)**: `--remote-addr <ADDR>`, `--session <ID>`, `-f/--follow`, `--api-key <KEY>`. **`remote session start` (positional `<DIR>` optional)**: `--remote-addr <ADDR>`, `--api-key <KEY>`. **`remote session kill` (positional `<SESSION_ID>` optional)**: `--remote-addr <ADDR>`, `--api-key <KEY>`.

**`new spec`**: `--interview`. **`new workflow`**: `--interview`, `--global`, `--format <toml|yaml|md>` (default `toml`). **`new skill`**: `--interview`, `--global`.

The `FlagKind` for each flag MUST be expressed using the typed catalogue variants — `Bool`, `String`, `OptionalString`, `Path`, `OptionalPath`, `Enum(&[…])`, `VecString`, `U16`. Repeatable flags (`--workdirs`, `--overlay`) are `VecString`. Trailing-var-arg flags (`remote run`'s `<COMMAND>...`) get a dedicated `ArgumentSpec::TrailingVarArgs` kind that the clap projection translates faithfully (with `trailing_var_arg(true).allow_hyphen_values(true)`).

#### 6.3 Cross-command frontend traits

The legacy code has Q&A flows that span multiple commands. Each is a separate Layer 2 frontend trait, following the same trait-per-concern pattern as `WorktreeLifecycleFrontend`:

##### 6.3a `MountScopeFrontend`

When `cwd != git_root`, every command that mounts a host directory into a container (`implement`, `chat`, `exec prompt`, `exec workflow`, `specs amend`, `claws *`, `init`, `ready` with audit) MUST prompt the user to choose between mounting the entire git root or just the current directory. The legacy implementation lives in `oldsrc/commands/implement.rs::confirm_mount_scope_stdin` and is called from `chat`, `implement`, and `exec`.

```rust
// src/command/commands/mount_scope.rs

pub enum MountScopeDecision {
    MountGitRoot,
    MountCurrentDirOnly,
    Abort,
}

pub trait MountScopeFrontend: UserMessageSink + Send + Sync {
    /// Prompt the user when cwd is below git_root. Show the two paths so the user can compare.
    /// Default behaviors per frontend:
    ///   - CLI: stdin prompt with `[r]oot / [c]urrent dir / [a]bort`.
    ///   - TUI: `MountScope` modal dialog (legacy keybindings preserved).
    ///   - Headless: never prompts; returns `MountGitRoot` by default unless the request body
    ///     specifies `mount_scope: "cwd"`.
    fn ask_mount_scope(
        &mut self,
        git_root: &Path,
        cwd: &Path,
    ) -> Result<MountScopeDecision, CommandError>;
}

pub struct MountScope;

impl MountScope {
    /// Resolve the effective mount path given cwd and git_root. Calls the frontend only when
    /// cwd != git_root; otherwise returns git_root unconditionally.
    pub fn resolve(
        cwd: &Path,
        git_root: &Path,
        frontend: &mut dyn MountScopeFrontend,
    ) -> Result<PathBuf, CommandError>;
}
```

Every command that mounts a host directory MUST call `MountScope::resolve` before constructing its `ContainerOption::WorkingDir` or equivalent. Cover with a per-command unit test that asserts `ask_mount_scope` fires only when paths differ.

The corresponding command frontend traits MUST add `MountScopeFrontend` as a supertrait bound: e.g. `trait ChatCommandFrontend: ContainerFrontend + MountScopeFrontend + Send`.

##### 6.3b `AgentSetupFrontend`

When `AgentEngine::ensure_available` would download a Dockerfile or build an image (i.e. agent is not yet available), the legacy TUI raises an `AgentSetupConfirm` dialog asking whether to set up the requested agent, fall back to the default agent, or abort. This is a Layer 2 lifecycle decision, NOT a Layer 1 engine concern — `AgentEngine` reports state, but the choice belongs to the command.

```rust
// src/command/commands/agent_setup.rs

pub enum AgentSetupDecision {
    /// Proceed with downloading the Dockerfile and building the image for the requested agent.
    Setup,
    /// Fall back to the configured default agent (which is already available).
    /// Only offered when the requested agent is not the default and the default is available.
    FallbackToDefault,
    /// Abort the command.
    Abort,
}

pub trait AgentSetupFrontend: UserMessageSink + Send + Sync {
    /// Called by Layer 2 BEFORE invoking `AgentEngine::ensure_available` when the agent is not
    /// already available. The frontend offers Setup / FallbackToDefault / Abort.
    /// `default_available` is true when fallback is a viable option.
    fn ask_agent_setup(
        &mut self,
        requested: &AgentName,
        default: &AgentName,
        default_available: bool,
        image_only: bool,  // true = Dockerfile is already present, only image build is needed
    ) -> Result<AgentSetupDecision, CommandError>;

    /// Called when the user previously chose `FallbackToDefault` for this workflow on this
    /// agent — Layer 2 caches the decision so subsequent steps in the same workflow do NOT
    /// re-prompt. The frontend MAY persist this choice across the command's lifetime.
    fn record_fallback(&mut self, requested: &AgentName, fallback: &AgentName);
}
```

Per-step / per-tab caching of fallback decisions (legacy `workflow_agent_fallbacks: HashMap<step_name, fallback_agent>`) lives in the `ExecWorkflowCommand` / `ImplementCommand` body, NOT in the engine. The command consults its own cache before calling `ask_agent_setup`.

Add `AgentSetupFrontend` as a supertrait bound on every command frontend trait that may launch an agent (every command except `headless *`, `remote *`, `config *`, `status`).

##### 6.3c `AgentAuthFrontend` — first-run keychain consent

The `auto_agent_auth_accepted: Option<bool>` repo-config flag governs whether amux silently injects keychain credentials into agent containers. On first run (flag is `None`), Layer 2 MUST prompt the user.

```rust
// src/command/commands/agent_auth.rs

pub enum AgentAuthDecision {
    /// Accept: inject keychain credentials and persist `auto_agent_auth_accepted: Some(true)`.
    Accept,
    /// Decline: do NOT inject credentials this run; persist `auto_agent_auth_accepted: Some(false)`.
    Decline,
    /// Decline once: do NOT inject credentials this run; do NOT persist (re-prompt next time).
    DeclineOnce,
}

pub trait AgentAuthFrontend: UserMessageSink + Send + Sync {
    fn ask_agent_auth_consent(
        &mut self,
        agent: &AgentName,
        env_var_names: &[&str],
    ) -> Result<AgentAuthDecision, CommandError>;
}
```

Layer 2 commands check `EffectiveConfig::auto_agent_auth_accepted` before launching an agent:
- `Some(true)` → silently inject credentials via `AgentEngine::resolve_agent_auth`.
- `Some(false)` → do NOT inject (no prompt).
- `None` → call `ask_agent_auth_consent`; on `Accept`/`Decline`, persist via `RepoConfig::update`.

Cover the flag matrix (None/true/false × Accept/Decline/DeclineOnce) with unit tests.

#### 6.4 Headless server lifecycle (start / kill / logs / status)

The legacy `commands/headless/` subtree mixes user-facing command logic with daemonization, PID-file management, log-file rotation, and SQLite session persistence. Split as follows:

**Layer 1 — `engine/headless/lifecycle.rs`** (NEW; introduced here in WI 0068, but the engine module belongs to Layer 1 — confirm with developer whether this becomes part of WI 0067 or stays here):

```rust
pub struct HeadlessLifecycle {
    paths: HeadlessPaths,        // Layer 0
}

impl HeadlessLifecycle {
    pub fn new(session: &Session) -> Self;

    pub fn pid_file_path(&self) -> &Path;
    pub fn log_file_path(&self) -> &Path;

    /// Read the PID file; verify the process is alive. Returns None if absent or stale.
    pub fn current_pid(&self) -> Result<Option<u32>, CommandError>;

    /// Write a fresh PID file with the current process's PID.
    pub fn write_pid(&self) -> Result<(), CommandError>;

    /// Remove the PID file (idempotent).
    pub fn clear_pid(&self) -> Result<(), CommandError>;

    /// Send SIGTERM to the recorded PID; wait up to `timeout` for the process to exit.
    pub async fn kill(&self, timeout: Duration) -> Result<KillOutcome, CommandError>;

    /// Daemonize via systemd (Linux) / launchd (macOS) / spawn-detached fallback (other).
    /// Returns the detached process's PID. Caller then exits.
    pub fn daemonize(&self, args: &[OsString]) -> Result<u32, CommandError>;

    /// Open the log file for append; rotate if the file exceeds `LOG_ROTATE_THRESHOLD`.
    pub fn open_log_for_append(&self) -> Result<File, CommandError>;
}

pub enum KillOutcome { ExitedCleanly, ExitedAfterSigKill, NotRunning }
```

**Layer 2 — `command/commands/headless/`**: thin wrappers (`HeadlessStartCommand`, `HeadlessKillCommand`, `HeadlessLogsCommand`, `HeadlessStatusCommand`) that consume `HeadlessLifecycle`. The actual HTTP server boot is performed by a Layer 3 frontend method (`HeadlessStartCommandFrontend::serve_until_shutdown`, per WI 0069 §3) — Layer 2 hands the lifecycle helper plus the assembled `HeadlessServeConfig` to the frontend.

`HeadlessStartCommand` MUST:

1. Refuse if `current_pid()` returns Some — print "headless already running on PID N".
2. If `--refresh-key`, generate a new API key, write the hash, print the key once. Print exactly the legacy banner format (capture as a constant in `src/command/commands/headless/banner.rs`).
3. If `--background`: call `daemonize` and exit cleanly.
4. Foreground: write PID, open log for append, hand off to the frontend.
5. Cleanup on Ctrl+C / SIGTERM: call `clear_pid()` (idempotent).

`HeadlessLogsCommand` streams the log file to stdout with `tail -F` semantics — the frontend handles the actual streaming; the command resolves the path and hands it off.

`HeadlessStatusCommand` reads PID, port, session count, and uptime via `current_pid()` and an HTTP `GET /v1/status` request to the running server (re-using `RemoteClient` from §2b).

##### 6.4a Workdir allowlist resolution

`headless start --workdirs A --workdirs B` MUST be merged with `GlobalConfig::headless.work_dirs` (a `Vec<PathBuf>`). The merge:

1. Concatenate CLI-supplied workdirs and config workdirs.
2. Canonicalize each via `OverlayPathResolver::canonicalize` (Layer 0).
3. Deduplicate.
4. Reject any path that does NOT exist with `CommandError::HeadlessWorkdirNotFound { path }`.
5. The merged-and-validated list is supplied to the headless server config.

Cover with unit tests for: empty CLI + nonempty config; nonempty CLI + empty config; both nonempty with overlap; nonempty CLI + missing path.

#### 6.5 Remote command — API key resolution

The legacy `oldsrc/commands/remote.rs::resolve_api_key` resolution order is:

1. `--api-key` flag (only if non-empty after trim).
2. `AMUX_API_KEY` env var (only if non-empty after trim).
3. `GlobalConfig::remote.default_api_key` — **only when** the resolved target_addr (after `--remote-addr` flag and `AMUX_REMOTE_ADDR` env are applied) MATCHES `GlobalConfig::remote.default_addr` after URL canonicalization (case-insensitive scheme, lowercase host, default-port elision, trailing slash normalization).
4. None — server may have `--dangerously-skip-auth` enabled; caller proceeds without authentication.

Update `RemoteClient::resolve_api_key` (§2b) signature to take both the target_addr and the global config:

```rust
pub fn resolve_api_key(
    session: &Session,
    target_addr: &Url,
    explicit: Option<&str>,
) -> Result<Option<ApiKey>, CommandError>;
```

Note the `Option<ApiKey>` return — `None` is a valid resolution outcome (the server may not require auth). Cover the URL-canonicalization rule with unit tests including:
- target = `http://1.2.3.4:9876/`, default = `http://1.2.3.4:9876` → match.
- target = `http://1.2.3.4`, default = `http://1.2.3.4:80/` → match.
- target = `https://example.com/`, default = `http://example.com/` → no match (different scheme).

#### 6.6 Remote command — HTTP timeouts

Legacy uses `connect_timeout = 10s`, `timeout = 600s` (commands can run long). Encode as `RemoteClient::CONNECT_TIMEOUT` and `RemoteClient::READ_TIMEOUT` constants. Cover with a unit test that asserts the constants and a mock-server test that verifies the values are applied to the `reqwest::Client`.

`stream_command` MUST disable the read timeout (or set it generously) so SSE streams don't hit the 600s ceiling on long-running commands. Document in rustdoc.

#### 6.7 Remote `run` argument forwarding

The clap projection MUST set `trailing_var_arg(true)` and `allow_hyphen_values(true)` for `remote run`'s `<COMMAND>...`. The catalogue's `ArgumentSpec::TrailingVarArgs` kind triggers both. Cover with a unit test that asserts `remote run -- exec prompt --yolo "hello"` parses without "unknown flag --yolo" errors.

#### 6.8 Status command — TUI tab annotation

`StatusCommand` MUST accept an optional context object that the TUI populates before invocation:

```rust
pub struct StatusCommandTuiContext {
    /// One entry per running TUI tab. The status command annotates each running container
    /// with the matching tab number and stuck indicator.
    pub tabs: Vec<TuiTabSnapshot>,
}

pub struct TuiTabSnapshot {
    pub tab_number: u32,
    pub container_name: Option<String>,  // matches the container's amux-... name
    pub is_stuck: bool,
    pub command_label: String,           // for display alongside the container row
}
```

In CLI / headless mode, the context is `None`; the command renders without tab annotations. In TUI mode, `TuiStatusCommandFrontend` provides the context via a frontend method. Cover with a test that asserts annotation columns appear only when context is `Some`.

#### 6.9 `ready --json` flag

When `--json` is set on `ready`, the command's `ReadyOutcome` MUST be serialized as JSON (per §571 generic rule) AND `--non-interactive` MUST be implied (Dispatch sets `AgentRunOptions::non_interactive = true` and disables all `ReadyFrontend::ask_*` prompts via the headless safe-defaults pattern). Cover with a unit test asserting both implications.

#### 6.10 `ImplementCommand` body

`ImplementCommand::run_with_frontend` body, in order:

1. Resolve mount path via `MountScope::resolve` (§6.3a).
2. Resolve effective agent + model: CLI flag > repo config > global config.
3. If agent is not available: call `AgentSetupFrontend::ask_agent_setup` (§6.3b). On `Setup` → call `AgentEngine::ensure_available`. On `FallbackToDefault` → swap the agent and continue. On `Abort` → return `CommandError::Aborted`.
4. Check `EffectiveConfig::auto_agent_auth_accepted`: if `None`, call `AgentAuthFrontend::ask_agent_auth_consent` (§6.3c). Persist the result.
5. If `--worktree`: construct `WorktreeLifecycle::for_work_item(git, git_root, work_item)` and call `prepare(frontend)`. Use the worktree path as the mount path for the container.
6. If `--workflow`: parse the workflow file (Layer 0) and run it via `WorkflowEngine`. Otherwise: construct a synthetic single-step workflow with the legacy implement prompt (`"Implement work item NNNN. Iterate until build/tests/docs succeed."`) and run it the same way.
7. After completion: call `WorktreeLifecycle::finalize(frontend, had_error)` if a worktree was used.
8. Map exit info to `ImplementOutcome`.

This sequence is the canonical order for every agent-launching command (chat, exec prompt, exec workflow follow the same shape with command-specific differences in the prompt construction step). Document the canonical order in `src/command/commands/agent_command_pattern.md` (a one-page maintainer reference, NOT user-facing docs).

## Edge Case Considerations:

- **Worktree implication rule**: `--yolo` or `--auto` implies `--worktree` for `exec workflow`. This implication MUST be computed in `Dispatch::build_command` (after reading flag values but before constructing `*Command`), NOT inside the command itself. The `--worktree` field on the constructed `*Command` reflects the post-implication value. Cover the combinations (yolo-only, auto-only, yolo+worktree explicit, no-yolo-no-auto) with catalogue unit tests.
- **Detached HEAD + worktree**: `WorktreeLifecycle::prepare` checks `GitEngine::is_detached_head` before creating the worktree. If detached, `UserMessageSink::warning(...)` is called with a message explaining the branch situation; the command continues (the user has been warned). Do NOT abort.
- **`UserMessageSink` queueing during PTY**: the CLI `ExecWorkflowCommandFrontend` (Layer 3) sets a `pty_active` flag to `true` before calling `ContainerExecution::wait` and to `false` after it returns. Any `UserMessageSink::write_message` calls during `wait` (e.g., from `WorkflowEngine` step transitions) are queued. The command calls `frontend.replay_queued()` immediately after `wait` returns and again after `WorktreeLifecycle::finalize` returns. Cover with a unit test using a `RecordingMessageSink` that verifies message order.
- **Post-workflow abort (non-zero exit)**: when `had_error` is true and the user chooses `PostWorkflowWorktreeAction::Merge`, the `WorktreeLifecycleFrontend::ask_post_workflow_action` call receives `had_error: true`. The frontend may render additional context ("the command exited with an error; merging may incorporate broken work"). The `WorktreeLifecycle` itself does not differentiate — it executes the merge regardless; the warning is the frontend's responsibility.
- **Merge conflict during `finalize`**: `git_engine.merge_branch` returns `EngineError::MergeConflict { branch, worktree_path }`. `WorktreeLifecycle::finalize` catches this, calls `frontend.report_merge_conflict(branch, worktree_path, git_root)`, and returns `Ok(())` — the conflict is not a fatal error from the command's perspective. The user resolves it manually outside amux.
- **Subcommand nesting (`exec prompt`, `headless start`)**: the catalogue must support arbitrary nesting. Test depth-2 lookups (`["exec", "prompt"]`, `["headless", "start"]`) explicitly.
- **Catalogue-clap drift**: if any flag exists in `clap` but not in the catalogue (or vice versa), the unit test `catalogue_clap_consistency` fails. Same for `catalogue_tui_consistency` and `catalogue_headless_consistency`.
- **Mutually exclusive flags**: today's clap uses `conflicts_with` and `requires`. The catalogue MUST encode these constraints in `FlagSpec` so projections honor them. ASK THE DEVELOPER if a richer constraint language is needed (e.g. "exactly one of {plan, yolo, auto}").
- **Per-command frontend trait composition**: some commands need both a `ContainerFrontend` and a `WorkflowFrontend` (e.g. `exec workflow`). Per-command frontend traits MUST be expressed as supertrait bounds (`trait ExecWorkflowCommandFrontend: ContainerFrontend + WorkflowFrontend`) so a single Layer 3 type satisfies them all.
- **Default value drift**: `aspec/uxui/cli.md` documents some defaults; the catalogue is the source of truth post-refactor. ASK THE DEVELOPER whether to regenerate `aspec/uxui/cli.md` from the catalogue (work item 0070's responsibility) or by hand.
- **`--json` output mode**: today some commands accept `--json` to produce structured output. In the new architecture, the command's `*Outcome` is a typed value; JSON serialization is a frontend concern, not a command concern. Ensure every `*Outcome` derives `Serialize`.
- **`--non-interactive` flag and `headless.alwaysNonInteractive` config**: these two concerns control whether a containerized agent runs in print-only mode (e.g. passes `--print` to Claude Code). They are distinct from the Q&A-decision non-interactive behavior handled by frontend safe-defaults. The path is: `Dispatch::build_command` reads the `--non-interactive` CLI flag and the `GlobalConfig::headless.alwaysNonInteractive` setting; if either is true, the constructed `*Command` receives `AgentRunOptions { non_interactive: true, .. }`, which `AgentEngine::build_options` translates into the agent-specific print flag inside the container. This mutation belongs in `Dispatch::build_command` after reading flags but before constructing the `*Command`. Cover with unit tests for both sources (explicit flag and headless config).
- **`AMUX_OVERLAYS` env validation**: today's `commands/mod.rs::run` validates this env up front for every command. In the new architecture, this validation belongs to `OverlayEngine::new` (Layer 1) or `EffectiveConfig::overlays` (Layer 0) — ASK THE DEVELOPER. Whichever layer owns it, every command path MUST trigger the validation early.
- **Agent-launching command canonical order**: every command that launches an agent (`implement`, `chat`, `exec prompt`, `exec workflow`, `specs amend`, `claws *`, `init` audit, `ready` audit) MUST follow the §6.10 canonical order: mount-scope resolve → agent resolve → ensure_available (with `AgentSetupFrontend` fallback) → keychain consent (`AgentAuthFrontend`) → worktree prepare → workflow run → worktree finalize → outcome. Cover with a per-command unit test asserting call order against a recording frontend.
- **`auto_agent_auth_accepted` persistence**: when the user accepts or declines (not "decline once") the keychain-consent prompt, the per-repo `RepoConfig::auto_agent_auth_accepted` flag MUST be updated and persisted to `<git-root>/.amux/config.json` BEFORE the agent container launches. This avoids re-prompting on subsequent commands. The persistence call is `RepoConfig::set_auto_agent_auth_accepted(repo_dir, accepted: bool)` from Layer 0. Cover the lifecycle with a per-repo config integration test.
- **`specs new` ↔ `new spec` aliasing**: both invocations MUST construct the same `*Command` and produce identical behavior. Cover with a unit test that calls `Dispatch::run_command(["specs", "new"])` and `Dispatch::run_command(["new", "spec"])` and asserts both produce the same constructor arguments.
- **`implement` ↔ `exec workflow` relationship**: `ImplementCommand` is a top-level command but its body MAY share helpers with `ExecWorkflowCommand`. Both MUST go through the same shared `agent_command_pattern` (§6.10). When `--workflow` is omitted on `implement`, the synthetic single-step workflow MUST use the literal legacy prompt template captured in `src/command/commands/implement_prompts.rs::DEFAULT_IMPLEMENT_PROMPT` — the exact string is preserved from `oldsrc/commands/implement.rs`.
- **`ready --json` implies `--non-interactive`**: when `--json` is set on `ready`, Dispatch sets the `ReadyEngineOptions { non_interactive: true }`. Frontends with `ReadyFrontend` impls MUST honor this by returning safe-defaults for every `ask_*` method (`ask_create_dockerfile` → true, `ask_run_audit_on_template` → false, `ask_migrate_legacy_layout` → false). Cover with a per-frontend unit test.
- **CommandCatalogue alias support**: the `CommandSpec::aliases` field MUST support both string aliases (`"wf"` for `exec workflow`) AND path aliases (`["specs", "new"]` aliasing `["new", "spec"]`). The clap projection translates string aliases via `Command::alias`; path aliases are resolved at dispatch time (`CommandCatalogue::lookup_with_aliases(["specs", "new"])` returns the `["new", "spec"]` spec). Cover with unit tests for both alias kinds.
- **Trailing-var-args parsing in TUI command box**: the TUI's `parse_command_box_input` MUST honor `ArgumentSpec::TrailingVarArgs` semantics — anything after the trailing-args boundary (`--`) is captured verbatim in the argument value. Cover with a unit test for `remote run -- exec prompt --yolo "hi"`.
- **Implement command without `--workflow`**: `ImplementCommand` synthesizes a single-step workflow internally when `--workflow` is absent. The synthetic workflow's `agent` and `model` come from CLI flags or config; the prompt comes from `DEFAULT_IMPLEMENT_PROMPT` with `{{work_item_number}}` substitution applied. Cover with a unit test.
- **`status --watch` cleanup on signal**: when the user hits Ctrl+C during `status --watch`, the command MUST exit cleanly without leaving the terminal in an inconsistent state. `StatusCommand::run_with_frontend` registers a SIGINT handler (or polls the frontend's cancellation surface) and breaks out of the watch loop on signal.

## Test Considerations:

### Test philosophy (read first)

Tests for Layer 2 are **designed and written from scratch** alongside the new dispatch and command structs. **Do not port tests from `oldsrc/commands/**/#[cfg(test)] mod tests` or from `oldsrc/cli.rs` test blocks.** Those tests assume the legacy parameter-style command entry points (`pub async fn run(args)`) and frontend-conflated business logic. Reusing them carries forward the very design we are replacing.

The narrow exception is a test that satisfies **all** of the following:

1. Asserts a precise behavioral invariant the new command MUST preserve (e.g. flag precedence ordering, `AMUX_OVERLAYS` env validation timing, `headless.alwaysNonInteractive` config behavior, exit-code mapping).
2. Compiles unchanged or with mechanical edits against the new `*Command` types.
3. Exercises only Layer 0 + Layer 1 + Layer 2 — no Layer 3, no legacy types.

If any old test is brought forward under this exception, the PR description MUST list it with a one-sentence justification. The default answer is "rewrite from scratch."

This work item produces **only Layer 2 unit tests** using fake engines and fake `CommandFrontend` / per-command frontends. **No real Docker, no real git beyond hermetic `git init` against `tempfile`, no real HTTP server, and no real CLI/TUI binary.** All cross-layer integration, end-to-end, parity, and binary-level smoke tests are 0070's responsibility against a freshly rebuilt `tests/` directory.

### Unit tests (colocated `#[cfg(test)] mod tests`)

- **`CommandCatalogue`**:
  - Every command and flag listed in `aspec/uxui/cli.md` is present in the catalogue with the documented name, kind, default, and `FrontendVisibility`. (Drive via a data-table test, not per-flag duplicated assertions.)
  - `lookup(["exec", "prompt"])` returns the expected spec; `lookup(["bogus"])` returns `None`; `lookup(["init", "bogus"])` returns `None`.
  - Mutually exclusive constraints in `FlagSpec` are honored by a `FlagSpec::conflicts_with` accessor.
- **Projections (consistency — these are Layer 2 unit tests, not integration tests)**:
  - `catalogue_clap_consistency`: build the clap command from the catalogue, walk every `Arg`, assert each is present in the catalogue with matching kind/default/help.
  - `catalogue_tui_consistency`: every catalogue command has a `TuiHint`; every documented flag appears in `tui_completions` for an appropriate prefix.
  - `catalogue_headless_consistency`: every catalogue command appears in `rest_route_table` and `openapi_schema`; method + path stable against a checked-in fixture.
  - **No drift test against `oldsrc`** — the catalogue is the new source of truth. Compare against `aspec/uxui/cli.md` and the checked-in projection fixtures, not against legacy clap definitions.
- **`Dispatch`** (with a recording `FakeCommandFrontend`):
  - For each catalogue entry, `Dispatch::run_command` builds the expected `*Command` struct with the expected field values (mock the constructor to record arguments).
  - Missing required flag → `CommandError::MissingRequiredFlag { command, flag }`.
  - Unknown flag (frontend supplies a value for a flag not in the catalogue) → `CommandError::UnknownFlag`.
  - Mutually exclusive flags both supplied → `CommandError::MutuallyExclusive`.
  - `parse_command_box_input("exec workflow my-workflow --yolo")` returns the expected `ParsedCommandBoxInput { path: ["exec", "workflow"], arguments: {"workflow_name": "my-workflow"}, flags: {"yolo": true} }`.
  - `parse_command_box_input` rejects unknown commands and unknown flags with structured errors that the TUI can render.
  - Non-interactive override sets `AgentRunOptions::non_interactive = true` before `*Command` construction (verify by inspecting the recorded constructor argument, not by behavior). Cover both sources: `--non-interactive` CLI flag and `GlobalConfig::headless.alwaysNonInteractive`.
  - `AMUX_OVERLAYS` env validation runs before any per-command construction (verify ordering by failing the env validator first and asserting no command was built).
- **`WorktreeLifecycle`** (colocated in `src/command/commands/worktree_lifecycle.rs`, using a `FakeGitEngine` and `RecordingWorktreeLifecycleFrontend`):
  - `prepare` happy path (no existing worktree, no uncommitted files): `create_worktree` called once, `report_worktree_created` called once, returns the worktree path.
  - `prepare` with uncommitted files on main branch, user chooses `Commit { message }`: `commit_all` called with the message, then `create_worktree` called.
  - `prepare` with uncommitted files, user chooses `UseLastCommit`: `commit_all` NOT called, `create_worktree` called.
  - `prepare` with uncommitted files, user chooses `Abort`: returns `CommandError::Aborted`, `create_worktree` NOT called.
  - `prepare` with existing worktree, user chooses `Recreate`: `remove_worktree` called, then `create_worktree` called.
  - `prepare` with existing worktree, user chooses `Resume`: `remove_worktree` NOT called, `create_worktree` NOT called.
  - `finalize` with `PostWorkflowWorktreeAction::Merge` and no uncommitted files in worktree: `merge_branch` called; on success `confirm_worktree_cleanup` called; on confirm `remove_worktree` + `delete_branch` called.
  - `finalize` with `Merge` and uncommitted files in worktree: `ask_worktree_commit_before_merge` called; if `Some(msg)` → `commit_all(worktree_path, msg)` before merge.
  - `finalize` with `Merge` and `GitEngine::merge_branch` returning `MergeConflict`: `report_merge_conflict` called, no `remove_worktree`, returns `Ok(())`.
  - `finalize` with `PostWorkflowWorktreeAction::Discard`: `remove_worktree` + `delete_branch` called, `report_worktree_discarded` called.
  - `finalize` with `PostWorkflowWorktreeAction::Keep`: no git calls, `report_worktree_kept` called.
  - `UserMessageSink` messages written during `prepare` (e.g. detached-HEAD warning) appear in the recording sink in order.
- **`RemoteClient`** (against a mock HTTP server using `wiremock` or `mockito`):
  - `resolve_api_key` with an explicit argument: returns the explicit value, ignoring env and file.
  - `resolve_api_key` with no explicit argument, `AMUX_API_KEY` env var set: returns the env value.
  - `resolve_api_key` with neither explicit nor env, key file present: reads from `~/.amux/api-key`.
  - `resolve_api_key` with no source available: returns `CommandError::MissingApiKey`.
  - `send_command` with a 200 response: returns the parsed `RemoteResponse`.
  - `send_command` with a non-2xx response: maps to the correct `CommandError` variant.
  - `stream_command` with a valid SSE stream: calls `on_event` for each event and `on_done` at stream close.
  - `stream_command` with a malformed SSE line: maps to `CommandError::MalformedSseEvent`.
  - `map_reqwest_error`: timeout → `CommandError::RemoteTimeout`; connection refused → `CommandError::RemoteConnectionRefused`.
- **Per-command unit tests** (`src/command/commands/<name>.rs`):
  - Each `*Command` has a focused test suite using a `FakeEngines` (mock `ContainerRuntime`, `GitEngine`, `OverlayEngine`, `AuthEngine`, `AgentEngine`, `WorkflowStateStore`) and a recording per-command frontend.
  - Happy path: command resolves flags, calls the expected engine methods with expected arguments, produces the expected `*Outcome`.
  - Frontend interactions: every per-command frontend method is exercised at least once.
  - `ExecWorkflowCommand` with `--worktree`: `WorktreeLifecycle::prepare` is called before the workflow engine and `WorktreeLifecycle::finalize` is called after, even when the engine returns an error.
  - `ExecWorkflowCommand` with `--yolo` (no explicit `--worktree`): Dispatch's implication rule sets `--worktree` before `ExecWorkflowCommand` is constructed; `ExecWorkflowCommand` sees `flags.worktree == true` without knowing about the implication.
  - `UserMessageSink::replay_queued` is called by `ExecWorkflowCommand` after `ContainerExecution::wait` and after `WorktreeLifecycle::finalize`. The recording frontend verifies the call order.
  - Error mapping: each upstream `EngineError` / `DataError` variant maps to a defined `CommandError` variant.
  - `*Outcome` `Serialize` round-trip is byte-stable for `--json` callers (the outcome itself is JSON-stable; how a frontend renders it is Layer 3).

### What does NOT belong in this work item

- Tests using real Docker, real container runtimes, real network, or real HTTP servers.
- Tests that drive a real Layer 1 engine end-to-end (e.g. real `ContainerRuntime::build`). Use the fake/mock at the trait surface defined in 0067.
- Tests in the top-level `tests/` directory. Leave it untouched; 0070 rebuilds it.
- Tests of any Layer 3 surface (CLI, TUI, headless) — those layers do not exist yet.
- Parity tests of any kind.

### Build & CI

- `cargo build --bin amux` (still from `oldsrc/`) succeeds.
- `cargo build --bin amux-next` succeeds — Layers 0+1+2 compile cleanly.
- `cargo test` passes including the new dispatch + per-command unit tests.

### Manual smoke test

- Run `amux` (still legacy code). Behavior must be identical to pre-refactor.

## Codebase Integration:

- Follow `aspec/architecture/2026-grand-architecture.md` as the source of truth.
- Follow `aspec/uxui/cli.md` for the user-facing command surface; do not change user-visible CLI behavior in this work item.
- Follow established conventions, best practices, testing, and architecture patterns from the project's `aspec/`.
- Do not edit `oldsrc/`. Do not delete `oldsrc/`. Both are in 0070's scope.
- Do not introduce upward calls from Layer 2 to Layer 3/4. Use traits owned by Layer 2.
- Do not introduce free `pub fn` for stateful command concerns. Prefer struct + methods.
- The PR description MUST link to `aspec/architecture/2026-grand-architecture.md` and to this work item, MUST list any developer-clarification questions raised, and MUST include a checklist confirming that every entry in `oldsrc/commands/` has a destination in `src/command/commands/` (and call out any items that turned out to be Layer 1 helpers instead).
- After this work item lands, the next agent picks up `0069-grand-architecture-layer-3-frontends-and-binary.md`.
