# Four-Layer Architecture — Quick Reference

**TL;DR**: awman is organized in four layers where lower layers never import from higher layers.

---

## The Layers

```
┌──────────────────────────────────────────┐
│ Layer 3: Frontend                        │  src/frontend/
│ CLI (clap), TUI (Ratatui), API HTTP │
│ RULE: Presentation-only, no business logic
├──────────────────────────────────────────┤
│ Layer 2: Command                         │  src/command/
│ Dispatch, per-command business logic     │
│ RULE: All business logic lives here
├──────────────────────────────────────────┤
│ Layer 1: Engine                          │  src/engine/
│ ContainerRuntime, WorkflowEngine, etc.   │
│ RULE: Core primitives, real I/O
├──────────────────────────────────────────┤
│ Layer 0: Data                            │  src/data/
│ Session, config, persistence             │
│ RULE: Types, storage, file I/O
└──────────────────────────────────────────┘
```

---

## Import Rules

| Layer | Can Import From | Cannot Import From |
|-------|---|---|
| 0 (data) | `std`, external crates, `crate::data::*` | Layers 1, 2, 3 |
| 1 (engine) | Layer 0 + external crates, `crate::engine::*` | Layers 2, 3 |
| 2 (command) | Layers 0–1 + external crates, `crate::command::*` | Layer 3 |
| 3 (frontend) | Layers 0–2 + external crates, `crate::frontend::*` | None (top layer) |

**Check**: `make architecture-lint` (runs in CI)

---

## What Goes Where

### Layer 0: Data (`src/data/`)
- Session, SessionState, SessionManager
- Config (repo, global, env vars, flags, effective)
- Workflow definitions, workflow state
- Worktree paths, overlay path resolution
- File I/O: reading/writing configs, SQLite, JSON
- `Prompt<D>` / `Choice<D>` (`src/data/prompt.rs`) — the shape of every
  interactive question: a title, body, keyed choices and a dismissal default.
  Layer 0 holds the shape (plain data, so a Layer 1 trait can name it); Layer
  2's `command::prompts` fills in the copy. A frontend maps a keypress or
  index to a `Choice`'s value and never builds a `D` from a string.
- **No business logic. No containers. No git. No network.**

### Layer 1: Engine (`src/engine/`)
- ContainerRuntime (Docker, Apple containers)
- WorkflowEngine (multi-step DAG execution)
- GitEngine (repos, worktrees, merges; owns diff summaries)
- OverlayEngine (container mounts, env vars)
- AuthEngine (TLS, API keys, credentials)
- SquadDaemonEngine (squad daemon bootstrap: store open/migrate, orphan-run
  reconciliation, scheduler spawn)
- SquadSupervisor (squad daemon lifecycle: ensure running, key state, health)
- `AgentMatrix` / `ContainerBackend` (`src/engine/agent/agent_matrix.rs`,
  `src/engine/container/backend.rs`) — the only per-agent and per-backend
  tables; adding an agent or a runtime backend is a new table row, not a new
  `match agent.as_str()` arm scattered across the engine.
- `HostAgentPinger` (`src/engine/ready/host_agent.rs`) — the only type
  permitted to spawn an agent binary on the host (see
  `aspec/architecture/security.md`).
- **Real systems (Docker, git, filesystem), no frontends.**

### Layer 2: Command (`src/command/`)
- Dispatch (router, command catalogue), `Startup` (Layer 4's one entry point
  into session + engine construction)
- `Engines::build` / `Engines::for_daemon` (the one engine-assembly path for
  the CLI/TUI host and for the API/squad daemons, respectively)
- `ApiServerRuntime` / `ApiSessionLifecycle` (API server bootstrap, worker
  pool, session close — not `src/frontend/api/`)
- `ResolvedFlags` (catalogue-owned flag defaults and `implies` resolution;
  commands read flags through this, never through hand-written defaults)
- `GatewayNeed` (catalogue attribute — `None`/`Running`/`IfRunning` — that
  tells `Dispatch` whether and how hard to resolve a squad daemon gateway
  before building a command)
- `CallerContext` (`src/command/dispatch/resolved.rs`) — which command path,
  frontend kind and local-user fact a command was built with; built once by
  `Dispatch`, not asked of the frontend per-call.
- `HeadlessDefaults` (`src/command/headless.rs`) — the named answer profiles
  (`::api()`, `::squad()`, `::cli()`) a command delegates to when no operator
  is attached, instead of each frontend answering its own `ask_*` inline.
- `LaunchPolicy` (`src/command/commands/launch_policy.rs`) — the decisions a
  command makes before putting an agent in front of a user: which agent,
  stdio vs. ACP, which context directories, how the session's end is
  reported.
- `command::prompts` — the constructors that fill in a Layer 0 `Prompt<D>`'s
  titles, labels and hotkeys; the single place prompt copy is written.
- Per-command types: InitCommand, ChatCommand, ExecWorkflowCommand,
  SquadAttachCommand, etc.
- All business logic (agent selection, defaults, error handling)
- Calls Layer 1 engines, reads/writes Layer 0
- Receives frontend traits to delegate user input
- **All logic shared by CLI, TUI, API.**

### Layer 3: Frontend (`src/frontend/`)
- CLI: clap-based CLI wrapper
- TUI: Ratatui-based interactive terminal UI
- API: HTTP router/bind/TLS only — bootstrap lives in `ApiServerRuntime` (L2)
- Squad daemon: Axum router/bind only — bootstrap lives in
  `SquadDaemonEngine` (L1) via L2's `SquadDaemonHandles`
- **No business logic. Implement frontend traits. Call Dispatch.**

---

## Core Patterns

### Trait Delegation (downward communication)

Lower layers request input from higher layers via traits:

```rust
// Layer 1 accepts a trait from Layer 2/3
pub fn execute_container(
    instance: ContainerInstance,
    frontend: &dyn ContainerFrontend,  // Trait from higher layer
) -> Result<...>
```

### Builder Pattern (within a layer)

```rust
// Layer 1: ContainerRuntime builds containers with options
let instance = runtime
    .build()
    .with_option(ContainerOption::Image(...))
    .with_option(ContainerOption::Entrypoint(...))
    .build()?;
```

### Session as Anchor

Every operation starts with a `Session`:

```rust
// Layers all use Session to access config, state, repo context
pub async fn run_command(
    session: &mut Session,
    command: &str,
) -> Result<Outcome>
```

---

## Adding a New Feature

1. **Data** (Layer 0): Define new types/config fields
2. **Engine** (Layer 1): Implement runtime primitives if needed
3. **Command** (Layer 2): Implement business logic, add to Dispatch
4. **Frontend** (Layer 3): Implement frontend traits, wire CLI/TUI/API
5. **Test**: Layer 0 (hermetic), Layer 1 (real systems), Layer 2 (integration), Layer 3 (parity)

---

## Common Mistakes

❌ **Frontend implements business logic**  
→ Move to `src/command/`

❌ **A daemon frontend (API, squad) bootstraps itself: opens its own store,
resolves its own engines, owns its own session map**  
→ That bootstrap is Layer 2/1's job (`Startup`, `Engines::build`/`for_daemon`,
`ApiServerRuntime`, `SquadDaemonEngine`, `SessionManager`). The frontend
constructs the router/listener and hands it an already-bootstrapped runtime —
see WI 0113 F-02/F-03/F-05.

❌ **Layer 1 calls Layer 2 or 3**  
→ Use trait delegation (Layer 2/3 passes trait to Layer 1)

❌ **Dense `pub fn` with 10 parameters**  
→ Create a struct with a builder or options pattern

❌ **Frontends have different commands**  
→ All commands come from `CommandCatalogue` (Layer 2)

---

## Validation

```bash
cargo build --release        # Single binary
make test-fast              # Hermetic tests
make test-full              # All tests (Docker needed)
make architecture-lint      # No upward imports
cargo clippy -- -D warnings # No warnings
```

---

## Example: Adding `awman foo` Command

```rust
// 1. Layer 0: Define data
pub struct FooConfig {
    pub enabled: bool,
}

// 2. Layer 1: Optional real-system code
// (skip if no containers/git/filesystem needed)

// 3. Layer 2: Business logic
pub struct FooCommand {
    session: Session,
    args: FooArgs,
}

impl FooCommand {
    pub async fn run_with_frontend(
        self,
        frontend: &dyn FooFrontend,
    ) -> Result<Outcome> {
        // Call Layer 1 engines via session
        // Use frontend trait to ask for user input
    }
}

impl Command for FooCommand {
    fn run_with_frontend(...) { ... }
}

// 4. Layer 2: Register in Dispatch
CommandCatalogue::register("foo", FooCommand::from_args)

// 5. Layer 3: Implement frontend trait
impl FooFrontend for CliApp {
    fn ask_user(...) -> Result<UserChoice> { ... }
}

impl FooFrontend for TuiApp {
    fn ask_user(...) -> Result<UserChoice> { ... }
}

impl FooFrontend for ApiApp {
    fn ask_user(...) -> Result<UserChoice> { ... }
}
```

All three frontends now support `foo` identically — because the logic is in Layer 2.

---

## Documentation

- **User guide**: `docs/architecture.md`
- **Full spec**: `aspec/architecture/2026-grand-architecture.md`
- **Design**: `aspec/architecture/design.md`
- **Security**: `aspec/architecture/security.md`

---

**Last Updated**: September 23, 2026 (WI 0114)
