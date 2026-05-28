# awman Architecture Overview

## For Contributors

awman is organized as a **four-layer architecture** that ensures clean separation of concerns and functional parity across CLI, TUI, and API frontends.

### The Four Layers

#### Layer 0: Data (`src/data/`)
Handles all persistent state, configuration, and file I/O.

- **Session and workflow state**: `Session` is the core type representing a session's context (git repo, config, agents, current execution state)
- **Configuration**: Repo-level (`.awman/config.json`) and global (`~/.awman/config.json`) config with environment variable merging
- **Persistence**: SQLite storage (API sessions), JSON (workflows and state)
- **File I/O**: Reading/writing configs, overlays, workspace directories

**Imports**: Only `std`, external crates, and `crate::data::*`

#### Layer 1: Engine (`src/engine/`)
Implements core runtime primitives: container management, workflow execution, git operations, overlays, and auth.

**Key components**:
- **ContainerRuntime**: Runs agents inside Docker or Apple containers with isolated mounts and environment
- **WorkflowEngine**: Executes multi-step workflows with state machine transitions
- **GitEngine**: Manages git repos, worktrees, and merges
- **OverlayEngine**: Constructs container overlays (mounts, env vars, auth injection)
- **AuthEngine**: Handles TLS, API keys, and agent authentication

**Imports**: Layer 0 + `crate::engine::*`

#### Layer 2: Command (`src/command/`)
Implements all business logic and command handlers. Each command (e.g., `init`, `chat`, `exec workflow`) is a separate type implementing a `Command` trait.

- **Dispatch**: Central router maintaining canonical command and flag definitions
- **Individual commands**: `InitCommand`, `ChatCommand`, `ExecWorkflowCommand`, etc.
- **Error handling**: User-friendly error messages and recovery suggestions

**Imports**: Layers 0–1 + `crate::command::*`

#### Layer 3: Frontend (`src/frontend/`)
Pure presentation layer. No business logic. Three implementations:

- **CLI** (`src/frontend/cli/`): Command-line interface using `clap`
- **TUI** (`src/frontend/tui/`): Interactive terminal UI using `Ratatui`
- **API** (`src/frontend/API/`): HTTP API server with WebSocket/SSE streaming

Frontends communicate with Layer 2 via **trait delegation**. For example:
- `ContainerFrontend` trait: provides PTY binding or stdout capture
- `WorkflowFrontend` trait: handles user choices during workflow execution
- `InitFrontend` trait: collects init-phase inputs

**Imports**: Layers 0–2 + `crate::frontend::*`

#### Layer 4: Binary (`src/main.rs`)
Single entry point. Sets up the chosen frontend and delegates.

### Design Principles

**1. No Upward Dependencies**
Lower layers never import from higher layers. All communication flows down, with higher layers passing trait objects to lower layers for delegation.

**2. Business Logic Belongs in Layer 2**
Frontends are presentation-only. Agent selection, flag defaulting, workflow step option computation — all happen in `src/command/`, not in frontends.

**3. Types Over Functions**
Prefer structured objects with methods over free `pub fn` signatures. For example, `ContainerRuntime` is a builder that accepts `Vec<ContainerOption>` rather than a dozen `run_with_*` functions.

**4. Test Each Layer**
- **Layer 0**: Hermetic tests (temp files, no network)
- **Layer 1**: Real-system tests (Docker, git, filesystem)
- **Layer 2**: Integration tests with real Layers 0+1
- **Layer 3**: Parity tests ensuring CLI/TUI/API behave identically
- **Layer 4**: Smoke tests of the binary

---

## For Users: How awman Works

You interact with awman through one of three frontends:

### Interactive Mode (TUI)
Run `awman` with no arguments to launch the interactive terminal UI. Manage multiple sessions (tabs), execute agents, and monitor workflows in real-time.

**Behind the scenes**:
1. `src/main.rs` detects interactive mode and launches the TUI frontend
2. TUI creates a `SessionManager` to track all open tabs
3. When you run a command (e.g., via the command box), TUI routes it to `Dispatch` in Layer 2
4. `Dispatch` creates the appropriate `Command` object and calls its `run_with_frontend()` method
5. The command executes via Layer 1 engines (container, workflow, git, etc.), reading/writing state in Layer 0
6. TUI receives the outcome and renders it

### Command Mode (CLI)
Run `awman <command> [flags]` to execute a single command and exit.

**Behind the scenes**:
1. `src/main.rs` parses the command line via `clap` (populated from `Dispatch`)
2. CLI frontend collects all flags and creates a `Dispatch` instance
3. `Dispatch` routes to the appropriate `Command` and calls `run_with_frontend()`
4. Same Layer 1–0 execution as TUI
5. CLI renders output to stdout/stderr and exits

### API Mode (HTTP API)
Run `awman api start` to launch a server providing HTTP endpoints for remote agents.

**Behind the scenes**:
1. API frontend binds to a port and starts an HTTP server
2. Incoming requests are routed to handlers that call `Dispatch`
3. Responses are streamed back as JSON or Server-Sent Events (SSE)
4. Session state is persisted in SQLite
5. All execution goes through Layers 2–0, same as CLI/TUI

---

## Critical Design Rules

### Rule 1: Container Isolation
**Agent code ONLY runs inside containers.** The host is never directly exposed to untrusted code. All mounts are validated; mount scope is confirmed with the user.

### Rule 2: Session is the Anchor
Every command execution operates within a `Session`. The session captures:
- Git repository context
- Agent configuration
- Merged config (repo, global, environment, flags)
- Current execution state

This ensures consistent behavior regardless of invocation mode.

### Rule 3: Identical Behavior Across Frontends
Because business logic lives in Layer 2 and frontends are presentation-only, all three frontends execute identical code. A workflow behaves the same in CLI, TUI, and API mode.

---

## Adding a New Feature

To add a new command or feature to awman:

1. **Define the data model** (Layer 0, `src/data/`)
   - Add structs to store new state
   - Add config fields if user-configurable
   - Add persistence/serialization as needed

2. **Implement business logic** (Layer 2, `src/command/`)
   - Create a new `Command` type implementing the `Command` trait
   - Call Layer 1 engines as needed
   - Handle errors with user-friendly messages

3. **Add to the Dispatch catalogue** (Layer 2, `src/command/dispatch.rs`)
   - Register the command and its flags in the canonical catalogue
   - Ensure all three frontends will populate identical flags

4. **Implement frontend-specific trait handlers** (Layer 3, `src/frontend/`)
   - If the command needs user input, define a trait in Layer 2
   - Implement the trait in each frontend (CLI, TUI, API)

5. **Write tests**
   - Layer 0: Hermetic config/persistence tests
   - Layer 1: Real-system tests if using containers/git
   - Layer 2: Integration tests of the command
   - Layer 3: Parity tests in `tests/cli_parity/`, `tests/tui_parity/`, `tests/api_parity/`

---

## Architecture Enforcement

The `make architecture-lint` command enforces layering rules:

```bash
make architecture-lint
```

This scans all imports and fails if a lower layer imports from a higher layer. It runs in CI on every PR.

---

## Further Reading

- **Detailed specification**: `aspec/architecture/2026-grand-architecture.md`
- **Design principles**: `aspec/architecture/design.md`
- **Security constraints**: `aspec/architecture/security.md`
- **Work item (WI 0073)**: `aspec/work-items/0073-grand-architecture-finalize.md`

---

[← Remote Mode](10-remote-mode.md)
