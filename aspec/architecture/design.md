# Project Architecture

## Overview

awman follows a **four-layer architecture** that separates data, business logic, command dispatch, and presentation. This design ensures functional parity across four frontend adapters (CLI, TUI, API, squad) while maintaining code health and enabling future frontend implementations.

Pattern: one executable with application/runtime dependencies statically linked
or embedded; only the platform system ABI may remain dynamic, as specified in
[`aspec/foundation.md`](../foundation.md) and checked by the release pipeline.

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
kind. With `runtime.builtin.builder.enabled`, a command whose `BuildPolicy` allows it
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

### Terminal transport and execution records

`data::terminal` owns the single `awman.terminal.v1` codec. The existing endpoint,
base64 bytes, snapshots, resize, ownership and reconnect behavior remain shared
by all clients. Guarded `submit_turn` and `close_execution` use typed UUID request
identities and execution/revision fences. Notifications are `automation_state`
(flattened snapshot), `turn_accepted`, `turn_result`, `execution_closed`,
`execution_replaced` and correlated `operation_error`. No legacy codec or
protocol negotiation is introduced. Browser clients observe these notifications;
they do not expose guarded mutation methods.

Layer 0 stores execution, turn and dedup records on the existing shared SQLite
connection. Command records project automation from `active_execution_id` rather
than keeping a second mutable snapshot. A pending lifecycle projection is bounded
recovery metadata for the existing event log. Layer 2 supplies already-decided
state, owns admission and publishes only after commit and acknowledged log flush.
Layer 1's hub provides deferred finalization and execution-bound producer handles;
it neither writes records nor decides readiness. An absent automation controller
refuses guarded requests without writing task bytes.

### Native automation ownership

The shared `ExecutionController` (Layer 2, `command::automation`) is the only owner
of automation admission, turn reservation, credential-operation state and
execution lifecycle; it is installed once into `TerminalOperations` by the daemon
runtime. A pure admission cell derives the status tier (`full` native, `best_effort`
OSC 7501, `none`) from ordered observations; every tier change invalidates readiness,
and `--automation=require_full` is re-evaluated under the same lock immediately before
each delivery and again at writer ingress. Admission order is: owner-local check, dedup
lookup, current-state checks, validate/encode, reserve turn + dedup + submitting
snapshot atomically, publish, ingress recheck, one write; delivery is never retried.
Layer 1 supplies adapters, the native channel reader and OSC decoder, and the hub's
deferred finalization and execution-bound producer handles; Layer 0 owns the records,
the SQLite store on the shared connection and the OS execution leases. Headless
automation reserves exactly one durable turn before the one-shot runs and requires
identity, a correlated native outcome and exit 0. Recovery runs before generic command
recovery: an unacknowledged turn becomes `delivery_unknown`, an acknowledged one
`execution_lost`, and a command is never re-queued.

`chat --automation` is the single sanctioned API-interactivity exception: a
daemon-owned PTY reachable only by the local owner (see `aspec/uxui/cli.md`). The
terminal's lifetime belongs to the execution, not to the HTTP request or any viewer.
Credential-update controller operations and replacement drivers are not part of the
advertised surface until native consumption is evidenced.

## Shared runtime, storage and streaming contracts (WI 0078)

These contracts close the open engineering decisions of the nextgen audit
(`aspec/review-notes/nextgen-audit-2026-11-20.md`, IDs in parentheses). Mount
authority (A01) and the host-ping resolver (A02) are in
[`security.md`](./security.md).

### Process-scoped and session-scoped settings (A03)

A process binds some settings once, when `Engines` is constructed. They are
described by one Layer 0 value, `ProcessBinding`:

| Field | Config key / source | Bound by |
|---|---|---|
| `runtime_kind: RuntimeKind` | `runtime.kind` | the constructed runtime engine |
| `builtin_state_dir: Option<PathBuf>` (builtin only) | `AWMAN_RUNTIME_BUILTIN_STATE_DIR`, `runtime.builtin.stateDir` | the process-wide builtin runtime |
| `auth_refresh: AuthRefreshSettings` | `agents.auth.refresh.{enabled,threshold,tick}` | the credential-refresh monitor |

Everything else is session-scoped and is resolved from each session's
`EffectiveConfig` at launch (builtin network, vCPUs, memory, disks, image
sources, agent selection and flags, workflow and automation settings).

`runtime.kind` is global-only and accepts `docker` (default), `apple`, `sbx`
and `builtin`. `sbx` and `builtin` are experimental. The builtin microVM is an
experimental preview on macOS Apple Silicon and Linux x86_64 GNU with KVM;
Linux ARM64 remains gated. Unsupported platforms fail explicitly. Builtin
session settings use the `runtime.builtin` object; `stateDir` is process-scoped
and the other settings are session-scoped as listed above. The optional
`runtime.builtin.builder.enabled` builds only through a local Docker Engine
selected by a Unix-socket `docker-local` source or an Apple Containers store
selected by `apple-local`, and imports from that same source. Registry, archive,
remote Docker and unconfigured sources cannot select a builder. `Ready` and
`SessionSetup` may build according to their individual policies;
`ImportOnly` (workflow preflight, dynamic workflows and squad evaluation) and
`CheckOnly` never build or probe a builder. `awman init` uses the selected
runtime's image build operation and does not use the builtin image builder.

- The effective process config is resolved exactly once, from global file,
  environment and startup `--set` in the normal precedence, **before** any
  runtime, credential monitor or `Core` is constructed. A one-shot CLI invocation
  includes its invocation repository layer before binding these settings; the
  multi-session daemon and TUI use global/env/startup settings only. `Engines` keeps the
  `ProcessBinding` it was built from, and construction fails if the engine it
  built does not report the bound `runtime_kind`. Daemon bootstrap uses that
  same resolved config; it never resolves a second, different one.
- A command-local `--set` (TUI command box, API `args`) is applied to the
  command's session clone and that session's `ProcessBinding` is compared with
  the process's before any command, queue row, session or runtime resource is
  allocated. Equal effective values are accepted. A difference is refused with
  `CommandError::ProcessScopedSetting { key, bound, requested }`, API error code
  `process_scoped_setting` (HTTP 409), and the message
  ``` `<key>` is fixed for this awman process (running with <bound>; requested <requested>). Restart the daemon or TUI with `--set <key>=<requested>`, or change the global config and restart. ```
  Engines are never rebuilt or hot-swapped under live sessions.
- A value the bound engine would ignore is never accepted silently, and no
  other isolation backend is substituted. A repository-layer `runtime.builtin.stateDir`
  that differs keeps its existing `BuiltinStateDirMismatch` refusal.

### Store opening and legacy migration (A04, A10)

`DataPaths::open_session_store(legacy_root)` is the only way a foreground or
daemon producer obtains the on-disk `SqliteSessionStore`. Embedded callers that
already hold a store (including in-memory) pass it in; nobody opens the
database by path a second time, and a failed open is an error, never a reason
to open or create another database. `AgentLaunchFrontend::automation_host`
returns a typed `AutomationHosting` (`Foreground` | `Shared(AutomationHost)`),
fallibly; a storage failure is not `Foreground`.

- **Lock.** The whole decide/copy/publish/retire sequence and the following
  open run under an OS-held exclusive lock on `<data root>/awman.db.migrate.lock`
  (`private_files::lock_private_exclusive`), waited for at most 30 s. The lock
  dies with its process; there is no age-based stealing.
- **Journal.** `<data root>/awman.db.migration.json` (private, written by
  staged atomic rename and fsynced before any copy):
  `{ "schema": 1, "migration_id": <uuid>, "source": <abs path>, "files": [{ "name": "awman.db" | "awman.db-wal" | "awman.db-shm", "len": u64, "sha256": hex }], "phase": "staging" | "verified" | "publishing" | "published", "started_at": rfc3339 }`.
- **Staging.** Files are copied into the private directory
  `<data root>/.awman.db.migrating-<migration_id>/`, fsynced and compared with
  the journal hashes (`verified`); they are then renamed into place, sidecars
  first and `awman.db` last (`publishing` → `published`); the legacy files are
  renamed aside as a backup and the journal and staging directory are removed
  last. Originals and backups are kept until then.
- **Resume.** In every phase, including `published`, the journal must name
  the current legacy source and every existing destination file must be
  recorded in the journal and equal its hash. A mismatch preserves both stores
  and the journal and returns `StoreMigrationConflict`; legacy retirement is
  forbidden until verification succeeds. A published journal with no destination
  is discarded; a pre-publication journal with no destination restarts from the
  untouched legacy files. A journal with no matching source and no destination
  is stale and is removed with its staging directory. Existing backup collisions
  preserve a non-identical legacy file as `<name>.pre-migration.<migration_id>`;
  only a byte-identical legacy copy may be removed. An explicit-root layout
  whose legacy and destination roots are equal is never migrated onto itself.
- **Conflict.** A legacy database and a destination database that the journal
  does not explain are two independent stores. Nothing is copied, merged,
  renamed or deleted (WAL/SHM included); the open fails with
  `DataError::StoreMigrationConflict { legacy, current }`, whose message names
  both paths, says both were preserved, and tells the user to stop awman, back
  both up, and move (not delete) the one they do not want to keep out of the
  way before retrying. Foreground commands fail the same way; they do not run
  on either database.

### Durable completion before publication (A05)

`AutomationHost` publishes through a typed `LifecyclePublisher`, not a
fire-and-forget closure: `reserve(payloads) -> LifecycleProjection` assigns the
next `<attempt>:<sequence>` cursors without writing, `publish(&projection)`
appends to `events.log`, waits for the flush acknowledgement and only then fans
out to live subscribers, and `emit(payload)` remains for non-terminal progress.

A turn completion (headless and guarded alike) is, in order:

1. Bound the result. A serialized result above the existing 8 MiB store limit
   becomes a `failed` turn with code `result_too_large`, no final message and
   `completion_verified = false`; it is never stored truncated as success.
2. `reserve` the terminal events (at most 8, at most 16 MiB: the store's
   existing projection bound).
3. For a headless final turn, one SQLite transaction commits the turn result,
   the execution transition to `ended`, the command's automation projection and
   `pending_events_json` = that projection. An interactive turn commits its turn,
   active execution snapshot and pending batch together; later turns remain
   possible. Its ended snapshot, assurance and command result commit separately
   before the final lifecycle publication.
4. `publish` the projection; on acknowledgement `clear_pending_events(cursor)`.
5. Finalize the command row and return the result. The existing
   `pending_events_json IS NULL` guards keep a command from finalizing earlier.

Profile export/writer cleanup has a separate durable
`agent_profile_cleanup` pending/complete/failed journal registered before launch.
Native turn facts stay immutable. Only this owned post-execution outcome can
downgrade command/execution cleanup, preserving an existing primary failure.
Command finalizers refuse pending cleanup; foreground completion waits for
`ProfileSession::leave`. Restart treats unresolved ownership as unconfirmed,
never upgrades it to success. The synchronous controller uses the same ordered
flush/sync acknowledgement protocol without waiting on an async writer task.

Failure handling:

- Any failed write before step 3 commits (delivery mark, observation, revision
  CAS, the transaction itself) stops the run: the agent is cancelled, owned
  resources are released, no completion event is published, and the caller gets
  a storage error and a non-zero exit with `completion_verified = false`.
- A failure in step 4 leaves the committed row with its pending projection. The
  running process returns a storage error ("recorded, not yet published") and
  writes no contradictory failure record. `ApiCommandLog::recover_lifecycle`
  publishes the batch exactly once on the next open (cursor and content match
  replays harmlessly; a conflict fails), then the command is finalized from the
  committed state. Replay and reconnect therefore always agree with the
  database. Recoverable failures also run this reconciliation in-process.
  Permanent storage failure remains recovery-required and is exposed by an
  id-less `operation_error` SSE frame with `storage_error`, followed by stream
  closure. It allocates no replay cursor, asserts no durable command completion,
  and tells the caller to repair storage/restart without resubmitting the task.
- Task input is delivered at most once. No failure or recovery path re-queues
  a command or re-sends a turn; an unacknowledged turn becomes
  `delivery_unknown` and an acknowledged one `execution_lost`, as before.

### Phase transcripts and startup grace (A08)

`ResolvedContainerOptions::{startup_grace, full_transcript}` are part of the
backend-neutral launch contract and every container-tier backend, builtin
included, honours them. `StartupGrace(Duration::MAX)` disables the first-byte
timeout only; cancellation and overall deadlines still apply.

A full transcript is spooled to disk, not held in memory:

- **Location and privacy.** An unnamed, delete-on-close file created with
  `tempfile::tempfile_in` inside `<data home>/spool/transcripts/`, a directory
  created and checked with `private_files::create_private_dir` (owner-only). The
  bytes written are the already-redacted stream.
- **Memory bound.** At most the default 100-line tail plus a 64 KiB write
  buffer per transcript stays in RAM. Consumers read the spool through a
  reader; no interface returns a spooled transcript above 1 MiB as one
  in-memory string.
- **Cleanup.** The file has no name, so it disappears when the owning
  execution drops it or the process dies; there is nothing to sweep.
- **Storage failure.** A write error, or reaching the 256 MiB per-step cap,
  stops spooling and is reported: the transcript handed to failure reports and
  remediation agents starts with
  `[awman: transcript incomplete after <n> bytes: <error>; the last 100 lines follow]`,
  and a warning is shown to the user. The step keeps its real exit status. A
  directory that cannot be made private fails the step before it starts. There
  is no silent tail-only fallback.

### Remote event streams (A13–A15)

One bounded decoder, `SseDecoder` (Layer 1, `engine::remote`), frames bytes
before any text decoding:

- Frames end at a blank line; `LF` and `CRLF` line endings, `data:` with or
  without one leading space, multi-line `data`, `id:`, `event:` and comment
  lines are supported. UTF-8 is decoded per complete frame, never per HTTP chunk.
- `MAX_SSE_FRAME_BYTES = 16 MiB` (twice the largest storable turn result). The
  buffer never holds more than one frame plus one read chunk. An oversized
  frame ends the follow with `EngineError::RemoteProtocol`; it is not retried,
  because replay would resend the same frame.
- A complete frame that is not valid UTF-8 or not a decodable event is a typed
  `Malformed` outcome: it is counted, reported to the user once at the end
  ("N events could not be decoded"), its `id:` still advances the cursor, and
  the stream continues.

A stream ends in exactly one of `Done` (the terminal event was delivered),
`Detached` (local cancellation; the server job is not cancelled and no request
is sent) or `Incomplete` (EOF or transport error before `Done`, including EOF
inside a frame). A transport ending is never treated as the command ending.

- Cancellation is a `CancellationToken` selected against connection
  establishment, every stream read and every reconnect delay.
- `Incomplete` reconnects with `Last-Event-ID: <attempt>:<sequence>` of the
  last event the sink accepted. The command is never resubmitted. Retries are
  bounded to 5 consecutive attempts that deliver no new event, with delays of
  2, 4, 8, 16 and 30 seconds; progress resets the count. Exhaustion reports an
  incomplete follow (non-zero exit) with the status command to use.
- The server's replay rule is unchanged (`ReplayFrom`): a kept attempt replays
  strictly after the cursor; an unknown, purged or future attempt replays the
  current attempt from its start. The client suppresses any event whose cursor
  is not greater than the last delivered one. Frames without an `id:` are
  live-only: delivered, never used as a cursor.

### ACP queue bounds (A25)

Every ACP queue is bounded by items and by bytes (the byte cost of an item is
the length of the wire line it came from). No queue is unbounded.

| Queue | Items | Bytes | When full |
|---|---|---|---|
| transport stdout (container → client) | 1024 | 8 MiB | reader stops reading (backpressure) |
| transport stdin (client → container) | 64 | 8 MiB | sender waits; cancellable |
| transport stderr (diagnostics) | 1024 | 1 MiB | oldest dropped, with a counted "N bytes of agent stderr dropped" notice |
| bulk updates (session updates, output chunks) | 1024 | 8 MiB | coalesce, then backpressure |
| control (permission requests, responses to client requests, agent-initiated requests) | 64 | 8 MiB | `EngineError::AcpOverflow { queue }`: session cancelled, explicit failure |
| pending client → agent requests | 64 | — | new request fails with `AcpOverflow` |

8 MiB equals `MAX_ACP_LINE_BYTES`, so one maximum-size valid line always fits;
worst-case queued memory per session is under 48 MiB including the framer.
The framer and bounded decode-ahead share 8 MiB (plus one read chunk), so
control and RPC responses can be decoded while bulk remains saturated within
that window. Unread RPC responses retain control credit until consumed/dropped.
A merged update must fit its byte budget; an oversized update fails with
`AcpOverflow { queue: "bulk" }`. Other surfaced queue names are
`transport_stdout`, `control`, and `pending_requests`; `stdin` is used only
for a non-waiting teardown send. Outbound lines beyond the line cap are refused
before charging a queue. A bridge unable to wait (currently builtin stdout)
charges before enqueue and fails with `transport_stdout` overflow. Docker/Apple
readers backpressure. Stderr ingress drops/counts new chunks while its pump is
stalled; the bounded diagnostic ring drops oldest. Both are counted and bounded.
Stdin credit returns after a reporting bridge writes to the child; builtin
currently releases on client hand-off. Teardown sends session/permission
cancellations only when stdin space is immediately available, then cancels the
transport and execution without waiting for a wedged peer.

- Control and bulk are separate queues. The driver polls control first, and it
  never waits for a control message without draining bulk at the same time, so
  a saturated bulk queue cannot block a permission reply, a prompt result or
  protocol progress.
- Only replaceable state is coalesced (latest value wins for the same key:
  plan, tool-call status, usage, mode/command lists). Message and output
  chunks, permission requests and results are never dropped; when they cannot
  be queued the reader stops reading and the container blocks on its pipe.
- Cancellation never depends on queue space: every blocked send and receive
  selects on the session's cancellation token, and cancelling stops the reader
  task and releases what is queued.
