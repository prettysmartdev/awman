# Work Item: Task

Title: Architecture audit remediation, part 1 — the Critical and High findings
(F-01 through F-12) from the 2026-09-03 architecture audit
Issue: `aspec/review-notes/0113-architecture-audit.md`

## Summary:
- The 2026-09-03 architecture audit (`aspec/review-notes/0113-architecture-audit.md`,
  hereafter "the report") found the mechanical layering gates green but the
  spirit of `aspec/architecture/2026-grand-architecture.md` eroded in three
  places: daemons bootstrapped inside `src/frontend/`, a TUI that has grown
  its own git engine, session policy, remote poller and `squad attach`
  implementation, and a catalogue that is no longer the single source of truth
  for flag defaults or for which commands exist.
- This work item fixes the one Critical and eleven High findings: **F-01
  through F-12**. The report is the "what" (evidence, `file:line`, why it
  matters); this document is the "how". Every section below cites the finding
  it implements. Do not re-derive the evidence here — read the report.
- The Medium and Low findings (F-13 through F-54) are WI 0114. Where a step in
  this work item naturally absorbs a 0114 item, the step says so; otherwise
  leave 0114 items alone.
- Implementing agents MUST read `aspec/architecture/2026-grand-architecture.md`
  and `aspec/architecture/four-layer-summary.md` in full before starting, and
  hold the report's rule table (T1–T3, L0–L4, S1–S2, P1–P2) in mind for every
  change. When in doubt about a layer boundary, ask the developer; do not
  guess.

## User Stories

### User Story 1:
As a: developer adding a fourth frontend (desktop app, editor extension,
Kubernetes operator)

I want to:
consume `Dispatch`, `Engines::build`, and the L2 daemon-bootstrap types
without re-implementing engine wiring, session fallback policy, squad
supervision, git diff summaries, remote polling, or `squad attach`

So I can:
build the new frontend as a presentation layer only, as Tenet 2 promises
(report F-01 through F-09).

### User Story 2:
As a: user of the CLI, TUI and API

I want to:
every command, flag default and flag implication to behave identically in all
three modes because they come from one catalogue

So I can:
trust that `--json`, `--yolo`, `--auto` and the per-command defaults mean the
same thing whichever frontend I use (report F-10, P2).

### User Story 3:
As a: maintainer

I want to:
the compiler to tell me about dead code, and the Docker and Apple container
backends to share one spawn path

So I can:
stop carrying a never-called sandbox backend surface and stop making every
spawn fix twice (report F-11, F-12).

## Implementation Details:

Work the steps in the order given. It is the report's "Proposed execution
order" filtered to F-01–F-12, with F-12 first because it is free and makes
every later deletion visible to the compiler. One finding per commit unless
a step says otherwise. After every step run `make pre-push`; after every step
that moves code across a layer boundary also run `make architecture-lint` on
its own and confirm it still passes.

### Step 0 — Decisions (resolved 2026-09-04)

The developer answered every open question; the full table is in the report
under "Decisions — 2026-09-04". The ones that shape this work item:

- **Q1 — strict Layer 0.** No git, network or process spawning in `src/data/`.
  Step 11 (F-07) moves the issue provider to Layer 1 as specified.
- **Q4 — squad adheres to the grand architecture with no exception.** All
  squad business logic moves down, the majority into the engine layer;
  `src/frontend/squad/` keeps only presentation and I/O and calls down
  through traits. Step 3 (F-02) is written to that decision, not to the
  report's milder "L2 bootstrap" recommendation.
- **Q7 — `api_allowed: false` is long-term policy for interactive/PTY
  commands.** Step 10 registers `squad attach` with `api_allowed: false`
  (it attaches a PTY) and records the P2 exception in the catalogue doc
  comment.
- **F-12 — delete** the never-called `SandboxBackend` methods and `SandboxId`
  (Step 1).
- **F-35 — delete** `AuthCommand` and `DownloadCommand` (0114), which Step 9
  must anticipate: give `BuiltCommand` no arms for them.

### Step 1 — F-12: remove the stale lint suppressions and the dead code they hide

Report: F-12 (`src/lib.rs:15`, `src/data/mod.rs:1`,
`aspec/review-notes/0113-architecture-audit-hidden-warnings.txt`).

1. Delete `#![allow(dead_code)]` from `src/lib.rs` and
   `#![allow(unused_imports)]` from `src/data/mod.rs`.
2. Build with `cargo clippy --all-targets -- -D warnings`. Resolve each of the
   29 warnings by **deletion**, not by a narrower `#[allow]`, unless the item
   is referenced by a later step in this work item:
   - `SandboxBackend::{start_sandbox, restart_sandbox, exec_in_sandbox, remove}`
     and `SandboxId` (`src/engine/sandbox/backend.rs:19-58`): delete
     (developer decision, report "Decisions" table). The trait keeps only
     what `SandboxRuntime` calls today.
   - `WorkflowEngine.git_engine` / `overlay_engine`: handled by 0114 F-34, but
     the fields must not survive this step with an `#[allow]`. Delete the two
     fields and their constructor parameters now (that part of F-34 is
     absorbed here); leave the `PhaseKind` consolidation to 0114.
   - `image_exists_locally` (`src/engine/agent/mod.rs:906-917`) and the wrong
     doc at `:110-111`: delete the function, fix the doc.
   - `src/frontend/cli/output.rs:11-48` five dead fns, `DockerBackend::is_available`
     (`docker.rs:39`), `daemon_guard.rs:19` import, and the rest of the list:
     delete.
3. `make pre-push` must pass with no new `#[allow]` anywhere. Add a check to
   `tools/architecture-lint.sh` (or a colocated test) that fails if a
   crate-level `#![allow(dead_code)]` or `#![allow(unused_imports)]`
   reappears under `src/`.

### Step 2 — F-05: one `Engines::build`, one `Startup`, and a Layer 4 that only picks a frontend

Report: F-05 (`src/main.rs:44-152`, `src/frontend/api/mod.rs:66-110`,
`src/frontend/squad/mod.rs:156-187`).

1. In `src/command/dispatch/mod.rs` next to `Engines`, add:
   - `Engines::build(global: &GlobalConfig, session: &Session) -> Result<Engines, EngineError>`
     — the exact construction `main.rs:99-152` performs today (runtime
     detection via `Engines::detect`, `GitEngine`, `OverlayEngine::new(&session)`,
     `AuthEngine::new(&session)`, `AgentEngine`, `EngineWorkflowStateStore::at_git_root`).
   - `Engines::for_daemon(kind: DaemonKind, paths: &DataPaths) -> Result<Engines, EngineError>`
     — the variant the API server and squad daemon use (`OverlayEngine::with_auth_resolver`,
     `AuthEngine::with_paths`, state store rooted at the daemon root). The two
     existing daemon copies differ only in these inputs; parameterise, do not
     keep two functions.
2. Add an L2 `Startup` type (`src/command/startup.rs`) whose `run(working_dir, env) -> Result<StartupOutcome>`
   performs, in today's order: removed-flag hint, legacy migration
   (`migration::migrate_global_dir`, `check_deprecated_env_vars`), global config
   load, runtime detection with the `UnknownRuntime` fatal path, git-root
   resolution, `migrate_repo_dir`, `Session::open_at_git_root`, then
   `Engines::build`. `StartupOutcome` carries `session`, `engines`,
   `fatal_runtime_error: Option<String>` and any messages to print.
3. Rewrite `src/main.rs` to: build clap from the catalogue, parse, call
   `Startup::run`, print its messages, choose CLI or TUI. `init_tracing` may
   stay in `main.rs` (process-level setup is Layer 4's job) but must not grow.
   Nothing else remains.
4. Replace the engine construction in `src/frontend/api/mod.rs` and
   `src/frontend/squad/mod.rs::build_engines` with `Engines::for_daemon`.
   (Steps 3 and 4 move the surrounding bootstrap; this step only removes the
   wiring duplication.)
5. Tests: the three `main.rs` routing tests keep passing; add a unit test in
   `dispatch/mod.rs` that `Engines::build` and `Engines::for_daemon` produce
   a container-tier bundle under the default config and a sandbox-tier bundle
   under `runtime: "docker-sbx-experimental"`. Fold the nine test copies of
   `make_engines` (0114 F-52) into `Engines::for_tests(root)` here, since this
   step touches every one of them anyway.

### Step 3 — F-02: the squad daemon becomes an engine; `frontend/squad` keeps router and bind

Report: F-02 (`src/frontend/squad/mod.rs:32-150`,
`src/frontend/squad/unattended.rs:232-266, 336-383, 579-737`).
Decision Q4: all squad business logic moves down, the majority into
`src/engine/squad/`; the frontend is presentation and I/O only, exactly like
the CLI, TUI and API. Squad gets no architectural exception.

Target layering after this step (the `squad` equivalent of
`ContainerRuntime` / `WorkflowEngine` / `ExecWorkflowCommand` / CLI):

| Layer | Owns |
|---|---|
| L0 `src/data/fs/` | `TaskStore`, `SquadPaths`, `DaemonPaths`, `ServerMeta`, a new `SquadRunLogs` (run-log file layout), `RunVerdict` (0114 F-47) |
| L1 `src/engine/squad/` | `SquadScheduler` (exists), `SquadAgentLauncher` (exists), plus new: `SquadDaemonEngine` (bootstrap: DB relocation, store open/migrate, orphan-run reconciliation, stray-container scan, scheduler spawn, `ServerMeta` persistence, shutdown), `SquadSupervisor` (daemon lifecycle: ensure running, key state, health — moved from L2), and the gateway transport (`RemoteTaskGateway` over the L1 HTTP client from 0114 F-28) |
| L2 `src/command/commands/squad/` | `SquadCommand` / `SquadDaemonCommand::run_start` (collects flags, config, `Engines`; runs runtime admission; calls `SquadDaemonEngine::bootstrap`; wires frontend traits), `LocalTaskEvaluator` (stays L2 because it drives `ExecWorkflowCommand` — this is the "go-between" role the grand architecture assigns to Layer 2), `LocalTaskGateway`, `HeadlessDefaults::squad()` (0114 F-13) |
| L3 `src/frontend/squad/` | Axum router, bind, TLS listener, `UnattendedFrontends` trait impls that only forward to L2 defaults and L0 log writers |

1. Create `SquadDaemonEngine` in `src/engine/squad/daemon.rs` with
   `bootstrap(deps: SquadDaemonDeps) -> Result<SquadDaemonEngine, EngineError>`
   performing, in the order the current doc comment calls load-bearing:
   legacy DB relocation, store open and migrate, orphan-run reconciliation,
   stray-container scan, scheduler construction and spawn, `ServerMeta`
   persistence. `SquadDaemonDeps` carries the `Arc<dyn AgentRuntimeEngine>`,
   `SquadPaths`, `DataPaths`, `Env`, and a `Box<dyn TaskEvaluator>` (the L2
   evaluator passed down through the existing trait). It exposes typed
   handles (`store`, `scheduler_handle`, `bind_addr`, `auth_mode`) and
   `shutdown()`. Runtime admission (`require_container_tier`) stays in L2
   because it is a policy about which tier squad supports; move its check
   into `Capabilities::squad_supported` (0114 F-40) so the engine can assert
   it too.
2. Move `SquadSupervisor` (`ensure_running`, `gateway_from_meta`,
   `key_state`, `daemon_is_running`, `probe_gateway`) from
   `src/command/commands/squad/daemon.rs` to `src/engine/squad/supervisor.rs`.
   It spawns the daemon through the L1 `DaemonSupervisor` (0114 F-29; until
   that lands it may call `DaemonProcess` directly, flagged with a TODO that
   0114 removes). Step 5 and Step 10 call it from L2.
3. `SquadDaemonCommand::run_start` (L2) becomes: collect config, admission
   check, `Engines::for_daemon`, construct `LocalTaskEvaluator` +
   `LocalTaskGateway`, `SquadDaemonEngine::bootstrap`, then hand a
   `SquadDaemonHandles { store, gateway, session, auth_mode, bind_addr }` to
   `SquadCommandFrontend::serve_squad_daemon`. `src/frontend/squad/mod.rs::serve`
   becomes: build router from the handles, bind, serve until shutdown,
   `engine.shutdown()`. Delete `serve_with` and `build_engines`.
4. `unattended.rs` persistence: move the run-log file layout
   (`step_log_file_name`, `begin_phase_step`, `prepare_container_log`, the
   path-traversal check) into an L0 `SquadRunLogs` type in
   `src/data/fs/squad_paths.rs`, mirroring `CommandLogWriter`. The unattended
   frontend calls it and opens no files itself.
5. `unattended.rs` policy answers stay as they are in this work item; 0114
   F-13 replaces them with `HeadlessDefaults::squad()`. Do not change any
   answer here.
6. After this step `src/frontend/squad/` must contain no `TaskStore`,
   `SquadScheduler`, `SquadSupervisor` or `DaemonProcess` call, no
   `std::fs`, and no decision beyond HTTP status mapping. `grep` for those
   identifiers is the acceptance check.
7. Tests: `tests/squad_daemon_e2e.rs`, `tests/squad_daemon_http.rs`,
   `tests/squad_mutual_exclusion.rs`, `tests/squad_runtime_discovery.rs`,
   `tests/squad_scheduler.rs` must pass unchanged. Add engine tests for
   `SquadDaemonEngine::bootstrap` against a temp `SquadPaths` (fresh store,
   store with orphaned runs, legacy DB present) and keep the sandbox refusal
   test (`tests/squad_sandbox_refusal.rs`) passing from L2.

### Step 4 — F-03: API server bootstrap, queue worker and session close move to Layer 2

Report: F-03 (`src/frontend/api/mod.rs:33-52, 66-110, 112-206, 244-261`,
`src/frontend/api/queue_worker.rs`, `src/frontend/api/routes.rs:534-671, 732-786`).

1. Create `src/command/commands/api_server/runtime.rs` with
   `ApiServerRuntime::bootstrap(config: ApiServeConfig, engines: Engines) -> Result<ApiServerRuntime, CommandError>`
   owning: store open and migrate; session restore from SQLite (including the
   `SessionType::Remote` reconstruction — keep the empty-placeholder behaviour
   but document it as a known gap in a code comment, and add `repo_url`/`branch`
   persistence as a follow-up note, not here); marking in-progress setups
   failed after restart and deleting their clones; purge of closed sessions
   older than 24 h; stale-command recovery; worker-pool sizing from
   `global_config.workers()`. It exposes `store`, `sessions`, `event_buses`,
   `paths`, `auth_mode` and a `spawn_workers(frontend_factory)` method.
2. Move `QueueWorker` to `src/command/commands/api_server/queue_worker.rs`.
   Its only frontend dependency becomes a trait
   `ApiCommandFrontendFactory { fn frontend_for(&self, cmd: &CommandRecord, ...) -> Box<dyn DispatchFrontend> }`
   that `src/frontend/api/` implements. Replace `derive_command_status` with
   `CommandOutcome::exit_code(&self) -> i32` and `CommandOutcome::is_partial_failure(&self) -> bool`
   on `CommandOutcome` in `src/command/dispatch/mod.rs`; the CLI's
   `outcome_exit_code` (`src/frontend/cli/mod.rs:446-455`) calls the same
   method (this absorbs 0114 F-42 — note the behaviour change there: the API
   now reports `error` for a partially failed `new skill --pull-all`).
3. Add `ApiSessionLifecycle` (same module) with
   `close(session_id) -> Result<CloseOutcome, CommandError>` where
   `CloseOutcome::{Closed, Draining { running_command_id, cancelled: Vec<String> }, AlreadyClosing, NotFound}`,
   and `setup_readiness(session_id) -> SetupReadiness`. Both
   `QueueWorker::post_execution_check` and `routes.rs::handle_close_session`
   call `close`; `resolve_setup_status` becomes a status-code map over
   `SetupReadiness`. There must be exactly one implementation of the
   drain-and-close ordering after this step.
4. `ApiServerCommandFrontend::serve_until_shutdown` takes `ApiServerRuntime`.
   `src/frontend/api/mod.rs::serve` shrinks to: router, TLS listener, signal
   handling, serve.
5. Tests: `tests/api_parity/*` and `tests/headless_integration.rs` pass
   unchanged. Add unit tests for `CloseOutcome` transitions (closed with no
   queue, draining with a running command, already closing, unknown id) and
   for `CommandOutcome::exit_code` covering `ExecWorkflow`, `ExecPrompt`,
   `New(Skill)` partial failure, and a plain success.

### Step 5 — F-04: squad routing and daemon supervision leave `cli::run` and the TUI `App`

Report: F-04 (`src/frontend/cli/mod.rs:100-116, 122-153, 522-536`,
`src/frontend/tui/app.rs:96-107, 296-627`).

1. Add `gateway_need: GatewayNeed` to `CommandSpec` in
   `src/command/dispatch/catalogue.rs` with `GatewayNeed::{None, Running, IfRunning}`.
   Set `Running` on `squad add|edit|list|show|remove|pause|resume|trigger`,
   `IfRunning` on `squad status`, `None` elsewhere. Delete both `matches!`
   lists in `cli/mod.rs`. Add `requires_container_tier: bool` to `CommandSpec`
   (true for the squad subtree) so the runtime-tier guard is catalogue-driven.
2. In `Dispatch::run_command`, before building the command: if the spec's
   `requires_container_tier`, call `require_container_tier(&self.engines)`;
   then resolve the gateway via a new L2 `SquadGatewayResolver::gateway_for(need: GatewayNeed) -> Result<Option<Arc<dyn TaskGateway>>, CommandError>`
   (a thin L2 type in `src/command/commands/squad/` that calls the L1
   `SquadSupervisor` from Step 3 for ensure_running → key_state and maps
   `EngineError` to `CommandError`). A freshly minted
   key is surfaced through a new `SquadCommandFrontend::show_key_setup(&mut self, setup: &SquadKeySetup)`
   method (CLI prints to stderr as today; TUI raises its existing dialog);
   `SquadKeyState::Missing` becomes a typed `CommandError::SquadKeyMissing`
   whose `Display` is the current `missing_squad_key_error` text. Delete that
   function from the CLI.
3. Add `SquadGatewayResolver::open_for_frontend(&self) -> Result<SquadStartup, SquadStartError>`
   (L2, over the L1 supervisor) returning `SquadStartup { gateway, key_state, key_setup: Option<SquadKeySetup> }`
   and a typed `SquadStartError::{DaemonConflict, SandboxRuntime, KeyMissing, Other(String)}`.
   Rewrite `App::{open_or_focus_squad_tab, build_squad_tab, ensure_squad_gateway, refresh_squad_key, assemble_squad_tab}`
   to call it and map each typed outcome to a dialog. Delete
   `wrap_squad_startup_error` and its string-prefix matching.
4. Move `App::squad_synthetic_session` to L0 as
   `Session::open_squad_root(env: &Env) -> Result<Session, DataError>` in
   `src/data/session.rs` (it creates the squad root directory and opens the
   session rooted there). The TUI calls it.
5. Tests: `tests/squad_cli_gateway.rs`, `tests/squad_auth_key.rs`,
   `tests/squad_tui_tab.rs` pass; add a catalogue test asserting every
   `squad` subcommand has a non-`None` `gateway_need` except `start|stop|logs|attach`,
   and a `Dispatch` test that a `Running` need with a stopped daemon and no
   key yields `CommandError::SquadKeyMissing`.

### Step 6 — F-06: `GitEngine::diff_summary` replaces the TUI's git spawns

Report: F-06 (`src/frontend/tui/git_sidebar.rs:96-234, 273-374`).

1. In `src/engine/git/` add `pub struct GitDiffSummary { branch: Option<String>, files: Vec<GitFileEntry>, added: u32, removed: u32 }`,
   `GitFileEntry { path, change: GitFileChangeType, added, removed }`, and
   `GitEngine::diff_summary(&self, root: &Path) -> Result<GitDiffSummary, EngineError>`.
   Move `parse_porcelain_status`, `parse_numstat`, `build_summary`, the
   untracked-file line counting and the "no commits yet → everything Added"
   fallback there, byte-for-byte in behaviour. If the types must be
   serialisable for the API later, put them in `src/data/git.rs` instead.
2. `git_sidebar.rs` keeps `GitSidebarState`, `sidebar_title`, `sidebar_width`
   and `start_git_diff_poll_task`, which now calls `engines.git_engine.diff_summary`.
   No `process::Command` and no `tokio::fs` remain in `src/frontend/tui/`
   outside tests.
3. Tests: move the sidebar's parser tests to `tests/engine/git_engine.rs` (or
   colocated in the engine); the TUI keeps only rendering tests.

### Step 7 — F-09: remote workflow polling moves to Layer 2

Report: F-09 (`src/frontend/tui/per_command/remote.rs:23-222`).

1. Add `TaskGateway::workflow_state(&self, task: &str) -> Result<Option<WorkflowState>, CommandError>`
   to `src/command/commands/squad/gateway.rs` (both `LocalTaskGateway` and
   `RemoteTaskGateway` implement it; the remote impl owns the
   `["tasks", task, "workflow"]` route).
2. Add `RemoteClient::job_status(&self, id) -> Result<JobStatus, CommandError>`
   with `JobStatus::{Queued, Running, Done, Error}` in
   `src/command/commands/remote_client.rs`; the `"done" | "error"` literals
   live there only.
3. Move `WorkflowStateSource` and `RemoteWorkflowPoller` to
   `src/command/commands/remote_client.rs` (or a `workflow_poll.rs` beside
   it). The poller keeps the 500 ms interval, final-refresh and
   errors-freeze rules and takes `on_state: Box<dyn FnMut(&WorkflowState) + Send>`.
   The TUI supplies a callback that converts to `WorkflowViewState`. Delete
   `src/frontend/tui/per_command/remote.rs`'s trait and impls and the
   re-exports in `tui/mod.rs:48-51`. `src/frontend/` must contain zero
   `pub trait` definitions after this step.
4. Tests: `tests/remote_integration.rs` passes; add a unit test for the
   poller using a fake `WorkflowStateSource` asserting the final refresh
   after a terminal status and that a transient error does not stop polling.

### Step 8 — F-08: `SessionManager` owns session creation for TUI and API

Report: F-08 (`src/frontend/tui/key_handler.rs:983-1004`, `src/frontend/tui/mod.rs:91`,
`src/frontend/api/mod.rs:135`, `src/frontend/api/routes.rs:51-70`,
`src/frontend/squad/state.rs:13-22`).

1. Add `SessionManager::open_or_create(&self, dir: PathBuf, opts: SessionOpenOptions) -> Result<SessionId, DataError>`
   in `src/data/session_manager.rs` using `Session::open_or_workdir_fallback`
   as the single non-git fallback policy, plus `get(&SessionId) -> Option<Arc<RwLock<Session>>>`
   and `remove`. Keyed lookup by string id must exist because the API stores
   ids in SQLite.
2. TUI: `handle_new_tab_path` and `App::add_tab` call `open_or_create`; a
   `Tab` stores the `SessionId` and reads the session through the manager.
   (The full "Tab as a view of SessionState" split is 0114 F-22; here only
   the ownership of the `Session` object changes.) Resolve `InitialTab::Normal(Session)`
   into `InitialTab::Normal` built from `ctx.session` while touching this
   (absorbs 0114 F-54's TUI half).
3. API and squad daemon: replace the `HashMap<String, Arc<RwLock<Session>>>`
   in `AppState`, `QueueWorker` (now L2 after Step 4) and `SquadAppState`
   with `Arc<SessionManager>` (absorbs 0114 F-54's API half).
4. Behaviour change to record in the changelog: the TUI's Ctrl-T fallback
   for non-git directories now matches the API's `open_or_workdir_fallback`.
5. Tests: `tests/tui_tabs.rs` and `tests/api_parity/routes.rs` pass; add a
   `SessionManager` test for git dir, non-git dir, and missing dir.

### Step 9 — F-10: the catalogue owns flag defaults and implications; per-command constructors

Report: F-10 (`src/command/dispatch/mod.rs:384-1080, 1092-1180`).

1. Add `Dispatch::resolve_flags(&self, path: &[&str]) -> Result<ResolvedFlags, CommandError>`
   that walks the `CommandSpec`'s `FlagSpec`s once: reads each flag through
   `CommandFrontend::flag_*`, applies `FlagDefault` when absent, then applies
   `implies` transitively (`json → non-interactive`, `yolo → worktree`,
   `auto → worktree`). `ResolvedFlags` exposes typed getters
   (`bool(name)`, `str(name)`, `u16(name)`, `enum(name)`) that panic only on
   a name absent from the spec (a programming error caught by the parity
   test). Delete the six literal defaults at `:406, :557, :691, :716, :944, :1008`
   and the hand-written implication `if`s at `:419-422, :543-546, :1342`.
2. Give every `*Command` a constructor
   `fn from_input(ctx: &BuildContext) -> Result<Self, CommandError>` where
   `BuildContext { flags: &ResolvedFlags, args: &ResolvedArgs, engines: &Engines, session: Session, gateway: Option<Arc<dyn TaskGateway>>, caller: CallerContext }`.
   Register it on `CommandSpec` as `build: fn(&BuildContext) -> Result<BuiltCommand, CommandError>`.
   `Dispatch::build_command` becomes: canonicalise path, `resolve_flags`,
   look up spec, call `spec.build`. `run_command` becomes a single generic
   `built.run_with_frontend(frontend)` through the `Command` trait (add a
   `BuiltCommand::into_command(self) -> Box<dyn Command>` if the enum must
   stay for the `exec workflow` carve-out).
3. Add a parity test in `src/command/dispatch/projections/parity_test.rs`
   that, for every `FlagSpec` with a `FlagDefault`, building the command with
   no flags supplied yields the catalogue default (compare against the
   command's flags struct via a `Debug` snapshot or typed accessors), and that
   every `implies` edge is honoured.
4. Do **not** change `parsed_input.rs` here (0114 F-26) beyond what compiling
   requires.
5. Tests: `tests/cli_parity/*`, `tests/api_parity/*`, `src/frontend/tui/tests/key_handler_tests.rs`
   pass unchanged.

### Step 10 — F-01: `squad attach` becomes a Layer 2 command

Report: F-01 (`src/frontend/tui/squad_attach.rs:56-522`, `src/frontend/tui/app.rs:631-642`,
`src/frontend/cli/mod.rs:74-78`, `src/frontend/cli/per_command/squad_attach.rs`,
`src/frontend/attach.rs`, catalogue `:1465`). Depends on Steps 5 and 9.

1. Create `src/command/commands/squad/attach.rs` with `SquadAttachCommand`
   (built by `from_input` from the catalogue's existing `attach` spec, with
   `gateway_need: GatewayNeed::Running` and the container-tier guard from
   Step 5). It owns: supervisor/gateway resolution; `list_task_containers`
   (runtime discovery by name prefix); `resolve_attach_target` and the two
   error constructors, moved verbatim from `src/frontend/attach.rs`; the
   evaluation-phase vs workflow-phase decision; the `SquadSlotDriver`
   reconcile logic (pure: step transitions → attach/exit actions), moved from
   the TUI; the container-id/name matching heuristic.
2. `run_with_frontend(frontend: Box<dyn SquadAttachFrontend>)` drives the
   attach loop: it calls `AgentRuntimeEngine::attach(&handle)` and hands each
   resulting `AgentInstance` to the frontend through
   `SquadAttachFrontend::on_slot_attached(step: &str, instance: AgentInstance)`
   / `on_slot_exited(step: &str)`, and polls workflow state through the Step 7
   poller. `SquadAttachFrontend: UserMessageSink + Send` also carries
   `ask_pick_candidate(&[SquadContainer]) -> Result<Option<usize>, CommandError>`
   for the ambiguous case. `BuiltCommand::SquadAttach(SquadAttachCommand)` is
   returned by `Dispatch::build_command` like `ExecWorkflow`.
3. CLI: `per_command/squad_attach.rs` becomes a `SquadAttachFrontend` impl
   that runs each attached instance with the existing container frontend.
   Delete the carve-out in `cli/mod.rs:74-78` and the raw
   `gateway.core().get(&["tasks", task, "workflow"])` call. TUI: `squad_attach.rs`
   becomes a `SquadAttachFrontend` impl that pushes `ContainerSlotEvent`s;
   delete the carve-out in `app.rs:631-642`. Delete `src/frontend/attach.rs`.
4. API: register `attach` with `api_allowed: false` (decision Q7: PTY
   commands are excluded from the API as long-term policy). Add the
   rationale to the catalogue's `api_allowed` doc comment and a one-line
   note under "Layer 3 / API frontend" in
   `aspec/architecture/2026-grand-architecture.md` naming the exclusion as
   the sanctioned P2 exception.
5. Also handle the bare `squad` carve-out (`cli/mod.rs:71-73`,
   `per_command::squad::run_bare`): route it through `Dispatch` as
   `squad status` with `GatewayNeed::IfRunning`, or register a `squad`
   default subcommand in the catalogue; either way `cli::run` must contain no
   path-literal branches after this step except the documented
   `exec workflow` carve-out.
6. Tests: `tests/squad_attach.rs`, `tests/squad_attach_frontends.rs`,
   `tests/squad_tui_tab.rs` pass; move the slot-driver unit tests to
   `src/command/commands/squad/attach.rs`; add a catalogue test that every
   command in the catalogue is buildable by `Dispatch` (this is the test that
   would have caught F-01).

### Step 11 — F-07: the issue provider moves to Layer 1

Report: F-07 (`src/data/issue/`). Decision Q1: strict Layer 0; proceed.

1. `git mv src/data/issue src/engine/issue`. `IssueSource`, `IssueSourceRouter`,
   `GithubIssueSource` and the provider files become `crate::engine::issue::*`.
   Leave `Issue`, `IssueSourceError`, `IssueSourceFlags` and `slugify` in
   `src/data/issue.rs` (a single file) as plain data, re-exported from the
   engine module for a smooth diff.
2. Replace the direct `git remote get-url` spawn with `GitEngine::remote_url`
   and give the router an `Arc<GitEngine>`; replace `std::env::var("GITHUB_TOKEN")`
   with a value passed in from `EnvSnapshot` (declare `GITHUB_TOKEN` in
   `src/data/config/env.rs`; this is 0114 F-37's first half and is absorbed
   here because the code is being moved anyway).
3. Update the three L2 call sites (`specs.rs`, `exec_prompt.rs`,
   `exec_workflow.rs`). `make architecture-lint` must pass.
4. Tests: `tests/data_layer/issue_e2e.rs` and `issue_integration.rs` move to
   `tests/engine/`; behaviour unchanged.

### Step 12 — F-11: one container process module for Docker and Apple

Report: F-11 (`src/engine/container/docker.rs:563-937`, `src/engine/container/apple.rs:542-935`).

1. Create `src/engine/container/process.rs` with:
   `ContainerCli { bin: &'static str, label: &'static str, start_delay: Duration, post_wait: PostWaitHook }`
   (constants `ContainerCli::DOCKER`, `ContainerCli::APPLE`); one
   `ContainerInstance` (the two 11-field structs are identical) and one
   `ContainerExecution { …, attach_socket: Option<AttachSocketGuard> }`;
   `SpawnRequest { io, argv, seeded, started_at, handle, bridge_cfg }`
   replacing the six 7-parameter signatures; `spawn_piped`,
   `spawn_piped_interactive`, `spawn_pty_bridged(cli, instance, req, post_bridge: Option<AttachHook>)`.
2. `DockerBackend` and `AppleBackend` keep their `ContainerBackend` impls
   (list/stats/stop parsing differ) and delegate spawning to `process.rs`
   with their `ContainerCli`. Apple passes its attach-socket server as the
   `post_bridge` hook; Docker passes `clear_stdio_nonblocking` as `post_wait`.
   The `bridge_config_for` pair collapses to one function parameterised by
   `ContainerCli`.
3. After this step `docker.rs` and `apple.rs` must contain no function whose
   body differs from the other's only by binary name. Verify with `diff -w`
   over the remaining non-test code and record the residual diff in the
   commit message.
4. Tests: `tests/engine/container_docker.rs`, `container_io.rs`,
   `credential_argv_docker.rs`, `tests/engine/acp_support.rs` pass; the Apple
   path is exercised under `AWMAN_DOCKER_INTEGRATION=1` on macOS CI per
   `aspec/devops/cicd.md`. Move the shared spawn tests into `process.rs`.

### Step 13 — Close-out

1. Re-run the report's Phase 2 metrics (`make architecture-lint`, clippy,
   file sizes, `pub fn` counts, `use crate::command::commands` count in
   `src/frontend`, `pub trait` count in `src/frontend`, suppressed-lint list)
   and append a "before/after" table to
   `aspec/review-notes/0113-architecture-audit.md` under a new
   `## Remediation — WI 0113` heading, listing each of F-01–F-12 as closed,
   deferred (with reason), or rejected (with the developer's decision).
2. Update `aspec/architecture/four-layer-summary.md`'s "Key Components" and
   "Common Mistakes" sections if any type introduced here (`Startup`,
   `Engines::build`, `SquadDaemonRuntime`, `ApiServerRuntime`, `GatewayNeed`,
   `ResolvedFlags`) changes the documented pattern. Fix the dead link at
   `four-layer-summary.md:497` (report F-53) while there.

## Edge Case Considerations:
- **Step 1**: a deleted "dead" item may be used only under a `cfg(target_os)`
  or feature gate the Linux build does not compile. Check macOS/Windows
  `cfg` blocks (`daemon_process.rs` already uses `cfg_attr` allows for this)
  before deleting; when in doubt keep the item and gate it explicitly.
- **Step 2**: `Startup::run` must preserve the exact ordering comment in
  today's `main.rs` (repo-dir migration before `Session::open` reads
  `RepoConfig`), and the `UnknownRuntime` path must still exit 2 from the
  CLI and show the fatal modal in the TUI.
- **Step 3/4**: daemon bootstrap has ordering invariants the current doc
  comments call load-bearing (runtime admission before DB relocation; store
  migrate before orphan reconciliation). Keep the order; keep the comments;
  the move is mechanical. In Step 3 the admission check now runs in L2
  *before* `SquadDaemonEngine::bootstrap` is called, which preserves the
  order.
- **Step 3**: `SquadSupervisor` moving to L1 must not drag `CommandError`
  with it; its errors become `EngineError::Squad*` variants and L2 maps them
  (the `From<EngineError> for CommandError` impl already exists).
- **Step 4**: session restore reconstructs `SessionType::Remote` with empty
  `repo_url`/`branch`. Do not "fix" this by changing the SQLite schema in this
  work item; note it for a follow-up.
- **Step 5**: `squad status` with no daemon running must still succeed with a
  "not running" summary (`GatewayNeed::IfRunning`), and `squad start|stop|logs`
  must not try to acquire a gateway.
- **Step 8**: the TUI's fallback for a non-git directory changes to the API's
  policy. Confirm with the developer that `open_or_workdir_fallback` is the
  intended single policy before landing; record the answer here.
- **Step 9**: `--json` implying `--non-interactive` today happens in two
  places (dispatch and `effective_non_interactive`). After `ResolvedFlags`
  applies `implies`, make sure the CLI's cached `non_interactive` field does
  not re-derive a different answer (0114 F-50 finishes this; here just keep
  parity tests green).
- **Step 10**: `squad attach --container <id>` with an id outside the task's
  discovered set must keep returning the current `not_in_task` error text
  (tests assert it). The ambiguous-candidate flow must keep the TUI's picker
  and the CLI's numbered prompt.
- **Step 12**: Apple's `resize_pty_forcing_winch` and the attach-socket server
  are the only genuinely Apple-specific behaviours; they must survive as
  hooks, not as a second copy of the spawn function.

## Test Considerations:
- Run the full gate after every step: `make pre-push` (fmt-check, clippy,
  test, architecture-lint). Steps 3, 4, 6, 7, 11 and 12 cross a layer
  boundary: run `make architecture-lint` alone as well and paste its output
  into the commit message.
- The Docker-backed suites (`make test-full`, `AWMAN_DOCKER_INTEGRATION=1`)
  must be run at least once at the end of Steps 4, 10 and 12.
- New tests named in each step are mandatory, not optional. The catalogue
  "every command is buildable by Dispatch" test (Step 10) and the
  "every FlagDefault and implies is honoured" parity test (Step 9) are the
  regression guards for the two root causes this work item addresses.
- Layer tests stay in their layer: engine tests in `tests/engine/` or
  colocated in `src/engine/`; frontend tests must not assert business
  outcomes (the report notes `src/frontend/tui/app.rs:1444` as an example of
  the wrong layer being pinned; that test moves with the logic in Step 9).

## Codebase Integration:
- follow established conventions, best practices, testing, and architecture patterns from the project's aspec.
- The governing document is `aspec/architecture/2026-grand-architecture.md`.
  Its Tenet 3 (typed objects over free functions) and the Layer 2 section
  ("the command package must collect EVERYTHING it needs … at instantiation
  time") are the shape every new type in this work item takes.
- Every new type introduced here (`Startup`, `SquadDaemonEngine`,
  `SquadSupervisor` in L1, `ApiServerRuntime`, `ApiSessionLifecycle`,
  `SquadAttachCommand`, `ResolvedFlags`, `BuildContext`) is a struct with a
  constructor and methods, never a bag of `pub fn`.
- Decision Q4 applies to every squad change in this and the next work item:
  squad is not an exception to the grand architecture. If a squad change
  would put a decision in `src/frontend/squad/`, it is wrong.
- Frontends may only gain trait impls and rendering code in this work item.
  A diff that adds a decision, a default, a path literal, a route literal, or
  a `process::Command` to `src/frontend/` is wrong by definition.
- Work items that this one absorbs in part: 0114 F-34 (dead fields only),
  F-42, F-52 (`Engines::for_tests`), F-54, F-37 (GITHUB_TOKEN only), F-53
  (dead link only). Mark those parts done in 0114 when landing.

## Documentation

After implementation is complete, update user-facing documentation in `docs/` to reflect the current state of the tool:

- **Update existing feature docs**: `docs/12-squad.md` if the `squad attach`
  ambiguity prompt or the missing-key message wording changes (Steps 5 and
  10); `docs/09-api-and-remote-mode.md` if the API gains `squad attach`
  (Step 10, per the Q7 decision) or the partial-failure status for
  `new skill --pull-all` (Step 4); `docs/02-using-the-tui.md` if the Ctrl-T
  non-git fallback wording changes (Step 8).
- **Update `docs/architecture.md`** to name `Startup`, `Engines::build` and
  the two daemon runtime types as the entry points, replacing any text that
  says the frontends construct engines.
- **Create new user guides only if a new user-visible feature warrants it** — none is expected from this work item.
- **Never create work-item-specific docs**; the report and this file are the
  only places implementation reasoning lives.
- **Keep all technical/implementation details in work item specs or code comments**, not in `docs/`.
- **Docs are for end users**, not for developers trying to understand implementation.

See `CLAUDE.md` for more guidance on documentation standards.
