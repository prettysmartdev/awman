# Work Item: Task

Title: Bring the builtin runtime into conformance with the grand architecture
Issue: n/a (2026-09-30 builtin-runtime architecture review)

Status: Open
Follows: [WI 0123](completed/0123-builtin-native-verification-and-release-closure.md),
[WI 0125](0125-builtin-image-source-builder.md) (the image-source builder,
merged in `f940ec98`; this item brings that code into conformance too, and
folds it into Step C1's agent-image preparer)

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
- WI 0125 then added the source-matched builder. It works and is well tested,
  but it was built onto the pre-refactor structure: almost all of it lives in
  `ReadyEngine`, it re-derives source and builder decisions several times, and
  it adds a third Dockerfile-path rule and a second import path next to the
  Layer 2 one.
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
- Line numbers below are as of commit `78dc8e05` (WI 0125 merged) and will
  shift as steps land. Re-locate each one by symbol.
- WI 0125's user-visible behaviour is fixed input to this item, not something
  to redesign. Its tests (listed under Test Considerations) are a regression
  suite: they must keep passing, moved or retyped as needed, without weakening
  their assertions. If a step would change WI 0125 behaviour, stop and ask.

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

### Decisions made by the developer (2026-10-01)

5. **API session setup keeps auto-building (C1).** As WI 0125 shipped it,
   session setup (`ReadyEngine` with `build: false`) builds when the
   compatible source store lacks the image. It gets its own `SessionSetup`
   `BuildPolicy`. Document it in `aspec/uxui/cli.md` next to the statement
   that workflow preflight and squad are import-only (`cli.md:205`).
6. **Docker builder eligibility is Unix sockets only (C1).** The code
   (`BuilderSelection::from_source`, `src/engine/ready/builder.rs:51`) is
   right; WI 0125's spec has been amended to drop named pipes.
7. **A repo-level `stateDir` that differs from the process's is rejected
   (A1).** That session's launch fails with a typed configuration error naming
   both paths. Same fail-closed shape as a widening network block.
8. **A missing tool flag fails closed on every runtime (C5).** An agent
   without an allowed/disallowed-tools flag is a configuration error on
   Docker, Apple and builtin alike.

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
The builtin runtime, including the WI 0125 builder, to follow the same
layering, typing and single-source-of-truth rules as the rest of awman, with a
spec that describes it.

So I can:
Extend it (WI 0124) without adding to known drift.

### User Story 4:
As a: user of the builtin runtime

I want to:
`awman ready`, API session setup, workflow preflight, squad evaluation and
agent launch to resolve, check and import images by one rule, building only
where WI 0125 allows it (`ready` and API session setup, decision 5), with
the same warnings and results in every frontend.

So I can:
Trust that an image a workflow or squad task accepts is the one `ready` would
have accepted, and that nothing builds behind my back.

## Implementation Details:

### Group A — Per-session builtin settings (High, security-relevant)

**A1. Resolve network, vCPU and memory per launch.**
- Problem today:
  - `BuiltinRuntimeSettings::resolve` (`src/engine/container/runtime.rs:99-114`)
    fixes the default vCPUs and memory and the network policy when the backend
    is built. (Per-launch CPU/memory overrides already flow through
    `ResolvedContainerOptions.cpu/.memory`; only the defaults and the network
    are process-wide.) The backend reads them in two places:
    - `BuiltinBackend::plan()` (`src/engine/container/builtin/backend.rs:219`;
      `resources::resolve(…, (self.settings.vcpus, self.settings.memory_mib))`
      at `:241-245`, `network::compile(&self.settings.network)` at `:280`);
    - `BuiltinBackend::start_background()` (`backend.rs:619`; `vcpus`,
      `memory_mib` and `network::compile(&self.settings.network)` at
      `:659-661`), which has no `ResolvedContainerOptions` at all.
  - For the TUI and API, that happens once, from the launch directory's
    config (`src/command/startup.rs:79`), and every tab or session reuses it:
    - TUI: `src/frontend/tui/app.rs:586-589` clones `self.engines` into each
      tab's `Dispatch`;
    - API: `src/command/commands/api_server/runtime.rs:91` (`bootstrap`) and
      `:242`/`:263`, plus `src/frontend/api/routes.rs:62`, `:363`.
  - The daemon path uses `agent_runtime::detect(&global)` and never sees repo
    config (`src/command/dispatch/mod.rs:183-184`, `Engines::for_daemon`;
    called from `squad/daemon.rs:209`).
  - Registries are split the same way: the backend's import uses process-wide
    `self.settings.registries` (`backend.rs:540`, `:556`), while `ReadyEngine`
    resolves sources with the session's
    `effective_config().builtin_runtime().registries`
    (`src/engine/ready/mod.rs:145`, `:1558`, `:1801`). A repo that sets
    registries gets one set for source resolution and another for the pull.
- Change:
  - Split the settings into a **process-wide** part and a **per-launch** part:
    - Process-wide `BuiltinRuntimeSettings`: `state_dir`, `ambient_overrides`,
      `test_isolation`.
    - Per-launch `BuiltinLaunchSettings`: default `vcpus`, default
      `memory_mib`, resolved `network`.
    - Per-session acquisition inputs (`image_source`, `images`, `registries`,
      `builder.enabled`) leave `BuiltinRuntimeSettings` entirely. They are
      inputs to C1's `AgentImagePreparer`, and the import request carries the
      session's registries to the backend, so the split above disappears.
  - Carry the per-launch part as a typed field of `ResolvedContainerOptions`
    and of whatever request type `start_background` takes after E1. It is
    populated by Layer 2 from the session's `EffectiveConfig` through a typed
    option, not a `pub fn`. Other container backends ignore it.
  - `plan()` and `start_background()` compile the network plan and size the
    VM from the launch settings. Remove the "there is no per-launch override"
    doc comment on the `network` field (`runtime.rs:82-85`).
  - Daemons (squad) resolve per-launch settings from each task's own session
    config, not from `GlobalConfig`.
  - Validation still fails closed. A repo block that would widen the global
    policy is a configuration error at launch, surfaced as a typed error.
  - A repo-level `builtin.stateDir` that resolves to a different directory
    than the process-wide one is a typed configuration error at launch naming
    both paths (decision 7). A repo value equal to the process's is accepted.
- Already correct (verified at `78dc8e05`, keep it that way):
  `builtin.builder.enabled` is read only per session
  (`EffectiveConfig::builtin_builder_enabled()`, `effective.rs:257`, from
  `ReadyEngine::new`, `ready/mod.rs:637`), never from `GlobalConfig` or
  `BuiltinRuntimeSettings`.
- Tests:
  - Two sessions from two repos with different `builtin.network` blocks share
    one `Engines`; each launch's compiled plan matches its own repo, for both
    `plan()` and `start_background()`.
  - A squad task inherits its repo's narrowing.
  - A repo-level `registries` block is used for both source resolution and
    the pull.
  - A repo-level `stateDir` differing from the process's fails that launch
    only; an equal one is accepted.
  - Lift `two_sessions_with_opposite_builder_settings_each_see_only_their_own`
    (`ready/mod.rs:3811`) to Layer 2: a TUI tab and an API session sharing one
    `Engines` each honour their own repo's `builtin.builder.enabled`.
  - Unit tests of the options mapping.

**A2. Drop the "informational" merged network.**
- `BuiltinRuntimeConfig::merged_over` (`src/data/config/builtin_runtime.rs:163`)
  carries `network: self.network.clone().or_else(|| base.network.clone())`
  (`:178-179`, commented "Informational only; enforcement resolves both
  layers"), on a struct the engine reads through
  `EffectiveConfig::builtin_runtime()`.
- Leave it out of the view the engine consumes, so nobody can read an
  unenforced value. Enforcement already goes through
  `EffectiveConfig::builtin_network()` (`effective.rs:264`).
- `BuiltinRuntimeConfig::validate()` (`:214`, network check `:224-227`) reads
  the merged `network`. Move that check into `builtin_network()` or show it is
  redundant with it, before removing the field.
- The `builder` merge WI 0125 added is already correct: field-wise
  (`BuiltinBuilderConfig::merged_over`, `:97-101`), explicit repo `false` wins,
  and it is not in the backend's view. No change.

### Group B — Frontend ownership redesign (High)

**B0. Exec-bridge release fix (done).**
- `drop(frontend)` before `done` (`src/engine/container/builtin/exec_bridge.rs:248`)
  and the regression test `frontend_is_released_before_wait_resolves`
  (`:667`, multi-thread runtime, `SlowDropFrontend` at `:629`) are on `main`
  from `7da4760c` and survived the WI 0125 merge. No action, except that their
  comments (`:245-247`, `:664`) say callers reclaim with `Arc::try_unwrap`;
  update them in B1.

**B1. Stop reclaiming the frontend with `Arc::try_unwrap`.**
- Problem today: `ExecWorkflowCommand` wraps its
  `Box<dyn ExecWorkflowCommandFrontend>` in `Arc<Mutex<…>>` (9 production
  sites: `prepare.rs:306`, `phase_container.rs:31`, `factory.rs:20`, `:85`,
  `dynamic.rs:557`, `:928`, `:1130`, `:1172`, `:1213`; plus 4 in
  `exec_workflow/tests.rs`). It hands clones to runtimes, then `panic!`s if it
  cannot `Arc::try_unwrap` it afterwards. These are the only two `try_unwrap`
  calls in `src/`:
  - `src/command/commands/exec_workflow/prepare.rs:719`
  - `src/command/commands/exec_workflow/dynamic.rs:669-670`
- Clones reach runtimes by two routes:
  - `AgentFrontendProxy` (`src/command/commands/exec_workflow/factory.rs:19`),
    handed over at `factory.rs:334` and `dynamic.rs:1001`;
  - `InteractivePhaseContainer` (`exec_workflow/phase_container.rs`), which
    already aliases `type SharedFrontend = Arc<Mutex<Box<dyn
    ExecWorkflowCommandFrontend>>>` (`:31`) and passes
    `Box::new(Arc::clone(&self.frontend))` straight to `run_with_frontend`
    (`:95`) through the blanket `impl AgentFrontend for Arc<Mutex<Box<F>>>`
    (`src/engine/agent_runtime/frontend.rs:124`). It is built from
    `prepare.rs:543` and `:655`, so it feeds the `prepare.rs:719` panic
    directly.
- Squad is affected too: `squad/evaluation.rs:841` runs an
  `ExecWorkflowCommand`.
- Change:
  - Introduce a Layer 2 handle type, `SharedWorkflowFrontend` (replacing the
    `phase_container.rs` alias), that owns the
    `Arc<Mutex<Box<dyn ExecWorkflowCommandFrontend>>>` for the whole command
    lifetime. It exposes the methods the command needs (`with(|f| …)`,
    `set_pty_active`, `replay_queued`, …) and is cheap to clone.
  - Every phase — preflight, leader, engine block, `execute_generated_workflow`,
    interactive phase containers, post-run worktree prompts — takes
    `&SharedWorkflowFrontend` (or a clone) instead of an owned `Box`. The
    functions that take `&mut dyn ExecWorkflowCommandFrontend` today
    (`offer_dynamic_resume`, `dynamic.rs:725`; the resume helper at
    `prepare.rs:95`) receive it through the handle.
  - Delete both `try_unwrap` sites, the reclaim comments and scoping
    (`prepare.rs:348-349`, `:718`), and update the B0 comments.
  - `AgentFrontendProxy` and `InteractivePhaseContainer` keep holding clones.
    A runtime that keeps its clone past `wait()` is then harmless.
- Audited at `78dc8e05`, no other reclaim sites: `chat`, `exec prompt`,
  `specs`, `new` hand runtimes a separate box from
  `container_frontend_for_pty()` (`chat.rs:326`, `:360`, `exec_prompt.rs:380`,
  `:419`, `specs.rs:246`, `:517`, `new/skill.rs:243`, `new/workflow.rs:163`);
  `config` only in tests; `squad/attach.rs:243` shares with callbacks only.
- Tests:
  - Workflow and dynamic-workflow command tests with a fake runtime that
    deliberately holds its frontend clone until after `wait()` resolves (and
    one that never drops it), through both the proxy and the interactive
    phase-container routes. All complete without panicking, and messages are
    still delivered.
  - The same through a squad evaluation.

**B2. Document the engine-side contract, and close the abort leak on every runtime.**
- After B1 nothing *depends* on early release, but a runtime that holds a
  frontend forever still leaks the PTY/IO sinks.
- On `AgentInstance::run_with_frontend` (`src/engine/agent_runtime/execution.rs:70-75`)
  and `AgentExecution::wait` (`:204-215`), document: "a runtime MUST drop the
  frontend no later than when `wait()` resolves, when the `wait()` future is
  dropped, or when the execution is dropped".
- The real leak is generic, not dsbx-specific: `AgentExecution::wait` moves
  the backend into `tokio::task::spawn_blocking(move || backend.wait_blocking())`.
  If the `wait()` future is dropped (abort, cancel), the blocking task keeps
  the backend — and its frontend/sink and PTY master — until the child exits.
  (`DSbxExecution.sink`, `src/engine/sandbox/dsbx/backend.rs:582`, is released
  correctly on plain drop and on `wait_blocking`.)
- Fix it once in `AgentExecution`: move the sink out before `spawn_blocking`,
  or cancel/kill on drop, so every `ExecutionBackend` gets it.
- Add one cross-runtime conformance test modelled on `SlowDropFrontend`, run
  against Docker/Apple (`src/engine/container/process.rs:223`), dsbx and
  builtin, covering normal `wait()` and a dropped `wait()` future. Use fake
  drivers where a daemon is unavailable.

### Group C — Single sources of truth and de-duplication (Medium)

**Where the WI 0125 builder lives today (reference for C1, C7, D2, D4, E1, F1–F3).**

| Concern | Location at `78dc8e05` |
|---|---|
| Config type, parse, merge | `BuiltinBuilderConfig` (`src/data/config/builtin_runtime.rs:83-120`); serde validation `deserialize_builder_enabled` `:124`, `deserialize_builder` `:139`; CLI `parse_enabled` via `config.rs:125-131`; field metadata `fields.rs:148`, `:305-309` |
| Read | `EffectiveConfig::builtin_builder_enabled()` (`effective.rs:257`) → `ReadyEngine::new` (`ready/mod.rs:637`) only |
| Alias normalisation | `ImageSourceSpec`/`ImageSourceKind` serde aliases and `legacy_names()` (`src/data/config/image_source.rs:36-102`); `OciSourceKey::{canonical_key, lookup_keys}` (`src/data/oci_source_key.rs`); lookup loop `src/engine/oci/mod.rs:361` |
| Source resolution | `ImageSourceResolver::resolve_for_image` → `ResolvedImageSource` (`src/engine/oci/resolve.rs:52-116`) |
| Eligibility | `BuilderSelection::{Local(LocalBuilderTarget), Incompatible(kind), Unconfigured}` and `from_source` (`src/engine/ready/builder.rs:27-67`); "disabled" is a separate `bool` |
| Build-only seam | `trait LocalImageBuilder { probe, inspect, build }` (`builder.rs:84`), `HostLocalImageBuilder` (`:102`), `LocalImageBuildRequest` (`:70`) |
| Host build helpers | free functions in `src/engine/container/host_cli_backend.rs`: `bound_docker_command` `:307`, `HostBuildRequest` `:319`, `build_local_docker` `:381`, `BuildArgv` `:437`, `HostCommandRunner` `:179-289`; module made `pub` in `container/mod.rs:19` for this |
| Planning and build | `ReadyEngine::prevalidate_build_targets` (`ready/mod.rs:131-234`), `prepare_local_image` (`:336-626`, warning `:351-366`, base per store `:497-580`, agent build `:590-617`) |
| Forced re-import | `ready/mod.rs:1625-1663`; `ImageImportRequest` fields (`agent_runtime/mod.rs:296-302`); `CachingAcquirer::with_expected_built_identity`/`with_export_selector` (`oci/mod.rs:289-302`, check `:481-497`) |
| Locks and receipts | `ImageBuildLockStore`/`BuildStoreKey` (`src/data/image_build_lock.rs`), `PendingBuiltImageStore` (`src/data/image_build_state.rs`), both on `src/data/fs/private_files.rs` |
| Results | `ReadyImageResult`, `ReadyBaseImageResult`, `ReadyImageWarning::IncompatibleBuilder` (`src/data/ready_image.rs`); `ReadySummary::{hint, image_action, outcome_line}` (`src/data/ready_summary.rs`); `ReadyPhase: Display` (`src/data/ready_phase.rs:32`) |
| Blocking bridges | `ready/build.rs` (`CancelOnDrop` `:20`, `inspect` `:54`, `run` `:71`), `ready/import.rs` (`ImageImporter` `:15`, second `CancelOnDrop` `:36`, `run` `:53`) |

Callers and their policies (WI 0125 behaviour to preserve):

| Caller | Builds? | Path |
|---|---|---|
| `awman ready` (`src/command/commands/ready.rs:255-296`) | Yes, per user flags | `ReadyEngine` |
| API session setup (`src/command/session_setup.rs:431-482`) | Yes when the source store lacks the image (decision 5) | `ReadyEngine` with `build: false` |
| Workflow preflight (`workflow_preflight.rs:407-495`), dynamic workflow (`dynamic.rs:525`, `:854`), squad evaluation (`evaluation.rs:540`, `:778`) | No — import-only (`cli.md:205`) | Layer 2 `import_agent_image` (`workflow_preflight.rs:498-542`) |
| `AgentEngine::ensure_available` (`src/engine/agent/mod.rs:127-160`; from `chat.rs:418`, `exec_prompt.rs:57`) | No — never acquires | errors with a hint |

**C1. One engine-level agent-image preparer, absorbing the builder.**
- Problem today:
  - Two import paths with different rules:
    - Layer 2 `import_agent_image` (`workflow_preflight.rs:498-542`) returns
      early on `runtime.image_exists(&tag)` (`:509`);
    - `ReadyEngine::import_agent_image` (`ready/mod.rs:1449-1739`) never skips
      acquisition (`:1482-1486`), checks provenance, and owns the whole
      builder flow.
  - Three Dockerfile-path rules: `repo_config().dockerfile_path_or_default()`
    in `ReadyEngine` (`:162`, `:519`, `:1474`, `:1806`), and
    `paths.project_dockerfile()` in preflight (`workflow_preflight.rs:519`)
    and `AgentEngine` (`agent/mod.rs:146`).
  - Source sets built in five places: four Layer 2
    `ImageSources::from_config(&….effective_config().builtin_runtime())` sites
    (`dynamic.rs:525`, `:854`; `squad/evaluation.rs:541`, `:779`) plus
    `ReadyEngine::new` (`ready/mod.rs:638`).
  - "Which source applies to this tag" is answered in five places:
    `resolve_source` (`ready/mod.rs:1486`) and `resolve_for_image` again
    (`:1562`) per image, `default_source_configured` (`:1792`),
    `single_image_default_source` (`:1832`) and
    `BuiltinRuntimeConfig::image_source_for` (`builtin_runtime.rs:208`, used by
    `AgentEngine`).
  - `BuilderSelection` is re-derived three times inside `ReadyEngine`, each
    rebuilding `Env::for_image_acquisition`, `ImageSourceResolver` and
    `BuildHint`: `prevalidate_build_targets` (`:144-185`),
    `prepare_local_image` (`:348`), `default_source_is_local` (`:1798-1825`).
    "Builder disabled" is a separate `bool` checked at `:1279`, `:1409`,
    `:1548`, `:1769`.
  - Two store identities: `ResolvedImageSource.store: String` (collision
    checks, `ready/mod.rs:214`) and `BuildStoreKey` (locks, receipts, base
    dedup), mapped at `:373-378`, with different `describe()` text.
  - The digest-mismatch check is duplicated (`ready/mod.rs:1646-1663` and
    `oci/mod.rs:481-497`).
  - `ReadyEngine` reads ambient process env in Layer 1: `data_home()`
    (`ready/mod.rs:1900`) calls `GlobalConfig::data_home_with(&Env::from_process())`.
  - "Warn once" for incompatible sources relies on a per-call `attempted`
    guard; the audit-rebuild pass (`:1277-1311`, which re-runs both import
    functions) can emit the warning a second time for the same tag.
  - `src/engine/ready/mod.rs` is 4453 lines (~1950 production, ~2500 tests).
- Change:
  - Add a typed Layer 1 `AgentImagePreparer` (the name `ImageAcquirer` is
    already the OCI cache trait at `src/engine/oci/mod.rs:82`). It is
    constructed from the session's resolved builtin config (D4) with its
    sources, registries and `builder_enabled` as required arguments, plus a
    `data_home` path passed in rather than read from the process env.
  - Its single entry point, e.g.
    `prepare(&AgentImageRequest { tag, agent, policy }, sink) -> Result<ReadyImageResult, EngineError>`,
    owns the full lifecycle for one image:
    1. resolve the source once (`builtin.images[tag]` before
       `builtin.imageSource`) into `ResolvedImageSource`;
    2. compute `BuilderSelection` once from it and the builder flag;
    3. decide whether a build is due under the caller's `BuildPolicy`;
    4. take the store/reference lock, build through `LocalImageBuilder` (base
       per store, then agent at the source reference), or emit the
       incompatible-source warning at most once per tag per run;
    5. acquire with `ReadyEngine`'s semantics (provenance-checked, never
       bypassed), forcing fresh acquisition after a successful build or a
       pending receipt;
    6. return `ReadyImageResult` with build and import outcomes kept separate.
  - `BuildPolicy` makes WI 0125's caller split explicit and typed:
    - `Ready { build, no_cache, refresh }` — `awman ready`;
    - `SessionSetup` — builds only when the compatible source store lacks the
      image, never forced (decision 5);
    - `ImportOnly` — workflow preflight, dynamic workflow, squad evaluation;
      never builds, never probes the builder, never warns about building;
    - `CheckOnly` — `AgentEngine::ensure_available`; never acquires.
  - Move `BuilderSelection` next to source resolution
    (`src/engine/oci/resolve.rs`), add a `Disabled` variant so the separate
    `bool` checks go away, and make it the only place that maps a source kind
    and endpoint to a builder. Keep today's eligibility rule exactly
    (Unix socket on Unix, Apple always; decision 6).
  - Use one store identity (`BuildStoreKey`) for collision checks, locks,
    receipts and base dedup.
  - Per-store base-image dedup (`prepared_bases`) and the store/reference
    locks live in the preparer, so every building caller gets them.
  - Keep one digest check, in `CachingAcquirer`.
  - `ReadyEngine` keeps the phase machine (Docker/Apple build phases, audit,
    summary) and delegates every builtin image to the preparer. Split
    `ready/mod.rs` along those lines: the preparer and its build/import
    bridges in `engine/image_prep/` (or similar), the phase machine in
    `ready/phases.rs`, and the fakes (`FakeImportRuntime`, `FakeLocalBuilder`,
    `FakeKitRuntime`) and tests in `ready/tests/`.
  - Remove the Layer 2 `import_agent_image`, the four Layer 2 `from_config`
    sites, `default_source_configured`/`single_image_default_source`/
    `default_source_is_local` and `BuiltinRuntimeConfig::image_source_for` in
    favour of the preparer.
  - One Dockerfile-path rule, used for the build context, the provenance check
    and every hint.
  - Merge the two `CancelOnDrop` types and the two blocking bridges
    (`build::run`, `import::run`) into one.
- Tests:
  - Preflight/ready parity on the same source and tag, builder on and off:
    preflight imports what `ready` would import and never calls the builder.
  - A recording builder/importer per `BuildPolicy`: zero builder calls and no
    build warning for `ImportOnly`/`CheckOnly`; for `Ready`, one warning per
    incompatible tag per run including the audit-rebuild pass; forced
    re-acquisition after a build.
  - Move WI 0125's ready-builder tests onto the preparer without weakening
    them.

**C2. One `MSB_*` block list.**
- Problem today:
  - `src/engine/container/builtin/embedded/mod.rs:6-26` (list `:6-20`, check
    loop `:21-26`) and `src/engine/container/builtin/worker.rs:24-46` (a
    `matches!`) each hard-code 13 variables.
  - `AMBIENT_MSB_OVERRIDES` (`src/data/config/env.rs:131`) lists only 5, and
    `builtin::open` (`builtin/mod.rs:72`, filled at `runtime.rs:111`) checks
    only those, so the other 8 fail later in `embedded::resolve()` through a
    different error path.
  - The docs are already ahead of the code: WI 0125 updated
    `aspec/uxui/cli.md:213-217`, `docs/07-configuration.md:575` and
    `docs/11-runtimes.md:276` to list all 13.
- Change:
  - Make `AMBIENT_MSB_OVERRIDES` the single list of all 13.
  - `embedded::resolve` consumes `settings.ambient_overrides` and does no env
    reads of its own.
  - The worker takes the list from `awman::data` now (`main.rs` already
    imports `awman::data`), so this does not wait for D1.
  - Add a test that the documented list in `cli.md` matches the constant.

**C3. One version constant.**
- `"0.7.2"` appears in `naming.rs:3`, `worker.rs:55`, `embedded/mod.rs:52` and
  `embedded/version.rs:8`. The libkrun commit is in `worker.rs:57`, and
  `worker.rs:55` also hard-codes `"worker_protocol": "0.7.2/18/awman-1"`,
  copying `naming::PROTOCOL` (the worker cannot import `naming` because
  `container/mod.rs` keeps `mod builtin` private).
- Derive all of them from one Layer 0 constant (see D1 for where it lives).
  `version.rs:8` is `static MSB_VERSION: [u8; 5] = *b"0.7.2"` in a
  `link_section`, and `PROTOCOL` is composed, so use a `const fn` byte copy
  and `concat!` from a macro literal (or equivalent).

**C4. One attach wire protocol.**
- Problem today:
  - `exec_bridge.rs:29-36` defines `STDOUT=0, INPUT=1, RESIZE=2, STDERR=3,
    EXIT=4, FAILED=5`, `MAX_FRAME`, `ATTACH_WRITE_TIMEOUT`, `write_frame`
    (`:316`), `write_frame_bounded` (`:325`) and `read_frame` (`:340`).
  - `src/engine/container/attach_socket.rs:46-57` uses the same
    `tag|u32 LE|payload` shape with `OUTPUT=0, STDIN=1, RESIZE=2, LAGGED=3`
    and `MAX_FRAME_LEN = 1 << 20`. Tag 3 means STDERR in one protocol and
    LAGGED in the other.
  - The socket paths also differ: `BuiltinPaths::attach` (`paths.rs:122`) vs
    `attach_socket_path` (`attach_socket.rs:95`, which reads
    `Env::from_process()` at `:85`).
- Change:
  - Create one `attach_protocol` module with a single tag enum covering
    output, stdin, resize, stderr, exit, failed and lagged (no tag reuse),
    plus shared frame codec, limits and bounded-write helpers.
  - Build both servers and clients on it.
  - Decide on one socket path resolver, or document why two are needed. The
    resolver takes its env, it does not read the process env.

**C5. One agent argv builder.**
- `agent_argv` (`builtin/backend.rs:134`) mirrors the agent tail of
  `docker::build_run_argv` (`docker.rs:875`, agent part ~`:1046-1133`), which
  Apple also uses (`process.rs:229`). They already differ: builtin errors when
  an allowed/disallowed-tools flag is missing (`backend.rs:158-160`),
  Docker/Apple skip it (`docker.rs:1069`, `:1076`).
- Extract a pure `AgentArgv::from_options(&ResolvedContainerOptions)` used by
  all three.
- A missing flag is a fail-closed configuration error on every runtime, because silently
  dropping a tool restriction is the less safe outcome (decision 8).

**C6. Shared output/stuck plumbing.**
- `exec_bridge::run` re-wires activity, first-byte and output-tail handling
  that `io_bridge::BridgeConfig` (`io_bridge.rs:30`) already owns.
  `spawn_stuck_detector` (`io_bridge.rs:103`) takes six positional arguments
  and has four call sites: `exec_bridge.rs:183`, `attach_socket.rs:435`,
  `io_bridge.rs:278`, `io_bridge.rs:403`.
- Extract a byte-stream-agnostic `OutputPump` or
  `StuckDetector::spawn(StuckConfig)` in `io_bridge`, and use it from all four.

**C7. Capability flags and host-CLI methods.**
- Problem today:
  - `arbitrary_env_vars`, `cpu_limits`, `fractional_cpu`,
    `per_resource_stats`, `persistent_lifecycle`, `host_paths_visible`,
    `session_label_supported` and `dind` have no production readers (only
    `capabilities.rs` tests and test fakes).
  - `exec_args`/`host_cli` sit on the cross-paradigm traits
    (`src/engine/container/backend.rs:43`, `:92`;
    `agent_runtime/mod.rs:170`, `:197`), even though `design.md` calls
    Docker-shaped argv private to the CLI backends. No production code calls
    them outside trait delegation (`runtime.rs:166`, `:263`, `:383`, `:394`);
    Docker and Apple use `host_cli_backend::cli_exec_args` internally
    (`docker.rs:432`, `apple.rs:385`). Sandbox implements both
    (`sandbox/runtime.rs:174`, `:213`), and about eight test fakes do too.
  - WI 0125 made `host_cli_backend` `pub` (`container/mod.rs:19`) so the
    builder could call its free functions directly.
  - `capabilities().image_acquisition` is static (`Import` for builtin,
    `builtin/backend.rs:49`). WI 0125 combines it with the builder flag in
    `ReadyEngine::new` (`ready/mod.rs:640-645`) to pick
    `ReadySummary.image_action`, from `builder_enabled` alone, not from each
    image's source compatibility.
- Change:
  - For each unused flag, either wire the Layer 2 decision that should branch
    on it (per the doc: "paradigm-specific decisions branch on
    `capabilities()`") or delete it. Record which in the commit message.
  - Delete `exec_args`/`host_cli` from the cross-paradigm traits. If a reader
    turns up, move them to a sub-trait named so it does not collide with the
    `host_cli_backend` module (e.g. `HostCliRuntime`); builtin does not
    implement it.
  - Adopt WI 0125's `LocalImageBuilder` as the preparer's only build handle.
    Move the host build helpers it calls behind `HostLocalImageBuilder` (or a
    `pub(crate)` facade), and make `host_cli_backend` private again. Agent
    execution stays unreachable from the build handle, and no code matches on
    the concrete runtime.
  - Keep `image_acquisition` static, and record the decision: what a runtime
    *can* do is a capability; what a session *will* do for an image is the
    preparer's `BuilderSelection`. F2 reads the latter.
- Document which `AgentStatus` values a frontend can expect. `Exited` is
  currently emitted only by builtin (`exec_bridge.rs:219`); emit it from every
  backend or document why not.

### Group D — Layer relocations (Medium)

**D1. Workspace crate for the worker and `apple_xpc`.**
- Problem today: `main.rs:21-26` pulls in
  `engine/container/builtin/worker.rs` and `engine/oci/apple_xpc.rs` via
  `#[path]` to get around `#![forbid(unsafe_code)]` in `lib.rs:12`
  (`main.rs:1` is `deny`). `worker.rs:2-7` in turn pulls in
  `embedded/kernel.rs` and `embedded/version.rs` via `#[path]`, and
  `version.rs` uses `#[allow(unsafe_code)]` for its `link_section`.
  Specifically:
  - `worker.rs:201-215` uses `unsafe` for `fcntl` and `fstat`;
  - `worker.rs` also parses its own arguments, embeds the kernel, calls
    `eprintln!`/`process::exit` (`:42-45`, `:70-71`, `:87-88`, `:103-109`), and
    prints version JSON (`:52`).
- Change:
  - Convert the repository to a Cargo workspace. Add a small crate (e.g.
    `crates/awman-host-shims`, or two crates if cleaner) that allows the
    minimal `unsafe`, with `// SAFETY:` comments. It owns the builtin worker
    entry point, `kernel.rs`, `version.rs` and the Apple XPC transport.
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
  - JSON read/atomic-write, rename/remove and lock primitives (`:509`, `:527`,
    `:713` `open_private_lock`, `:771`, `:814-849`);
  - `SourceFingerprint` (`:92`; `of` at `:98` calls `std::fs::metadata`).
- WI 0125 already set the pattern: `src/data/fs/private_files.rs`
  (`create_private_dir`, `lock_private_exclusive`, `read_private_json`,
  `write_private_json_atomic`, `remove_private_file`; "conventions match the
  image cache's"), with `data/image_build_lock.rs` and
  `data/image_build_state.rs` owning only record formats. Those primitives now
  duplicate `cache.rs`'s, `oci/verify.rs:348`, `:362` and `apple_store.rs:698`.
- Change:
  - Move the record types and layout constants into `src/data/oci_cache/`,
    next to `oci_identity.rs`, built on `data/fs/private_files.rs`. Remove the
    duplicate primitives listed above.
  - `CacheRecord.config` is an `engine::oci::archive::ImageConfigSummary`
    (`archive.rs:75`); it must move to Layer 0 with the record. (A different
    type with the same name is at `builtin/driver.rs:75`; rename one.)
  - Keep acquisition, verification and prune policy in `engine/oci`.
  - Legacy-alias handling is already in Layer 0 (`image_source.rs`,
    `oci_source_key.rs`) and does not move. The engine-side `lookup_keys()`
    loop (`oci/mod.rs:361`) stays.

**D3. Move the builtin state-dir layout to Layer 0.**
- These are filesystem layout, persistence and path resolution:
  - `BuiltinPaths::resolve` (`paths.rs:16-38`, creating `run`, `cache`,
    `logs`, `sandboxes`, `volumes`, `snapshots`, `secrets`, `attach`, `db`);
  - `secure_create` (`:176-238`);
  - the lock API (`:41-128`: `lock`, `import_lock`, `import_lock_cancellable`,
    `image_lease`, `lock_controlled` at `:78`; `LOCK_WAIT`/`IMPORT_LOCK_WAIT`
    at `:130-131`);
  - `attach()` socket paths (`:122`);
  - the runtime-version marker in `catalog.rs:9-54`.
- Add `src/data/builtin_state.rs` (modelled on `worktree_paths.rs`, with locks
  built on `data/fs/private_files.rs` as `image_build_lock.rs` is) to own
  them.
- The engine keeps socket-budget checks and SDK-specific policy.

**D4. Resolve builtin config in Layer 0.**
- Replace the validation and defaulting inside
  `BuiltinRuntimeSettings::resolve` (`runtime.rs:99`; only caller
  `agent_runtime/mod.rs:419`) with
  `EffectiveConfig::builtin_settings() -> Result<ResolvedBuiltinConfig, DataError>`.
  It returns:
  - the process-wide and per-launch parts from A1, with defaults and the
    network narrowing applied;
  - the acquisition inputs C1 needs: normalised image sources, registries and
    `builder_enabled`.
- `builtin.builder.enabled` validation already runs in Layer 0 at deserialize
  time (`deserialize_builder_enabled`, `builtin_runtime.rs:124`) and in
  `config set` (`parse_enabled`); leave it there.
- The engine only consumes `ResolvedBuiltinConfig`. `ReadyEngine` stops
  calling `ImageSources::from_config` and `builtin_builder_enabled()` itself.
- Update the comments at `builtin_runtime.rs:4-5` and `effective.rs:243-245`,
  which both say the reader applies defaults, to say Layer 0 does.

### Group E — Tenet 3 typing and async hygiene (Medium/Low)

**E1. Typed objects instead of OS-touching free functions.**

| Now | Becomes |
|---|---|
| `exec_bridge::run` (`exec_bridge.rs:122`; 7 positional args, incl. untyped `cleanup: Option<(Arc<dyn SandboxDriver>, String, bool)>` and `RunInput`) | `ExecBridge::new(ExecBridgeOptions { handle, session, cleanup: Option<CleanupTarget>, attach_path, persistent_stdin, seeded_prompt, leases, io }).run(frontend)`; `RunInput` folds in |
| `exec_bridge::serve` (`:289`) | `AttachServer::bind(path, &session)` (the `Server` struct at `:279` already exists) |
| `exec_bridge::cancel(control, owner, grace)` (`:93`) | a method on the execution's cancel handle, holding `control` and `CancelTarget` |
| `paths::check_socket_budget`, `paths::secure_create` (`paths.rs:163`, `:176`) | methods on the D3 state type / `BuiltinPaths` |
| `catalog::check` (`catalog.rs:9`) | `Catalog` type or a method on the state type |
| `embedded::resolve()` (`embedded/mod.rs:3`, no args) | `EmbeddedRuntime::resolve(&ambient_overrides)` (after C2) |
| `oci::cached_image_config`, `oci::prune_cache` (`oci/mod.rs:225`, `:234`) | callers use `OciCache::open(..)?.…` |
| `oci::archive::validate_archive` (5 args, `archive.rs:89`) and `validate_archive_controlled` (6 args, `:106`) | `ArchiveValidator` |
| `ContainerBackend::build_image` / `start_background` / `exec_in_background_streaming` (`container/backend.rs:121`, `:160`, `:176`; 4–5 args) | request structs. `build_image` takes one `BuildRequest` unified with WI 0125's `HostBuildRequest` (`host_cli_backend.rs:319`) and `LocalImageBuildRequest` (`builder.rs:70`); `BuildArgv` stays the single argv builder |
| WI 0125 free functions: `ready::build::inspect` (5 args), `build::run`, `import::run`, `normalize_build_reference` (`ready/mod.rs:1910`), `builder::classify_docker_inspect`, `builder::build_control`, `host_cli_backend::build_local_docker`, `bound_docker_command` | methods on `AgentImagePreparer`, `BuilderSelection` or `HostLocalImageBuilder` (C1/C7) |
| `ReadyEngine` public setters `with_local_builder`, `with_image_importer`, `with_host_ping`, `with_build_stores` (`ready/mod.rs:679-703`) | constructor dependencies of the preparer, or `#[cfg(test)]` |

- Pure helpers (`network::compile`, `resources::resolve`, `naming::*`) stay
  as functions; they are within the tenet's allowed exceptions.

**E2. Remove the double `take_io`.**
- `instance.rs:46` calls `frontend.take_io()` only to read `initial_size`, then
  wraps the frontend in `CapturedFrontend` (`:99`), whose `take_io` is
  `expect("I/O taken once")` (`:120`).
- Pass `AgentIo` straight into `ExecBridgeOptions` (as Docker's `SpawnRequest`
  does) and delete `CapturedFrontend`'s I/O juggling.
- Replace `backend.rs:399` `expect("launch plan owns its labels")` with
  `ok_or(...)`.

**E3. No blocking calls on async worker threads.**
- `run_with_frontend` must run inside Tokio (`exec_bridge` spawns tasks), but
  it blocks:
  - `MsbDriver::call` blocks on `rx.recv()` (`msb_driver.rs:118-128`), reached
    from `build()` via `driver.image_config()` (`:455-458`) and
    `hypervisor_available()`, and from the VM boot; `MsbDriver::open` blocks
    on another `rx.recv()` at `:110`;
  - lease waits sleep in `lock_controlled` (`paths.rs:107`) when reached with
    a non-zero wait: `paths.lock()` (`LOCK_WAIT`) via `catalog::check` in
    `MsbDriver::open` and `backend.rs:590`, `:610`, and
    `import_lock_cancellable` (`IMPORT_LOCK_WAIT`, `backend.rs:564`).
    (`build()` → `plan()` → `image_lease(…, false)` uses `Duration::ZERO` and
    never sleeps.)
- Run these through `spawn_blocking` or make the driver async. Whichever you
  pick, keep `AgentInstance::run_with_frontend` synchronous.
- `exec_bridge.rs:101-108` spawns an OS thread that polls with `sleep` on
  every cancel, from `Execution::cancel` (`:62`), `cancel_handle` (`:68`) and
  the stuck detector (`:180`), so several can run at once. Replace this with a
  single `tokio::time`-based cancel task per execution.

### Group F — Errors, frontends, spec and docs (Medium/Low)

**F1. Typed, runtime-neutral errors.**
- Problem today:
  - `src/frontend/cli/mod.rs:267-269` maps every `EngineError::Container` to
    "awman requires Docker; install Docker Desktop…", and `:467-470` gives it
    exit code 3 ("missing container runtime"). Builtin raises that variant
    (e.g. "no live attach endpoint", `exec_bridge.rs:407-408`), and so does
    almost every WI 0125 builder failure, so builder and builtin failures
    print Docker install advice and exit 3.
  - The TUI prints the raw error, so the two frontends disagree.
  - The CLI lists twelve builtin/OCI variants just to `format!("{e}")` them
    (`cli/mod.rs:328-339`), with no default arm.
- Change:
  - Add builtin-specific variants (e.g. `BuiltinAttachUnavailable`).
  - Move all user guidance into `EngineError`'s `Display`, or into a
    `user_message()` in the engine, so all frontends render identical text.
    Exit codes follow the variant, not a blanket `Container` mapping.
  - Cut the CLI table down to formatting only, with a default arm.
- Engine error strings that embed command hints become typed variants whose
  hints are rendered consistently:
  - `builtin/backend.rs:232` and `:646` ("run awman ready…", `Config`);
  - `msb_driver.rs:34` ("awman clean") and `:37` ("run awman ready to import
    it");
  - `builtin/mod.rs:116-121` (`unsupported_target_reason`);
  - `external_build_hint` (`oci/resolve.rs:142-166`; callers `:131`,
    `ready/mod.rs:1733`, `agent/mod.rs:158`). It still says "the builtin
    runtime imports images; it does not build them", which is wrong when
    `builtin.builder.enabled` is on with a compatible source. Make the hint
    depend on the image's `BuilderSelection`.
- WI 0125 builder outcomes still carried as strings become typed:
  - builder unavailable (`ready/builder.rs:318-323`, "Apple Containers image
    transport is unavailable" `:339`, the driver-mismatch and buildx-inspect
    errors in `host_cli_backend.rs`, and the ~15 other `Container(String)`
    errors in `builder.rs:175-477`);
  - builder unreachable, stale receipt, receipt not cleared
    (`ready/mod.rs:418-426`, `:433-439`, `:458-466`, `:1674`);
  - build-ok/import-failed and "run `awman ready --build`"
    (`ready/mod.rs:1707-1720`, free-text warnings; the data is already typed
    in `ReadyImageResult`).
  Build exit failures (`ImageBuildExitNonzero`) and the incompatible-source
  warning (`ReadyImageWarning::IncompatibleBuilder`) are already typed; keep
  them.

**F2. Per-image ready hint.**
- Mostly done by WI 0125: `ReadySummary::hint()` (`src/data/ready_summary.rs:222`)
  exists and the CLI, TUI and API all render it with `rows()` and
  `outcome_line()`; no frontend computes `has_missing` any more.
- Remaining gap: `image_action` (`ReadyImageAction::{Build, Import,
  BuildAndImport}`) is chosen once per summary from `builder_enabled`
  (`ready/mod.rs:640-645`), not per image, and the not-ready case falls back
  to a generic "complete the required image steps" line.
- Change: record per missing image whether `awman ready` would build it,
  import it, or needs a source configured, from the preparer's
  `BuilderSelection` (C7 decision), and derive `hint()` from that.

**F3. `ReadyPhase` display parity.**
- Mostly done by WI 0125: `ReadyPhase` implements `Display`
  (`src/data/ready_phase.rs:32`), and the TUI (`tui/per_command/ready.rs:44-47`)
  and API (`api/command_frontend.rs:1044-1050`) both print `ready: {phase}`.
  Build and import outcomes are separate in rows, `ReadyStep`s and
  `ready --json` (`commands/ready.rs:156-175`).
- Remaining: the CLI still suppresses the phase (`cli/per_command/ready.rs:43-46`).
  Make one choice for all three frontends, and route the build-ok/import-failed
  text through the F1 typed path so it reads the same everywhere.

**F4. Update the grand architecture and the summary.**
- In `2026-grand-architecture.md`:
  - Redefine the tiers by capability shape: OCI images, ephemeral lifecycle
    and host mounts make the container tier; a persistent per-session VM with
    its own lifecycle makes the sandbox tier.
  - Add the builtin backend to the container-tier driver list (`:56-57`).
  - Update the runtime-selection bullet (`:61`, still
    `agent_runtime::detect(&GlobalConfig)` with three strings) to
    `detect_effective(&EffectiveConfig)` (`agent_runtime/mod.rs:389`) and all
    four runtime strings, including `builtin-experimental`.
  - Note that builtin's per-launch settings (A1) are session-scoped.
  - Describe the WI 0125 builder as a source-matched build step owned by the
    Layer 1 `AgentImagePreparer`, reaching the host Docker/Apple build
    integration only through `LocalImageBuilder`, with `BuildPolicy` deciding
    which commands may build. Agent execution never leaves the builtin
    runtime.
- Update `four-layer-summary.md` to match.
- Record the B1 frontend-ownership rule in the Layer 2 section: commands own
  their frontend through a shared handle and never reclaim it.
- `security.md`, `cli.md` and `design.md` already describe the builder
  exception (WI 0125). Check that they still match after C1 (names, caller
  policy) and fix any drift. In `cli.md`, state that API session setup may
  build when the compatible source store lacks the image (decision 5), next
  to the import-only statement for preflight and squad (`:205`).
- In `cli.md`'s runtime section, record that `builtin.stateDir` is
  effectively process-wide and a differing repo value is rejected
  (decision 7), and that agents missing a tool-restriction flag are rejected
  on every runtime (decision 8).

**F5. Spec `AWMAN_BUILTIN_STATE_DIR`.**
- It is first in the state-dir precedence (`env.rs:109`; `effective.rs:275`)
  and is already in the user docs (`docs/07-configuration.md:425`, `:574`;
  `docs/11-runtimes.md:266`; `docs/13-cleaning-up.md:30`).
- The only gap is `aspec/uxui/cli.md`, which mentions `stateDir` (`:168`) but
  not the env var. Add it there.

## Edge Case Considerations:

- **A1:**
  - A TUI tab opened in a non-git directory, or a repo with no `builtin`
    block, uses the global policy exactly as today.
  - A repo block that widens the global policy fails that launch only; other
    tabs keep working.
  - `stateDir` stays process-wide. A repo-level `stateDir` that differs from
    the process's fails that launch with a typed config error naming both
    paths (decision 7); other tabs keep working.
  - A background (`start_background`) launch gets the same per-launch settings
    as a foreground one.
- **A1, runtime switching:** changing `runtime` via `awman config` mid-session
  keeps working, because `Engines::detect_result` still permits it.
- **B1:**
  - Parallel workflow steps still pair each step's `Running` report with its
    own `take_io` under one lock (`factory.rs:35-42`).
  - The leader-phase `engine_tx` registration and the later engine overwrite
    still target the same shared slot.
  - Pause/Restart/Abort paths in the leader loop still flush queued messages
    exactly once.
- **B2:** an execution whose `wait()` future is dropped mid-await (command
  abort) releases its frontend and PTY handles promptly on every runtime.
- **C1:**
  - An image present in the runtime but from a different source is
    re-acquired by preflight too (ready semantics). This changes preflight
    behaviour, which used to skip it, and must be noted in the release notes.
    Preflight still never builds.
  - Import-less runtimes (Docker/Apple) keep their build path in `ReadyEngine`.
  - Builder disabled: behaviour is exactly today's disabled mode, including
    `--build`/`--no-cache` acquisition-refresh semantics.
  - Mixed per-image sources in one `ready` run: compatible images build,
    incompatible ones warn once and import, and neither redirects the other.
  - Build succeeded, import failed: a later `ready` retries the import from
    the pending receipt; a preflight in between does not accept the stale
    installed image as the newly built one.
  - Two processes (e.g. `ready` and an API session setup) building the same
    store/reference are serialised by `ImageBuildLockStore`, as today.
- **C2:** any `MSB_*` variable in the full list now fails at `builtin::open`
  with `AmbientRuntimeOverride`. Surface the same error in all three
  frontends.
- **C4:**
  - Keep the builtin rule that a lagging attach client gets FAILED and never a
    clean exit.
  - Keep the 1 MiB frame cap and the bounded-write timeout.
- **C5:** a missing tool flag becomes an error on Docker and Apple too
  (decision 8); note it in the release notes. Check `awman ready`'s agent matrix still
  passes for every agent.
- **D1:**
  - The worker is re-exec'd from the same binary path, including after an
    in-place upgrade (`embedded/mod.rs` comment). That must still hold.
  - Windows and Linux builds without the `builtin-runtime` feature must still
    compile.
- **D2:** caches written before this item (including legacy
  `docker-store`/`apple-store` keys) still hit without re-acquisition.
- **E3:** a VM boot cancelled while in `spawn_blocking` must still stop the
  VM, and must not leave a lease file or a half-registered sandbox.

## Test Considerations:

- **Unit tests:** every new type and every moved function
  (`BuiltinLaunchSettings` mapping, `EffectiveConfig::builtin_settings`,
  `AgentImagePreparer`, `BuildPolicy`, `BuilderSelection` incl. `Disabled`,
  the `attach_protocol` codec, `AgentArgv`, `ExecBridgeOptions`, the D2/D3
  Layer 0 types, per-image `ReadySummary::hint`, `EngineError` display text
  and exit codes).
- **Integration:**
  - two-repo, one-`Engines` session test for network, resources, registries
    and builder flag (A1);
  - squad task inherits repo narrowing (A1);
  - fake-runtime workflow, dynamic-workflow and squad tests that hold or never
    release the frontend, through proxy and phase-container routes (B1);
  - cross-runtime frontend-release conformance test incl. dropped `wait()` (B2);
  - preflight/ready parity on the same source and tag, builder on and off,
    compatible and incompatible sources (C1);
  - legacy-spelling cache records still hit after the D2 move (keep
    `cache.rs:999` and `oci/mod.rs:1046`
    `legacy_docker_cache_key_is_found_without_source_contact`);
  - attach round-trip on both servers over the shared codec (C4);
  - Docker/Apple vs builtin argv parity table (C5).
- **Regression — keep green throughout, relocating as needed:**
  - `frontend_is_released_before_wait_resolves`; it must fail with the
    `drop(frontend)` line removed.
  - WI 0125 config tests: `builtin_runtime.rs` (`:357`, `:373`, `:384`,
    `:396`, `:407`, `:432`), `effective.rs` (`:1322`, `:1355`),
    `fields.rs:362`, `global.rs:439`, `repo.rs:549`, `config.rs` (`:1160`,
    `:1181`, `:1196`, `:1211`, `:1502`).
  - WI 0125 alias/cache/source tests: `image_source.rs:298`,
    `oci_source_key.rs:63`, `cache.rs:999`, `oci/mod.rs:635`, `:1046`,
    `resolve.rs` (`:259`, `:334`, `:358`, `:401`).
  - WI 0125 builder, bridge and store tests: `ready/builder.rs` (`:528`–`:782`),
    `apple_store/tests.rs` (`:194`, `:209`), `ready/build.rs`, `ready/import.rs`,
    `image_build_lock.rs:126`, `image_build_state.rs:129`,
    `private_files.rs` (`:199`, `:217`).
  - WI 0125 `ReadyEngine` tests (`ready/mod.rs:3579`–`:4430`): every
    `builder_*` test, `two_sessions_with_opposite_builder_settings_each_see_only_their_own`,
    `build_reference_normalization_detects_docker_equivalents`,
    `prevalidation_marks_colliding_agent_outputs_without_probing`,
    `unbound_source_with_pending_receipt_forces_fresh_import`,
    `failed_prevalidation_closes_the_cached_image_progress_row`.
  - WI 0125 caller/frontend tests: `workflow_preflight.rs:735`,
    `session_setup.rs:811`, `dispatch/mod.rs:2270`, `commands/ready.rs`
    (`:416`, `:440`, `:470`, `:496`), `ready_summary.rs` (`:295`, `:312`),
    `api/command_frontend.rs` (`:1228`, `:1246`), `tui/per_command/ready.rs`
    (`:166`, `:182`, `:219`).
- **End-to-end:**
  - CLI, TUI (headless test harness) and API each surface identical text for
    a builtin attach failure (F1), the ready hint (F2), the incompatible-source
    builder warning and a build-ok/import-failed result (F1/F3).
  - `awman exec workflow --dynamic` completes on `builtin-experimental` on
    Apple Silicon. Record native evidence in the style of WI 0123.
- WI 0125 shipped no opt-in native Docker/Apple builder scenarios (all its
  tests use `FakeLocalBuilder`/`RecordingRunner`). This item does not add
  them, and must not claim native builder coverage; report it as a WI 0125
  follow-up.
- **Build:** `make all`, `make test` and the static release build produce one
  `awman` binary after D1, with and without `--features builtin-runtime`.
  `cargo fmt --check` and `cargo clippy --all-targets` are clean.
- Where Docker or KVM is unavailable, use fake drivers, and state explicitly in
  the PR which suites were not run.

## Codebase Integration:

- Follow established conventions, best practices, testing and architecture
  patterns from the project's aspec, especially
  `aspec/architecture/2026-grand-architecture.md` (Tenets 1–3, Layer 0/1/2
  responsibilities) and `aspec/architecture/security.md` (the network
  narrowing, ambient-override refusals and the builder's source-matched
  exception must stay fail-closed).
- Suggested execution order:
  1. A1/A2 → B1/B2 (user-visible safety and crash fixes first; B0 is done);
  2. C2/C3 → D4 → C1 (D4 gives C1 its typed inputs);
  3. D2/D3 → C4/C5/C6/C7 → E1–E3;
  4. D1;
  5. F1–F5 (F4 may land early, since it is documentation only).
- One step per commit; reference the step ID (e.g. `WI0126 A1`) in each commit
  subject.
- When a step conflicts with WI 0124 (Linux KVM) or with the structure WI 0125
  shipped, this item takes precedence: restructure the code and update those
  items' references rather than preserving the old structure. WI 0125's
  user-visible behaviour is not up for change; if a step would alter it, stop
  and ask the developer.

## Documentation

After implementation is complete, update user-facing documentation in `docs/`
to reflect the current state of the tool:

- `docs/11-runtimes.md` and `docs/07-configuration.md`:
  - repo-level `builtin.network`, `vcpus`, `memoryMib` and `registries` now
    apply per session in the TUI, the API and squad;
  - `stateDir` scope;
  - verify the refused `MSB_*` list and `AWMAN_BUILTIN_STATE_DIR` text still
    match the code (both are already documented).
- `docs/04-security-and-isolation.md`: the repo network block narrows the
  policy for every launch in that repo.
- Any doc describing `awman ready`, session setup or workflow preflight image
  behaviour: one acquisition rule (C1); `ready` and API session setup may
  build, preflight and squad stay import-only and now re-acquire an image
  from a different source.
- `docs/07-configuration.md`: `builtin.stateDir` at repo scope must match the
  process-wide value or the launch is rejected.
- Agent/tool-restriction docs: a missing allowed/disallowed-tools flag is now
  an error on every runtime, not only builtin.
- Any doc quoting builder errors, warnings, the ready hint or exit codes:
  match the unified F1/F2 text.
- **Update existing feature docs**; create new user guides only if a new
  user-visible feature warrants it.
- **Never create work-item-specific docs**. Keep technical and implementation
  detail in this work item and in code comments.

See `CLAUDE.md` for more guidance on documentation standards.
