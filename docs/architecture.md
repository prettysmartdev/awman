# awman Architecture

This page is for contributors: how the source is organized and the rules that keep the CLI, TUI, API and squad behaving the same way. The authoritative specification lives in [`aspec/architecture/`](../aspec/architecture/) — start with [`four-layer-summary.md`](../aspec/architecture/four-layer-summary.md), then [`2026-grand-architecture.md`](../aspec/architecture/2026-grand-architecture.md) and [`design.md`](../aspec/architecture/design.md). Security constraints are in [`security.md`](../aspec/architecture/security.md).

---

## The four layers

awman has four frontends — the CLI, the TUI, the API server and the squad daemon — over one shared core. Every frontend is a presentation shell; behaviour lives below it, so a command does the same thing wherever it is run.

```
Layer 4: binary    src/main.rs — parses argv, runs Startup, picks the CLI or TUI
Layer 3: frontend  src/frontend/ — CLI, TUI, API HTTP, squad HTTP: input and output only
Layer 2: command   src/command/ — Dispatch, the command catalogue, per-command business logic
Layer 1: engine    src/engine/  — containers, workflows, git, overlays, auth, agents, images
Layer 0: data      src/data/    — Session, config, workflow definitions/state, filesystem and database access
```

Three tenets:

1. **No upward calls.** A lower layer never imports from a higher one. When an engine needs input or output, it defines a frontend trait (for example `WorkflowFrontend`, `AgentFrontend`, `ReadyFrontend`) and a higher layer implements it.
2. **Frontends present; they don't decide.** Agent selection, defaults, headless answers and error handling live in Layer 2 or below.
3. **Typed objects over free functions.** Significant abstractions are structs with methods.

| Layer | May import from |
|---|---|
| 0 `data` | `crate::data` and external crates |
| 1 `engine` | Layers 0–1 |
| 2 `command` | Layers 0–2 |
| 3 `frontend` | Layers 0–3; engine types and frontend traits only — engine behaviour is reached through Layer 2 |
| 4 `main.rs`, `lib.rs` | anything |

`make architecture-lint` (`tools/architecture-lint.sh`) enforces these rules and runs in CI and in `make pre-push`. The component tools suite checks the lint against private fixture trees and scanner failures.

---

## Workspace layout

| Path | What it is |
|---|---|
| `src/` | The `awman` library crate: the four layers. `src/lib.rs` is `#![forbid(unsafe_code)]`. |
| `src/main.rs` | The `awman` binary entry point (built by the `awman-cli` package). |
| `crates/awman-cli/` | Builds the `awman` binary and holds the safe wiring between native host integrations and the engines (on macOS, the XPC transport for the Apple image store). Also registers the integration test suites. |
| `crates/awman-host-shims/` | Binary-side native integrations for the builtin runtime: the embedded VM worker entry, kernel preparation and SDK version metadata. Unsafe code is confined here, outside the library. |
| `third_party/` | Vendored dependencies. |
| `tests/` | Integration and end-to-end suites. |
| `tools/`, `scripts/` | Lint, release and measurement scripts. |
| `templates/`, `skills/` | Agent Dockerfile templates, the agent-definition template and bundled skills, embedded in the binary. |

The builtin microVM runtime is compiled only with the `builtin-runtime` Cargo feature.

---

## Layer 0: data (`src/data/`)

Plain types and storage. Nothing here spawns a process, opens a network socket, calls `git` or touches a container.

- **`Session`** (`session.rs`) — the ruling type for every operation: working directory, resolved Git root, loaded configs and runtime state. The CLI opens one per invocation, the TUI one per tab, the API one per API session. `SessionManager` (`session_manager.rs`) is the concurrent collection, with an optional `SessionStore` for persistence.
- **Configuration** (`config/`) — `RepoConfig` (`<git-root>/.awman/config.json`), `GlobalConfig` (`~/.awman/config.json`, or `$AWMAN_CONFIG_HOME`), `EnvSnapshot` (every environment variable awman reads, captured once), `FlagConfig`, and `EffectiveConfig`, the one place precedence is applied (flags, then env, then repo, then global, then built-in default). Also here: builtin-runtime settings, runtime selection, image sources, custom agent definitions and config migrations.
- **Workflows** — `workflow_definition.rs` (parsing), `workflow_dag.rs` (references and cycle detection), `workflow_state.rs` and `workflow_state_store.rs` (state persisted to `<git-root>/.awman/workflows/<repohash8>-[<wi>-]<name>.json`).
- **Filesystem and database** (`fs/`) — a typed object for every path family awman owns: API paths and the SQLite store shared by the API server and squad (`api_db.rs`, `awman.db`), skill, workflow, agent and context directories, auth paths, daemon PID/env files, squad task storage, prerequisite grants, logs.
- **Shared shapes** — `message.rs` (`UserMessage`, `UserMessageSink`, the supertrait of every frontend trait), `prompt.rs` (`Prompt<D>`/`Choice<D>`, the shape of every interactive question), `step_status.rs`, `ready_phase.rs` and `ready_summary.rs`, execution and setup events.
- **`DataError`** (`error.rs`) — every Layer 0 failure.

---

## Layer 1: engine (`src/engine/`)

Reusable runtime mechanics. Engines do real I/O against Docker, git and the filesystem, but never print: user-visible output flows through `UserMessageSink` or the engine's frontend trait. No `clap`, `crossterm`, `ratatui` or PTY handling.

| Module | Owns |
|---|---|
| `agent_runtime` | The cross-runtime contract: `AgentRuntimeEngine`, `AgentExecution`, `AgentFrontend`, runtime capabilities, and `detect_effective`, which selects the runtime from config (`docker`, `apple-containers`, `builtin-experimental`, `docker-sbx-experimental`). `conformance` checks every runtime against the contract. |
| `container` | `ContainerRuntime`, the container tier. Backends (`docker.rs`, `apple.rs`, `builtin/`) are private to the module behind the `ContainerBackend` trait; `host_cli_backend.rs` is shared by the CLI-driven ones. |
| `container/builtin` | The embedded microVM backend: driver, exec bridge, image catalog, networking, resource limits, state paths. |
| `sandbox` | `SandboxRuntime`, the sandbox tier (Docker Sandboxes). |
| `oci`, `image_prep` | Image acquisition and verification (registries, Docker Engine export, Apple store, archives) and source-matched image builds for the builtin runtime. |
| `agent` | `AgentRegistry`: first-party agents (`first_party.rs`, the only place in the engine that branches on an agent name) and custom TOML definitions (`custom.rs`). |
| `workflow` | `WorkflowEngine`: step ordering, parallel groups, yolo countdowns, stuck detection, setup/teardown steps (including `poll_ci`), state persistence. Agents arrive through an `AgentExecutionFactory`; worktree lifecycle is left to Layer 2. |
| `git` | `GitEngine`: Git root resolution, worktrees, merges, pushes, diff summaries. |
| `overlay` | `OverlayEngine`: resolved mounts, env, skills, agent settings, credential files. |
| `auth`, `credential_refresh` | API keys, TLS, keychain credentials, secret redaction; live credential leases. |
| `ready`, `init` | The multi-phase `ReadyEngine` and `InitEngine`. `ready/host_agent.rs` (`HostAgentPinger`) is the only code allowed to run an agent binary on the host. |
| `squad`, `daemon` | The squad daemon engine, scheduler, evaluator and supervisor; daemon process supervision. |
| `remote` | Shared HTTP transport and typed clients for the API and squad daemons. |
| `acp`, `agent_output` | Agent Client Protocol sessions; structured agent output translation. |
| `issue`, `aspec` | Issue providers; aspec template retrieval. |

All Layer 1 failures are `EngineError` (`error.rs`), which wraps `DataError`.

---

## Layer 2: command (`src/command/`)

All business logic. Commands call engines, read and write Layer 0, and talk to the user only through their frontend trait.

- **`CommandCatalogue`** (`dispatch/catalogue/`) — the single definition of every command, subcommand, flag and argument: kinds, defaults, implications, conflicts and which frontends see it. Everything else is a projection of it (`dispatch/projections/`):
  - `clap.rs` — the CLI parser;
  - `raw_args.rs` — the parser for the TUI command box and API `args` arrays;
  - `tui_hints.rs` — command-box hints and completions;
  - `markdown.rs` — [`docs/14-command-reference.md`](14-command-reference.md). A test fails when the committed file is stale; `make docs-reference` regenerates it.

  A parity test runs every catalogue command through both the clap and raw-args projections and requires identical results.
- **`Dispatch`** (`dispatch/mod.rs`) — routes parsed input to a typed command. `dispatch/build.rs` maps each runnable catalogue leaf to its command constructor. `ResolvedFlags` applies catalogue defaults and implications; `CallerContext` records which frontend and command path a command was built with.
- **`Engines`** (`dispatch/engines.rs`) — the one engine-assembly path: `Engines::build` for the CLI/TUI host, `Engines::for_daemon` for the API and squad daemons.
- **`Command` trait** (`commands/command_trait.rs`) — `run_with_frontend(self, frontend) -> Result<Outcome, CommandError>`. Each command (`commands/`: `chat`, `exec_prompt`, `exec_workflow/`, `init`, `ready`, `config`, `new/`, `specs`, `agents/`, `squad/`, `remote`, `api_server`, `clean`) declares its own frontend trait and typed outcome. Shared steps that ask the user — `worktree_lifecycle`, `mount_scope`, `agent_setup`, `agent_auth`, `agent_trust`, `workflow_prerequisites` — have their own frontend traits. `launch_policy.rs` decides how an agent is launched (which agent, stdio or ACP, context directories).
- **Answers and copy** — `headless.rs` (`HeadlessDefaults`: what a question is answered with when nobody is there to ask, per frontend) and `prompts.rs` (the text of every prompt).
- **Startup and sessions** — `startup.rs` (`Startup`: migrations, config load, Git root, `Session`, engines), `session_create.rs` and `session_setup.rs` (API session validation and setup).
- **Daemon bootstrap** — `command/daemon_runtime` owns one daemon process, database and executor for squad, worker push/pull and the singleton engine. Frontends serve the shared local and network routes.
- **`CommandError`** (`error.rs`) wraps `EngineError` and `DataError`.

---

## Layer 3: frontend (`src/frontend/`)

Each frontend implements the per-command frontend traits and renders outcomes. None of them makes a business decision.

- **CLI** (`cli/`) — `CliFrontend` (`command_frontend.rs`) and its per-command implementations (`per_command/`). Messages that arrive while an agent owns the terminal are queued and replayed afterwards (`user_message.rs`). `parallel.rs` draws the multi-agent chrome for parallel workflow groups.
- **TUI** (`tui/`) — Ratatui. `app.rs` (central state), `tabs.rs` and `tabs/` (per-tab state), `event_loop.rs`, `key_handler.rs` and `keymap.rs` (the full shortcut map), `mouse_handler.rs`, `command_box.rs` and `hints.rs`, `dialogs/` and `dialog_router.rs`, `pty.rs`, `render.rs` and `render/`, `container_view.rs`, `workflow_view.rs` (Workflow Overview), `acp_view.rs`, the squad tab and indicator, and `per_command/`.
- **API** (`api/`) — an Axum HTTP server (TLS via rustls) that runs commands in-process through `Dispatch`. `routes.rs` defines the routes, `command_frontend.rs` (`ApiDispatchFrontend`) implements every per-command frontend trait, `serve.rs` holds the listener primitives shared with squad, and `error.rs` the stable JSON error envelope.
- **squad** (`squad/`) — the squad daemon's HTTP router and listener, serving the `squad` part of the catalogue. Its unattended frontend takes its answers from `HeadlessDefaults::squad`.

`render_helpers.rs` holds presentation helpers shared across frontends.

---

## Layer 4: binary (`src/main.rs`)

`main.rs` contains no business logic:

1. If the process was started as the builtin runtime's VM worker, hand off to `awman-host-shims` and exit.
2. Intercept retired flags with a migration hint.
3. Build clap from `CommandCatalogue` (no command or flag name is hard-coded) and parse argv.
4. Run `Startup` to open the `Session` and build `Engines`, and package them in a `RuntimeContext`.
5. Route: no subcommand opens the TUI; bare `awman squad` on a terminal opens the TUI on the squad tab; anything else runs in the CLI.

The shared daemon starts from `awman daemon start`; its worker, squad, and engine features run in that process.

---

[← Custom Agents](15-custom-agents.md) · [Back to contents](contents.md)
