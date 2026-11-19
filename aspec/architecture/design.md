# Project Architecture

## Overview

awman follows a **four-layer architecture** that separates data, business logic, command dispatch, and presentation. This design ensures functional parity across four frontend adapters (CLI, TUI, API, squad) while maintaining code health and enabling future frontend implementations.

Pattern: single statically-linked binary

For the complete architectural specification, see [`aspec/architecture/2026-grand-architecture.md`](./2026-grand-architecture.md).

## Design Principles

### Principle 1: Simplicity over Conciseness
Intermediate developers should feel at home in this codebase. Code is optimized for readability and maintainability, not brevity.

### Principle 2: Layered Testing
Unit tests, integration tests, and end-to-end tests are combined to achieve maximal coverage while keeping tests focused on their layer's concerns.

### Principle 3: Layered Architecture
Strict unidirectional dependencies between layers prevent cross-cutting concerns and ensure that lower layers never depend on higher layers.

### Agent version ownership

Users select agent binary versions through agent Dockerfiles and installation templates. Agent definitions, readiness checks and launch admission must never require an exact binary version or reject an upgrade for differing from a bundled default. The sole version distinction is OpenCode 1.x versus 2.x, whose breaking interfaces have separate adapters; those adapters check only the major inside the image. Other agents have no version admission policy. Bundled artifact choices are installation defaults, not compatibility requirements.

## Four-Layer Architecture

```
┌─────────────────────────────────────┐
│  Layer 4: Binary                    │ src/main.rs
│  (Entry point only)                 │
├─────────────────────────────────────┤
│  Layer 3: Frontends                 │ src/frontend/{cli,tui,api,squad}
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
- On-disk data contracts (JSON schemas, SQLite migrations, and the in-place upgrade of the builtin runtime's SQLite catalog: allowlisted protocol versions only, backed up to `db.pre-<protocol>` with a manifest first, restored through the `awman-catalog-restore` journal, committed by rewriting the state directory's runtime marker last)

**Constraint**: Imports only from `std`, third-party crates, and `crate::data::*`

### Repository checkout naming

`data::checkout_paths` owns generated Git branch names, checkout directory
names, repository keys and checkout path construction. Commands choose the
kind of checkout; engines perform Git operations and verify ownership. Neither
layer constructs naming prefixes or derives clone directory names itself.
Domain layout types (API, squad, worker and skill-library paths) delegate their
checkout locations to this module. There are no legacy naming generators,
compatibility exports, historical path searches or automatic migrations.
awman is beta software and does not guarantee naming/path compatibility across
versions. Existing files are not deleted or renamed by path resolution.

- Local work items retain `awman/work-item-NNNN` and `NNNN/`; workflows retain
  `awman/workflow-<name>` and `wf-<name>/` under the hashed repository key.
- GitHub issues use `<number>-<title-slug>` and `awman/<number>-<title-slug>`.
  Other remote items use their provider slug and stable key. GitHub issues
  never use that other-provider format as an alternative name.
- Generated workflow components preserve ordinary names. Unsafe characters,
  Git-invalid dot sequences, reserved device names and oversized components
  are encoded with an eight-hex SHA-256 suffix; branch and directory use the
  same component. Creation, resume and recreation all resolve the same
  canonical branch and path. Unhashed repository directories, previous GitHub
  issue names and unencoded workflow spellings are not searched or resumed.
- Session branch/PR names and paths follow the CLI selector contract. Explicit
  branches, paths and workflow `into` values are validated by their existing
  entry points, never slugified as though they were generated names.
- Remote sessions and workflow clone steps derive their default directory from
  the same URL/SCP/local-path leaf policy, stripping `.git` and encoding unsafe
  names. Workflow clones pass that directory explicitly to Git, including when
  transport credentials change the URL used for cloning.
  The obsolete per-session `repo/` and `worktree/` checkout slots are removed;
  API session initialization does not create placeholder checkout directories.
- Lease clones retain `<workRoot>/leases/<lease_id>/`; rehomed lease worktrees
  retain `wf-<run_id>/` and `awman/<run_id>`. Squad workspaces and skill-library
  clones retain their task/library namespaces. These lifetimes are distinct
  from reusable repository-keyed worktrees.

### Layer 1: Engine (`src/engine/`)
- Container-class runtime backends (Docker, Apple containers, builtin microVM)
- Workflow execution engine
- Git operations (init, worktree, merge)
- Overlay management (mounts, env vars, auth)
- Authentication and TLS
- Squad scheduling, OCI acquisition, ACP transport, and credential refresh; the complete current module list is in the grand architecture’s Layer 1 section.

**Constraint**: Imports from Layer 0 + `crate::engine::*` only

### Layer 2: Command (`src/command/`)
- Command dispatch and routing through the catalogue-driven build table
- `Startup` orders initialization, `Engines` assembles engine handles, and `HeadlessDefaults` supplies unattended answer profiles.
- Business logic for each command (`init`, `ready`, `exec`, `chat`, etc.)
- Workflow step execution coordination
- Error handling and user messaging

**Constraint**: Imports from Layers 0–1 + `crate::command::*` only

### Layer 3: Frontend (`src/frontend/`)
- CLI (clap-based command-line interface)
- TUI (Ratatui-based terminal UI)
- API (HTTP API server)
- Squad (HTTP adapter over the squad daemon runtime)

**Constraint**: Frontends are presentation-only. All business logic lives in Layer 2. Frontends communicate with lower layers via traits that delegate user input and receive outcomes for display.

**Frontends must NOT**:
- Implement agent selection or default logic
- Compute workflow step options
- Validate unsupplied flags

### Layer 4: Binary (`src/main.rs`)
- Single entry point
- Sets up chosen frontend (CLI, TUI, API, or squad)
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

All agent code execution occurs inside an isolated container or microVM managed by the `ContainerRuntime` (Layer 1). The host is never directly exposed to untrusted code. The builtin backend uses a Linux microVM; selecting it does not run agent code on the host. It is selected with `runtime.kind: "builtin"` and released experimentally for macOS on Apple Silicon and Linux x86_64 GNU with KVM (`RELEASED_TARGETS` in `engine::container::builtin`). Linux ARM64 remains gated on separate native acceptance. Linux x86_64 uses private-memory boot so the process-owned kernel callback allocation cannot become shared writable guest RAM.

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

Container-tier agent launches keep initial prompts of at most 64 KiB in their
normal delivery channel. Larger prompts that would be argv operands are staged
verbatim in a private temporary file, mounted read-only, and replaced with a
short instruction to read and follow the entire file as the user's task. This
applies equally to interactive and argv-based headless agents on Docker, Apple
and builtin; stdin and ACP delivery stay unchanged. Prompt mounts follow the
existing confirmed-exit lifetime guards, including failed launches and retries.
No prompt content is truncated, shell-expanded or promoted to a system prompt.

The engine owns a backend contract expressed as operations, not as a required
CLI binary. It selects a backend, checks availability, creates/attaches/stops
agent instances, streams execution, lists and removes agents, inspects/removes
images, and either builds or imports images according to backend capabilities.
Docker and Apple container backends implement these operations through their
host CLIs. The builtin backend (a container-tier driver: tiers follow capability
shape — OCI images, ephemeral lifecycle, host mounts — not isolation
technology) implements them through the embedded SDK and
microVM worker; it has no host runtime CLI, and operations that do not apply
are reported as unsupported. Command orchestration chooses operations from
backend capabilities (including build, import, or kit image acquisition), and
frontends render shared outcomes. Docker-shaped argv is private to the CLI
backends and is not the general runtime contract.

### Builtin backend capabilities and image sources

The builtin backend imports Linux OCI images and executes cached images. Its
normal acquisition sources are an explicitly selected OCI registry, Docker
Engine image store, Apple Containers store, or archive. A source is typed and
explicit, so a registry reference cannot resolve against a same-named daemon
image. Image identity records reference, platform, content digests, and source
kind. With `builtin.builder.enabled`, a command whose `BuildPolicy` allows it
may build through the local Docker Engine (Unix socket) or Apple Containers
store selected by that image's resolved source, then import from that same
source. `ready` (`Ready`), API session setup, and the agent availability check behind
`chat`/`exec prompt` may build. With an enabled compatible local builder,
`SessionSetup` checks source input hashes and builds missing or stale images,
even when an older copy is installed. Workflow prerequisite solves (static,
dynamic and worker leases) and squad evaluation use the same decision.
The prerequisite review reports pending builds and imports before preparation.
`ready --build` forces a refresh. Launches without a usable enabled builder
retain their check-only behavior. Other sources retain their normal import behavior and
never select a substitute builder. Build and import remain separate stages;
successful build alone is not readiness. Source acquisition is not required
after an image is cached.

After freshness and pending-receipt handling, image preparation verifies the
installed image's complete acquisition source, platform and any required built
identity before opening import progress. A matching installed image is reused
without entering acquisition/import, even if the acquisition archive cache is
absent. Explicit refreshes, new builds and pending imports still acquire their
required content. Archive sources retain acquisition's replaced-file check.
An import progress row describes an actual acquisition attempt, never an
unconditional claim that an existing image is being imported again.

The Layer 1 `AgentImagePreparer` owns this lifecycle for one image: it resolves
the image's source once, derives a single `BuilderSelection` (local Docker,
local Apple, incompatible, unconfigured or disabled) from it, builds through
the `LocalImageBuilder` handle when the policy allows, and acquires with
provenance checks. `LocalImageBuilder` exposes build-time operations only; it
cannot start or run an agent, which stays inside the builtin runtime.

Image-source configuration belongs to the data layer (which also resolves the
session's builtin settings into process-wide and per-launch parts), acquisition,
build and import to the engine, and `ready` orchestration to the command layer.
Apple image freshness inspection accepts both the older `name/index/variants`
output (including supported Apple Containers 0.12.0) and the newer per-variant
manifest digests. An index digest can identify build inputs, but is never used
as a single-platform manifest receipt. A source build requiring such a receipt
must obtain it from inspection or the build itself. Invalid metadata diagnostics
identify the image, backend and failed field.
The builtin runtime itself is process-wide (state directory, embedded runtime,
ambient-override check), while network policy, vCPUs and memory are per-launch
settings taken from each session's effective config. Backends do not
silently switch sources or substitute Docker, Apple containers, SBX, an
installed Microsandbox runtime, a host agent, or a downloaded/extracted helper
when an operation is unavailable.

Acquisition is bounded and idempotent: each attempt resolves the same source
spec to the same endpoint/reference/credentials, stages into a fresh private
directory, and either commits a fully validated archive or nothing. Only
transport-shaped failures (connection reset, disconnect, truncation, stall,
5xx) are retried, up to a fixed attempt cap and an overall deadline that also
bounds each request; auth, config, certificate, digest/platform mismatch,
rejected-archive and missing-image errors are final on first occurrence.
Local copy, validation/decompression reads, cache hashing and lock waits now
check the same cooperative deadline. A successful attempt is checked again
before publication, and a reference is not renamed after an observed expiry.
Ready passes a caller cancellation token to a blocking import worker; dropping
the ready future also cancels that operation. HTTP connect, headers, token and
body waits poll the same control through an asynchronous transport and drop
the request on cancellation. Progress uses a bounded channel. Filesystem
syscalls and the platform DNS resolver cannot be preempted; executor shutdown
waits at most 100 ms for resolver work. Projection and import-lock waits check
cancellation. Once the SDK mutating import begins, it completes the import and
identity publication before reporting cancellation, avoiding orphaned writes
from a dropped extraction future. Native cancellation/crash scenarios remain
mandatory in WI 0123; these safe points do not prove a hard wall-clock bound
for SDK extraction or filesystem calls.

Archive leases cover cache lookup/publication, projection and SDK import.
Per-image shared leases cover launch planning until the SDK creates its rootfs
reference; imports and removals require an exclusive lease. The SDK catalog's
transactional image-in-use check then protects referenced images. A competing
mutation fails explicitly instead of changing a planned image underneath a
consumer. Cross-process lock tests do not replace native multi-VM validation.

### Builtin network policy

The builtin backend's guest network is a data-owned, engine-compiled policy,
not a host firewall or helper process. `BuiltinNetworkConfig`
(`src/data/config/builtin_network.rs`) is the persisted global/repo config —
mode (`none`/`allowlist`/`public`), allowed names, authorized host loopback
ports, and global-only nameservers/CA trust — validated and layered so a
repository config can only narrow the global ceiling, never widen it. The
engine compiles that into a `NetworkPlan`
(`src/engine/container/builtin/network.rs`), a pure function with no SDK or
VM dependency, so its rules are covered by hermetic tests independent of
hardware. The driver applies the compiled plan (never the SDK's own defaults)
to every session VM — foreground and background alike — at the SDK/VMM
boundary; enforcement happens in the SDK's user-space network stack inside the
host-side worker, which is the only place a guest packet is ever seen or
decided on.

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

A command owns its frontend for its whole lifetime. Commands that hand the same frontend to several phases or runtimes hold it behind a shared handle (`SharedWorkflowFrontend`) and pass clones; they never reclaim it. In exchange, a runtime must drop the frontend no later than when `AgentExecution::wait()` resolves, when the `wait()` future is dropped, or when the execution is dropped.

### Workflow prerequisite lifecycle

Workflow startup owns one preflight plan, one probe pass and one aggregate
report. The plan includes agent images alongside setup/teardown credentials,
Git requirements and container tools. Static workflows select their resolved
step agents; fresh dynamic workflows select the leader and every agent offered
in its available-agent list. Resume selects the saved workflow's concrete
requirements without requiring a leader image.

All checks use `PrerequisitePreflight` and the serializable
`WorkflowPrerequisiteReport`. Command orchestration presents one initial
review, resolves explicit flags, remembered decisions, policy and mode defaults,
and prepares approved images before any leader or workflow agent starts.
Image setup has no separate modal or post-leader build pass.
For building runtimes and enabled local source builders, the initial image
review compares the same input hashes as preparation, including the effective
agent recipe after generated-Dockerfile synchronization and the project image
on which it depends. Stale or unlabelled images, and agents whose project image
needs rebuilding, are reported as requiring preparation, never satisfied merely
because their tag exists. Inspection failures remain unknown. The review does
not write recipes, download them, build, import, or alter build receipts.
Preparation preserves the installed-image fallback after a failed refresh and
records the remaining stale/unknown state in the final report; dynamic scope
narrowing accepts that prepared fallback without retrying the failed build.
`--check-prerequisites` uses the same plan and probes without granting
credentials, building/importing images or creating a worktree.

Generation can only narrow the prepared scope. It projects the original report,
drops unused grants and validates image membership without rerunning startup
probes or asking again. An unreviewed agent is rejected; an unapproved credential
remains unavailable under the unresolved policy. The bounded, noninteractive
credential freshness recheck immediately before teardown remains separate from
startup consent and can never widen it.
