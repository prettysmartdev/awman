# Project Architecture

## Overview

amux follows a **four-layer architecture** that separates data, business logic, command dispatch, and presentation. This design ensures functional parity across three frontend modalities (CLI, TUI, Headless) while maintaining code health and enabling future frontend implementations.

Pattern: single statically-linked binary

For the complete architectural specification, see [`aspec/architecture/2026-grand-architecture.md`](./2026-grand-architecture.md).

## Design Principles

### Principle 1: Simplicity over Conciseness
Intermediate developers should feel at home in this codebase. Code is optimized for readability and maintainability, not brevity.

### Principle 2: Layered Testing
Unit tests, integration tests, and end-to-end tests are combined to achieve maximal coverage while keeping tests focused on their layer's concerns.

### Principle 3: Layered Architecture
Strict unidirectional dependencies between layers prevent cross-cutting concerns and ensure that lower layers never depend on higher layers.

## Four-Layer Architecture

```
┌─────────────────────────────────────┐
│  Layer 4: Binary                    │ src/main.rs
│  (Entry point only)                 │
├─────────────────────────────────────┤
│  Layer 3: Frontends                 │ src/frontend/{cli,tui,headless}
│  (Presentation + Input, no logic)   │
├─────────────────────────────────────┤
│  Layer 2: Command                   │ src/command/
│  (Business logic, command dispatch) │
├─────────────────────────────────────┤
│  Layer 1: Engine                    │ src/engine/
│  (Runtime primitives)               │
├─────────────────────────────────────┤
│  Layer 0: Data                      │ src/data/
│  (Types, config, persistence)       │
└─────────────────────────────────────┘
```

### Layer 0: Data (`src/data/`)
- Configuration (repo and global config)
- Session and workflow state
- File I/O and database access
- Environment variable handling
- On-disk data contracts (JSON schemas, SQLite migrations)

**Constraint**: Imports only from `std`, third-party crates, and `crate::data::*`

### Layer 1: Engine (`src/engine/`)
- Container runtime (Docker/Apple containers)
- Workflow execution engine
- Git operations (init, worktree, merge)
- Overlay management (mounts, env vars, auth)
- Authentication and TLS

**Constraint**: Imports from Layer 0 + `crate::engine::*` only

### Layer 2: Command (`src/command/`)
- Command dispatch and routing
- Business logic for each command (`init`, `ready`, `exec`, `chat`, etc.)
- Workflow step execution coordination
- Error handling and user messaging

**Constraint**: Imports from Layers 0–1 + `crate::command::*` only

### Layer 3: Frontend (`src/frontend/`)
- CLI (clap-based command-line interface)
- TUI (Ratatui-based terminal UI)
- Headless (HTTP API server)

**Constraint**: Frontends are presentation-only. All business logic lives in Layer 2. Frontends communicate with lower layers via traits that delegate user input and receive outcomes for display.

**Frontends must NOT**:
- Implement agent selection or default logic
- Compute workflow step options
- Validate unsupplied flags

### Layer 4: Binary (`src/main.rs`)
- Single entry point
- Sets up chosen frontend (CLI, TUI, or Headless)
- Delegates to frontend for all functionality

## High-level Data Flow

```
User Input
    ↓
Frontend (Layer 3) receives input
    ↓
Frontend calls Dispatch::run_command() (Layer 2)
    ↓
Command business logic executes (Layer 2)
    ↓
Command delegates to Engine (Layer 1)
    ↓
Engine reads/writes Data (Layer 0)
    ↓
Frontend receives Outcome
    ↓
Frontend renders output
    ↓
Output to user
```

## Execution Isolation

All agent code execution occurs inside isolated containers managed by the `ContainerRuntime` (Layer 1). The host is never directly exposed to untrusted code.

- **Mount scope validation**: Git root, current working directory, or abort
- **Auth isolation**: API keys stored in secure hashing, env vars injected at container startup only
- **TLS enforcement**: Self-signed certificates with stable fingerprints

## Key Components

### Session Management
`Session` is the core orchestration type that captures:
- Working directory and Git repository context
- Agent configuration and available agents
- Merged configuration (repo, global, environment, flags)
- Current `SessionState` (ongoing command execution, workflow state, errors)

The CLI is a single-session frontend (one session per invocation). The TUI and Headless frontends manage multiple sessions concurrently via `SessionManager`.

### Command Dispatch
`Dispatch` is the central command router. It:
- Maintains a canonical catalogue of all commands and flags
- Routes command strings to appropriate `Command` implementations
- Provides frontend-specific command hints and completions
- Ensures all frontends implement identical flag sets

### Trait-Based Delegation
Lower layers request input/output from higher layers via traits:
- `ContainerFrontend`: Handle PTY, stdin/stdout for container execution
- `WorkflowFrontend`: Handle user choices during workflow execution
- `InitFrontend`: Handle initialization prompts
- Similar traits for each command needing user interaction

This approach decouples business logic from presentation while preserving the ability to customize behavior per frontend.