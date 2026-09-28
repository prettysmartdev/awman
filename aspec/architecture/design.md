# Project Architecture

## Overview

awman follows a **four-layer architecture** that separates data, business logic, command dispatch, and presentation. This design ensures functional parity across three frontend modalities (CLI, TUI, API) while maintaining code health and enabling future frontend implementations.

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
│  Layer 3: Frontends                 │ src/frontend/{cli,tui,api}
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
- Container-class runtime backends (Docker, Apple containers, builtin microVM)
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
- API (HTTP API server)

**Constraint**: Frontends are presentation-only. All business logic lives in Layer 2. Frontends communicate with lower layers via traits that delegate user input and receive outcomes for display.

**Frontends must NOT**:
- Implement agent selection or default logic
- Compute workflow step options
- Validate unsupplied flags

### Layer 4: Binary (`src/main.rs`)
- Single entry point
- Sets up chosen frontend (CLI, TUI, or API)
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

All agent code execution occurs inside an isolated container or microVM managed by the `ContainerRuntime` (Layer 1). The host is never directly exposed to untrusted code. The builtin backend uses a Linux microVM; selecting it does not run agent code on the host.

- **Mount scope validation**: Git root, current working directory, or abort
- **Auth isolation**: API keys stored in secure hashing, env vars injected at container startup only
- **TLS enforcement**: Self-signed certificates with stable fingerprints

The builtin VM worker is the same awman executable re-executed with a private
internal worker-mode descriptor. The binary routes that reserved mode before
normal frontend, Tokio, and TUI startup; normal invocations continue through
the standard binary-to-frontend path. The worker takes over its process for the
VM lifetime. It is not hosted in the main process or a thread, and it is not a
separately extracted helper executable. Kernel and guest-agent payloads are
compiled into the executable; writable disks, image caches, databases, sockets,
and logs remain runtime state on disk.

### Runtime backend operations

The engine owns a backend contract expressed as operations, not as a required
CLI binary. It selects a backend, checks availability, creates/attaches/stops
agent instances, streams execution, lists and removes agents, inspects/removes
images, and either builds or imports images according to backend capabilities.
Docker and Apple container backends implement these operations through their
host CLIs. The builtin backend implements them through the embedded SDK and
microVM worker; it has no host runtime CLI, and operations that do not apply
are reported as unsupported. Command orchestration chooses operations from
backend capabilities (including build, import, or kit image acquisition), and
frontends render shared outcomes. Docker-shaped argv is private to the CLI
backends and is not the general runtime contract.

### Builtin backend capabilities and image sources

The builtin backend imports existing Linux OCI images and executes cached
images. It does not build images. Images can be acquired from an explicitly
selected OCI registry, a Docker Engine image store, or an archive; Apple
Containers store acquisition is currently blocked pending a supported export
API. A source is typed and explicit, so a registry reference cannot resolve
against a same-named daemon image. Image identity records reference, platform,
content digests, and source kind. Users build images externally with the
existing project Dockerfiles, then configure or import the resulting image.
Source acquisition is not required after an image is cached.

Image-source configuration belongs to the data layer, acquisition and import
to the engine, and `ready` orchestration to the command layer. Backends do not
silently switch sources or substitute Docker, Apple containers, SBX, an
installed Microsandbox runtime, a host agent, or a downloaded/extracted helper
when an operation is unavailable.

## Key Components

### Session Management
`Session` is the core orchestration type that captures:
- Working directory and Git repository context
- Agent configuration and available agents
- Merged configuration (repo, global, environment, flags)
- Current `SessionState` (ongoing command execution, workflow state, errors)

The CLI is a single-session frontend (one session per invocation). The TUI and API frontends manage multiple sessions concurrently via `SessionManager`.

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
