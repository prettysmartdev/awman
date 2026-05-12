# Four-Layer Architecture — Quick Reference

**TL;DR**: amux is organized in four layers where lower layers never import from higher layers.

---

## The Layers

```
┌──────────────────────────────────────────┐
│ Layer 3: Frontend                        │  src/frontend/
│ CLI (clap), TUI (Ratatui), Headless HTTP │
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
- **No business logic. No containers. No git. No network.**

### Layer 1: Engine (`src/engine/`)
- ContainerRuntime (Docker, Apple containers)
- WorkflowEngine (multi-step DAG execution)
- GitEngine (repos, worktrees, merges)
- OverlayEngine (container mounts, env vars)
- AuthEngine (TLS, API keys, credentials)
- **Real systems (Docker, git, filesystem), no frontends.**

### Layer 2: Command (`src/command/`)
- Dispatch (router, command catalogue)
- Per-command types: InitCommand, ChatCommand, ExecWorkflowCommand, etc.
- All business logic (agent selection, defaults, error handling)
- Calls Layer 1 engines, reads/writes Layer 0
- Receives frontend traits to delegate user input
- **All logic shared by CLI, TUI, Headless.**

### Layer 3: Frontend (`src/frontend/`)
- CLI: clap-based CLI wrapper
- TUI: Ratatui-based interactive terminal UI
- Headless: HTTP API server
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
4. **Frontend** (Layer 3): Implement frontend traits, wire CLI/TUI/Headless
5. **Test**: Layer 0 (hermetic), Layer 1 (real systems), Layer 2 (integration), Layer 3 (parity)

---

## Common Mistakes

❌ **Frontend implements business logic**  
→ Move to `src/command/`

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

## Example: Adding `amux foo` Command

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

impl FooFrontend for HeadlessApp {
    fn ask_user(...) -> Result<UserChoice> { ... }
}
```

All three frontends now support `foo` identically — because the logic is in Layer 2.

---

## Documentation

- **User guide**: `docs/10-architecture-overview.md`
- **Full spec**: `aspec/architecture/2026-grand-architecture.md`
- **Design**: `aspec/architecture/design.md`
- **Security**: `aspec/architecture/security.md`

---

**Last Updated**: May 8, 2026 (WI 0073)
