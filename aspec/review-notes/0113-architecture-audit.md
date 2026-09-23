# Architecture Audit — 2026-09-03

Scope: working tree at `415f780d` plus uncommitted edits. Prior audit: `aspec/review-notes/0073-architecture-audit.md` (2026-05-08, 168 files). Tree today: 280 files, 137,408 lines.

## Summary

The mechanical gates are green: `make architecture-lint` reports zero upward imports, clippy is clean, and there are no build warnings. Underneath that, the tree has grown 4x since the May audit and the spirit of the architecture has eroded in three places: the squad and API daemons are bootstrapped inside `src/frontend/`, the TUI has grown its own git engine, session policy, remote poller and `squad attach` implementation, and the catalogue is no longer the single source of truth for flag defaults or for what commands exist. A crate-wide `#![allow(dead_code)]` whose justification references the deleted `oldsrc/` hides 29 warnings, including an entire never-called sandbox backend surface.

Findings: 1 Critical, 11 High, 28 Medium, 14 Low (54 total). The two largest recommendations are (a) an L2 `Engines::build()` plus L2 daemon-bootstrap types so `main.rs`, the API server and the squad daemon stop wiring engines three separate ways, and (b) making `Dispatch` honour catalogue `FlagDefault`/`implies` generically and registering per-command constructors so the 700-line `build_command` match disappears.

| Area | Status | Findings |
|---|---|---|
| Layering (T1) | PASS | 0 (lint clean; no `use crate as`) |
| Frontend business logic (T2) | FAIL | 19 |
| Typed objects (T3) | WEAK | 7 |
| Catalogue and parity (L2, P2) | FAIL | 6 |
| Layer 0 / Layer 1 placement (L0, L1) | WEAK | 9 |
| Binary layer (L4) | WEAK | 1 |
| Security (S1, S2) | PASS | 0 (one question) |
| Spirit / consistency | WEAK | 6 |
| Simplification | — | 6 |

Baseline metrics are in `0113-architecture-audit-metrics.md`; the 29 hidden warnings are listed in `0113-architecture-audit-hidden-warnings.txt`. This report is the "what"; the fixes are specified in work items `aspec/work-items/0113-architecture-audit-critical-and-high.md` and `aspec/work-items/0114-architecture-audit-medium-and-low.md`.

---

## Findings

### F-01: `squad attach` is implemented in the frontends, twice, with no Layer 2 command
- **Rule**: T2, L2, P2
- **Severity**: Critical
- **Kind**: Violation
- **Where**: `src/frontend/tui/squad_attach.rs:341-522`, `src/frontend/tui/app.rs:631-642`, `src/frontend/cli/mod.rs:74-78`, `src/frontend/cli/per_command/squad_attach.rs`, `src/frontend/attach.rs:80-120`; catalogue entry `src/command/dispatch/catalogue.rs:1465`
- **What**: `app.rs:632` says "`squad attach` has no Layer-2 command; intercept it before the ordinary Dispatch spawn, mirroring `cli::run`'s carve-out". The TUI resolves the supervisor and gateway, probes workflow state, lists runtime containers by name prefix, diffs step transitions into attach/exit actions and calls `AgentRuntimeEngine::attach`. The CLI has a second implementation; a shared L3 helper (`frontend/attach.rs`) holds the candidate-resolution rules and error text; the API has nothing.
- **Why it matters**: The catalogue lists a command Dispatch cannot build; two frontends own the flow and the third lacks it.
- **Recommended action**: Create `SquadAttachCommand` in `src/command/commands/squad/attach.rs` owning supervisor/gateway resolution, `list_task_containers`, `resolve_attach_target`, phase detection and the slot-reconcile driver; expose `BuiltCommand::SquadAttach` from `Dispatch::build_command` returning an `AgentInstance` (same carve-out shape as `exec workflow`) and a `SquadAttachFrontend` trait. Delete `src/frontend/attach.rs` and both carve-outs.
- **Size**: L
- **Behavior change**: none intended; API gains the command

### F-02: The squad daemon is bootstrapped inside `src/frontend/squad/`
- **Rule**: T2
- **Severity**: High
- **Kind**: Violation
- **Where**: `src/frontend/squad/mod.rs:32-150` (`serve`, `serve_with`), `:156-187` (`build_engines`); `src/frontend/squad/unattended.rs:232-266, 336-383, 579-737`
- **What**: `serve_with` performs runtime admission, legacy DB relocation, store open and migrate, orphan-run reconciliation, stray-container scan, `SquadScheduler`/`LocalTaskGateway` construction and `ServerMeta` persistence; its doc comment calls the order "load-bearing". `build_engines` is a third copy of engine wiring. `unattended.rs` encodes the daemon's run policy as constant trait answers and opens per-step log files with `std::fs`.
- **Why it matters**: `SquadDaemonCommand::run_start` (L2) hands the whole runtime to a frontend trait; a fourth host would reimplement all of it.
- **Recommended action**: Add `SquadDaemonRuntime::bootstrap(config, engines)` in `src/command/commands/squad/daemon.rs` owning every step up to the router; change `SquadCommandFrontend::serve_squad_daemon` to receive the bootstrapped runtime and only build the router and bind. Move the unattended answer table into an L2 `HeadlessDefaults` (see F-13) and the run-log file layout into `src/data/fs/squad_paths.rs`.
- **Size**: M
- **Behavior change**: none

### F-03: API server bootstrap, queue worker and session close flow live in the API frontend
- **Rule**: T2
- **Severity**: High
- **Kind**: Violation
- **Where**: `src/frontend/api/mod.rs:33-52, 66-110, 112-206, 244-261`; `src/frontend/api/queue_worker.rs:45-72, 74-320, 329-416`; `src/frontend/api/routes.rs:534-671, 732-786`
- **What**: `api::serve` rebuilds `Engines`, restores sessions from SQLite (reconstructing `SessionType::Remote` with empty placeholders at `:145-152`), marks in-progress setups failed and deletes their clones, purges closed sessions, recovers stale commands and sizes the worker pool. `QueueWorker` owns the claim loop, `derive_command_status` (maps `CommandOutcome` variants to "done"/"error"), panic conversion and the session drain/close state machine. `handle_close_session` re-implements that drain flow a second time; `resolve_setup_status` decides "assume ready" on unknown.
- **Why it matters**: These are lifecycle decisions of API mode that any HTTP host must make identically; two copies of the close rule exist inside one frontend already.
- **Recommended action**: `ApiServerRuntime::bootstrap(config, engines)` in `src/command/commands/api_server/` owning store open/migrate, session restore, restart-failure marking, stale recovery and worker spawning; move `QueueWorker` there behind a frontend-factory trait; add `CommandOutcome::exit_code()` in `src/command/dispatch/mod.rs` shared by CLI and API; add an L2 `ApiSessionLifecycle::close(id) -> CloseOutcome` and `setup_readiness(id)` that routes map to status codes only.
- **Size**: L
- **Behavior change**: none

### F-04: Squad command routing and daemon supervision are hard-coded in `cli::run` and duplicated in the TUI `App`
- **Rule**: T2, P2
- **Severity**: High
- **Kind**: Violation
- **Where**: `src/frontend/cli/mod.rs:100-116, 122-153, 522-536`; `src/frontend/tui/app.rs:96-107, 296-627`
- **What**: `cli::run` holds two `matches!` lists of squad subcommand names (duplicating the catalogue), runs the runtime-tier guard, drives `SquadSupervisor` (ensure_running, key_state, `with_squad_gateway`) and authors the missing-key text. `App` repeats the same sequence for the squad tab, classifies errors by string prefix (`:99-102`), and `squad_synthetic_session` (`:611-627`) creates a directory and a `Session` by hand.
- **Why it matters**: Business flow leaking upward into two frontends is exactly the drift the spec was written to stop.
- **Recommended action**: Add a `gateway_need: GatewayNeed::{None, Running, IfRunning}` attribute to `CommandSpec`; have `Dispatch::run_command` acquire the gateway via an L2 `SquadSupervisor::gateway_for(need)`; surface `SquadKeyState::Minted{setup}` through a `SquadCommandFrontend::show_key_setup` method and `Missing` as a typed `CommandError::SquadKeyMissing`. Add `SquadSupervisor::open_for_frontend() -> Result<SquadStartup, SquadStartError>` with a typed error enum for the TUI. Move `squad_synthetic_session` to `Session::open_squad_root(&Env)` in L0.
- **Size**: M
- **Behavior change**: none

### F-05: Engine wiring is done three times, and `main.rs` does far more than clap plus frontend selection
- **Rule**: L4, L2
- **Severity**: High
- **Kind**: Violation
- **Where**: `src/main.rs:44-152`; `src/frontend/api/mod.rs:66-110`; `src/frontend/squad/mod.rs:156-187`
- **What**: `main.rs` runs legacy-path migration, loads global config, resolves the git root, opens the session and constructs `GitEngine`, `OverlayEngine`, `AuthEngine`, `AgentEngine` and the workflow state store into `Engines`. The API server and squad daemon each repeat the construction with small differences (auth resolver source, state-store root).
- **Why it matters**: The spec says Layer 4 "builds clap, picks a frontend, delegates. That's it." Three copies of the engine graph guarantee drift (the API copy already comments that its state store path is "temporary").
- **Recommended action**: `Engines::build(&GlobalConfig, &Session)`/`Engines::for_daemon(paths)` in `src/command/dispatch/mod.rs`; a `Startup` type in L2 (or L0 for migration) that performs migration and session open. `main.rs` becomes clap, `Startup::run`, frontend pick. This unblocks F-02 and F-03.
- **Size**: M
- **Behavior change**: none

### F-06: The TUI git sidebar is its own git engine
- **Rule**: T2, L1
- **Severity**: High
- **Kind**: Violation
- **Where**: `src/frontend/tui/git_sidebar.rs:96-234, 273-374` (`run_git` at `:336`, file reads at `:366-370`)
- **What**: The sidebar shells out to `git status --porcelain`, `git branch --show-current`, `git diff --numstat HEAD` and `git rev-parse`, reads untracked files to count lines, and owns the porcelain/numstat parsers plus the "no commits yet" fallback rules. `GitEngine` exposes nothing that returns per-file change type plus line counts.
- **Why it matters**: `GitEngine` is spec'd as "any and all Git operations"; a fourth frontend showing a diff summary would copy all of this.
- **Recommended action**: `GitEngine::diff_summary(&Path) -> Result<GitDiffSummary, EngineError>` in `src/engine/git/`; move the summary types and parsers there. The TUI keeps only the poll task and rendering.
- **Size**: M
- **Behavior change**: none

### F-07: The GitHub issue provider (git, gh, HTTP) is a 1,650-line engine inside Layer 0
- **Rule**: L0
- **Severity**: High
- **Kind**: Violation
- **Where**: `src/data/issue/github.rs:191-196` (`git remote get-url`), `:253-266` (`gh issue view`), `:319-335` (blocking `reqwest` + `GITHUB_TOKEN`); `src/data/issue/mod.rs:119` (`IssueSource` trait), `router.rs`
- **What**: L0 spawns `git` and `gh` and falls back to HTTP. All consumers are L2 (`specs.rs`, `exec_prompt.rs`, `exec_workflow.rs`).
- **Why it matters**: Four-layer summary: L0 has "No git. No network."
- **Recommended action**: Move `src/data/issue/` to `src/engine/issue/` as `IssueEngine` (or keep `IssueSourceRouter` as the typed entry point). Leave `Issue`, `IssueSourceError`, `IssueSourceFlags`, `slugify` in L0 as plain data.
- **Size**: L
- **Behavior change**: none

### F-08: Session construction policy lives in the frontends; `SessionManager` is created but never used
- **Rule**: T2, L0
- **Severity**: High
- **Kind**: Violation
- **Where**: `src/frontend/tui/key_handler.rs:983-1004`; `src/frontend/tui/mod.rs:91`, `src/frontend/tui/app.rs:116`; `src/frontend/api/mod.rs:135`; `src/frontend/api/routes.rs:51-70`; `src/frontend/squad/state.rs:13-22`
- **What**: Ctrl-T opens a `Session` and, on error, silently re-opens with the directory as git root. The API uses a different constructor (`Session::open_or_workdir_fallback`) and keeps its own `HashMap<String, Arc<RwLock<Session>>>`. `SessionManager::in_memory()` is threaded into `App` and never called.
- **Why it matters**: The spec names `SessionManager` as the multi-session owner; the non-git fallback policy already differs between TUI and API.
- **Recommended action**: `SessionManager::open_or_create(dir, opts)` in L0 using one fallback policy; TUI tabs and the API state hold a session id into it.
- **Size**: M
- **Behavior change**: TUI non-git fallback becomes identical to the API's

### F-09: Remote workflow polling, route paths and job-status semantics live in the TUI
- **Rule**: T2, L2
- **Severity**: High
- **Kind**: Violation
- **Where**: `src/frontend/tui/per_command/remote.rs:23-108` (`WorkflowStateSource` trait, the only frontend-owned trait), `:110-222` (`RemoteWorkflowPoller`)
- **What**: The two impls hard-code the squad route `["tasks", task, "workflow"]` (`:100`) and the API terminal statuses `"done" | "error"` (`:75`); the poller owns the 500 ms interval, the final-refresh rule and the errors-freeze-view rule.
- **Why it matters**: Route knowledge in a frontend drifts from the server; the CLI would need the same poller for parity.
- **Recommended action**: `TaskGateway::workflow_state(name)` and `RemoteClient::job_status(id) -> JobStatus` in L2; move the poller to L2 as a typed object taking an `on_state` callback.
- **Size**: M
- **Behavior change**: none

### F-10: `Dispatch::build_command` re-types catalogue defaults by hand; `implies` is never honoured
- **Rule**: L2
- **Severity**: High
- **Kind**: Violation
- **Where**: `src/command/dispatch/mod.rs:384-1080` (697-line match), `:403-406, 554-557, 688-691, 712-717, 941-944, 1005-1008`; `:419-422, 543-546, 1342`; `:1092-1180` (second 15-arm match)
- **What**: `unwrap_or_else(|| "claude")` at `:406` restates `FlagDefault::Str("claude")` (catalogue `:464`); the same for `9876`, `"6h"`, `"gitroot"`, `"local"`, `"toml"`. `if flags.json { non_interactive = true }` and `if yolo || auto { worktree = true }` restate the catalogue's `implies`, which has no non-test consumer anywhere. Four commands already use the better `read_*_flags` pattern (`:1280-1367`).
- **Why it matters**: The catalogue is not actually the source of truth for defaults or implications; clap uses one value, TUI/API dispatch another.
- **Recommended action**: Resolve `FlagDefault` and `implies` once and generically into a `ResolvedFlags` view; give each `*Command` a `from_input(&ResolvedFlags, &Engines, Session) -> Result<Self>` registered beside its `CommandSpec`; `build_command` collapses to lookup plus call.
- **Size**: L
- **Behavior change**: none (the six literals equal today's catalogue defaults)

### F-11: Docker and Apple backends are one backend written twice
- **Rule**: P1
- **Severity**: High
- **Kind**: Simplification
- **Where**: `src/engine/container/docker.rs:563-937` vs `src/engine/container/apple.rs:542-935`
- **What**: `spawn_piped_docker` vs `spawn_piped_apple` differ in 51 of ~92 lines, every one of which is `"docker"`↔`"container"` or a type name (`diff -w`). Six 7-parameter spawn functions, two execution backends, two instance structs and two bridge configs exist in parallel. The only real Apple-specific code is the attach-socket server (`apple.rs:601-640`). `apple.rs:770` documents that ACP was added "for parity" as a copy.
- **Why it matters**: Every spawn fix is a two-file change with a diff review to prove parity.
- **Recommended action**: `src/engine/container/process.rs` with `ContainerCli { bin, label, start_delay }`, one `ContainerInstance`/`ContainerExecution`, one `SpawnRequest` (collapsing the 7-param signatures), and `spawn_piped`/`spawn_piped_interactive`/`spawn_pty_bridged(…, post_bridge: Option<AttachHook>)`. Backends keep their own list/stats/stop parsing.
- **Size**: M
- **Behavior change**: none

### F-12: Crate-wide `#![allow(dead_code)]` with a stale justification hides 29 warnings
- **Rule**: P1
- **Severity**: High
- **Kind**: Spirit
- **Where**: `src/lib.rs:15` ("until oldsrc/ is deleted" — `oldsrc/` no longer exists); `src/data/mod.rs:1` (`#![allow(unused_imports)]`)
- **What**: Compiling a scratch copy with both attributes removed produces 29 warnings (list in `hidden-warnings.txt`), including: `SandboxBackend::{start_sandbox, restart_sandbox, exec_in_sandbox, remove}` never called and `SandboxId` never constructed (`src/engine/sandbox/backend.rs:19-58`); `WorkflowEngine.git_engine`/`overlay_engine` never read (`src/engine/workflow/mod.rs:194-195`); `image_exists_locally` never used and shelling to `docker` directly (`src/engine/agent/mod.rs:908`); five dead functions in `src/frontend/cli/output.rs:11-48`; `DockerBackend::is_available` unused (`docker.rs:39`); an unused import at `src/data/fs/daemon_guard.rs:19`.
- **Why it matters**: The spec forbids leaving legacy cruft; a layer-wide lint suppression is where cruft accumulates unseen.
- **Recommended action**: Delete both attributes, delete the dead items (or wire the sandbox trait methods to real callers if they are planned), and keep `-D warnings` green.
- **Size**: S
- **Behavior change**: none

### F-13: Three different headless answer tables (API, squad, CLI non-TTY)
- **Rule**: T2, P2
- **Severity**: Medium
- **Kind**: Violation
- **Where**: `src/frontend/api/command_frontend.rs:574-585, 843-903, 1023-1031`; `src/frontend/squad/unattended.rs:605-614, 640-690, 726-737`; `src/frontend/cli/per_command/exec_workflow.rs:110-120`
- **What**: `ask_agent_setup`: API returns `Setup` only if the default is available else `Abort`; squad always `Setup`. Merge mode: API `Squash`, squad `LeaveBranch`. Pre-worktree uncommitted files: API `Commit`, squad `UseLastCommit`. `confirm_resume`: API `true`, squad `false`.
- **Why it matters**: The same `exec workflow` behaves differently depending on which headless host ran it.
- **Recommended action**: One L2 `HeadlessDefaults` with named profiles (`::api()`, `::unattended_daemon()`) if the daemon legitimately needs a stricter one; all three frontends call it.
- **Size**: M
- **Behavior change**: none if profiles preserve today's answers

### F-14: Bearer-key verification is re-implemented in the API frontend, weaker than `AuthEngine`
- **Rule**: T2, L1
- **Severity**: Medium
- **Kind**: Violation
- **Where**: `src/frontend/api/serve.rs:43-87` vs `src/engine/auth/mod.rs:413-431`
- **What**: `check_bearer_auth` hashes with SHA-256 and compares with `ct_eq`, without the sentinel-hash timing defence that `AuthEngine::verify_api_key` has. `AuthEngine` is built in `api::serve` but never used for request auth. `AuthMode` is defined in `routes.rs`.
- **Why it matters**: The spec assigns "authentication logic for the API server" to `AuthEngine`; two implementations of a security check already differ.
- **Recommended action**: `AuthEngine::request_auth_mode(skip)` and `AuthEngine::verify_bearer(mode, header)`; the frontend keeps header extraction and the HTTP envelope. The squad daemon builds an `AuthEngine` over `SquadPaths::daemon()`.
- **Size**: S
- **Behavior change**: the sentinel comparison now applies on the request path

### F-15: TUI `spawn_command` derives business facts from command and flag names
- **Rule**: T2
- **Severity**: Medium
- **Kind**: Violation
- **Where**: `src/frontend/tui/app.rs:26-38, 772-775, 777-798, 800-817, 845-853`
- **What**: `is_containerized = matches!(path.first(), Some("chat" | "exec"))`; an 11-line ASCII "INTERACTIVE MODE" banner that no other frontend shows; `yolo_mode = yolo || auto` re-derived from flag names; `agent_name_from_parsed` calls L2 `resolve_agent` then hard-codes a `"claude"` fallback (tested at `:1444-1471` as a frontend test asserting a business default); gateway injection keyed on `path.first() == "squad"`.
- **Why it matters**: Adding a containerized command means editing the TUI; the `--auto`⇒yolo rule already lives in L2.
- **Recommended action**: `CommandSpec` attributes `runs_agent_container`, `wants_squad_gateway`; `Dispatch::yolo_effective(&parsed)`; make the built command expose the resolved agent display name. Decide whether the banner is a `ChatCommand`/`ExecPromptCommand` message (see Q6).
- **Size**: S
- **Behavior change**: none

### F-16: Container stats polling policy in the TUI with a fabricated `AgentHandle`
- **Rule**: T2, T3
- **Severity**: Medium
- **Kind**: Violation
- **Where**: `src/frontend/tui/app.rs:1024-1094, 1180-1213`
- **What**: The TUI owns the 3 s cadence, the in-flight dedupe set, the "name known → `stats()`, else first of `list_running_all()`" strategy, and builds `AgentHandle { started_at: now, image_tag: "" }` to satisfy the L1 signature.
- **Recommended action**: An L2 `ContainerStatsSampler` (or `AgentRuntimeEngine::stats_by_name`) that emits typed `StatsSample { tab_key, step_name, stats }`; replaces the `(usize, String, AgentStats)` tuple channels behind the `type_complexity` allows.
- **Size**: M
- **Behavior change**: none

### F-17: Squad daemon health classification in the TUI indicator
- **Rule**: T2
- **Severity**: Medium
- **Kind**: Violation
- **Where**: `src/frontend/tui/squad_indicator.rs:53-78, 115-135`
- **What**: `probe_once` drives `SquadSupervisor` (pidfile → sidecar → gateway list → 401) and `classify` encodes the six-state precedence.
- **Recommended action**: `SquadSupervisor::health() -> SquadHealth` in `src/command/commands/squad/daemon.rs`; the TUI keeps the 10 s loop and the coloured dot.
- **Size**: S
- **Behavior change**: none

### F-18: The workflow control board's "simple advance" case is computed from step states in the TUI
- **Rule**: T2
- **Severity**: Medium
- **Kind**: Violation
- **Where**: `src/frontend/tui/per_command/workflow_frontend.rs:23-125` (esp. `:35-45`), `:455-470`
- **What**: The TUI scans `step_states` for failures and pending steps to pick the lightweight confirm vs the full board, and re-applies permission guards the engine already encoded in `AvailableActions`.
- **Recommended action**: Add `simple_advance: Option<SimpleAdvance>` and `current_step_name` to `AvailableActions` (engine); TUI picks the dialog from those fields.
- **Size**: S
- **Behavior change**: none

### F-19: Dialog option sets, labels and defaults are authored per frontend and have already drifted
- **Rule**: L3, P2
- **Severity**: Medium
- **Kind**: Violation
- **Where**: `src/frontend/tui/per_command/init.rs:57-61` vs `src/frontend/cli/per_command/init.rs:88` (labels differ); `"6h"` literal in `src/frontend/tui/per_command/squad.rs:172-177`, `src/frontend/cli/command_frontend.rs:161`, `src/command/dispatch/mod.rs:691`, `src/command/dispatch/catalogue.rs:1126`; `src/frontend/tui/per_command/exec_workflow.rs:40-46`, `worktree_lifecycle.rs:158-161`, `specs.rs:37-40`; CLI `command_frontend.rs:156-176, 300-310`, `per_command/mount_scope.rs:141-157`
- **What**: Hotkeys, labels, the Esc→default policy and fallback values ("6h", cwd, `Dockerfile.dev`, `GitRoot`) are written in each frontend. Only `PostWorkflowWorktreePrompt` uses the L2-supplied-prompt pattern. Non-test TUI+CLI long user-facing strings: 120.
- **Recommended action**: Generalise `PostWorkflowWorktreePrompt` into `Prompt<D> { title, body, choices: Vec<Choice<D>>, default_on_dismiss }` in L2 and pass it to every `ask_*`; read the interval default from the catalogue.
- **Size**: M
- **Behavior change**: none (labels converge)

### F-20: Config dialog validates keys and uses a tab-separated string protocol between two TUI files
- **Rule**: T2, T3
- **Severity**: Medium
- **Kind**: Violation
- **Where**: `src/frontend/tui/dialog_router.rs:70-80, 86-170, 193-260` (`:134` builds `"field\tvalue\trepo"`); `src/frontend/tui/per_command/config.rs:51-64` re-parses it
- **What**: `is_valid_map_key` mirrors `AgentName` rules; the router knows which fields are maps/arrays/secrets and computes the next guidance index.
- **Recommended action**: Extend `ConfigFieldRow` with a typed `kind`; return `DialogResponse::ConfigEdit(ConfigEditRequest)`; let `ConfigCommand` reject bad keys.
- **Size**: M
- **Behavior change**: none

### F-21: Startup command policy decided by a raw `.git` probe, duplicated
- **Rule**: T2
- **Severity**: Medium
- **Kind**: Violation
- **Where**: `src/frontend/tui/mod.rs:135-162`; `src/frontend/tui/key_handler.rs:1008-1035`
- **What**: `if git_root.join(".git").exists() { "ready" } else { "status --watch" }`, copied verbatim.
- **Recommended action**: `Session::is_git_repo()` in L0 and `Dispatch::startup_command(&Session)` in L2.
- **Size**: S
- **Behavior change**: none

### F-22: `Tab` carries its own execution/workflow/container state instead of viewing `SessionState`
- **Rule**: T2 (spec: "TabState replaced by Session")
- **Severity**: Medium
- **Kind**: Spirit
- **Where**: `src/frontend/tui/tabs.rs:31-37, 94-150, 470-590`; `src/frontend/tui/tabs/overlay_lifecycle.rs:112-201`; `src/frontend/tui/workflow_view.rs:254-323`; vs `src/data/session.rs:211-217`
- **What**: `Tab` owns a cloned `Session` plus ~50 fields (`execution_phase`, `workflow_state`, `stuck`, `yolo_mode`, …) while `SessionState.current_command/current_workflow/current_container` are never written by anyone. `WorkflowStepView.status` is a `String` matched on "pending"/"running"/"done"/"error".
- **Recommended action**: Split `Tab` into view state plus a session id into `SessionManager`; have commands update `SessionState`; type the step status. Depends on F-08 and the answer to Q3.
- **Size**: L
- **Behavior change**: none intended

### F-23: The TUI imports the CLI, and a box renderer lives in Layer 0
- **Rule**: P1, L0
- **Severity**: Medium
- **Kind**: Violation
- **Where**: `src/frontend/tui/mod.rs:21` (`use crate::frontend::cli::RuntimeContext`); `src/frontend/tui/per_command/ready.rs:60` (CLI `render_summary_box`); `src/data/step_status.rs:34-99`; `src/command/commands/remote.rs:720-731`
- **What**: `RuntimeContext` (engines + session bundle) is defined in the CLI and consumed by the TUI; `render_summary_box` draws a Unicode box in L0 and is called from L2 `remote.rs`.
- **Recommended action**: Move `RuntimeContext` to `src/command/dispatch/`; move `render_summary_box` and glyphs to a shared frontend render helper; `remote.rs` returns the `ReadySummary` for the frontend to render.
- **Size**: S
- **Behavior change**: none

### F-24: `TuiCommandFrontend::new` takes 19 parameters; tuple channels hide simple structs
- **Rule**: T3
- **Severity**: Medium
- **Kind**: Simplification
- **Where**: `src/frontend/tui/command_frontend.rs:64, 108-130`; `src/frontend/tui/app.rs:135-149, 713-733`; five test copies (`per_command/init.rs:135-184`, `ready.rs`, `clean.rs`, `mount_scope.rs`, `workflow_frontend.rs`)
- **What**: 15 of the 19 arguments are `tab.x.clone()` shared slots; three `type_complexity` allows hide `(usize, String, AgentStats)` and `Result<(RemoteTaskGateway, SquadKeyState), String>`.
- **Recommended action**: `TabSharedState` (Clone) on `Tab`, `DialogChannels`, `StatsSample`, `SquadStartupResult`; `new(parsed, dialogs, io, shared)`; `TabSharedState::for_tests()`.
- **Size**: M
- **Behavior change**: none

### F-25: The catalogue and `aspec/uxui/cli.md` have drifted in both directions
- **Rule**: L2, P2
- **Severity**: Medium
- **Kind**: Violation (spec drift)
- **Where**: `src/command/dispatch/catalogue.rs:402-437` (`clean`, absent from cli.md), `:1270` (`squad edit`), `:1449` (`squad trigger`), `:1069` (`--agent-models`), `:1511, 1527` (`remote exec workflow|prompt`), `:1708-1775`, `:1876` (`toml|yaml` only); `aspec/uxui/cli.md:85-158` (`remote run <cmd>`, `session start <dir>`, `--format md`)
- **What**: The spec documents commands the binary does not have and omits ones it does. `requires_runtime` is also hard-coded as `!matches!(path.first(), Some("config"))` (`:221-223`) instead of a `CommandSpec` field.
- **Recommended action**: Generate the per-command reference in `cli.md` from a `CommandCatalogue::markdown_reference()` projection, checked by a test; add `requires_runtime: bool` to `CommandSpec`. Needs Q8/Q9 answered first.
- **Size**: S
- **Behavior change**: none

### F-26: Two parallel catalogue-driven token parsers with divergent semantics
- **Rule**: P1, P2
- **Severity**: Medium
- **Kind**: Simplification
- **Where**: `src/command/dispatch/parsed_input.rs:35-243` (TUI path) vs `src/command/dispatch/projections/raw_args.rs:265-500` (API path)
- **What**: Both walk `CommandSpec` for long/short flags, `--`, trailing args, conflicts and positionals (~200 lines each). The TUI parser does not validate enum values or coerce numbers; error kinds differ.
- **Recommended action**: Reduce `parsed_input::parse` to tokenise plus path resolution, then delegate to `raw_args::parse_against_spec`.
- **Size**: M
- **Behavior change**: TUI gains enum/number validation at parse time (parity)

### F-27: `commands/mod.rs` holds the overlay grammar and config-source merge (L0 work) as free functions
- **Rule**: L0, T3
- **Severity**: Medium
- **Kind**: Violation
- **Where**: `src/command/commands/mod.rs:52-130, 142-166, 188-270, 473-605, 607-764`; tests `:765-1804` (90% of the file)
- **What**: `parse_overlay_spec`/`parse_overlay_list` (the `dir()/ssh()/env()/skill()/context()` DSL), `collect_all_overlay_specs` (merges global, repo, `AWMAN_OVERLAYS`, CLI, workflow and step values), `resolve_launch_mode`, `resolve_agent`, `resolve_context_overlays`. Merging config sources is spec'd as L0; `RepoConfig.overlays` is an opaque `Vec<String>` only L2 can validate.
- **Recommended action**: `src/data/config/overlays.rs` for the grammar types, parser and `collect_all_overlay_specs` (as `EffectiveConfig::collected_overlays(cli, workflow, step)`); keep `resolve_agent`/`resolve_launch_mode`/`resolve_context_overlays` in L2 on a `LaunchPolicy` struct in its own module; `mod.rs` becomes declarations.
- **Size**: L
- **Behavior change**: none

### F-28: HTTP transport (`HttpCore`, SSE client) is a network primitive living in Layer 2
- **Rule**: L1
- **Severity**: Medium
- **Kind**: Spirit
- **Where**: `src/command/commands/http_core.rs:1-14, 60-91`; `src/command/commands/remote_client.rs:265-457`
- **What**: `HttpCore` is the tree's single `reqwest::Client` builder (timeouts, bearer, cert pinning). Engine-layer HTTP users (`engine/agent/download.rs`, `engine/workflow/poll_ci.rs`) cannot reuse it because it sits above them.
- **Recommended action**: Move `HttpCore`, `RemoteClient`, `ExecutionEventSink`/`RemoteEventSink` to `src/engine/remote/` returning `EngineError`; `RemoteCommand` stays L2.
- **Size**: M
- **Behavior change**: none

### F-29: Git, network and process supervision inside Layer 0 beyond the issue provider
- **Rule**: L0
- **Severity**: Medium
- **Kind**: Violation
- **Where**: `src/data/fs/context_dirs.rs:91` (`git remote get-url` in a path resolver); `src/data/network/aspec_tarball.rs:21-44` (`reqwest` download); `src/data/fs/daemon_process.rs:256-746` (`systemd-run`, `launchctl`, `ps`, `tasklist`, `taskkill`, spawning the daemon binary), `src/data/fs/daemon_guard.rs`
- **What**: The remote-URL parser is also duplicated three times in L0 (`context_dirs.rs:106-144`, `issue/github.rs:214-237`, `fs/skill_library.rs:74-112`). PID/meta file handling in `daemon_process.rs:60-245` is legitimate L0; the process-management half is not. `api_server.rs:274-275` calls the free `is_process_alive`/`pid_is_awman` directly.
- **Recommended action**: `ContextDirResolver::repo_dir(remote_url: Option<&str>, git_root)` with `GitEngine` supplying the URL and one shared `parse_owner_repo`; `AspecDownloader` in `src/engine/aspec/`; `DaemonSupervisor` in `src/engine/daemon/` owning spawn/kill/liveness while `DaemonPaths`/`ServerMeta` stay in L0. Gate on Q1.
- **Size**: L
- **Behavior change**: none

### F-30: A dead second workflow-state model and a duplicated agent-precedence rule in Layer 0
- **Rule**: P1
- **Severity**: Medium
- **Kind**: Simplification
- **Where**: `src/data/session.rs:137-180` (`StepStatus`, `WorkflowStepRecord`, `WorkflowInvocation`), `:211-217`; `src/data/fs/workflow_state.rs:19-107` vs `src/data/workflow_state_store.rs:16-102` (two `pub struct WorkflowStateStore`); `src/data/mod.rs:50` (`EngineWorkflowStateStore` re-export); `src/data/session.rs:490-505` vs `src/data/config/effective.rs:86-95`
- **What**: Nothing outside `session.rs` reads `WorkflowInvocation` or the three `SessionState` fields. The `fs/workflow_state.rs` store is used only for its `sha256_hex`/`sanitize_name_for_filename` helpers. `resolve_default_agent` re-encodes the flag > repo > global chain that `EffectiveConfig::agent()` calls its single source of truth.
- **Recommended action**: Delete the dead types (or wire them per Q3), keep one `WorkflowStateStore` under its plain name, derive `default_agent` from `EffectiveConfig`.
- **Size**: M
- **Behavior change**: none

### F-31: Config is loaded from disk ad hoc in frontends, engines and commands instead of through `Session`
- **Rule**: T2, L2
- **Severity**: Medium
- **Kind**: Violation
- **Where**: `src/frontend/tui/per_command/init.rs:49`, `src/frontend/cli/per_command/init.rs:83` (`RepoConfig::load(...).unwrap_or_default()` to build a prompt title, plus `"Dockerfile.dev"` default); `src/command/commands/api_server.rs:191-196` (re-implements `EffectiveConfig::api_work_dirs()`); `src/engine/init/mod.rs:161, 201, 273, 436, 496`; `src/command/commands/squad/commands.rs:538, 597`; `squad/gateway.rs:305`; `src/frontend/api/mod.rs:71`; `src/frontend/squad/mod.rs:160`; `src/main.rs:54`
- **What**: Each site does an independent `GlobalConfig::load().unwrap_or_default()`, swallowing parse errors `Session::open` would surface; two frontends read repo config to learn a path the trait could pass.
- **Recommended action**: `InitFrontend::ask_dockerfile_setup` receives the display path; `api_server.rs` uses `session.effective_config().api_work_dirs()`; `InitEngine` holds its `RepoConfig`; the rest route through the owned `Session`.
- **Size**: M
- **Behavior change**: config parse errors may surface where they are silently defaulted today

### F-32: Runtime and agent identity are branched on by string in Layer 1, contrary to the modules' own invariants
- **Rule**: L1, P1
- **Severity**: Medium
- **Kind**: Violation
- **Where**: `src/engine/container/runtime.rs:82-88` and four more `"apple-containers" =>` arms (`:121, 197, 283, 317`) though `ContainerBackend::cli_binary()` exists; `src/engine/agent/mod.rs:4-5` ("All agent-name branching lives in `agent_matrix.rs`") vs `:360, 567, 616, 666`; `src/engine/overlay/mod.rs:409-598, 663-682`, `src/engine/auth/keychain.rs:46-70`, `src/engine/ready/mod.rs:98-110` (four per-agent `match agent.as_str()` tables)
- **What**: `agent_settings_overlays_with_credentials` (211 lines) is five identical 10-line arms plus two special cases; `AgentMatrix` has no mount, credential-source or ping fields, so per-agent facts are spread over four files.
- **Recommended action**: Delegate to `backend.cli_binary()` and add `display_name()`/probe args to `ContainerBackend`; extend `AgentMatrix` with `settings_mount`, `skills_mount`, `credential_source`, `ping_argv`, `static_env`; collapse the tables.
- **Size**: M
- **Behavior change**: none

### F-33: Two different traits named `AgentFrontend`, plus re-export shims that give L0 types an engine path
- **Rule**: P1
- **Severity**: Medium
- **Kind**: Simplification
- **Where**: `src/engine/agent/frontend.rs:12-22` vs `src/engine/agent_runtime/frontend.rs:81-110`; `src/engine/step_status.rs:1`, `src/engine/ready/phase.rs:1`, `src/engine/ready/summary.rs:1` (`pub use crate::data::…::*`)
- **What**: A setup-progress trait and an I/O-binding trait share a name; `ReadyFrontend`/`InitFrontend` re-declare the same two methods inline. Twenty-plus sites import `crate::engine::step_status::StepStatus` while L0 imports the data path.
- **Recommended action**: Rename to `AgentSetupFrontend` and have `ReadyFrontend: AgentSetupFrontend`; delete the three shim files and import from `crate::data`.
- **Size**: S
- **Behavior change**: none

### F-34: `WorkflowEngine` stores engines it never calls, and setup/teardown are the same phase machine written twice
- **Rule**: P1, T3
- **Severity**: Medium
- **Kind**: Simplification
- **Where**: `src/engine/workflow/mod.rs:194-195, 324-326, 428-430, 452-460` (7- and 8-param constructors); `:2362-2453` vs `:2454-2550`, `:2551` (`run_shell_phase_step(phase: &str)`), `:2590` (`set_phase_step_failed(is_setup: bool)`), `:2736` vs `:2793`; `src/engine/workflow/frontend.rs:122-135` (10 callbacks = 5 identical pairs, implemented in 6 frontends and 8 test fakes)
- **What**: `git_engine`/`overlay_engine` have zero method calls in 3,273 non-test lines. `run_setup`/`run_teardown` and their remediation twins differ only by field names and callback names; the setup path discards stdout/stderr that teardown captures.
- **Recommended action**: Delete the two fields and params; introduce `WorkflowEngineDeps`/`WorkflowSpec` to drop `resume_with_state_root`; `enum PhaseKind { Setup, Teardown }` with one `run_phase(kind, …)` and `on_phase_step_*(kind, …)` callbacks (10 → 5 trait methods).
- **Size**: M
- **Behavior change**: none (setup gaining teardown's failure-file behaviour should be a deliberate follow-up)

### F-35: Dead commands and empty frontend traits inflate the trait surface every frontend must implement
- **Rule**: T3, P1
- **Severity**: Medium
- **Kind**: Simplification
- **Where**: `src/command/dispatch/mod.rs:324-325` (`BuiltCommand::Auth`/`Download` with no build arm and no catalogue spec); `src/command/commands/auth.rs`, `download.rs`; `src/command/commands/api_server.rs:115-117` (three empty traits with no impl), `:111` and `:122` (identical `serve_until_shutdown`); `specs.rs:257-270` (re-declares `HasAgentFrontend` methods); `set_pty_active` declared four times with two defaults; `src/command/commands/squad/commands.rs:336-372` (`SquadDaemonCommand` is a second `Command` impl re-wrapped by `SquadCommand`)
- **What**: 33 `pub trait`s in `src/command`: 16 per-command, 6 helper-flow, 4 dead or empty, 3 empty aliases, 4 non-frontend. `DispatchFrontend` requires bounds for commands nobody can invoke.
- **Recommended action**: Delete `AuthCommand`/`DownloadCommand` and their traits (or register them in the catalogue); delete the three empty `ApiServer*` traits; one `AgentLaunchFrontend: MountScope + AgentSetup + AgentAuth + HasAgent { set_pty_active; set_stuck_sender }` extended by Chat/ExecPrompt/ExecWorkflow/Specs; fold `SquadDaemonCommand` into `SquadCommand`.
- **Size**: M
- **Behavior change**: none

### F-36: `WorkflowProxy` and `AgentFrontendProxy` are 300 lines of hand-written forwarding
- **Rule**: P1
- **Severity**: Medium
- **Kind**: Simplification
- **Where**: `src/command/commands/exec_workflow.rs:222-462, 463-528`
- **What**: 37 one-line `self.0.lock().unwrap().x(args)` methods that must be updated whenever `WorkflowFrontend` changes.
- **Recommended action**: Blanket `impl<F: WorkflowFrontend + ?Sized> WorkflowFrontend for Arc<Mutex<Box<F>>>` in `src/engine/workflow/frontend.rs` (and likewise for `AgentFrontend`); delete both proxies.
- **Size**: S
- **Behavior change**: none

### F-37: Environment variables are read and parsed outside Layer 0 and are undeclared in `Env`
- **Rule**: L0
- **Severity**: Medium
- **Kind**: Violation
- **Where**: `src/engine/workflow/poll_ci.rs:29` and `src/data/issue/github.rs:331` (`GITHUB_TOKEN`); `src/engine/container/attach_socket.rs:77` (`AWMAN_ATTACH_DIR`); `src/frontend/api/session_setup.rs:463-471` (`AWMAN_API_VERBOSE_SETUP`, with truthiness parsing in L3); `src/engine/container/docker.rs:1288-1292`, `options.rs:474, 480`, `sandbox/dsbx/session_config.rs:89` (passthrough values read at spawn time)
- **What**: `src/data/config/env.rs:3-4` states reads are funnelled through `Env`; none of these three names appears in `Env::from_process()` (`:178-201`). `poll_ci.rs` also builds its own `reqwest` client (`:142`) and re-shells `git rev-parse`/`git remote` (`:45-73`) instead of using `GitEngine`.
- **Recommended action**: Declare the constants and typed accessors on `EnvSnapshot`; pass resolved values in; wrap the CI poller as `CiPoller { git, token }`.
- **Size**: S
- **Behavior change**: none

### F-38: The sanctioned host-side agent ping is a pair of free functions, and the credential monitor is a process-global singleton
- **Rule**: T3, S1 (auditability)
- **Severity**: Medium
- **Kind**: Spirit
- **Where**: `src/engine/ready/mod.rs:123-160, 178-245` (`ping_local_agent`, `refresh_host_credential`), called from `src/engine/credential_refresh/monitor.rs:342`; `monitor.rs:612-625` (`static GLOBAL_MONITOR: OnceLock`) reached from `docker.rs:607, 668, 782` and `apple.rs:586, 696, 787` via `credential_refresh::global()`, installed by `src/command/dispatch/mod.rs:83`; `src/engine/squad/launcher.rs:47-70, 166-200` (`drive_unattended_agent` and friends as free fns)
- **What**: The only S1-exempt code path is callable from any module as a bare `pub async fn`; the spawn paths depend on hidden global state instead of an injected option.
- **Recommended action**: `HostAgentPinger` struct held by `ReadyEngine` and `CredentialRefreshMonitor`; carry the monitor (or a `CredentialLeaseFactory`) in `ResolvedContainerOptions`; move the squad launcher free fns onto `SquadAgentLauncher`.
- **Size**: M
- **Behavior change**: none

### F-39: Direct `git` spawns in `exec_workflow.rs`, one in the wrong directory
- **Rule**: L1
- **Severity**: Medium
- **Kind**: Violation
- **Where**: `src/command/commands/exec_workflow.rs:1532-1545` (`git config user.name/email` with no `current_dir`), `:2975-2984` (`worktree_git_status` = `git status --porcelain`, which `GitEngine::uncommitted_files` already does)
- **What**: The identity probe runs in awman's process cwd, so a repo-local identity is not honoured; the status probe swallows errors `GitEngine` would surface.
- **Recommended action**: `GitEngine::identity_configured(&Path) -> Result<GitIdentity>`; replace `worktree_git_status` with `uncommitted_files`.
- **Size**: S
- **Behavior change**: identity check becomes repo-scoped (latent bug fix; note in changelog)

### F-40: Commands switch whole flows on which concrete runtime handle is `Some`
- **Rule**: L1
- **Severity**: Low
- **Kind**: Spirit
- **Where**: `src/command/commands/ready.rs:190` (`if self.engines.sandbox_runtime.is_some()` selects a different ready flow); `clean.rs:218, 404, 458`; `src/engine/sandbox/mod.rs:29-35` (`ready_sbx_agent` free fn called from `ready.rs:195`)
- **Recommended action**: Branch on `engines.runtime.capabilities().kit_declarative`; add an "image store" capability for clean; `SandboxRuntime::ready_agent(...)` method.
- **Size**: S
- **Behavior change**: none

### F-41: `SessionSetupObserver` requires the frontend to implement persistence
- **Rule**: T2
- **Severity**: Low
- **Kind**: Spirit
- **Where**: `src/command/session_setup.rs:68, 74, 83` (`persist_status`, `register_session`, `persist_and_cleanup`); `src/frontend/api/session_setup.rs:22-141, 176-260`
- **Recommended action**: Split into a presentation-only `SessionSetupPresenter`; `SessionSetup` takes `SessionManager` and the status store directly; move `SessionSetupState` transition logic onto the L0 type.
- **Size**: M
- **Behavior change**: none

### F-42: Exit-code and outcome-failure policy differ between CLI and API
- **Rule**: P2
- **Severity**: Low
- **Kind**: Spirit
- **Where**: `src/frontend/cli/mod.rs:446-455` (`outcome_exit_code` peeks into `NewOutcome::Skill`); `src/frontend/api/queue_worker.rs` (`derive_command_status` treats `New` as always done)
- **Recommended action**: `CommandOutcome::is_partial_failure()`/`exit_code()` in L2 (shared with F-03).
- **Size**: S
- **Behavior change**: API reports `error` for a partially failed `new skill --pull-all`

### F-43: A second Levenshtein "did you mean" in the TUI
- **Rule**: L2, P2
- **Severity**: Low
- **Kind**: Violation
- **Where**: `src/frontend/tui/command_box.rs:20-52` (`:46`); L2 already has one at `src/command/commands/config.rs:319`
- **Recommended action**: `CommandCatalogue::suggest(&[&str])` carried in `CommandError::UnknownCommand { suggestions }`.
- **Size**: S
- **Behavior change**: suggestions may improve for nested paths

### F-44: Panic-log path resolution and file append in the TUI event loop
- **Rule**: L0
- **Severity**: Low
- **Kind**: Violation
- **Where**: `src/frontend/tui/event_loop.rs:38-70`
- **Recommended action**: `data::fs::PanicLog { path(&Env), append(report) }`; the hook formats and calls it.
- **Size**: S
- **Behavior change**: none

### F-45: `WorkflowFrontend` pushes engine channels into frontends; ready/init steps are keyed by free-form strings; engines render transcript text
- **Rule**: T1, T3, L3
- **Severity**: Low
- **Kind**: Spirit
- **Where**: `src/engine/workflow/frontend.rs:104-121, 181-190` (`set_engine_sender`, `set_stuck_sender`, `set_parallel_step_io`, …); `src/engine/ready/frontend.rs:23`, `init/frontend.rs:33`, `agent/frontend.rs:15` (`step: &str`); `src/engine/git/mod.rs:18-48` (`"$ {cmd}"` echo), `src/engine/ready/mod.rs:619-627` (`"> greeting"`/`"< response"`), `src/engine/workflow/mod.rs:2629-2639`
- **Recommended action**: One `attach_engine(EngineHandles)` method; `ReadyStep`/`InitStep` enums with `Display` in L0; typed events (`command_started`, `report_ping`, `report_ci_poll`) with default impls that keep today's text.
- **Size**: M
- **Behavior change**: none

### F-46: Agent/model default logic and a permission policy inside Layer 1
- **Rule**: L1, L2
- **Severity**: Low
- **Kind**: Spirit
- **Where**: `src/engine/squad/scheduler.rs:397-425` (parses `agent::model` leader syntax and picks the first `agentsToModels` entry for log labels, duplicating the L2 evaluator); `src/engine/acp/session.rs:66-72` (`auto_approve = yolo || auto`)
- **Recommended action**: Have `TaskEvaluator` report the resolved pair; pass an explicit `PermissionPolicy` into `AcpSession::new`.
- **Size**: S
- **Behavior change**: none

### F-47: Serialization contracts defined in Layer 1 and raw filesystem writes in Layer 2
- **Rule**: L0
- **Severity**: Low
- **Kind**: Violation
- **Where**: `src/engine/squad/verdict.rs:43-50, 81-109` (`RunVerdict`, written by the leader agent into the container — an external file contract); `src/command/commands/squad/gateway.rs:359-473` (task config writes), `squad/daemon.rs:467` (`std::env::set_var`), `squad/evaluation.rs:1002, 1022` (build-log files); `src/command/commands/api_server/banner.rs:5-13` (box-drawing in L2)
- **Recommended action**: `RunVerdict` to `src/data/fs/squad_verdict.rs`; `TaskStore`/`SquadPaths` helpers for the writes; emit the API key as data and render the banner in the CLI.
- **Size**: S
- **Behavior change**: TUI/API stop receiving box-drawing text unless they render it

### F-48: Stringly-typed session kind and step status crossing layer and wire boundaries
- **Rule**: T3, L0
- **Severity**: Low
- **Kind**: Spirit
- **Where**: `src/command/session_create.rs:103-109`, `src/command/commands/remote.rs:534-536` (`match "local" | "remote"` though `data::session::SessionType` exists); `src/data/fs/api_db.rs` `insert_session_full` (7 `&str` params); `src/data/execution_event.rs:26-31` (`from_status: String, to_status: String`), `src/frontend/api/command_frontend.rs:651-658`, `queue_worker.rs:438, 501`
- **Recommended action**: `SessionKind` enum with `FromStr`; `NewSessionRow` struct; serde-tagged `StepStatusKind` in `EventPayload` (same snake_case strings on the wire; gate on Q10).
- **Size**: S
- **Behavior change**: none on the wire if names are preserved

### F-49: Three commands are built without a `Session` and probe frontend identity at run time
- **Rule**: L2
- **Severity**: Low
- **Kind**: Spirit
- **Where**: `src/command/commands/status.rs:179-181, 213` (`frontend.tui_context()`); `squad/commands.rs:308-320, 752` (`frontend.is_local_user_session()`); `api_server.rs:132`
- **Recommended action**: Pass a `CallerContext { local_user, tui_tab }` from `Dispatch` at construction; give all three the `Session`.
- **Size**: S
- **Behavior change**: none

### F-50: Non-interactive flag has two sources of truth inside the CLI; `flags_applied` is hard-coded in a route
- **Rule**: T2
- **Severity**: Low
- **Kind**: Spirit
- **Where**: `src/frontend/mod.rs:25-31`, `src/frontend/cli/command_frontend.rs:357-373` (reads `--non-interactive` from clap directly and caches it) while `mount_scope.rs:141`, `agent_setup.rs:182` call `stdin_is_tty()` directly; `src/frontend/api/routes.rs:952-955` (`flags_applied: {yolo:true, non_interactive:true}` vs catalogue `API_FLAG_DEFAULTS` at `raw_args.rs:158`); `src/frontend/squad/routes.rs:282-289` (squad-subtree rule)
- **Recommended action**: `CommandFrontend::input_available()` (TTY fact) with `Dispatch` computing effective non-interactive; serialise the catalogue profile; `FrontendKind::SquadDaemon` visibility in the catalogue.
- **Size**: S
- **Behavior change**: `--non-interactive` on a TTY consistently suppresses all prompts

### F-51: Oversized functions and files that should be split (behaviour-preserving)
- **Rule**: P1
- **Severity**: Low
- **Kind**: Simplification
- **Where**: `src/engine/workflow/mod.rs` (7,622 lines, 57% tests; 7 responsibilities) → `workflow/{mod, single_step, parallel, control, phases, queries}.rs` + `tests/`; `src/command/commands/exec_workflow.rs` (5,943; `run_with_frontend` 423 lines, `execute_prepared` 544, `run_dynamic` 361, `drive_leader_agent` 11 params) → `exec_workflow/{mod, proxies, factory, prepare, execute, dynamic, image, issue}.rs` plus a neutral `commands/workflow_preflight.rs` for the eight helpers `squad/evaluation.rs:21-26` imports from it; `src/engine/overlay/mod.rs` → `{agent_settings, claude, skills, credential_file}.rs`; `src/command/dispatch/catalogue.rs` → per-group files with `EXEC_PROMPT_FLAGS`/`SQUAD_EDIT` derived by const fn from `AGENT_RUN_FLAGS_NO_WORKTREE`/`SQUAD_ADD` (93% / 64% identical today); `src/frontend/tui/render/dialog.rs:7-870` (one 864-line match) → per-variant `render_*`; `src/frontend/tui/key_handler.rs:23-759` (737 lines) → `focus_context`, `intercept_dialog_keys`, grouped action handlers; `src/frontend/tui/app.rs::tick_all_tabs` (310 lines, six labelled sections); `clean.rs::discover` (186 lines, four numbered categories) and `new.rs::run_with_frontend` (532 lines, three arms) → per-category/per-subcommand fns; `config.rs:23-530` seven string-keyed lookups over one field list → a `ConfigFieldSpec` table in `src/data/config/fields.rs`; `src/engine/agent/mod.rs:214-236` vs `:515-543` etc. (three verbatim validation/mode-flag blocks) → `AgentMatrix::validate_run`/`mode_flags`
- **Size**: L in aggregate (each piece S–M)
- **Behavior change**: none

### F-52: Test fixture duplication
- **Rule**: P1
- **Severity**: Low
- **Kind**: Simplification
- **Where**: `fn make_engines` copied into 9 test modules (identical 33-line body in `dispatch/mod.rs:1436`, `exec_workflow.rs:3684`, `config.rs:1838`, `tui/app.rs:1374`, …); `fn make_session` defined 23 times; `TuiCommandFrontend::new(…19 args…)` copied in 5 TUI test modules; `tests/helpers/mod.rs` has `TestEnv` but no `Engines` builder; 7 `#[ignore]` tests whose bodies are `todo!()` at `exec_workflow.rs:5846-5894`
- **Recommended action**: `Engines::for_tests(root)` under `cfg(test)`; `TestEnv::engines()`; `TabSharedState::for_tests()` (F-24); delete or implement the seven stubs.
- **Size**: S
- **Behavior change**: none

### F-53: Stale documentation pointers
- **Rule**: —
- **Severity**: Low
- **Kind**: Spirit
- **Where**: `aspec/architecture/four-layer-summary.md:497` points to `docs/10-architecture-overview.md`, which does not exist (`docs/10-github-integration.md` does; `docs/architecture.md` is the real page); `src/data/message.rs:25-27` says `UserMessageSink` is "Defined by Layer 1" (it is L0); `src/engine/agent/mod.rs:110-111` says production callers pass `image_exists_locally` (they do not)
- **Recommended action**: Fix the three references.
- **Size**: S
- **Behavior change**: none

### F-54: API keeps a raw session map instead of `SessionManager`; `InitialTab` carries a redundant `Session`
- **Rule**: L0, P1
- **Severity**: Low
- **Kind**: Simplification
- **Where**: `src/frontend/api/routes.rs:51-70`, `src/frontend/squad/state.rs:13-22`; `src/frontend/tui/mod.rs:68-72` (`#[allow(clippy::large_enum_variant)]` on `InitialTab::Normal(Session)` while `ctx.session` already carries it)
- **Recommended action**: Fold into F-08; `InitialTab { Normal, Squad }` built from `ctx.session`.
- **Size**: S
- **Behavior change**: none

---

## Large refactor proposals

### R-A: Engine assembly and daemon bootstrap in Layer 2 (F-05, F-02, F-03, F-04)
Replaces: three hand-wired `Engines` constructions and two daemon bootstraps in `src/frontend/`. Target: `Engines::build(&GlobalConfig, &Session)`, `Engines::for_daemon(paths)`, `Startup::run()` (migration + session open), `ApiServerRuntime::bootstrap`, `SquadDaemonRuntime::bootstrap`, `QueueWorker` under `src/command/commands/api_server/`. Order: F-05 first (pure move), then F-02, then F-03 (largest), then F-04. Proof: `make pre-push`, `tests/api_parity/live_server.rs`, `tests/squad_daemon_e2e.rs`, `tests/squad_daemon_http.rs`, plus `make architecture-lint` after each move. Needs a work item.

### R-B: Catalogue-owned defaults and per-command constructors (F-10, F-26, F-35, F-25)
Replaces: the 697-line `build_command` match, the second `run_command` match, six duplicated default literals, unconsumed `implies`, and the second token parser. Target: `ResolvedFlags` produced once by `Dispatch` from `FlagDefault`/`implies`; `CommandSpec.build: fn(&BuildContext) -> Result<BuiltCommand>`; `parsed_input::parse` delegating to `raw_args`. Proof: `tests/cli_parity/*`, `src/command/dispatch/projections/parity_test.rs`, TUI key-handler tests. Needs a work item.

### R-C: `squad attach` as a Layer 2 command (F-01)
Replaces: `frontend/attach.rs`, `tui/squad_attach.rs` driver logic, `cli/per_command/squad_attach.rs` flow. Target: `SquadAttachCommand` + `SquadAttachFrontend` + `BuiltCommand::SquadAttach`. Proof: `tests/squad_attach.rs`, `tests/squad_attach_frontends.rs`, `tests/squad_tui_tab.rs`. Needs a work item.

### R-D: Issue engine and overlay grammar relocation (F-07, F-27, F-29)
Replaces: `src/data/issue/` (L0 → L1), overlay DSL and source merge (L2 → L0), `aspec_tarball`/`DaemonProcess` process half (L0 → L1). Each is a mechanical move guarded by `make architecture-lint`. Gate on Q1. Needs a work item.

### R-E: Single container process module (F-11)
Replaces: six 7-parameter spawn functions across `docker.rs`/`apple.rs`. Target: `container/process.rs` with `ContainerCli`, `SpawnRequest`, one instance/execution type. Proof: `tests/engine/container_docker.rs`, `container_io.rs`, `credential_argv_docker.rs`, and the Apple path under `AWMAN_DOCKER_INTEGRATION`. Needs a work item.

### R-F: `Tab` as a view of `SessionState` (F-22, F-08)
Depends on Q3. Not recommended before R-A and F-08 land.

---

## Questions for the developer

1. **Layer 0 scope.** The grand architecture says L0 holds "external storage, api types, filesystem interaction" and "handling the API mode directories"; the four-layer summary says "No git. No network." Which governs for (a) the aspec tarball download, (b) the GitHub issue provider, (c) daemon process supervision (`systemd-run`/`launchctl`/spawn)? F-07 and F-29 assume the stricter reading.
2. **Serializable outcomes.** 53 `Serialize` types in `src/command` (`*Outcome`, `CommandOutcome`) are the `--json` and API-response contracts. Are they an "external contract" that must move to `data::outcomes`, or L2-owned results the frontends merely serialise? This decides whether ~19 files change.
3. **`SessionState`.** Nothing outside L0 reads or writes `current_command`/`current_workflow`/`current_container`. Should it be wired as the ruling in-flight state (and `Tab` derive from it, F-22), or deleted so `WorkflowEngine` owns state (F-30)?
4. **`frontend/squad`.** Is it intended as a fourth frontend, or as the daemon's runtime host? The doc header says "frontend and bootstrap"; the code is ~90% bootstrap.
5. **Headless profiles.** Are the differing API vs squad answers (squash-merge + auto-commit vs leave-branch + never-commit; agent setup) deliberate per-host policy or drift? Either way F-13 puts them in L2, but as one table or two named profiles.
6. **Interactive banner.** Is the TUI-only "INTERACTIVE MODE" banner (`app.rs:777-798`) intentional, or should `ChatCommand`/`ExecPromptCommand` emit it via `UserMessageSink` so the CLI shows it too?
7. **`api_allowed: false`** for clean/init/ready/chat/specs/status/config/new/remote: long-term parity policy, or transitional? Today P2 parity is enforced by exclusion.
8. **`remote` surface.** `cli.md` documents `remote run <command…>` and `session start <dir>`; the code has `remote exec workflow|prompt` and `session start --type/--workdir/--repo-url`. Which is right?
9. **`new workflow --format md`** is documented in `cli.md:90` but the catalogue accepts only `toml|yaml`. Intentional removal?
10. **API wire schema.** Is the `EventPayload` string schema (`"pending"`/`"running"`/`"done"`, `"failed"`) frozen for external clients? F-48 must preserve exact names if so.
11. **`docker.sock` mount** under `allow_docker` (`docker.rs:1308-1325`) is the only host path outside the S2 set. Should `security.md` list it as sanctioned?
12. **`ReadyEngine`/`InitEngine`** build and run audit agents through a concrete `Arc<ContainerRuntime>` (`ready/mod.rs:738-745`, `init/mod.rs:391-402`), so they cannot run under `SandboxRuntime`; sandbox readiness is routed separately via `ready_sbx_agent`. Intended?
13. **Cross-command imports.** `squad/evaluation.rs:21-26` imports eight helpers from `exec_workflow.rs`. Acceptable within L2, or should they move to a neutral `commands/workflow_preflight.rs` (F-51)?

## Decisions — 2026-09-04

Answers from the developer to the questions above, plus three edge-case
decisions raised by the work items. These are binding for WI 0113 and 0114.

| # | Decision |
|---|---|
| Q1 | **Strict.** No git, network or process spawning in Layer 0. F-07 (issue provider), F-28 (HTTP client) and F-29 (tarball download, daemon supervision) all move to Layer 1. |
| Q2 | **L2-owned.** `*Outcome`/`CommandOutcome` stay beside their commands. Only on-disk / cross-process contracts move to L0 (F-47 `RunVerdict`). |
| Q3 | **Wire it.** `SessionState` becomes the ruling in-flight state; commands update it and the TUI `Tab` derives from it (F-22). F-30 deletes only the parallel dead types. |
| Q4 | **All squad business logic moves down, the majority into the engine layer.** `frontend/squad` keeps only presentation and I/O (router, bind, trait impls) and calls down through traits like every other frontend. Squad must adhere to the grand architecture with no exception. This is stronger than the F-02 recommendation: the daemon runtime (store, scheduler, reconciliation, gateway) is an engine, and the L2 command wires frontend traits into it. |
| Q5 | **Deliberate.** Two named L2 profiles reproducing today's answers: `HeadlessDefaults::api()` and `HeadlessDefaults::squad()` (not `::unattended_daemon()`). |
| Q6 | **Delete the banner entirely.** No frontend shows the "INTERACTIVE MODE" banner. |
| Q7 | **Long-term policy.** `api_allowed: false` stays for interactive/PTY commands; record it as a sanctioned P2 exception in the catalogue doc comment and the grand architecture. |
| Q8 | **The code is right**, and `aspec/uxui/cli.md` should not document specific commands and flags at all. It becomes the source of truth for generalised UI/UX standards and best practices; the per-command reference is user documentation and belongs in `docs/`, generated from the catalogue. |
| Q9 | **Intentional.** `--format md` is gone; the reference lists `toml|yaml`. |
| Q10 | **Not frozen.** The `EventPayload` strings may be renamed; the typed enums use whatever names read best. |
| Q11 | **Yes.** Document the `docker.sock` mount under `--allow-docker` in `security.md` as a sanctioned exception to S2. |
| Q12 | **Make `ReadyEngine`/`InitEngine` runtime-agnostic in WI 0114** (`Arc<dyn AgentRuntimeEngine>`; `ready_sbx_agent` folds into the trait). |
| Q13 | **Move to a neutral module.** The eight helpers `squad/evaluation.rs` imports from `exec_workflow.rs` go to `commands/workflow_preflight.rs`; `LeaderSpec` to `data/config`. |
| F-12 | **Delete** the never-called `SandboxBackend` methods and `SandboxId`. |
| F-35 | **Delete both** `AuthCommand` and `DownloadCommand`; keep the tarball logic as a private helper of `InitCommand`. |
| F-19 | **Abort on Esc everywhere** in the `new` interviews; the TUI stops writing a file named `workflow`. |

---

## Proposed execution order

Small safe wins first, then the two refactors that unblock the rest.

1. F-12 (remove stale lint suppressions, delete dead items) — S, no dependencies
2. F-53, F-52 (docs pointers, test stubs and `Engines::for_tests`) — S
3. F-33, F-36, F-35 (trait renames, blanket proxies, dead commands and empty traits) — S/M
4. F-34 (drop dead `WorkflowEngine` fields, `PhaseKind`) — M
5. F-37, F-39, F-40, F-44, F-46, F-47 (env vars, git spawns, capability branches, small L0/L1 moves) — S each
6. F-14, F-42, F-43, F-21, F-17, F-18 (small T2 pull-downs into L2/L1) — S each
7. F-05 (`Engines::build`, `Startup`) — M; unblocks 8–9
8. F-02 then F-03 then F-04 (daemon bootstraps and squad routing into L2) — R-A, work item first
9. F-06, F-09, F-15, F-16, F-19, F-20, F-24, F-31 (TUI/CLI business logic into L2/L1) — M each; F-24 first since it shrinks the others' diffs
10. F-10 then F-26 then F-25 (catalogue-owned defaults, single parser, regenerate `cli.md`) — R-B, work item first; F-25 needs Q8/Q9
11. F-01 (`SquadAttachCommand`) — R-C, work item first; after F-04
12. F-08, F-13, F-41, F-49, F-50, F-54 (session manager, headless defaults, observer split) — M
13. F-07, F-27, F-29, F-30, F-28, F-38 (layer relocations) — R-D, after Q1
14. F-11, F-32 (container process module, agent matrix consolidation) — R-E
15. F-51, F-45, F-48 (file splits, trait shape, typed statuses) — as capacity allows
16. F-22 (Tab as SessionState view) — R-F, after Q3 and item 12

---

## Remediation — WI 0113

Close-out pass, 2026-09-05. Covers F-01 through F-12 (the Critical and High
findings WI 0113 owns). Evidence: the `step-*.md` handoffs in
`/awman/context/workflow/`, `baseline-metrics.md` (captured before Step 1),
and direct inspection of the current tree (no `remediation.md` file was found
in the shared workflow context directory — none of the prior steps wrote
one — so disposition below is sourced from the individual step handoffs plus
code inspection, not a consolidated remediation log).

### Disposition of F-01 – F-12

| # | Finding | Severity | Status | Evidence |
|---|---|---|---|---|
| F-01 | `squad attach` duplicated in both frontends, no L2 command | Critical | **Closed** | Step 10. `src/command/commands/squad/attach.rs` (730 lines) owns `SquadAttachCommand`/`SquadAttachFrontend`; `src/frontend/attach.rs` deleted; CLI/TUI reduced to frontend-trait impls. `gateway_need: Running`, `requires_container_tier: true`, `api_allowed: false` (Q7 policy) registered in the catalogue. |
| F-02 | Squad daemon bootstrapped inside `src/frontend/squad/` | High | **Closed** | Step 3. `SquadDaemonEngine`/`SquadDaemonDeps` (`src/engine/squad/daemon.rs`, 489 lines) and `SquadSupervisor` (`src/engine/squad/supervisor.rs`, 660 lines) own bootstrap and lifecycle; `src/frontend/squad/mod.rs` shrank from 187 to 53 lines and contains no `TaskStore`/`SquadScheduler`/`SquadSupervisor`/`DaemonProcess`/`std::fs` reference (grep-verified in the step's own acceptance check). |
| F-03 | API bootstrap, queue worker, session close in the API frontend | High | **Closed** | Step 4. `ApiServerRuntime`/`ApiSessionLifecycle` (`src/command/commands/api_server/runtime.rs`, 495 lines) own bootstrap, worker sizing, session restore and the one drain-and-close path; `QueueWorker` moved to `src/command/commands/api_server/queue_worker.rs`; `src/frontend/api/mod.rs` shrank from 326 to 75 lines. Absorbs 0114 F-42 (`CommandOutcome::exit_code`/`is_partial_failure`) — see 0114 status below. |
| F-04 | Squad routing/tier guard hard-coded in `cli::run` and duplicated in TUI `App` | High | **Closed** | Step 5. `GatewayNeed`/`requires_container_tier` are `CommandSpec` catalogue attributes; both `matches!` lists deleted from `cli/mod.rs`; `SquadGatewayResolver::gateway_for`/`open_for_frontend` replace the TUI's `wrap_squad_startup_error` string-prefix matching with a typed `SquadStartError` enum. New catalogue test `every_squad_subcommand_declares_whether_it_needs_a_gateway` is the regression guard. |
| F-05 | Engine wiring done three times; `main.rs` overgrown | High | **Closed** | Step 2. `Engines::build`/`Engines::for_daemon` (one assembly path each) and `Startup`/`StartupOutcome` (`src/command/startup.rs`, 137 lines) exist; `src/main.rs` shrank from 281 to 206 lines and now only builds clap, calls `Startup::run`, and picks CLI/TUI. Also absorbed 0114 F-52's `Engines::for_tests(root)`, folding nine `make_engines` test copies onto it. |
| F-06 | TUI git sidebar re-implements a git engine | High | **Closed** | Step 6. `GitEngine::diff_summary`/`GitDiffSummary`/`GitFileEntry` added to `src/engine/git/`; `src/frontend/tui/git_sidebar.rs` shrank from 772 to 134 lines. Acceptance grep (`process::Command\|tokio::fs` under `src/frontend/tui`) returned no matches. |
| F-07 | GitHub issue provider is a 1,650-line engine inside Layer 0 | High | **Closed** | Step 11. `src/data/issue/` moved to `src/engine/issue/` (`mod.rs` 346 lines, `github.rs` 1,172 lines); `src/data/issue.rs` (137 lines) keeps only `Issue`/`IssueSourceError`/`IssueSourceFlags`/`slugify`. `git remote get-url` replaced by `GitEngine::remote_url`; `GITHUB_TOKEN` declared in `EnvSnapshot` instead of a direct `std::env::var` read (this also closes the `GITHUB_TOKEN` half of 0114 F-37 — see below). |
| F-08 | Session construction policy duplicated in frontends; `SessionManager` unused | High | **Closed** | Step 8. `SessionManager::open_or_create`/`get`/`get_by_key`/`remove` (`src/data/session_manager.rs`) is now used by the TUI, API and squad daemon. The TUI's Ctrl‑T non-git fallback was changed to the API's `open_or_workdir_fallback` policy; this was recorded as an assumption pending developer confirmation (`step-08-session-manager.md`), and the developer confirmed it as the intended single policy on 2026-09-08 after being shown the concrete behavioral difference — see "Known issues at close-out" below and `CHANGELOG.md`. |
| F-09 | Remote workflow polling/routes/job-status live in the TUI | High | **Closed** | Step 7. `TaskGateway::workflow_state`, `RemoteClient::job_status`/`JobStatus`, and `RemoteWorkflowPoller` moved to `src/command/commands/remote_client.rs` (1,147 lines); `src/frontend/tui/per_command/remote.rs` deleted. `grep -R -n '^pub trait ' src/frontend` returns no matches (confirmed again at close-out: 0 `pub trait` under `src/frontend`, baseline was 1). |
| F-10 | `Dispatch::build_command` hand-retypes catalogue defaults; `implies` never honoured | High | **Closed** | Step 9 (no handoff note was left in the shared workflow directory; verified directly against the current tree at close-out). `Dispatch::resolve_flags` and `ResolvedFlags` (`src/command/dispatch/resolved.rs`, 20,694 bytes) exist; `BuildContext`/`CommandSpec::build` exist; the two mandated parity tests exist and pass: `every_flag_default_is_what_resolution_answers` and `every_implies_edge_is_honoured` (`src/command/dispatch/projections/parity_test.rs`). |
| F-11 | Docker and Apple container backends are one backend written twice | High | **Closed** | Step 12. `src/engine/container/process.rs` (927 lines) owns the shared spawn path (`ContainerCli`, `ContainerInstance`, `ContainerExecution`, `SpawnRequest`, `spawn_piped*`); `docker.rs` 2,305→1,713 lines, `apple.rs` 1,166→665 lines. The step's own `diff -w` check found and collapsed 3 additional duplicates outside the cited line ranges (`ContainerBackend::stop`, `::exec_args`, the two `new()` constructors); residual diff is documented as the two backends' genuinely different `list`/`stats`/`attach`/image-parsing code. Docker-backed integration suite (`AWMAN_DOCKER_INTEGRATION=1`) was **not run** — no Docker daemon in this environment; flagged for a Docker-capable host before this closes. |
| F-12 | Crate-wide `#![allow(dead_code)]`/`#![allow(unused_imports)]` hides 29 warnings | High | **Closed** | Step 1. Both crate-level allows removed; all 29 warnings resolved by deletion (18), `#[cfg(test)]` gating of test-only helpers (4: `RemoteClient::send_command`, `AgentExecution::finished`, `CliUserMessageQueue::pty_active`, `UnattendedFrontend::new`/`with_mount_scope`), or making a field genuinely read (1: `apple.rs`'s `attach_socket`, via `drop(self.attach_socket.take())`). A reappearance guard was added to `tools/architecture-lint.sh` (fails if a crate/module-level `#![allow(dead_code)]`/`#![allow(unused_imports)]` reappears under `src/`) and demonstrated live during Step 1's own verification pass. |

All twelve findings are closed; none were deferred or rejected. F-08's
policy-confirmation caveat was open as of the first close-out pass
(2026-09-05) and was resolved by developer decision on 2026-09-08 — see
"Known issues at close-out" below.

### Before / after — Phase 2 metrics

All "after" numbers below were measured today (2026-09-05) in this
container with `cargo 1.94.0`, immediately after the twelve steps above.
"Before" numbers are copied verbatim from
`/awman/context/workflow/baseline-metrics.md`, captured before Step 1's
changes.

**`make architecture-lint`**

| | Before | After |
|---|---|---|
| Result | `architecture-lint: OK — all imports respect the layering rules` | `architecture-lint: OK — all imports respect the layering rules` |

**`cargo clippy --all-targets -- -D warnings`**

| | Before | After |
|---|---|---|
| Result | Clean build, but only because 29 warnings were hidden behind two crate-level `#![allow(...)]` (see `0113-architecture-audit-hidden-warnings.txt`) | Clean build, **0 warnings, no suppression** — both crate-level allows are gone and every one of the 29 hidden warnings was resolved by deletion, `#[cfg(test)]` gating, or making the field genuinely read |

**File sizes (line counts) — files named in F-01..F-12**

| File | Before | After | Note |
|---|---|---|---|
| `src/lib.rs` | 29 | 26 | F-12 |
| `src/data/mod.rs` | 51 | 50 | F-12 |
| `src/engine/sandbox/backend.rs` | 69 | 32 | F-12 |
| `src/engine/workflow/mod.rs` | 7,622 | 7,492 | F-12 (dead-field part of F-34) |
| `src/engine/agent/mod.rs` | 2,706 | 2,694 | F-12 |
| `src/frontend/cli/output.rs` | 53 | 13 | F-12 |
| `src/engine/container/docker.rs` | 2,305 | 1,713 | F-11 |
| `src/engine/container/apple.rs` | 1,166 | 665 | F-11 |
| `src/engine/container/process.rs` | — (new) | 927 | F-11 |
| `src/data/fs/daemon_guard.rs` | 339 | 338 | F-12 |
| `src/main.rs` | 281 | 206 | F-05 |
| `src/command/startup.rs` | — (new) | 137 | F-05 |
| `src/frontend/api/mod.rs` | 326 | 75 | F-03 |
| `src/command/commands/api_server/runtime.rs` | — (new) | 495 | F-03 |
| `src/frontend/squad/mod.rs` | 187 | 53 | F-02 |
| `src/frontend/squad/unattended.rs` | 1,123 | 1,038 | F-02 (run-log layout moved to L0; policy answers untouched, 0114 F-13) |
| `src/frontend/tui/git_sidebar.rs` | 772 | 134 | F-06 |
| `src/data/issue/mod.rs` | 457 | — (moved) | F-07 |
| `src/data/issue.rs` | — (new) | 137 | F-07 (plain data types only) |
| `src/engine/issue/mod.rs` | — (new) | 346 | F-07 |
| `src/engine/issue/github.rs` | — (new, was `data/issue/github.rs` 1,086) | 1,172 | F-07 |
| `src/frontend/tui/per_command/remote.rs` | 222 | — (deleted) | F-09 |
| `src/command/commands/remote_client.rs` | — | 1,147 | F-09 |
| `src/command/dispatch/mod.rs` | 2,167 | 1,643 | F-10 |
| `src/command/dispatch/catalogue.rs` | — (not separately tracked before) | 3,345 | F-04/F-10 |
| `src/frontend/tui/squad_attach.rs` | 554 | 292 | F-01 |
| `src/command/commands/squad/attach.rs` | — (new) | 730 | F-01 |
| `src/frontend/attach.rs` | 229 | — (deleted) | F-01 |
| `src/engine/squad/daemon.rs` | — (new) | 489 | F-02 |
| `src/engine/squad/supervisor.rs` | — (new) | 660 | F-02 |
| `src/command/commands/squad/supervisor.rs` | — (new, was part of the old `daemon.rs`) | 341 | F-02/F-04 |

**`pub fn` counts**

| | Before | After |
|---|---|---|
| Total under `src/` | 1,007 | 1,123 |
| `src/data` | 400 | 419 |
| `src/engine` | 250 | 274 |
| `src/command` | 164 | 276 |
| `src/frontend` | 193 | 154 |

The total rose (new typed L1/L2 entry points — `Engines::build`/`for_daemon`,
`Startup`, `SquadDaemonEngine`, `SquadSupervisor`, `ApiServerRuntime`,
`ResolvedFlags`, `SquadAttachCommand`, `GitEngine::diff_summary`, etc. — each
add a handful of `pub fn`), concentrated in `src/command` (+112) and
`src/engine` (+24); `src/frontend`'s count **dropped** (193 → 154), which is
the expected direction: frontends lost business-logic functions and kept
only presentation/trait-impl code.

**`use crate::command::commands` under `src/frontend`**

| Before | After |
|---|---|
| 121 | 108 |

**`pub trait` count under `src/frontend`**

| Before | After |
|---|---|
| 1 | 0 |

Matches Step 7's (F-09) acceptance check: `src/frontend/` defines zero
`pub trait`s after this work item (all frontend-facing traits now live in
`src/command/commands/`).

**Suppressed-lint list (`#[allow(...)]` / `#![allow(...)]` under `src/`)**

| | Before | After |
|---|---|---|
| Crate-level (`#![allow(dead_code)]` / `#![allow(unused_imports)]`) | 2 | 0 |
| Item/module-level | 14 | 11 |
| **Total** | **16** | **11** |

Item-level deltas from baseline, all expected:
- `src/frontend/tui/mod.rs:68` `#[allow(clippy::large_enum_variant)]` — **removed**. It sat on `InitialTab::Normal(Session)`; Step 8 collapsed that to a unit variant built from `ctx.session` (absorbs 0114 F-54), which needed no `#[allow]`.
- `src/engine/workflow/mod.rs:452` `#[allow(clippy::too_many_arguments)]` — **removed**. Step 1 deleted `WorkflowEngine`'s `git_engine`/`overlay_engine` fields and constructor parameters (the dead-field part of 0114 F-34), which dropped the constructor's argument count back under the clippy threshold.
- `src/engine/container/docker.rs:839` + `src/engine/container/apple.rs:846` (two narrow RAII `#[allow(dead_code)]` on each backend's own `leases` field) — **merged into one**, `src/engine/container/process.rs:625`, because Step 12 (F-11) unified the two backend-specific instance structs into a single shared `ContainerInstance`.
- The remaining 9 pre-existing item-level allows (line numbers shifted by the deletions above, content and justification unchanged) are untouched: `tui/app.rs` ×3 `type_complexity`, `tui/command_frontend.rs` ×2, `tui/per_command/squad.rs` ×1 `redundant_closure_call`, `exec_workflow.rs` ×2 `too_many_arguments`, `squad/evaluation.rs` ×1, `acp/client.rs` ×1.
- **No new suppression was added anywhere** to make a warning disappear; the one new `#[allow]` (`process.rs:625`) replaces two pre-existing, equally-justified ones rather than adding a new kind of suppression.

### Known issues at close-out — resolved 2026-09-08

A review-correctness pass (`/awman/context/workflow/review-correctness.md`,
timestamped 2026-09-04 23:45) ran after Steps 1–12 landed and found two
Blockers and two Majors. The workflow's `remediate` step was marked complete,
but no `remediation.md` (or any other note) documenting a remediation pass
existed anywhere in `/awman/context/workflow/`, and file modification times
showed nothing under `src/`/`tests/` had changed after `review-correctness.md`
was written — so as of the first close-out pass (2026-09-05), three of the
four findings were confirmed still present by direct re-inspection, and the
fourth (developer confirmation) was still outstanding. **All four are now
resolved**, per developer direction on 2026-09-08:

1. **Blocker — `GatewayNeed::IfRunning` mints and persists a squad key on a
   read-only status check when no daemon is running.** **Fixed.**
   `SquadSupervisor::endpoint_from_meta` (`src/engine/squad/supervisor.rs`)
   now checks `read_meta()` first and only calls `provision_key()` once a
   sidecar is confirmed present; `probe_endpoint` and `ensure_running` are
   unchanged (the latter still mints its key before spawning, deliberately,
   for its own documented reason). New regression test
   `endpoint_from_meta_never_mints_a_key_when_no_daemon_is_running` (same
   file) asserts no `squad_key.hash` is written and no key is generated on a
   fresh root. `cargo test --lib engine::squad::supervisor`: 7/7 passed.
2. **Major — a bare `awman squad` TUI launch with an unknown `runtime:`
   config loses the required fatal modal.** **Fixed.** `src/main.rs` now
   computes a `LaunchMode` (`Cli` / `TuiNormal` / `TuiSquad`) before startup
   and feeds `Engines::detect` an empty path whenever the invocation resolves
   to any TUI form — including the bare-squad TUI, whose parsed command path
   (`["squad"]`) is non-empty but must not be read as a CLI-mode signal.
   New tests `launch_mode_is_tui_matches_each_variant` and
   `bare_squad_has_a_non_empty_path_despite_resolving_to_the_tui`
   (`src/main.rs`) pin this. `cargo test --bin awman`: 5/5 passed.
3. **Major — the CLI's cached `non_interactive` flag does not account for
   `--json`'s `implies non-interactive` resolution.** **Fixed.**
   `CliFrontend::new` (`src/frontend/cli/command_frontend.rs`) now derives its
   cached mode from a new `explicit_non_interactive` helper that ORs the raw
   `--non-interactive` flag with the raw `--json` flag, matching the
   catalogue's `json -> non-interactive` `implies` edge that `ResolvedFlags`
   already applies to the built command. New tests
   `json_alone_implies_explicit_non_interactive`,
   `explicit_non_interactive_flag_alone_is_still_honoured`, and
   `neither_json_nor_non_interactive_is_not_explicit` (same file) pin all
   three cases. `cargo test --lib frontend::cli::command_frontend`: 32/32
   passed.
4. **Blocker — the Ctrl-T non-git fallback policy change (F-08/Step 8) had no
   recorded developer confirmation.** **Resolved by developer decision,
   2026-09-08: keep `open_or_workdir_fallback` as the single shared policy.**
   The developer was shown the concrete behavioral difference before
   deciding: the pre-0113 TUI caught *any* `Session`-open error and silently
   fell back to using the directory as its own git root (Ctrl-T never
   failed); `open_or_workdir_fallback` (`src/data/session.rs:346`) only
   catches the specific `GitRootNotFound` case — any other error (a
   corrupted `.git` directory, a git-resolution failure, a malformed
   `.awman/config.json`) now surfaces as a visible "Failed to open session:
   ..." and the tab does not open. This is the intended, stricter behavior.
   Recorded in `CHANGELOG.md`.

All four fixes above were verified with `cargo fmt --check`,
`cargo clippy --all-targets -- -D warnings`, and `bash tools/architecture-lint.sh`
(all clean), plus the full `cargo test --lib` suite: 2,485 passed, 0 failed,
7 ignored (up from the 2,481/0/7 baseline by exactly the 4 new lib-level
regression tests above; the 2 `main.rs`-bin tests are counted separately).

### Test-suite / `make pre-push` status at close-out

- `make architecture-lint`, `cargo fmt --check`, and
  `cargo clippy --all-targets -- -D warnings` all pass cleanly (0 warnings)
  as of this close-out.
- The unit/lib suite (`cargo test --lib`) passes cleanly in isolation:
  2,481 passed, 0 failed, 7 ignored (the 7 are the documented
  Docker-integration `#[ignore]` tests).
- One additional stale test was found and fixed during this close-out (test
  file only, not production code): `tests/api_parity/rename_0077.rs::api_startup_log_message_contains_awman_and_api_mode`
  scanned for a lifecycle log string only when it was the sole token on its
  source line; Step 4 (F-03) reformatted the `tracing::info!` call onto one
  line with keyed fields ahead of the message literal, which the old scan
  missed. Fixed to scan quoted segments per line instead of whole trimmed
  lines; verified passing in isolation.
- **A single, uninterrupted `make pre-push` run could not be completed in
  this container.** This container has accumulated, over the full 18-step
  workflow, thousands of zombie processes (`/proc` shows ~6,700+ zombies
  parented by PID 1, which runs no reaper) — the exact catastrophic
  resource-exhaustion mode `test-plan.md` (the `test-hardening` step)
  independently hit and documented, which recommended re-running
  `make pre-push` in "a fresh/recovered container." This close-out
  additionally found the `TooManyLinks` (`/tmp`, ext2/3-style
  ~65,000-hard-link-per-directory ceiling) symptom is **not** purely a
  concurrency artifact of running many test binaries at once: three
  successive brand-new, empty `TMPDIR`s (created fresh, never reused) each
  independently hit the same ceiling within a single serialized
  (`--test-threads=1`) full-suite run — i.e. the directory's link count
  appears not to be reclaimed by this environment even after
  `tempfile::tempdir()`'s `Drop` removes the directory, so the ceiling is a
  function of cumulative tempdir churn over the run's lifetime, not of how
  many tempdirs are live at once. Reducing parallelism therefore does not
  avoid it; only a filesystem with reclaimed link counts (i.e. a fresh
  container) does. Individually, every major test group has been
  observed passing at some point across this work item's steps (see each
  step's own validation section above and the `step-*.md` files), and the
  `--lib` target passed cleanly in isolation on this close-out's first,
  least-polluted attempt (2,481 passed, 0 failed, 7 ignored) — a later
  attempt against an already-degraded `TMPDIR` showed 4 of 2,481 lib tests
  failing, all four with the identical `TooManyLinks` symptom and none a
  code-level failure. But **this close-out
  cannot certify one single clean `make pre-push` invocation** in this
  container. This must be re-verified in a fresh container before WI 0113 is
  signed off as fully green — do not treat this report's individual passing
  runs as equivalent to a certified `make pre-push` pass.

## Remediation — WI 0114

Close-out pass, 2026-09-23. Covers F-13 through F-54 (the Medium and Low
findings WI 0114 owns) plus F-55, F-56 and F-57 from the v0.12 re-audit
(Group G). Evidence: `/awman/context/workflow/50-final-review.md` (the
independent adversarial review against the tree at `91e00af..fc81410`),
`51-remediation.md` (the eleven commits that cleared every should-fix finding
the review raised), every `step-group-*.md` / `NN-group-*.md` handoff, and
direct re-inspection of the tree at `HEAD` (`ad53bbf`) — commit subjects are
not trusted on their own (`50-final-review.md` note 16 records that group-g
wrote no handoff and one commit's subject covers two findings).

**F-57 verified independently by this pass, not taken on trust.** Both guards
were read directly in `tools/architecture-lint.sh` (`layer-render` at line
421, `dispatch-bypass` at line 370 — neither commented out, neither carrying
an allowlist) and `make architecture-lint` was re-run alone: `architecture-lint:
OK — all imports respect the layering rules`, exit 0. F-57 is closed.

### Disposition of F-13 – F-54, plus F-55 – F-57

Status vocabulary matches `50-final-review.md`: **closed**, **closed
(deviation)** — done with a recorded judgment call the developer should
confirm, **deferred** — with the reason and the remaining scope, **rejected**
— with the decision, **rejected-superseded** — a specific step rejected
because a later, deliberate design decision overrides it.

| ID | Status | Commit(s) | Detail |
|---|---|---|---|
| F-13 | closed (deviation) | `cf87ef8`, `e1bd88d` | `HeadlessDefaults::{api,squad,cli}` — three named profiles, not Q5's two named profiles; developer to confirm the third (`cli()`) is intended (recorded in `assumptions.md`). Table test pins every cell. Q4 residue (the squad leader's mount scope, chosen in `src/frontend/squad/unattended.rs`) cleared in `e1bd88d`: `SquadRunFrontends::leader_frontend` now takes `mount_scope` from the same expression `workflow_frontend` always used. |
| F-14 | closed (deviation) | `971c50a` | One `AuthMode` (engine-owned); `request_auth_mode`/`verify_bearer`; timing-shape test. Deviation, recorded and left as-is: step 2's squad-daemon `AuthEngine` was not built — `src/engine/squad/daemon.rs:199` still calls `AuthMode::resolve_for_daemon` over raw paths rather than building an `AuthEngine` over `SquadPaths::daemon()`. The reason lived only in commit `971c50a`'s message; it is recorded here so it survives history rewrites. |
| F-15 | closed | `27fbcd4`, `f40c875` | "INTERACTIVE mode" banner deleted in both TUI and CLI (Q6); `LaunchDisplay`; gateway keying on `path.first() == "squad"` replaced by `GatewayNeed`. |
| F-16 | closed | `149b12e` | `ContainerStatsSampler` + `AgentRuntimeEngine::stats_by_name`; the tuple stats channel is gone. |
| F-17 | closed | `9f1bc16`, `14b3e7c` | `SquadSupervisor::health()` in L1; `SquadIndicatorPoller` keeps only cadence and colour. The one classification the first pass left in the TUI (`SquadGatewayResolver::from_env` failure → `Unreachable`) moved to `SquadGatewayResolver::health_from_env` (L2) in `14b3e7c`; no deviation remains. |
| F-18 | closed | `49e9c38` | `AvailableActions::simple_advance` / `current_step_name` computed by the engine; the TUI's permission-guard duplication is gone. |
| F-19 | deferred (partly done) | `b31a4f4`, `679e7e1`, `e439f2c`, `46bd4a1` | Done: `Prompt<D>`/`Choice<D>` (L0, `src/data/prompt.rs`), `command::prompts` (L2 copy), `ask_spec_kind`, `ask_task_interval`, the squad confirm dialog (F-55), the CLI's squad-interview bodies and its `yes_no`/`read_line`/`read_multiline`/`pick_numbered` helpers (no longer TTY-testing themselves — `e439f2c`, which also closed four live F-50 misses: `init`'s `ask_replace_aspec`/`ask_run_audit` and `ready`'s `ask_create_dockerfile`/`ask_run_audit_on_template`), and the Dockerfile-setup prompt including its six frontend tests that used to pin `CreateNew` on a dismissal (`46bd4a1`). **Still deferred:** the remaining `ask_*` methods that keep a bespoke TUI dialog — `ask_task_repo`, `ask_task_overlay`, `ask_edited_*`, and the init/ready yes/no steps whose Enter-default legitimately *is* the Layer 2 profile's value. Reason: each has a bespoke TUI dialog; converting piecemeal leaves the TUI less consistent than converting as a batch, which is new work beyond this item's should-fix list. |
| F-20 | closed | `01f3f5e` | `DialogResponse::ConfigEdit(ConfigEditRequest)`; `is_valid_map_key`'s re-derivation of `AgentName`'s rules is gone. |
| F-21 | closed | `e372c4b` | `Session::is_git_repo` (L0); `Dispatch::startup_command`/`FrontendAction::startup_for` (L2) backs both TUI call sites. |
| F-22 | closed (deviation) | `34d5413`, `00eceb6` | `Tab` derives phase, workflow overview and container name from `SessionState` each tick; typed `StepViewStatus` replaces the string matches. Deviation, recorded: `Tab` was not split into a `TabView` type, and `stuck`/`yolo_mode` stay on `Tab` rather than moving — both named as deliberate simplifications from the drafted shape. |
| F-23 | closed | `ae676b3` | `RuntimeContext` moved to L2; `render_summary_box`/`StepStatus::glyph` to a shared L3 `render_helpers`. `src/frontend/tui` imports nothing from `src/frontend/cli`. |
| F-24 | closed | `c3478ff` | `TabSharedState`, `DialogChannels`, `TabSharedState::for_tests()`; all four `type_complexity` allows removed. |
| F-25 | closed | `cf87ef8`, `3658159` | Steps 1–3 (`CommandCatalogue::markdown_reference`, generated `docs/14-command-reference.md`, `cli.md` rewritten to UX standards only) landed in `cf87ef8` under an F-13 commit subject — do not infer status from commit subjects (`50-final-review.md` note 16). Step 4 (`requires_runtime` as a `CommandSpec` attribute on all 47 literals, plus the `make docs-reference` target) closed in `3658159`, which also folded `opens_tui_when_bare` into the same per-command-attribute pass and deleted the `TUI_WHEN_BARE` catalogue-level list, per `assumptions.md`. |
| F-26 | closed | `d2fe0e0` | One token parser (`parsed_input::parse` → `raw_args::parse_against_spec`); the TUI rejects `--launch-mode banana` / `--port abc` at parse time, same as the API; `ready -ab` reads `unknown flag: -ab`. |
| F-27 | closed (deviation) | `cf4d2a4` | Overlay grammar, `DirectorySpec`/`ContextScope`/`OverlayPermission` moved to L0; `LaunchPolicy` (L2) holds `resolve_agent`/`resolve_launch_mode`/etc. Deviation: load-time validation of `RepoConfig.overlays` (`DataError::InvalidOverlaySpec`) was deliberately not added — recorded, not silently dropped. |
| F-28 | closed | `6ad20fc` | `engine::remote::{http_core, client, events}`; one async `reqwest::Client` builder shared by the remote client. |
| F-29 | closed (deviation) | `844b426` | `DaemonSupervisor` (the only type that may spawn the daemon binary), `AspecDownloader`, `RemoteSlug`. Deviation: `ContextDirResolver::repo_dir`'s parameter is named `remote_effective_url`, not the report's `remote_url`, to name what it actually is; `skill_library.rs`'s own remote-slug parser was kept rather than unified (recorded). Windows is unverified by a real cross-compiled build in this container (see the test-suite note below). |
| F-30 | closed | `a0dc210` | One workflow-state store (`WorkflowStateStore`, `src/data/fs/workflow_state.rs`); `resolve_default_agent` deleted in favour of `Session::open_at_git_root` deriving `default_agent` from `EffectiveConfig::agent()`; hash helpers moved to `fs/hash.rs`. |
| F-31 | closed | `f8100bf` | Config reads route through `Session`/`Engines`; `DataError::ConfigParse` names the file, line and reason; the config-load lint guard is live (two allowlisted, deliberate sites remain: `command/dispatch/mod.rs` and `engine/init/mod.rs`, down from 13 production sites at baseline — see metrics below). |
| F-32 | closed (deviation) | `09b634a`, `d023af6`, `dd7cb51` | `ContainerBackend`/`AgentMatrix` are the only per-backend and per-agent tables; `"apple-containers" =>` arms and `agent.as_str() ==`/`match agent.as_str()` under `src/engine` are both at 0 (see metrics). Deviation, recorded and known: the antigravity host-ping still runs a binary literally named `antigravity`, but antigravity ships `agy` — the ping always fails. This is a **known issue**, stated in `docs/releases/unreleased.md`, not fixed here (fixing it changes the one sanctioned host-agent-execution path and needs its own work item). |
| F-33 | closed | `3f8c7f0` | `AgentImageFrontend` (renamed from the taken `AgentSetupFrontend`); all five re-export shim files deleted, including the fourth one (`frontend/api/session_setup.rs`) found after the item was written. |
| F-34 | closed | `f3e84de` | `PhaseKind::{Setup,Teardown}`; `run_phase`/`run_single_phase_step`/`run_phase_remediation` collapse the setup/teardown twins; five `on_phase_step_*` callbacks replace the ten; `WorkflowEngineDeps`/`WorkflowSpec` replace the 5–6-parameter constructors. Setup's failure-file behaviour is release-noted as an intentional improvement; the API's `phase` string is still exactly `"setup"`/`"teardown"`. |
| F-35 | closed (deviation) | `081dea0` | `AuthCommand`/`DownloadCommand`/their frontends deleted (tarball logic kept as a private `InitCommand` helper); the three empty `ApiServer*CommandFrontend` traits merged; `AgentLaunchFrontend` added. Deviation: `set_pty_active` was made **required**, not defaulted — a stronger reading of the drafted "pick one default" instruction, so a frontend cannot silently drop PTY state. `Command`-trait impl count is 14 (verified below), matching the corrected target. |
| F-36 | closed (deviation) | `72f8353` | `WorkflowProxy` deleted in favour of a blanket `impl<F: WorkflowFrontend + ?Sized> WorkflowFrontend for Arc<Mutex<Box<F>>>`. Deviation: `AgentFrontendProxy` was kept rather than deleted — it owns per-container I/O that the blanket-impl shape does not fit — and this is recorded rather than silently diverging from the drafted "delete both". |
| F-37 | steps 1, 2, 4 closed; **step 3 rejected-superseded** | `822d280`, `3929ec6` | Steps 1, 2 and 4 closed: `AWMAN_ATTACH_DIR`/`AWMAN_API_VERBOSE_SETUP` declared in `EnvSnapshot`; `poll_ci.rs` wrapped as `CiPoller`; the `env-var` lint guard is live in `tools/architecture-lint.sh`, allowlisting only `#[cfg(test)]` code and the one deliberate production site named in step 3. **Step 3 is rejected as drafted, not deferred** — implementing it would reintroduce the WI 0116 regression and put possibly-secret values into world-readable argv: `resolve_env_passthrough` (`src/engine/container/docker.rs:706`) resolves through `host_var` **at spawn time, on purpose**, because inside the squad daemon a task's `env()` value arrives over the authenticated socket and lives only in the Layer 0 daemon overlay — it never enters the daemon's own process environment, and pre-resolving it at L2 from an `EnvSnapshot` (what step 3 as drafted asks for) would put it back there. Turning the passthrough into `EnvLiteral` would also change the emitted `docker` argv from name-only `-e NAME` to `-e KEY=VALUE`; `docker.rs:800-816` documents why that must never happen — argv is world-readable through `/proc/<pid>/cmdline`, so a host value that may be a secret must stay out of it. That is a security regression, not a refactor, so it is rejected outright rather than left open. The one remaining raw `std::env::var` loop (`src/engine/sandbox/dsbx/session_config.rs:100`) is deliberately *not* routed through `host_var` for the opposite reason: that map is written in the clear to `<workspace>/.awman/session.json`, and reading through the daemon overlay would make a socket-pushed value eligible for a plaintext file. Both exceptions are named in the `env-var` guard's allowlist by path, with the reasoning in the guarded files' own comments. |
| F-38 | closed | `169440a` | `HostAgentPinger` (`src/engine/ready/host_agent.rs`) is the only type in the tree that may spawn an agent binary on the host; `install_global`/`global()`/`OnceLock` deleted, `ResolvedContainerOptions` carries an explicit `lease_factory` (`options_without_a_monitor_disable_leases` proves the no-monitor path); `SquadAgentLauncher` owns the run-log-directory helpers. `security.md`'s S1 note is updated in this close-out pass to name `HostAgentPinger` (see below). Release-noted side effect: the squad daemon's `authRefresh` is now global-only (docs updated in `99de7b4`). |
| F-39 | closed | `b8e4c9a` | `GitEngine::identity_configured`/`GitIdentity`; `worktree_git_status` replaced by `git_engine.uncommitted_files`. Behaviour change (release-noted): a repo-local `user.name`/`user.email` is now honoured. |
| F-40 | closed | `ab9b288` | Steps 1–3 (capability-based branching in `ready.rs`/`clean.rs`, `Capabilities::has_image_store`, `SandboxRuntime::ready_agent`); step 4 was already done at the item's review pass (`Capabilities::squad_supported()`). |
| F-40b | closed (deviation) | `dc15cc4`, `246a3fc` | `AgentRuntimeEngine::{ready_agent, image_exists, image_home_dir}`; `ReadyEngine`/`InitEngine`/`AgentEngine` take `Arc<dyn AgentRuntimeEngine>` instead of a concrete `Arc<ContainerRuntime>`; `ready.rs` has no `sandbox_runtime.is_some()` branch left. Deviation, developer to confirm: `ContainerRuntime::ready_agent` returns `EngineError::UnsupportedOnRuntime` rather than the sandbox tier growing image-build support (recorded in `assumptions.md`). |
| F-41 | closed | `244efec` | `SessionSetupPresenter` (five methods) split out; `SessionSetupState` mutation logic moved onto the L0 type in `src/data/session_setup_event.rs`; the API bus is a pure broadcaster. |
| F-42 | closed (by WI 0113) | — | `CommandOutcome::exit_code`/`is_partial_failure`; both CLI and API's `QueueWorker` call the same method. Nothing was open for 0114. |
| F-43 | closed | `8fd1f3f` | Levenshtein helper moved to `src/data/text.rs`; `CommandCatalogue::suggest` covers nested paths; `CommandError::UnknownCommand { path, suggestions }` — both frontends only format. |
| F-44 | closed | `4873a7d` | `PanicLog::from_env`/`path`/`append` in L0; the TUI hook formats and calls it. |
| F-45 | closed | `b3d5f12` | `attach_engine(EngineHandles)` replaces the four `set_*` setters; `ReadyStep`/`InitStep`/`AgentSetupStep` enums with `Display`; `GitFrontend::command_started`, `ReadyFrontend::report_ping`, `WorkflowFrontend::report_ci_poll` added with default text-writing impls, so no frontend output changed unless it opted in. |
| F-46 | closed | `a0e6c8e` | `TaskEvaluator`/scheduler reports resolved `(agent, model)` back instead of re-parsing `agent::model` strings; `AcpSession::new` takes a typed `PermissionPolicy`. |
| F-47 | closed | `1464630`, `4ad8f1f`, `9fa7e3b` | Steps 1–2 (`RunVerdict` in L0, `TaskStore`/`SquadPaths` helpers, the key published to the L0 overlay and read back via `PUBLISHED_KEYS`) closed early. **Step 3 was open through the first review pass** (`api_server.rs` still called `banner::render_api_key_banner` and pushed box-drawing art up as `UserMessage.text`) and was the review's one blocking finding; fixed in `9fa7e3b`: `ApiKeyDisclosure` (L2, one-shot), `ApiServerCommandFrontend::show_api_key` made **required** (not defaulted, so a frontend cannot silently drop the only copy of the key — the same hazard the pre-existing `SquadCommandFrontend::show_key_setup` default has, deliberately not repeated); the CLI draws the box, the TUI and API state the key as text; `banner.rs` deleted. This is also the commit that landed F-57 guard 1 (see below). |
| F-48 | closed | `1aa5978` | `WorkflowStepTransition`, `CommandStatus`, `WorkflowPhaseTransition`, session `type` and `awman squad env` row `state` are typed enums; no wire value was renamed; `persistence` is now **absent** rather than `""` when no daemon answered (release-noted and documented). |
| F-49 | closed (deviation) | `d9d58a0` | `CallerContext { path, frontend, local_user }` built once by `Dispatch`; `StatusCommandFrontend::tui_context()`/`SquadCommandFrontend::is_local_user_session()` deleted. Deviation: `tui_context` stays as a live data hook on `CallerContext` rather than disappearing entirely (recorded as deliberate). |
| F-50 | closed | `39e811e`, `e439f2c` | `CommandFrontend::input_available()` (TTY fact only) feeds `ResolvedFlags::is_non_interactive`; `effective_non_interactive` and the cached CLI field are gone; `CommandCatalogue::frontend_profile(FrontendKind)`; `FrontendKind::SquadDaemon` added to `FrontendVisibility`. `e439f2c` closed four live misses of this pattern the review did not name (`init`'s and `ready`'s remaining `yes_no`-backed prompts, which still tested the TTY directly inside `ask_*` until then). |
| F-51 | closed (deviation) | `35cbde8`…`d6c85de` (11 commits) | All eleven file-split/typed-status bullets landed; `exec_workflow.rs`, `workflow/mod.rs`, `dispatch/catalogue.rs` and `tui/render/dialog.rs` are now directories (see the scope-drift table below). Deviation: `SQUAD_EDIT`'s flag-array derivation and the `env_state.rs` code split were not done (only its tests were split) — recorded, not silently dropped. One user-visible string collapsed from three wordings to one during the splits (`exec workflow: failed to create worktree: {e}`); release-noted rather than restored, because restoring it would widen `WorktreeName` with a variant the type deliberately does not have. |
| F-52 | closed | `72c7bf1`, `4558fc0` | `Session::for_tests*`/`IsolatedEnv::engines` replace the 23 `make_session` / 11 `make_engines*` copies (down to 15 / 2 — see metrics; the residue is legitimate per-test fixtures, not duplication of the deleted kind); 0 `todo!()` in production code. The thirteen integration test files that had never compiled (`tests/{async_cancellation,download_integration,headless_integration,init_unification,memory_bounds,modular_dockerfiles,overlays_integration,performance_stress,remote_integration,runtime_integration,shortcuts_0054,terminal_selection,tui_tabs}.rs`) were confirmed dead one-by-one (each given a temporary `[[test]]` entry and `cargo check --test` run against it individually, all thirteen fail at the pre-0072 import surface) and deleted in `4558fc0`. Neither `tui_tabs.rs` nor `download_integration.rs` — the two files this item's own text names as live regression suites — was ever registered or buildable; live TUI tab coverage is `src/frontend/tui/tabs/tests.rs` and `tests/squad_tui_tab.rs`. |
| F-53 | closed | `9cf0ff2` | `UserMessageSink`'s doc comment now names its own layer as Layer 0, correcting a "Defined by Layer 1" comment. |
| F-54 | closed (by WI 0113) | — | `SessionManager` used by the API. Nothing was open for 0114. |
| F-55 | closed | `679e7e1`, `7f79073` | `SquadConfirmAction` deleted; `Dialog::SquadActionConfirm` carries a `Prompt<SquadConfirmDecision>` built by `SquadCommand::confirm_prompt` → `prompts::squad_task_confirm`; the TUI maps `y`/`n` to the choice's `value`; dispatch goes through `CommandCatalogue::action_input`, not a hand-built `ParsedCommandBoxInput`. F-57 guard 2 landed in the same commit (`679e7e1`), pasted, with no allowlist. |
| F-56 | closed | `f2a3650` | `KeyDisclosure` carries the key, shell and export line with no rendering; the CLI draws the banner (`frontend/cli/per_command/squad.rs`), the TUI shows none. Note: the completing commit is `f2a3650` (tests-audit) because group-g's own work was never committed — do not infer this from `git log --grep=F-56` alone. |
| F-57 | closed | `679e7e1` (guard 2), `9fa7e3b` (guard 1) | Both guards verified live by this close-out pass directly against `tools/architecture-lint.sh` and by re-running `make architecture-lint` alone (see above): `layer-render` (guard 1, no allowlist — landed in the same commit that cleared F-47 step 3's last hit) and `dispatch-bypass` (guard 2, no allowlist — landed in the same commit that cleared F-55's last hit). Neither guard is commented out and neither allowlist covers a site either guard was meant to forbid. |

### Before / after — Phase 2 metrics

"Before" is `/awman/context/workflow/03-baseline.md`, captured 2026-09-22
before any 0114 change (297 `.rs` files, 150,956 lines under `src/`,
`Cargo.toml` still `0.12.0`, no commit hash available in that container).
"After" is measured today (2026-09-23) at `HEAD` (`ad53bbf`), same container
family, `cargo 1.94.0`. `src/` is now **356 `.rs` files, 160,623 lines** (+59
files, +9,667 lines — F-51's splits create new files faster than they delete
lines; F-33/F-35/F-52 net-delete elsewhere).

**`make architecture-lint`**

| | Before | After |
|---|---|---|
| Result | `architecture-lint: OK` | `architecture-lint: OK` |
| Guards present | Tenet 1 (Layers 0–2), F-12 `#![allow]` guard, WI 0116 keychain-argv guards (2) | Same four, **plus** `env-var` (F-37), `config-load` (F-31), `dispatch-bypass` (F-57 guard 2), `layer-render` (F-57 guard 1) — eight guards total, verified by reading the script |

**`make pre-push`**

| | Before | After |
|---|---|---|
| Result | Not run by the planning step (no code changed yet); the WI 0113 close-out could not certify one clean run in its container (zombie/`TooManyLinks` exhaustion) | **exit 0** — 3,493 passed / 0 failed / 1 ignored (the one ignored test is `regenerate_command_reference`, a generator, not a check). Re-run by this close-out pass, verbatim. |

**Scope-drift files (`wc -l`) — the five files the work item's own review flagged as having grown since the report**

| File | Report (2026-09-03) | Baseline (2026-09-22) | After (2026-09-23) |
|---|---:|---:|---|
| `src/command/commands/exec_workflow.rs` | 5,943 | 7,195 | **split** into `exec_workflow/` (7 files, 6,510 lines total) |
| `src/engine/workflow/mod.rs` | 7,622 | 8,154 | **split** into `engine/workflow/` (16 files incl. tests, 11,359 lines total; `mod.rs` itself is 947) |
| `src/command/dispatch/catalogue.rs` | 3,014 | 3,433 | **split** into `dispatch/catalogue/` (9 files, 3,865 lines total) |
| `src/frontend/tui/render/dialog.rs` | 1,450 | 1,713 | **split** into `render/dialog/` (5 files, 1,882 lines total) |
| `src/frontend/tui/squad_indicator.rs` | 232 | 398 | **71** (health/classify logic moved to `engine/squad/supervisor.rs`, 1,146 lines, and `command/commands/squad/supervisor.rs`, 454 lines) |

None of the five is a single file over ~1,500 lines any more; each is now
several files with one responsibility apiece, per User Story 3 (F-51, F-34,
F-25, F-17).

**`Command` trait implementations**

| | Before | After |
|---|---:|---:|
| `grep -rnE 'impl (Command\|dispatch::Command) for' src/` | 17 | **14** |

Matches F-35's corrected target exactly. The 14: `remote, clean, config,
ready, specs, status, api_server, init, exec_prompt, chat, exec_workflow,
squad/attach, squad/commands, new`. `auth`, `download` and the squad-daemon
sub-commands (`run_start`/`run_stop`/`run_status`/`run_logs`) are gone,
folded into `InitCommand` (tarball helper) and `SquadCommand` respectively.
The anchored grep returns 14 exactly; an unanchored `grep -rn` also matches a
doc-comment line in `exec_workflow/execute.rs` that quotes the impl but is
not one.

**Per-agent / per-backend tables (F-32)**

| Metric | Before | After |
|---|---:|---:|
| `agent.as_str() ==` under `src/engine` | 4 | **0** |
| `match agent.as_str()` under `src/engine` | 6 tables | **0** (the one remaining hit is a comment in `agent_matrix.rs` naming what it replaced) |
| `"apple-containers" =>` arms | 6 | **0** |

**Lint suppressions**

| Metric | Before | After |
|---|---:|---:|
| `type_complexity` allows | 4 | **0** |
| All `#[allow(...)]`/`#![allow(...)]` under `src/` | 11 item-level, 0 crate-level | **7** item-level, 0 crate-level |

**F-57 guard pre-counts**

| Guard | Before (baseline pre-count) | After |
|---|---:|---|
| Guard 1 (`layer-render`, box-drawing below L3) | 7 hits (`key_setup.rs` ×2, `banner.rs` ×4, `api_server.rs` ×1) | **0 hits, no allowlist** — verified live by reading `tools/architecture-lint.sh` and re-running `make architecture-lint` |
| Guard 2 (`dispatch-bypass`, hand-built `ParsedCommandBoxInput`) | 15 hits (9 production, 6 test fixtures) | **0 hits, no allowlist** — same verification |

**Config-load discipline (F-31)**

| Metric | Before | After |
|---|---:|---|
| `GlobalConfig::load()` / `RepoConfig::load(` outside `src/data/` and `src/command/startup.rs`, production code | 13 sites | **2 sites**, both allowlisted by the `config-load` guard as the one deliberate read each names (`command/dispatch/mod.rs`, `engine/init/mod.rs`) |

**Other Phase 2 metrics**

| Metric | Before | After |
|---|---:|---:|
| `pub fn` total under `src/` | 1,194 | 1,358 |
| … `src/data` / `src/engine` / `src/command` / `src/frontend` | 452 / 302 / 282 / 158 | 497 / 368 / 330 / 163 |
| `pub struct` per layer (data/engine/command/frontend) | 89 / 132 / 163 / 50 | 100 / 154 / 158 / 51 |
| `pub trait` per layer (data/engine/command/frontend) | 4 / 14 / 35 / 0 | 4 / 19 / 28 / 0 |
| `use crate::command::commands` under `src/frontend` | 112 | 115 |
| `#[ignore]` tests | 8 | **1** (the docs-reference generator; F-52 deleted the dead-file `#[ignore]`s) |
| `todo!()` in `src/` | 7 | **0** |
| `fn make_session` copies | 23 | **15** |
| `fn make_engines*` copies | 11 | **2** |
| `impl WorkflowFrontend for` (real + fakes) | 14 (6 real incl. the proxy, 8 fakes) | 14 (5 real + a blanket forwarding impl that doesn't match this grep, 9 fakes) |
| `"6h"` literals outside the catalogue, non-test | 3 | **0** (the catalogue's `FlagDefault::Str("6h")` is the only production value; every other hit is a doc comment or a `Prompt`/test literal) |
| `reqwest::Client` builders/constructors, non-test | 6 | **2** (`engine/remote/http_core.rs` — the shared client `poll_ci`, `remote_client` and `agent/download.rs` now use — and `engine/issue/github.rs`, documented as the one deliberate holdout) |
| Re-export shim files (`pub use crate::…::*`, ≤2 lines) | 5 | **0** |
| Direct `stdin_is_tty()` calls in `src/frontend/cli` | 22 lines (~19 production) | **8** (one definition, the rest resolution/test code) |
| `pub trait` in `src/command` | 35 | 28 |

### Machine checks re-run at close-out

| Check | Result |
|---|---|
| `bash tools/architecture-lint.sh` | `architecture-lint: OK — all imports respect the layering rules`, exit 0 |
| `make architecture-lint` | Same, exit 0 |
| `make pre-push` | exit 0 — 3,493 passed / 0 failed / 1 ignored |
| F-57 guard 1 script read | Live at `tools/architecture-lint.sh:421` (`layer_render_matches=`), no allowlist, not commented out |
| F-57 guard 2 script read | Live at `tools/architecture-lint.sh:370` (`dispatch_bypass_matches=`), no allowlist, not commented out |

### `aspec/architecture/security.md` and `aspec/architecture/four-layer-summary.md`

Both updated by this close-out pass:

- **`security.md`**: the S1 "Sole exception" bullet now names `HostAgentPinger`
  (`src/engine/ready/host_agent.rs`) instead of the deleted `ping_local_agent`
  free function, states it is the only type in awman permitted to spawn an
  agent binary on the host, and distinguishes it from
  `engine::daemon::DaemonSupervisor` (the only type permitted to spawn the
  *daemon* binary — a separate exception, per `13-group-d.md` §7 and
  `14-group-e.md` §4). Per report decision Q11, a new sanctioned-exception
  bullet documents the `docker.sock` mount under `--allow-docker` as a
  deliberate, opt-in widening beyond the current-directory-only mount rule
  (S2) — a socket, not a directory, off by default, built only by
  `docker.rs`'s `allow_docker` argv path.
- **`four-layer-summary.md`**: "What Goes Where" gained `Prompt<D>`/`Choice<D>`
  under Layer 0 (F-19), `AgentMatrix`/`ContainerBackend` and `HostAgentPinger`
  under Layer 1 (F-32, F-38), and `CallerContext`, `HeadlessDefaults`,
  `LaunchPolicy` and `command::prompts` under Layer 2 (F-49, F-13, F-27,
  F-19) — none of these four named types were documented in this file before
  0114. "Last Updated" bumped to September 23, 2026 (WI 0114).

### Release notes

`docs/releases/unreleased.md` (committed in `99de7b4`) carries every
user-visible behaviour change from this item, grouped by what it means for a
reader rather than by finding number: `chat`/workflow exit-code and status
parity (F-22, F-42), `--non-interactive` honoured on a TTY (F-50, F-19), a
malformed `config.json` failing a daemon start instead of silently defaulting
(F-31), the squad daemon's `authRefresh` going global-only (F-38), `persistence`
omitted vs. `""` (F-48), interactive prompts no longer self-answering on a
pipe (F-19/F-50), GitHub remote-URL matching widened (pre-existing fix folded
in), the repo-local git identity fix (F-39), nested "did you mean" (F-43), a
remote session's tab colour fix, the setup/teardown failure-file parity
(F-34), the TUI command box matching the API's validation (F-26), typed wire
enums with no renamed values (F-48), one `ready` summary everywhere, the
API-key and squad-key banners drawn per frontend instead of as shared
box-drawing text (F-47, F-56), the mount-scope/workspace/Dockerfile prompts
reading the same in the CLI and the TUI (F-19), one worktree-creation failure
message (F-51 note 12), and the "INTERACTIVE mode" banner's removal (F-15,
Q6). A "Known issues" section states the antigravity host-ping name mismatch
(F-32) rather than shipping it silently. `docs/07-configuration.md` documents
`authRefresh`'s new global-only scope for the squad daemon.

### Residue not owned by close-out (recorded, not re-litigated)

The following are named in `50-final-review.md` notes 15–22 and confirmed
still true at `HEAD`; none is this item's to fix and each is recorded so the
next audit does not re-derive it: commit hygiene on seven pre-existing
cross-layer commits with no `architecture-lint` paste (history, not
reversible without a rewrite); process gaps in group-b and group-g, which
wrote no handoff files; Windows and macOS unverified by a real cross-compiled
build in this container (`rustup target add` dies in `aws-lc-sys`); no
container-gated test path exercised (no Docker daemon in this container);
four pre-existing Tenet 2 shapes flagged "for the next audit" at baseline
`91e00af` (the CLI's `exec workflow` fast path bypassing `Dispatch::admit`,
`ApiDispatchFrontend`'s no-op `show_key_setup`, `error_exit_code`, the
`CardStatus` precedence table); four developer decisions pending in
`assumptions.md` (`HeadlessDefaults::cli()` as a third profile, `ready_agent`
returning `UnsupportedOnRuntime`, the antigravity ping name, `authRefresh`
global-only); midpoint note 26 (two wire fields still typed as `String`); and
F-55's argument-key round-trip test being vacuous by construction until a
second squad subcommand argument exists.

All should-fix and blocking findings from the adversarial review are FIXED.
Nothing was rejected in remediation; F-37 step 3 is this item's only
rejected(-superseded) step, and it was rejected at the work-item-review stage
(2026-09-22), not during remediation.
