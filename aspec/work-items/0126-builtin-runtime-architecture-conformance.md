# Work Item: Task

Title: Bring the builtin runtime into conformance with the grand architecture
Issue: n/a (2026-09-30 builtin-runtime architecture review)

Status: Open
Follows: [WI 0123](completed/0123-builtin-native-verification-and-release-closure.md),
[WI 0125](0125-builtin-image-source-builder.md) (the image-source builder,
implemented on branch `awman/work-item-0125`; this item brings that code into
conformance too, and folds it into Step C1's image acquirer)

## Summary:

- A review of the `builtin-experimental` runtime against
  `aspec/architecture/2026-grand-architecture.md` found no hard layer
  violations: no upward imports, and no Layer 2/3 code that matches on the
  concrete runtime. It did find breaks with the architecture's intent and some
  drift:
  - builtin settings are resolved once per process instead of per `Session`;
  - an unwritten frontend-ownership contract caused a production panic;
  - several pieces are duplicated and have diverged;
  - filesystem and persistence code sits in the engine;
  - the spec does not describe the builtin runtime at all.
- This item fixes all of it, in six groups. Each group is its own PR series,
  one step per commit:
  - **A** — per-session builtin settings (security-relevant);
  - **B** — frontend ownership redesign (fixes the panic class);
  - **C** — de-duplication and a single source of truth;
  - **D** — layer relocations and the worker crate;
  - **E** — Tenet 3 typing and async hygiene;
  - **F** — errors, frontends and spec/docs.
- Implementing agents MUST read `aspec/architecture/2026-grand-architecture.md`
  and `aspec/architecture/four-layer-summary.md` in full before starting.
  When the right layer for something is ambiguous, ask the developer; do not
  guess.
- Line numbers below are as of commit `ac771426`, **before** WI 0125 (builder)
  landed, and will have shifted. Re-locate each one by symbol. WI 0125's code
  is not cited by line at all: Step C0 inventories it first.
- WI 0125 is in scope. Every group below that touches image acquisition,
  config, `ready`, errors or frontends must cover the builder's code as well
  as the pre-existing builtin code. WI 0125's tests are a regression suite for
  this item: they must keep passing (moved or retyped as needed) without
  weakening their assertions.

### Decisions already made by the developer (2026-09-30)

1. **Per-session settings are launch options, not one runtime per session.**
   - Process-wide: one builtin runtime (state dir, embedded runtime,
     ambient-override check).
   - Per launch: network policy, vCPUs and memory, resolved from each
     `Session`'s `EffectiveConfig`.
2. **Builtin stays in the container tier.** The grand-architecture doc is
   amended so tiers are defined by capability shape, not by isolation
   technology.
3. **Frontend ownership is redesigned, not just documented.** Callers never
   give up ownership of their frontend to a runtime, so no
   `Arc::try_unwrap` reclamation is needed anywhere.
4. **The builtin worker and `apple_xpc` move into a new workspace crate.**
   `main.rs` makes a single dispatch call.

## User Stories

### User Story 1:
As a: user with a repo that narrows the builtin network policy

I want to:
Have that repo's `builtin.network`, `vcpus` and `memoryMib` apply wherever an
agent for that repo runs: CLI, a TUI tab opened on it, an API session or a
squad task.

So I can:
Rely on the repo-level narrowing as the security control it is documented to be.

### User Story 2:
As a: user running dynamic or static workflows on any runtime

I want to:
Workflows never to panic when a runtime holds its frontend a little longer
than expected.

So I can:
Use `exec workflow --dynamic` on `builtin-experimental` reliably.

### User Story 3:
As a: maintainer

I want to:
The builtin runtime to follow the same layering, typing and single-source-of-
truth rules as the rest of awman, with a spec that describes it.

So I can:
Extend it (WI 0124) and maintain the WI 0125 builder without adding to known
drift.

### User Story 4:
As a: user of the builtin runtime with `builtin.builder.enabled`

I want to:
`awman ready`, workflow preflight and squad evaluation to build and import
images by one rule, with the same warnings and results in every frontend.

So I can:
Trust that whichever command prepares an image, it was built (or skipped with
a warning) and imported exactly as `ready` would have done.

## Implementation Details:

### Group A — Per-session builtin settings (High, security-relevant)

**A1. Resolve network, vCPU and memory per launch.**
- Problem today:
  - `BuiltinRuntimeSettings::resolve` (`src/engine/container/runtime.rs:98-114`)
    fixes vCPUs, memory and the compiled network plan when the backend is
    built (`src/engine/container/builtin/backend.rs:241-280`).
  - For the TUI and API, that happens once, from the launch directory's
    config (`src/command/startup.rs:79`), and every tab or session reuses it
    (`src/frontend/tui/app.rs`, `src/frontend/api/.../runtime.rs`).
  - The daemon path uses `agent_runtime::detect(&global)` and never sees repo
    config (`src/command/dispatch/mod.rs:184`).
- Change:
  - Split the settings into a **process-wide** part and a **per-launch** part:
    - Process-wide `BuiltinRuntimeSettings`: `state_dir`, `ambient_overrides`,
      `test_isolation`, `image_source`/`images`/`registries` (until C1 moves
      those).
    - Per-launch `BuiltinLaunchSettings`: `vcpus`, `memory_mib`, resolved
      `network`.
    - `builtin.builder.enabled` (WI 0125) is in neither part. It is an
      acquisition setting that WI 0125 already resolves per session (repo
      overrides global). Do not move it into process-wide state; C1 makes it
      an input of the session's `ImageAcquirer`. If WI 0125 reads it anywhere
      from process-wide settings or `GlobalConfig` (e.g. a daemon path), fix
      that here with the same per-session rule.
  - Carry the per-launch part as a typed field of `ResolvedContainerOptions`.
    It is populated by Layer 2 from the session's `EffectiveConfig` through a
    typed option, not a `pub fn`. Other container backends ignore it.
  - `BuiltinBackend::plan()` compiles the network plan and sizes the VM from
    the launch settings. Remove the `runtime.rs:84` "there is no per-launch
    override" behaviour and its comment.
  - Daemons (squad) resolve per-launch settings from each task's own session
    config, not from `GlobalConfig`.
  - Validation still fails closed. A repo block that would widen the global
    policy is a configuration error at launch, surfaced as a typed error.
- Tests:
  - Two sessions from two repos with different `builtin.network` blocks share
    one `Engines`; each launch's compiled plan matches its own repo.
  - A squad task inherits its repo's narrowing.
  - A squad task and a TUI tab honour their own repo's `builtin.builder.enabled`
    (repo `false` over global `true`, and the reverse).
  - Unit tests of the options mapping.

**A2. Drop the "informational" merged network.**
- `BuiltinRuntimeConfig::merged_over` (`src/data/config/builtin_runtime.rs:84`)
  carries `network: self.network.or(base.network)`, commented as
  informational only, on a struct the engine reads.
- Leave it out of the view the engine consumes, so nobody can read an
  unenforced value.
- WI 0125 added a `builder` field to the same struct and merge. Check that its
  merge is field-wise (`enabled` inherits only when omitted, explicit repo
  `false` wins) and that it is not in the backend's view either.

### Group B — Frontend ownership redesign (High)

**B0. Land the exec-bridge release fix.**
- The builtin exec bridge now drops the boxed frontend before signalling
  `done`, and the regression test `frontend_is_released_before_wait_resolves`
  (multi-thread runtime, slow-drop frontend) covers it. Both landed on `main`
  in `7da4760c` (the pre-worktree commit), ahead of WI 0125.
- Confirm they survived the WI 0125 merge; if not, re-apply them as the first
  commit. They are the stop-gap for the `dynamic.rs:670` "no other Arc
  references remain after leader phase" panic.

**B1. Stop reclaiming the frontend with `Arc::try_unwrap`.**
- Problem today: `ExecWorkflowCommand` wraps its
  `Box<dyn ExecWorkflowCommandFrontend>` in `Arc<Mutex<…>>` (13 occurrences),
  hands clones to runtimes through `AgentFrontendProxy`
  (`src/command/commands/exec_workflow/factory.rs:19`), then `panic!`s if it
  cannot `Arc::try_unwrap` it afterwards:
  - `src/command/commands/exec_workflow/prepare.rs:719`
  - `src/command/commands/exec_workflow/dynamic.rs:669`
- Change:
  - Introduce a Layer 2 handle type, e.g. `SharedWorkflowFrontend`, that owns
    the `Arc<Mutex<Box<dyn ExecWorkflowCommandFrontend>>>` for the whole
    command lifetime. It exposes the methods the command needs (`with(|f| …)`,
    `set_pty_active`, `replay_queued`, …) and is cheap to clone.
  - Every phase — preflight, leader, engine block, `execute_generated_workflow`,
    post-run worktree prompts — takes `&SharedWorkflowFrontend` (or a clone)
    instead of an owned `Box`. The functions that take `&mut dyn
    ExecWorkflowCommandFrontend` today (e.g. `dynamic.rs:725`,
    `prepare.rs:95`) receive it through the handle.
  - Delete both `try_unwrap` sites and the "scoped so proxy + factory are
    dropped before we reclaim" comments and scoping (`prepare.rs:348-349`).
  - `AgentFrontendProxy` keeps holding a clone. A runtime that keeps its clone
    past `wait()` is then harmless.
  - Apply the same pattern anywhere else a command reclaims a frontend after
    handing it to a runtime. Audit `chat`, `exec prompt`, `specs`, `new`,
    `config`, `squad` and `phase_container`.
- Tests:
  - Workflow and dynamic-workflow command tests with a fake runtime that
    deliberately holds its frontend clone until after `wait()` resolves (and
    one that never drops it). Both complete without panicking, and messages
    are still delivered.

**B2. Document the engine-side contract anyway, and test it on every runtime.**
- After B1 nothing *depends* on early release, but a runtime that holds a
  frontend forever still leaks the PTY/IO sinks.
- On `AgentInstance::run_with_frontend` and `AgentExecution::wait`
  (`src/engine/agent_runtime/execution.rs:70-75`, `:204`), document:
  "a runtime MUST drop the frontend no later than when `wait()` resolves (or
  when the execution is dropped)".
- Add one cross-runtime conformance test modelled on the builtin
  `SlowDropFrontend` test, run against Docker/Apple (`process.rs`), dsbx
  (`src/engine/sandbox/dsbx/backend.rs:582`, which holds it as
  `DSbxExecution.sink`) and builtin. Use fake drivers where a daemon is
  unavailable.
- Fix dsbx so that dropping the execution without waiting releases the sink.

### Group C — Single sources of truth and de-duplication (Medium)

**C0. Inventory the WI 0125 builder before changing anything.**
- WI 0125 was implemented against the pre-refactor structure, so its code is
  spread across the same paths this item restructures. Before C1, list in the
  PR description, by symbol, where the merged builder code:
  - parses, merges, validates and exposes `builtin.builder.enabled`
    (`src/data/config/builtin_runtime.rs`, config field metadata,
    `config show`/`config set`);
  - normalises `docker-store`→`docker-local` and `apple-store`→`apple-local`,
    including cache-key and persisted-identity compatibility
    (`src/data/config/image_source.rs`, `src/engine/oci/`);
  - decides builder eligibility from the resolved source and endpoint
    (expected in or near `src/engine/oci/resolve.rs`);
  - plans and runs builds, orders the base image per store, tags at the
    source reference and forces re-acquisition (expected in
    `src/engine/ready/mod.rs`);
  - reaches the Docker/Apple build code (`docker.rs`, `apple.rs`, the host
    CLI backend);
  - emits the incompatible-source warning and the separate build/import
    results, and how each frontend renders them;
  - is (or is not) reached from workflow preflight and squad evaluation.
- Flag every place where WI 0125 duplicated logic from the Layer 2 import
  path or from `ReadyEngine` instead of sharing it. Those are C1 inputs.
- No code changes in this step; it is the map the later steps work from.

**C1. One engine-level image acquirer, including the builder.**
- Problem today:
  - `import_agent_image` in `src/command/commands/workflow_preflight.rs:498-541`
    (Layer 2) returns early on `runtime.image_exists(&tag)`.
  - `ReadyEngine` (`src/engine/ready/mod.rs:810-850`) deliberately never skips
    acquisition and checks provenance.
  - They use different Dockerfile-path hints (`paths.project_dockerfile()` vs
    `repo_config().dockerfile_path_or_default()`).
  - Six Layer 2 call sites build
    `ImageSources::from_config(&….effective_config().builtin_runtime())`:
    - `src/command/session_setup.rs:448`
    - `src/command/commands/ready.rs:242`
    - `src/command/commands/exec_workflow/dynamic.rs:525`, `:854`
    - `src/command/commands/squad/evaluation.rs:541`, `:779`
  - `ReadyEngine` silently treats sources as empty if a caller forgets
    `.with_image_sources` (`ready/mod.rs:60`).
  - WI 0125 added build-then-import behaviour for `builtin-experimental`
    (source-matched builder, per-store base-image ordering, forced
    re-acquisition after a build, incompatible-source warnings). Wherever it
    lives after C0, it was layered onto the paths above rather than onto one
    shared acquirer, so the "one rule" problem now covers building as well
    as importing.
- Change:
  - Add a typed Layer 1 `ImageAcquirer`, constructed from the session's
    effective config with its image sources **and** its builder policy
    (`builtin.builder.enabled`). Both are required constructor arguments, not
    optional builder calls.
  - It owns the whole WI 0125 lifecycle for one image:
    1. resolve the source (`builtin.images[tag]` before
       `builtin.imageSource`), with aliases already normalised;
    2. derive builder eligibility from that resolved source and endpoint
       (local `docker-local` socket/pipe → Docker, `apple-local` → Apple
       Containers, anything else → incompatible);
    3. decide whether a build is due, using the matching runtime's ordinary
       readiness rules plus `--build`/`--no-cache`/audit triggers;
    4. build through the matching host backend, or emit the
       incompatible-source warning once and skip;
    5. acquire with `ReadyEngine`'s semantics (provenance-checked, never
       bypassed), forcing fresh acquisition after any successful build;
    6. return a typed result that keeps build and import outcomes separate.
  - Express the builder eligibility decision as a typed Layer 0/1 value (e.g.
    `BuilderSelection::{Docker(endpoint), Apple, Incompatible(kind), Disabled}`)
    computed in one place next to source resolution in `oci/resolve.rs`. No
    other module re-derives it from the source kind.
  - Per-store base-image deduplication and same-store/reference
    serialisation live inside the acquirer (or a type it owns), so every
    caller gets them, not only `ready`.
  - `ReadyEngine`, workflow preflight, squad evaluation and `AgentEngine`
    (`src/engine/agent/mod.rs:143`) all use it.
  - Remove the Layer 2 `import_agent_image`, the six `from_config` sites, and
    any builder-specific glue WI 0125 added in Layer 2 or in `ReadyEngine`
    that the acquirer now owns.
  - Use one Dockerfile-path rule, for both the build context and the
    provenance check.
- Policy for non-`ready` callers: preflight, squad evaluation and
  `AgentEngine` must follow WI 0125's rule ("do not silently broaden which
  commands trigger automatic builds"). The acquirer takes an explicit
  `BuildPolicy` from its caller (e.g. `ready` passes its flags; preflight
  passes whatever WI 0125 shipped for preflight). Record in C0 what WI 0125
  shipped for each caller and preserve it; if it differs between callers
  without a stated reason, ask the developer before unifying.
- Tests:
  - Preflight/ready parity on the same source and tag, with the builder on
    and off.
  - A recording builder/importer proves each caller goes through the
    acquirer: zero builder calls for incompatible sources, one warning per
    image per run, forced re-acquisition after a build.
  - Move WI 0125's ready-builder tests onto the acquirer without weakening
    them.

**C2. One `MSB_*` block list.**
- Problem today:
  - `src/engine/container/builtin/embedded/mod.rs:6-24` and
    `src/engine/container/builtin/worker.rs:19-33` each hard-code 13
    variables.
  - `AMBIENT_MSB_OVERRIDES` (`src/data/config/env.rs:131`) and
    `aspec/uxui/cli.md:208` list only 5.
  - `builtin::open` checks the 5, so the other 8 fail later through a
    different error path.
- Change:
  - Make `AMBIENT_MSB_OVERRIDES` the single list of all 13.
  - `embedded::resolve` consumes `settings.ambient_overrides` and does no env
    reads of its own. The worker crate (D1) takes the list from `data`.
  - Update `cli.md` and the user docs.

**C3. One version constant.**
- `"0.7.2"` appears in `naming.rs:3`, `worker.rs:55`, `embedded/mod.rs:52` and
  `embedded/version.rs:8`, and the libkrun commit appears in `worker.rs:55`.
- Derive all of them from `embedded/version.rs`.

**C4. One attach wire protocol.**
- Problem today:
  - `exec_bridge.rs:28-35` defines `STDOUT=0, INPUT=1, RESIZE=2, STDERR=3,
    EXIT=4, FAILED=5`, its own `MAX_FRAME`, `read_frame` and `write_frame`.
  - `src/engine/container/attach_socket.rs:46-57` uses the same
    `tag|u32 LE|payload` shape with `LAGGED_FRAME=3`.
  - The socket paths also differ (`BuiltinPaths::attach` vs
    `attach_socket_path`).
- Change:
  - Create one `attach_protocol` module with a single tag enum covering
    output, stdin, resize, stderr, exit, failed and lagged, plus shared frame
    codec, limits and bounded-write helpers.
  - Build both servers and clients on it.
  - Decide on one socket path resolver, or document why two are needed.

**C5. One agent argv builder.**
- `backend.rs:134` `agent_argv` mirrors `docker.rs:1057-1133`, and they
  already differ: builtin errors when an allowed/disallowed-tools flag is
  missing (`backend.rs:158`), Docker skips it (`docker.rs:1069`).
- Extract a pure `AgentArgv::from_options(&ResolvedContainerOptions)`.
- Pick the behaviour for a missing flag. The default is the fail-closed error,
  because silently dropping a tool restriction is the less safe outcome.
  Confirm with the developer if the Docker behaviour must stay.

**C6. Shared output/stuck plumbing.**
- `exec_bridge::run` re-wires activity, first-byte and output-tail handling
  that `io_bridge::BridgeConfig` already owns, and calls
  `spawn_stuck_detector` with six positional arguments (`exec_bridge.rs:183`,
  `io_bridge.rs:103`).
- Extract a byte-stream-agnostic `OutputPump` or
  `StuckDetector::spawn(StuckConfig)` in `io_bridge`, and use it from both
  paths.

**C7. Capability flags have production readers or are removed.**
- Problem today:
  - `arbitrary_env_vars`, `cpu_limits`, `per_resource_stats`,
    `persistent_lifecycle`, `host_paths_visible`, `session_label_supported`
    and `dind` are only asserted in tests.
  - `exec_args`/`host_cli` sit on the cross-paradigm trait
    (`src/engine/container/backend.rs:92`, `agent_runtime/mod.rs:170,197`),
    even though `design.md` calls Docker-shaped argv private to the CLI
    backends.
- Change:
  - For each unused flag, either wire the Layer 2 decision that should branch
    on it (per the doc: "paradigm-specific decisions branch on
    `capabilities()`") or delete it. Record which in the commit message.
  - Move `exec_args`/`host_cli` to a `HostCliBackend` sub-trait that builtin
    does not implement.
  - The WI 0125 builder calls into the Docker/Apple build code while the
    selected runtime is builtin. It must reach those through a narrow,
    build-only handle that the acquirer is given (e.g. a `HostImageBuilder`
    trait implemented by the Docker and Apple backends), not by holding a full
    `ContainerBackend`/`HostCliBackend` or by matching on the concrete runtime.
    Agent execution stays unreachable from that handle.
  - `capabilities().image_acquisition` must stay truthful once the builder
    exists: builtin can now build for compatible sources when enabled. Either
    make the capability reflect the session's builder selection, or keep it
    static and have Layer 2 read the acquirer's `BuilderSelection` instead.
    Pick one and record it; F2 depends on the answer.
- Document which `AgentStatus` values a frontend can expect. `Exited` is
  currently emitted only by builtin (`exec_bridge.rs:219`); emit it from every
  backend or document why not.

### Group D — Layer relocations (Medium)

**D1. Workspace crate for the worker and `apple_xpc`.**
- Problem today: `main.rs:21-26` pulls in
  `engine/container/builtin/worker.rs` and `engine/oci/apple_xpc.rs` via
  `#[path]` to get around `#![forbid(unsafe_code)]` in `lib.rs`. Specifically:
  - `worker.rs:201-215` uses `unsafe` for `fcntl` and `fstat`;
  - `worker.rs` also parses its own arguments, embeds the kernel, calls
    `eprintln!`/`process::exit`, and prints version JSON.
- Change:
  - Convert the repository to a Cargo workspace. Add a small crate (e.g.
    `crates/awman-host-shims`, or two crates if cleaner) that allows the
    minimal `unsafe`, with `// SAFETY:` comments. It owns the builtin worker
    entry point and the Apple XPC transport.
  - `main.rs` makes one call, e.g.
    `if awman_host_shims::dispatch_worker(...) { return … }`, plus the
    existing `install_transport` injection.
  - The crate depends on `awman` only for Layer 0 constants (C2/C3). It must
    not reach into engine internals; inject anything else through a trait.
  - Keep `awman`'s `forbid(unsafe_code)`.
  - Update `Makefile`, `Dockerfile.dev`, the CI build/test matrix and the
    `builtin-runtime` feature wiring so `make all`/`make test` and the static
    build still produce one `awman` binary.

**D2. Move OCI cache persistence to Layer 0.**
- `src/engine/oci/cache.rs` owns filesystem work that Layer 0 should:
  - serialized `CacheRecord` (`:71`) and `RefRecord` (`:115`);
  - `CACHE_DIR` (`:57`);
  - `rename`/`remove`/`flock` primitives;
  - `SourceFingerprint::of` (`:99`, `std::fs::metadata`).
- Move the record types, the layout constants and the atomic read/write/lease
  primitives into `src/data/oci_cache/`, next to `oci_identity.rs`.
- Keep acquisition, verification and prune policy in `engine/oci`.
- WI 0125's legacy-alias compatibility for cache keys and persisted
  identities (`docker-store`/`apple-store` records read as their canonical
  `docker-local`/`apple-local` kinds) moves with the record types. Old caches
  must still hit without re-acquisition after the move; keep WI 0125's
  cross-spelling cache tests and run them against the Layer 0 types.

**D3. Move the builtin state-dir layout to Layer 0.**
- These are filesystem layout, persistence and path resolution:
  - `BuiltinPaths::resolve` (`paths.rs:16-38`, creating
    run/cache/logs/sandboxes/db);
  - `secure_create` (`:176-180`);
  - lock files (`:78`);
  - `attach()` socket paths (`:118`);
  - the runtime-version marker in `catalog.rs:10-50`.
- Add `src/data/builtin_state.rs` (modelled on `worktree_paths.rs`) to own
  them.
- The engine keeps socket-budget checks and SDK-specific policy.

**D4. Resolve builtin config in Layer 0.**
- Replace the validation and defaulting inside
  `BuiltinRuntimeSettings::resolve` (Layer 1) with
  `EffectiveConfig::builtin_settings() -> Result<ResolvedBuiltinConfig, DataError>`.
  It returns both the process-wide and per-launch parts from A1, with defaults
  and the network narrowing applied, plus the resolved acquisition inputs C1
  needs: normalised image sources and `builder.enabled`.
- Fold WI 0125's `builtin.builder.enabled` validation (boolean only, error
  names the key) into the same Layer 0 resolution rather than leaving it in a
  separate validator.
- The engine only consumes it.
- Reconcile the contradictory comments at `builtin_runtime.rs:4-5` and
  `effective.rs:246`.

### Group E — Tenet 3 typing and async hygiene (Medium/Low)

**E1. Typed objects instead of OS-touching free functions.**

| Now | Becomes |
|---|---|
| `exec_bridge::run` (7 positional args, incl. untyped `(Arc<dyn SandboxDriver>, String, bool)`) | `ExecBridge::new(ExecBridgeOptions { handle, session, cleanup: Option<CleanupTarget>, attach_path, persistent_stdin, seeded_prompt, leases, io }).run(frontend)` |
| `exec_bridge::serve` | `AttachServer::bind(path, &session)` (the `Server` struct already exists) |
| `exec_bridge::cancel` | method on `CancelTarget` |
| `paths::check_socket_budget`, `paths::secure_create` | methods on the D3 state type / `BuiltinPaths` |
| `catalog::check` | `Catalog` type or a method on the state type |
| `embedded::resolve` | `EmbeddedRuntime::resolve(&settings)` |
| `oci::cached_image_config`, `oci::prune_cache` | callers use `OciCache::open(..)?.…` |
| `oci::archive::validate_archive` (5 args) | `ArchiveValidator` |
| `ContainerBackend::build_image` / `exec_in_background_streaming` / `start_background` (4–5 args) | request structs; the C7 `HostImageBuilder` takes the same `BuildRequest` |
| Any free functions WI 0125 added for builder eligibility, build planning or forced re-acquisition (see C0) | methods on `ImageAcquirer` / `BuilderSelection` (C1) |

- Pure helpers (`network::compile`, `resources::resolve`, `naming::*`) stay
  as functions; they are within the tenet's allowed exceptions.

**E2. Remove the double `take_io`.**
- `instance.rs:46` calls `frontend.take_io()` only to read `initial_size`, then
  wraps the frontend in `CapturedFrontend`, whose `take_io` is
  `expect("I/O taken once")` (`instance.rs:120`).
- Pass `AgentIo` straight into `ExecBridgeOptions` (as Docker's `SpawnRequest`
  does) and delete `CapturedFrontend`'s I/O juggling.
- Replace `backend.rs:399` `expect("launch plan owns its labels")` with
  `ok_or(...)`.

**E3. No blocking calls on async worker threads.**
- `run_with_frontend` must run inside Tokio (`exec_bridge` spawns tasks), but
  it blocks in two places:
  - it boots the VM through `MsbDriver::call`, which blocks on `rx.recv()`
    (`msb_driver.rs:118-128`);
  - `paths.rs:107` sleeps in a lease-wait loop reached from `build()`.
- Run these through `spawn_blocking` or make the driver async. Whichever you
  pick, keep `AgentInstance::run_with_frontend` synchronous.
- `exec_bridge.rs:101-108` spawns an OS thread that polls with `sleep` on
  every cancel, and the stuck detector and a user cancel can each start one.
  Replace this with a single `tokio::time`-based cancel task per execution.

### Group F — Errors, frontends, spec and docs (Medium/Low)

**F1. Typed, runtime-neutral errors.**
- Problem today:
  - `src/frontend/cli/mod.rs:266-268` maps every `EngineError::Container` to
    "awman requires Docker; install Docker Desktop…". Builtin raises that
    variant, e.g. for "no live attach endpoint" at `exec_bridge.rs:407`.
  - The TUI prints the raw error, so the two frontends disagree.
  - The CLI also lists twelve builtin/OCI variants just to `format!("{e}")`
    them (`cli/mod.rs:326-338`).
- Change:
  - Add builtin-specific variants (e.g. `BuiltinAttachUnavailable`).
  - Move all user guidance into `EngineError`'s `Display`, or into a
    `user_message()` in the engine, so all frontends render identical text.
  - Cut the CLI table down to formatting only, with a default arm.
- Engine error strings that embed command hints should become typed variants
  whose hints are rendered consistently:
  - `backend.rs:227` ("run awman ready…");
  - `msb_driver.rs:34` ("awman clean");
  - `mod.rs:119`;
  - `oci/resolve.rs:69` `external_build_hint` (WI 0125 likely changed this
    hint now that builtin can build; re-check its wording against the builder
    setting).
- WI 0125's builder outcomes must be typed variants too, not strings:
  - builder unavailable (missing CLI, daemon down, unsupported Apple host);
  - build failed / cancelled;
  - build succeeded but import/install failed;
  - the incompatible-source warning (a typed warning on the existing message
    path, naming image, source kind and the continued import, using only
    canonical source names).
  All three frontends render identical text for each.

**F2. The ready hint comes from lower layers.**
- `src/frontend/cli/per_command/ready.rs:65-73` and
  `src/frontend/tui/per_command/ready.rs:54-62` each compute `has_missing` and
  hard-code "build or import". The API (`api/command_frontend.rs:1050`) shows
  nothing.
- Add `ReadySummary::hint() -> Option<String>` (Layer 0), choosing the
  build/import wording when the summary is produced. All three frontends
  render it.
- With WI 0125, "build or import" for builtin depends on the session's
  builder selection, not only on the runtime. The summary records, per
  missing image, whether `awman ready` would build it (compatible source,
  builder enabled), import it, or needs a source configured, and the hint is
  derived from that (see the C7 decision on `image_acquisition`).
- If WI 0125 added its own hint text in a frontend, remove it in favour of
  this one.

**F3. `ReadyPhase` display parity.**
- The TUI prints `ready: {phase:?}` (`tui/per_command/ready.rs:45`), the API
  prints `Ready phase: {phase:?}` (`api/command_frontend.rs:1047`), and the
  CLI suppresses the phase.
- Give `ReadyPhase` a `Display`, as `ReadyStep` has, and make one consistent
  choice across all three frontends.
- This includes the separate build and import phases/results WI 0125 added.
  Build success followed by import failure must read the same way in the CLI,
  TUI and API, and the API must expose both outcomes in its structured output.

**F4. Update the grand architecture and the summary.**
- In `2026-grand-architecture.md`:
  - Redefine the tiers by capability shape: OCI images, ephemeral lifecycle
    and host mounts make the container tier; a persistent per-session VM with
    its own lifecycle makes the sandbox tier.
  - Add the builtin backend to the container-tier driver list (`:54-56`).
  - Update the runtime-selection bullet (`:61`) to `detect_effective(&EffectiveConfig)`
    and all four runtime strings, including `builtin-experimental`.
  - Note that builtin's per-launch settings (A1) are session-scoped.
  - Describe the WI 0125 builder as a source-matched build step owned by the
    Layer 1 `ImageAcquirer`: builds are delegated to the host Docker/Apple
    build integration through a build-only handle, agent execution never
    leaves the builtin runtime. Confirm `security.md` and `cli.md` already
    carry WI 0125's import-only exception; fix them if not.
- Update `four-layer-summary.md` to match.
- Record the B1 frontend-ownership rule in the Layer 2 section: commands own
  their frontend through a shared handle and never reclaim it.

**F5. Document `AWMAN_BUILTIN_STATE_DIR`.**
- It is first in the state-dir precedence (`env.rs:109`) but documented only in
  `localdev.md`. Add it to `cli.md`'s runtime section and the user docs.

## Edge Case Considerations:

- **A1:**
  - A TUI tab opened in a non-git directory, or a repo with no `builtin`
    block, uses the global policy exactly as today.
  - A repo block that widens the global policy fails that launch only; other
    tabs keep working.
  - `stateDir` stays process-wide. A repo-level `stateDir` that differs from
    the process's is either rejected with a clear error or ignored with a
    warning. Ask the developer which before implementing.
- **A1, runtime switching:** changing `runtime` via `awman config` mid-session
  keeps working, because `Engines::detect_result` still permits it.
- **B1:**
  - Parallel workflow steps still pair each step's `Running` report with its
    own `take_io` under one lock (`factory.rs:36-42`).
  - The leader-phase `engine_tx` registration and the later engine overwrite
    still target the same shared slot.
  - Pause/Restart/Abort paths in the leader loop still flush queued messages
    exactly once.
- **B2:** a dsbx execution dropped without `wait()`, e.g. on command abort,
  must release its sink and not leak PTY handles.
- **C1:**
  - An image present in the runtime but from a different source is re-acquired
    (Ready semantics). This changes preflight behaviour, which used to skip it,
    and must be noted in the release notes.
  - Import-less runtimes (Docker/Apple) keep their build path.
  - Builder disabled: behaviour is exactly WI 0125's disabled mode (the
    pre-builder import-only path), including `--build`/`--no-cache`
    acquisition-refresh semantics.
  - Mixed per-image sources in one run: compatible images build, incompatible
    ones warn and import, and neither redirects the other, whichever caller
    (ready, preflight, squad) drives the acquirer.
  - A build that succeeds but whose import fails must not leave preflight
    believing the image is ready, and a later run must retry the import.
  - Concurrent callers (e.g. two squad tasks) targeting the same store and
    reference are serialised by the acquirer, not only within one `ready`.
- **C2:** any `MSB_*` variable in the full list now fails at `builtin::open`
  with `AmbientRuntimeOverride`. Surface the same error in all three
  frontends.
- **C4:**
  - Keep the builtin rule that a lagging attach client gets FAILED and never a
    clean exit.
  - Keep the 1 MiB frame cap and the bounded-write timeout.
- **C5:** a missing tool flag becomes an error on Docker too, if the fail-closed
  default is confirmed. Check `awman ready`'s agent matrix still passes for
  every agent.
- **D1:**
  - The worker is re-exec'd from the same binary path, including after an
    in-place upgrade (`embedded/mod.rs` comment). That must still hold.
  - Windows and Linux builds without the `builtin-runtime` feature must still
    compile.
- **E3:** a VM boot cancelled while in `spawn_blocking` must still stop the
  VM, and must not leave a lease file or a half-registered sandbox.

## Test Considerations:

- **Unit tests:** every new type and every moved function (`BuiltinLaunchSettings`
  mapping, `EffectiveConfig::builtin_settings`, `ImageAcquirer`, the
  `attach_protocol` codec, `AgentArgv`, `ExecBridgeOptions`, the D2/D3 Layer 0
  types, `ReadySummary::hint`, the `ReadyPhase` `Display`, `EngineError`
  display text).
- **Integration:**
  - two-repo, one-`Engines` session test (A1);
  - squad task inherits repo narrowing (A1);
  - fake-runtime workflow and dynamic-workflow tests that hold or never
    release the frontend (B1);
  - cross-runtime frontend-release conformance test (B2);
  - preflight/ready parity test on the same source and tag, builder on and
    off, compatible and incompatible sources (C1);
  - legacy-spelling cache records still hit after the D2 move;
  - attach round-trip on both servers over the shared codec (C4);
  - Docker vs builtin argv parity table (C5).
- **Regression:** keep and extend `frontend_is_released_before_wait_resolves`;
  it must fail with the `drop(frontend)` line removed. Keep every WI 0125
  builder test (config, source resolution, alias/cache compatibility, ready
  build/import, warnings, failure recovery, opt-in native Docker/Apple
  scenarios) passing throughout, relocated onto the new types where needed.
- **End-to-end:**
  - CLI, TUI (headless test harness) and API each surface identical text for
    a builtin attach failure (F1), the ready hint (F2), the incompatible-source
    builder warning and a build-ok/import-failed result (F1/F3).
  - `awman exec workflow --dynamic` completes on `builtin-experimental` on
    Apple Silicon. Record native evidence in the style of WI 0123.
- **Build:** `make all`, `make test` and the static release build produce one
  `awman` binary after D1, with and without `--features builtin-runtime`.
  `cargo fmt --check` and `cargo clippy --all-targets` are clean, including
  the pre-existing fmt drift in `tests/builtin_runtime/network_resources.rs`.
- Where Docker or KVM is unavailable, use fake drivers, and state explicitly in
  the PR which suites were not run.

## Codebase Integration:

- Follow established conventions, best practices, testing and architecture
  patterns from the project's aspec, especially
  `aspec/architecture/2026-grand-architecture.md` (Tenets 1–3, Layer 0/1/2
  responsibilities) and `aspec/architecture/security.md` (the network
  narrowing and ambient-override refusals must stay fail-closed).
- Prerequisite: WI 0125 (builder) is merged to `main` before any step here
  starts. Rebase onto it; do not refactor the pre-builder code and then port
  the builder across.
- Suggested execution order:
  1. B0 → A1/A2 → B1/B2 (user-visible safety and crash fixes first);
  2. C0 → C2/C3 → D4 → C1 (C0 maps the builder; C1 absorbs it);
  3. D2/D3 → C4/C5/C6/C7 → E1–E3;
  4. D1;
  5. F1–F5 (F4 may land early, since it is documentation only).
- One step per commit; reference the step ID (e.g. `WI0126 A1`) in each commit
  subject.
- When a step conflicts with WI 0124 (Linux KVM) or with the structure WI 0125
  (builder) shipped, this item takes precedence: restructure the code and
  update those items' references rather than preserving the old structure.
  WI 0125's user-visible behaviour is not up for change here; if a step would
  alter it, stop and ask the developer.

## Documentation

After implementation is complete, update user-facing documentation in `docs/`
to reflect the current state of the tool:

- `docs/11-runtimes.md` and `docs/07-configuration.md`:
  - repo-level `builtin.network`, `vcpus` and `memoryMib` now apply per
    session in the TUI, the API and squad;
  - `stateDir` scope;
  - the full list of refused `MSB_*` variables;
  - `AWMAN_BUILTIN_STATE_DIR`.
- `docs/04-security-and-isolation.md`: the repo network block narrows the
  policy for every launch in that repo.
- Any doc describing `awman ready` or workflow preflight image behaviour: one
  acquisition rule (C1). With the builder enabled, workflow preflight and
  squad build/warn/import exactly as `ready` does (or as WI 0125 scoped each
  caller; see C1). Check `docs/11-runtimes.md`'s builder section still matches.
- Any doc quoting builder errors, warnings or the ready hint: match the unified
  F1/F2 text.
- **Update existing feature docs**; create new user guides only if a new
  user-visible feature warrants it.
- **Never create work-item-specific docs**. Keep technical and implementation
  detail in this work item and in code comments.

See `CLAUDE.md` for more guidance on documentation standards.
