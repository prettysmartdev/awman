# Work Item: Feature

Title: Complete singleton daemon engine with TUI and CLI frontends
Issue: issuelink

Depends on: 0139 (config and API surface), 0140 (single daemon and local API), 0141 (terminal hub, interaction, status and host-terminal hygiene). Engine host and frontend protocol are one indivisible acceptance gate.

## Summary:
- Host awman instances in the one daemon and make the TUI and CLI use them through the frontend protocol. Ship the engine host, local terminal subscriptions, dialog relay, hand-off, environment isolation and builtin/daemon parity together.
- This item is complete only when a user can run the TUI and CLI against the daemon with the same behavior as the builtin engine.

## Part: Singleton engine host

### Summary:
- Today every `awman` TUI process builds its own awman instance in-process. `tui::run()` creates an `App` that holds:
  - an in-memory `SessionManager` (`src/data/session_manager.rs`);
  - one `Tab` per session with its own command thread and channels;
  - the git poll;
  - the `SquadIndicatorPoller`;
  - container slots fed by `ContainerLifecycle` (`src/frontend/tui/tabs.rs`).

  The CLI does the same for each command. Nothing outside that process can drive or present its work.
- **Add `engine`, the third feature of the 0140 daemon:** a singleton awman instance host that runs this same non-UI half of awman **once, in the daemon**, and drives any number of **frontends**: one or more TUI instances, the Rex integration (0154), and any future awman frontend (a web UI, an IDE extension…). Frontends speak the frontend protocol (0142) over the daemon's local listener.
- **Acceptance gate:** a developer can start the single daemon, select `tui.backend=daemon`, run the TUI and CLI with builtin parity, and return to the builtin backend. The engine host, frontend protocol, local terminal subscriptions and hand-off all ship together. A host with no usable frontend does not complete this item.
- **Engine terminal transport:** register engine PTYs with 0141's shared terminal hub and consume its local `awman.terminal.v1` subscriptions, including snapshot/reconnect, resize, exit, input ownership and host-TTY filtering. There is one terminal protocol and one hub for every daemon feature.
- **Instance model, unchanged from today.**
  - The engine hosts **instances**. An instance is exactly what one TUI process holds in-process today.
  - Typing `awman` creates a new instance. Two `awman`s are two independent instances, exactly as today.
  - An instance has one **primary** frontend (the process the user started) and any number of **secondary** frontends (for example, Rex tab windows running `awman view --instance <id> --tab <session-id>`).
- **The parity contract is the point.** A TUI or CLI on the daemon `engine` must be **indistinguishable** from one on the builtin in-process engine. That covers keys, dialogs, commands, output, errors, exit codes, environment, config resolution and lifecycle.
  - A conformance suite runs the existing TUI and CLI behaviour tests against both backends and requires identical outcomes.
  - The builtin engine stays the default outside host integrations, and stays fully supported.
- Each instance runs its host-side operations with the **launching frontend's environment, cwd and argv**, never the daemon's. So `PATH`, Docker context, SSH agent, credential helpers and git config are the user's shell's, just as in-process.

### User Stories

#### User Story 1:
As a: user

I want to:
run awman's TUI with the daemon `engine` as its backend and notice no difference at all from the normal TUI

So I can:
get host integrations such as Rex (0154) and future frontends without relearning anything or losing any behaviour.

#### User Story 2:
As a: frontend author (Rex integration, a future web or IDE frontend)

I want to:
attach to an awman instance hosted by one long-lived engine, receive its state and dialogs, and claim its window-management requests

So I can:
present awman natively in my host without re-implementing sessions, workflows, dialogs or container lifecycle.

#### User Story 3:
As a: security-conscious user

I want to:
know that `engine` is reachable only from my own user on this machine unless I explicitly expose it, and that my shell environment is never written to disk or forwarded to containers

So I can:
enable it, or let a host integration enable it ephemerally, without widening awman's attack surface.


### Implementation Details:
- Advertise `engine_instances` and `frontend_protocol` in `capabilities` in `GET /v1/status` when the engine instance routes and frontend WebSocket contract ship. WI0155 adds a separate flag for its later `focus_work` request.

#### 1. The feature
- `EngineFeature` implements 0140's Layer 1 `DaemonFeature` lifecycle (`start(&Core)`, `drain()`, `stop_now()`, `status()`) in `src/engine/daemon/features/engine.rs`. Layer 2's `EngineInstanceHost` owns sessions, Dispatch, command threads, dialogs, admission and exit policy. Layer 3 maps the engine API and frontend WebSocket to typed Layer 2 operations; neither `EngineFeature` nor its primitives construct routes.
- **Routes.** These are the `engine` rows of 0139 §5. They follow its conventions: `ins_`/`tab_` ID prefixes, `{items, next_cursor}` lists, `{error, code}` errors and `snake_case` fields.

  | Route | Purpose |
  |---|---|
  | `POST /v1/engine/instances` | Create an instance. Returns `{id: "ins_…"}`. |
  | `GET /v1/engine/instances?annotation=k=v` | List instances (repeatable filter): `id`, `primary_frontend_kind`, `annotations`, `tab_count`, `running_jobs`, `created_at`. The filter is how integrations find "the instance for this window". |
  | `GET /v1/engine/instances/{id}` | One instance, with its frontends. |
  | `DELETE /v1/engine/instances/{id}` | Quit, with today's exit policy. |
  | `PATCH /v1/engine/instances/{id}` | `{annotations}` only. |
  | `GET /v1/engine/instances/{id}/frontend` | WebSocket `awman.frontend.v1` (0142). |
  | `GET /v1/engine/instances/{id}/tabs`, `GET\|PATCH /v1/engine/instances/{id}/tabs/{id}` | Tabs with their session, container slots and annotations. `PATCH` accepts `{annotations}` only. |
  | `GET /v1/engine/instances/{id}/dialogs`, `POST /v1/engine/instances/{id}/dialogs/{id}/answer` | Pending dialogs, and answering one with `{response}`. The first answer wins and every frontend gets `DialogClosed{answered_by}`, exactly as when a frontend answers. |
  | `POST /v1/engine/instances/{id}/ui_requests` | `{request: "open_overview" \| "focus_workflow_window" \| …}`, the same `UiRequest` enum as the frontend protocol (0142). It is routed to the claiming host presenter, or handled by the primary frontend. |

  - Agent terminals of engine sessions are the core `GET /v1/terminals?owner_id=ins_…` and `GET /v1/terminals/{id}` (0141 hub and 0139 route contract). There is no engine-specific terminal route.
  - There is **no `actions` route**. Friendly names such as `instances.list`, `dialogs.answer`, `steps.approve` and `overview.open` are client-side `awman daemon api` actions over these routes (0139 §6).
- **Local listener only by default.** The `/v1/engine/*` routes are served on the network listener only when `engine` is **persistently** enabled *and* listed in `daemon.network.features`. Even then they are explicitly out of scope until a remote frontend has its own security review. The docs say so.
- **Config** (scope `global`, rows in the 0139 key table):
  ```json
  "daemon": { "features": { "engine": { "enabled": false, "allowEphemeral": true, "idleTimeout": "15m", "maxConcurrentJobs": 4 } } }
  ```
  `awman config set daemon.features.engine.enabled true` turns it on persistently and applies live to a running daemon (0139 §4).
- **Ephemeral enablement** (0140): a local frontend's hand-off (0142) may send `PATCH /v1/features/engine {enabled: true, ephemeral: true}`, a core route on the local listener.
  - The ephemeral engine is never served on the network listener.
  - It disables itself after `idleTimeout` with no frontends, no instances, no running engine work and no presenter hold. A host presenter may hold the engine while it presents other features' work (0155). If no feature is left, the daemon exits.
  - `allowEphemeral: false` forbids it, and hand-offs then fall back to builtin under `auto` (0142).
  - `awman daemon status` and `GET /v1/status` show the feature state `ephemeral` (with the reason, e.g. `rex`), `on`, `off` or `draining`.
- **Executor share.** Engine sessions' commands go through the shared executor under `daemon.features.engine.maxConcurrentJobs`, inside the host-wide `daemon.maxConcurrentJobs`, with round-robin claims across features, so a flood of `worker` jobs can't starve interactive sessions and vice versa.
- **Interaction.** Engine sessions are always `interactive`: agents run in a PTY at the frontend's terminal size, and dialogs are relayed to frontends in Part B instead of taking yolo answers. Yolo sessions keep their countdowns, exactly as in-process. The per-feature interaction settings in 0141 are independent of this behavior.

#### 2. Instance model and lifecycle (parity)
- `EngineInstance` (`src/command/instance/mod.rs`) owns the command-side state that `App` owns today, minus rendering and input:
  - `SessionManager`;
  - the ordered tab list, each with its session, `execution_phase`, `TabSharedState`, command thread and dialog channels;
  - container slots and their `ContainerLifecycle` / `ContainerSlotEvent` queue;
  - per-tab git poll;
  - `SquadIndicatorPoller`;
  - yolo and stuck state;
  - the active-tab index as a *hint* (each frontend keeps its own focus).
- **Creation.** `POST /v1/engine/instances` is followed by the primary frontend's `Hello` (0142), which supplies `env`, `cwd`, `argv` and `term_size`. The initial tab is opened exactly as `tui::run()` does today (`InitialTab::Normal | Squad`, then auto-spawning `ready` or `agents status --watch`).
- **Frontends.**
  - Exactly one **primary** per instance.
  - **Secondaries** attach to an existing instance (`role: Secondary`) and may render one tab (`awman view --instance <id> --tab <session-id>`) or all of them.
  - A second `Primary` hello for the same instance is refused (`409 primary_exists`).
- **Lifecycle mapping, identical to today:**
  - Quit from any frontend (`Quit`, after `QuitConfirm` as today) → today's TUI-exit policy for the whole instance. All frontends get `Detach{reason: "quit"}`.
  - The primary frontend disconnects without `Quit` (killed or hung up) → treated as today's TUI process being killed: the same exit policy, with no prompt. A short grace period (2 s) covers a reconnect after a transient socket error (0142), so parity holds for genuine exits only.
  - A secondary frontend disconnects → nothing happens to the instance. Host presenters may map it to a tab action (0154 maps closing a Rex tab window to close-tab).
  - Close tab → today's close-tab semantics (cancel the running command, end the session). Closing the last tab quits the instance, as today.
  - `Ctrl-\` detach containers → identical to today.
- **Nothing survives that wouldn't survive in-process**, and nothing is killed that wouldn't be. There is no "detached instance" state.

#### 3. Per-instance environment, cwd, argv and config
- `Hello.env` (the frontend's full environment), `cwd` and `argv` are kept **in memory only**, in the instance, as an `EnvSnapshot` (`src/data/config/env.rs`).
- Every host-side spawn for that instance goes through the instance's `EnvSnapshot` instead of the daemon process environment: docker, container, sbx, git, gh, credential helpers, host secret commands (`src/engine/auth/host_secret_command.rs`), editor launches, and mount-scope checks.
  - This needs an audit of every `std::env::var` / `Command::new` on the session path. Route them through `EnvSnapshot` and `host_cli::program` (`src/engine/host_cli.rs`), adding explicit `env_clear().envs(snapshot)` where a process is spawned.
  - A lint-style test (grep in CI) forbids new direct `std::env` reads under `src/engine` and `src/command` outside an allow-list.
- Container environment rules are **unchanged**. Containers get exactly what they get today from an in-process TUI with the same environment, plus the 0141 deny-list.
- **Config** is resolved per session exactly as in-process, with 0139's precedence: default → global (`$AWMAN_HOME/config.json`, or the XDG/`HOME` location, taken from the instance's env) → repo `GITROOT/.awman/config.json` → `AWMAN_*` variables from the instance's env → `--set` from its `argv`. Repo-scope `tui.*` keys resolve against each tab's repo. Config reload behaviour matches today.
- **Docker context is per instance.** Two instances launched from shells with different `DOCKER_HOST`/`DOCKER_CONTEXT` each talk to their own Docker, as two TUIs do today. Runtime handles in the shared core are keyed by the resolved runtime endpoint, so the core may hold several.

#### 4. Annotations, the integration registry, and host presenters
- **Annotations** are the only way integrations attach metadata. No integration-specific type appears in the protocol, the store or `src/data/`.
  - The type is `Annotations` (`src/data/annotations.rs`), a `BTreeMap<String, String>` with keys namespaced by integration (`rex/session`, `rex/block`, `rex/window`).
  - Keys are ≤63 characters of `[a-z0-9./-]`. Values are ≤256 bytes with no control characters. There are at most 32 entries per object. Anything else is rejected with `400 invalid_annotation`.
  - They live on:
    - the frontend connection, from `Hello.annotations` (0142);
    - the instance;
    - each tab;
    - each terminal subscription, from `awman attach --annotation k=v` (0141) and `awman view --annotation k=v` (0142).
  - They are listed by `awman daemon api instances.list annotation.<k>=<v> --json` (client-side, 0139 §6), which calls `GET /v1/engine/instances?annotation=k=v`. Presenters set them through their engine hook, and API clients through `PATCH` on the instance or tab.
  - They are kept in memory with the instance and dropped when it ends.
- **Integration registry** (`src/command/integrations/mod.rs`). This Layer 2 registry is the compiled-in list of integration descriptors and the source of hand-off policy. Each one provides:
  - `name()` (e.g. `"rex"`), which is also its annotation namespace and its `tui.integrations.<name>` config key;
  - a frontend-side `detect(env, config) -> HostRequirement` (`None | Optional(Annotations) | Required(Annotations)`), called by 0142's hand-off;
  - an optional `setup` / `remove` / `status`, used by the generic `awman integration …` command (0156);
  - an optional Layer 3 `HostPresenter` adapter registered through the Layer 2 interface below.
- `trait HostPresenter: Send + Sync` is defined by Layer 2 and implemented by Layer 3 host adapters:
  - `fn name(&self) -> &'static str`. A presenter **claims every frontend whose `Hello.annotations` contain keys in its namespace**.
  - `fn on_ui_request(&self, instance, frontend, req: UiRequest) -> UiOutcome`.
  - `fn on_instance_event(&self, instance, ev: InstanceEvent)`, for tab opened/closed, container-window state changed, terminal opened/closed/detached (with the subscription's annotations and the reason, for example `hangup`), program status and instance exit.
  - Through `PresenterHandle`, it can set or clear annotations **in its own namespace only** on instances, tabs and terminals. For example, Rex records `rex/window` on a tab.
- The Layer 2 instance host routes a frontend's `UiRequest`s to the presenter that claims it. If none does, it replies `UiAck{claimed_by_host: false}`, and the frontend handles the request natively (a tab switch in its own tab bar). That means a frontend with no host behaves exactly like today's TUI.
- Presenters are Layer 3 tasks supervised separately from the Layer 1 feature. A presenter panicking or erroring never affects the instance: its requests fall back to `claimed_by_host: false`, and a single `Notice` goes out.

#### 5. Splitting `App` into UI state and instance state (refactor plan)
1. **Identify** `App` fields (`src/frontend/tui/app.rs`) and `Tab` fields (`src/frontend/tui/tabs.rs:789`) that are UI-only, and keep them in the frontend:
   - focus and `active_dialog` rendering state;
   - the command-box editor;
   - scroll state;
   - the vt100 parser (fed by the hub);
   - mouse state;
   - `container_window_state` (Hidden/Minimized/Maximized), as a view choice reported to the engine for presenters.
2. **Move** command and instance behavior behind an `InstanceBackend` trait defined in Layer 2 (`src/command/instance/backend.rs`) with two Layer 2 implementations:
   - `LocalInstanceBackend`, which builds `EngineInstance` in-process (today's behaviour, the builtin engine);
   - `RemoteInstanceBackend` (0142), which talks to the daemon. The TUI holds only this Layer 2 facade and never calls a Layer 1 engine directly.
3. `Tab` keeps its existing channel types (`command_result_rx`, `dialog_request_rx`, `dialog_response_tx`, `TabSharedState`, `ContainerSlotEvent`). Only the producer side moves, so most TUI code is untouched.
4. `tui::run()` picks a backend from 0142's hand-off result and nothing else changes.

This order keeps every step shippable: step 2 with only `LocalInstanceBackend` is a pure refactor, verified by the existing test suite.

#### 6. Parity contract and conformance suite
- **Contract.** For any input sequence (keys, mouse, resize, CLI argv and stdin), the daemon engine must produce the same engine-side outcomes as the builtin engine:
  - sessions created;
  - commands run, with their arguments;
  - dialogs raised, their order and content;
  - container lifecycle;
  - git and worktree effects;
  - stdout, stderr and exit codes.

  The rendered TUI frames must also be identical.
- **Allowed differences:** none in the TUI or CLI. `awman daemon status` and `awman daemon api instances.list` show engine state. Host presenters may add host-native presentation (0154), but never change outcomes.
- **Suite** (`tests/engine_parity/`):
  - Run the existing TUI tests (`src/frontend/tui/tests/*`, `tabs/tests.rs`) and CLI end-to-end tests twice, with a backend switch (`AWMAN_TEST_BACKEND=builtin|daemon`).
  - The daemon run starts an in-process daemon core on a temp `<data>` with a Unix socket.
  - Fakes are shared: fake runtimes and fake agents.
  - Assertions compare recorded engine-side outcome traces and frame snapshots between the two runs.
- **Latency budget:** under 5 ms added per command or dialog round trip on the local listener (p95, measured in the suite). Rendering never waits on the socket.

#### 7. Instance listing
- `awman daemon api instances.list [annotation.<k>=<v>]... [--json]` shows instances, their tabs, frontends and annotations. It is a client-side action (0139 §6) over the read-only local route `GET /v1/engine/instances`, and `instances.get id=ins_…` maps to `GET /v1/engine/instances/{id}`. The instance count also appears in `awman daemon status`.
- `awman agents status` / `list` include engine-instance agents exactly as they include in-process ones today.

#### 8. Security
- `/v1/engine/*` is served on the **local listener only** by default (0140): an owner-only Unix socket with a peer-UID check or an owner-only Windows named pipe with a caller identity check.
- `Hello.env` may contain secrets (tokens in env vars).
  - It travels only over the authenticated local socket or named pipe.
  - It is held in memory, never logged (its `Debug` is redacted) and never written to the store, events or crash reports.
  - It is dropped when the instance ends.
- `SecretInput` dialog responses (`SecretString`) cross the socket under the same rules and stay redacted in every log.
- Containers receive nothing beyond today's rules and the 0141 deny-list. The daemon socket, named pipe and endpoint sidecar are never mounted into containers.
- An ephemeral engine is local-only, whatever `network.features` says.

### Edge Case Considerations:
- **The daemon crashes:** every frontend loses its backend. The TUI shows today's fatal-error dialog (as if the engine thread panicked) and exits with today's code. Containers follow today's crash behaviour. On restart, the engine does not resurrect instances; there is no detached state.
- **A frontend crashes:**
  - The primary → today's killed-TUI exit policy after the 2 s grace period.
  - A secondary → no effect on the instance.
- **Two primaries for one instance:** the second gets `409 primary_exists`. A new `awman` always creates a new instance, so this only happens with a misused hidden entry point.
- **Env with secrets:** handled as in §8. Tests assert that secrets never appear in logs or the store.
- **Docker context differs between instances:** each instance uses its own (§3). A runtime handle cache keyed by endpoint avoids rebuilding clients.
- **Squad tab in the engine:** the `InitialTab::Squad` and squad-tab views read the daemon store directly, as today. When the daemon itself has `squad` enabled, the instance's squad view and the `squad` feature share the store with no extra IPC.
- **Daemon upgraded while instances run:** handled by 0142's version handshake. Running instances keep the old daemon until they quit.
- **`engine` disabled while instances run** (`awman config set daemon.features.engine.enabled false`): this drains, and running commands finish. Frontends get `Detach{reason: "engine disabled"}` and the TUI shows the same fatal-style notice as a crash, so a user never keeps working on a half-dead engine. Cancelling immediately is the explicit `awman daemon api jobs.cancel feature=engine` (0139 §4).
- **Host-side interactive subprocesses** (`DialogRequest::TerminalRelease`/`TerminalRestore`, used when awman hands the terminal to a host process such as an auth flow): the process runs in the daemon in a PTY with the instance's env, and its bytes are relayed to the frontend's terminal through the local hub transport above. The behaviour the user sees is the same as today. See 0142.
- **`idleTimeout` while a frontend is reconnecting:** the grace period counts as "has frontends".

### Test Considerations:
- The parity suite (§6) is the acceptance gate for this item and for 0142.
- A local engine terminal reconnects with a screen snapshot; resize, input ownership, host-TTY filtering and exit behavior match the builtin path through 0141's hub. The network terminal behavior is already covered by 0141.
- Unit:
  - `EngineInstance` lifecycle state machine (quit, primary loss with grace, secondary loss, close last tab);
  - primary/secondary admission;
  - ephemeral idle timing, including reconnect grace;
  - annotation validation (key grammar, size and count limits, namespace ownership: a presenter can't write another namespace);
  - `HostPresenter` routing by annotation namespace, with fallback to `claimed_by_host: false` when the presenter errors.
- `EnvSnapshot` routing: run a fake `docker`/`git` on a per-instance `PATH` through `host_cli::program`, and check that two instances with different `PATH`/`DOCKER_HOST` each hit their own fake. **Never mutate the process-wide `PATH`** (per the recent CI fix).
- Security:
  - the engine routes refuse network access by default and reject wrong local peer identities on Unix and Windows;
  - an ephemeral engine is never on the network listener;
  - an env containing `GITHUB_TOKEN=secret` never appears in logs, events, the store or crash output.
- Refactor safety: after step 2 of §5 (`LocalInstanceBackend` only), the full existing suite passes unchanged.

### Codebase Integration:
- Follow `aspec/architecture/2026-grand-architecture.md`: Layer 1 supplies reusable runtime mechanics; Layer 2 owns instances, Dispatch and command lifecycle; Layer 3 owns TUI/CLI/API/WebSocket presentation; Layer 0 owns exchangeable frames. Builtin and daemon frontends use the same Layer 2 contracts.
- follow established conventions, best practices, testing, and architecture patterns from the project's aspec.
- **Layer 0** `src/data/annotations.rs`, plus serializable instance snapshots, dialog and event types that cross the protocol. Do not move the existing `TabSharedState` wholesale into Layer 0: it contains command handles and UI feeds. Split it into Layer 0 wire data, Layer 2 command state and Layer 3 view state.
- **Layer 1** `src/engine/daemon/features/engine.rs` and reusable runtime/terminal primitives only. It cannot import `Dispatch`, TUI types, an HTTP router or a presenter.
- **Layer 2** `src/command/instance/` (`EngineInstance`, `EngineInstanceHost`, backend trait, local and remote implementations, lifecycle and command channels) and `src/command/integrations/mod.rs` (registry and `HostPresenter` trait).
- **Layer 3** `src/frontend/tui/{app,tabs}.rs` keeps rendering, input and UI-only state; `src/frontend/api/` adapts typed Layer 2 engine operations to HTTP and WebSocket frames.
- `src/data/session_manager.rs` is unchanged and simply lives in the instance.
- Every tab points to a Layer 0 `Session`; `EngineInstanceHost` orchestrates its Layer 0 `SessionManager` rather than maintaining a parallel TUI/API session type. Every command, including relayed CLI commands, is built through Layer 2 Dispatch with that session.
- Preserve the observable behavior of `ContainerLifecycle` / `ContainerSlotEvent`, `TabSharedState` and the command-thread spawn in `App::spawn_command`, but split them by ownership. The command-thread spawn and cancellation move to Layer 2; presentation-only feeds and terminal parsing stay in Layer 3; protocol-facing snapshots/events live in Layer 0. No lower layer imports `src/frontend/tui`.
- Coordinate with 0129's architecture findings: land its daemon and API read-model cleanups (A01–A03) first, so `engine` builds on them.
- Catalogue: no new command. This item adds the `instances.list|get`, `dialogs.list|answer` and `ui.request` rows to the client-side action table of `awman daemon api` (`src/command/commands/daemon/actions.rs`, 0139 §6). 0156 adds the `steps.approve` and `overview.open` compositions. The server gains no action registry. Engine-served commands use `FrontendKind::Daemon { feature: Engine, listener: Local }` (0140).

### Documentation

After implementation is complete, update user-facing documentation in `docs/` to reflect the current state of the tool:

- **Update existing feature docs** (e.g., if implementing headless features, update `docs/08-headless-mode.md`)
- **Create new user guides only if a new user-visible feature warrants it** (e.g., `docs/10-my-feature.md`)
- **Never create work-item-specific docs** (e.g., no "WI 0123 implementation guide" in published docs)
- **Keep all technical/implementation details in work item specs or code comments**, not in `docs/`
- **Docs are for end users**, not for developers trying to understand implementation

See `CLAUDE.md` for more guidance on documentation standards.

Specifically for this item:
- The daemon guide (`docs/16-daemon.md`, from 0140): the `engine` feature, what it is for, persistent versus ephemeral, `daemon.features.engine.*` (`idleTimeout`, `allowEphemeral`, `maxConcurrentJobs`), `awman daemon api instances.list`, turning it on with `awman config set daemon.features.engine.enabled true`, and the fact that it behaves exactly like the normal TUI.
- `docs/04-security-and-isolation.md`: engine is local-only, and how the shell environment is handled.
- `docs/14-command-reference.md`: the `instances.*`, `dialogs.*` and `ui.request` actions of `awman daemon api`.
- `docs/09-api-and-remote-mode.md`: the `/v1/engine/*` routes in the 0139 route reference.
- New `docs/19-engine-and-frontends.md` (shared with 0142): running TUIs on the daemon engine, instances and frontends, and how integrations use annotations.

## Part: Frontend protocol and TUI/CLI hand-off

### Summary:
- 0142 hosts awman instances in the daemon's `engine` feature. This item defines how a TUI or CLI process becomes a **frontend** of such an instance, and when it does so.
- **Protocol `awman.frontend.v1`.** It is a WebSocket on `/v1/engine/instances/{id}/frontend`, on the daemon's local listener. It serialises the channels a `Tab` already uses to talk to its command thread (`command_result_rx`, `dialog_request_rx`, `dialog_response_tx`, `TabSharedState`, the `ContainerSlotEvent` queue). The TUI keeps its rendering and input code; only the producer side of those channels moves.
- **Hand-off.** Each invocation decides once, before dispatch, whether it runs on the **builtin** in-process engine or the **daemon** engine. The inputs are one config key, `tui.backend: "auto" | "daemon" | "builtin"` (overridable for one run with 0139's generic `--set tui.backend=…` or `AWMAN_TUI_BACKEND`), whether an enabled host integration requires the daemon (Rex, 0154), and each integration's `tui.integrations.<name>.enabled` (likewise overridable with `--set`). There are no dedicated flags.
  - Under `auto`, the daemon engine is used when a host integration requires it, or when `engine` is persistently enabled. Otherwise today's builtin engine is used.
  - Any failure under `auto` falls back to builtin with a one-line notice.
- **Parity.** A TUI or CLI frontend is indistinguishable from the builtin engine (0142's contract): the same keys, dialogs, output, prompts, Ctrl-C behaviour, exit codes and fatal-error handling.
- Agent terminal bytes travel over 0141's hub subscriptions, never over the frontend socket.

### User Stories

#### User Story 1:
As a: user

I want to:
run `awman`, `awman chat` or `awman exec workflow …` against the daemon engine and see exactly the same screens, prompts, output and exit codes as with the builtin engine

So I can:
use host integrations such as Rex without anything else changing, including in scripts.

#### User Story 2:
As a: user who upgrades awman while a daemon is running

I want to:
have the new binary quietly restart an idle daemon, or ask me before restarting one with running work

So I can:
upgrade without stale-daemon surprises or lost agents.

#### User Story 3:
As a: frontend author (Rex today, a web or IDE frontend later)

I want to:
a documented, versioned protocol with typed frames and a host-claim mechanism for window-management requests

So I can:
present awman natively without forking its TUI.


### Implementation Details:

#### 1. Hand-off — `src/command/engine_handoff.rs` (Layer 2)
- **Inputs:**
  - `tui.backend: "auto" | "daemon" | "builtin"`: default `"auto"`, scope `repo` (global and repo), one row in 0139's key table. For one run, use `--set tui.backend=builtin` or `AWMAN_TUI_BACKEND=builtin`; 0139 removes per-setting flags, so there is no `--engine`;
  - `tui.integrations.<name>.enabled: "auto" | "on" | "off"` for each registered integration, which `detect()` reads. For one run, use `--set tui.integrations.rex.enabled=off`. There is no integration-specific or integration-wide flag;
  - `HostRequirement` (`None | Optional(Annotations) | Required(Annotations)`), returned by each enabled integration's `detect()` in 0142's registry. Rex (0154) returns `Required` with its `rex/*` annotations when Rex is `Present`. Detected annotations from all integrations are merged into `Hello.annotations`; a namespace clash is a bug and is rejected.
- **Decision:**

  | `tui.backend` | Host requires daemon | `engine` persistently on | Result |
  |---|---|---|---|
  | `builtin` | any | any | builtin, and the host integration does not run |
  | `daemon` | any | any | daemon. It is started, or `engine` is enabled ephemerally, if needed. Failure → exit nonzero |
  | `auto` | yes | any | daemon, ephemeral if needed. Failure → builtin + notice |
  | `auto` | no | yes | daemon. Failure → builtin + notice |
  | `auto` | no | no | builtin (today) |
- **Steps to reach the daemon:**
  1. Read 0140's server-meta sidecar.
  2. If no daemon is running, start one in the background, as `awman daemon start --background` does, with `engine` enabled ephemerally (an internal local-only API call, not a CLI verb).
  3. If a daemon runs with `engine` off, `PATCH /v1/features/engine {enabled: true, ephemeral: true}` (a core route, local listener only, 0139). This is refused with `409 ephemeral_not_allowed` when `daemon.features.engine.allowEphemeral` is `false`.
- **Version handshake:** `GET /v1/status` returns `{build, protocols: {frontend: "awman.frontend.v1", terminal: "awman.terminal.v1"}, features: […]}` (0139). On a mismatch:
  - The daemon has no running work in any feature → stop it and start the new binary's daemon, silently.
  - It has running work → the TUI shows a `YesNo` dialog: "The awman daemon is from an older version and is running N jobs. Restart it now? Running work will be stopped." The CLI prints the same advice and exits 2 (or falls back to builtin under `auto`). A declined restart under `auto` falls back to builtin with a notice.
  - An incompatible daemon is never driven silently.
- **Fallback notice.** One line in the TUI status bar (CLI: stderr), for example `engine: unavailable: daemon failed to start: <data>/daemon/api.sock: permission denied`. When the daemon engine works, nothing is shown.
- `awman config show` shows `tui.backend` with its effective value and source, plus the resolved backend for this shell (`backend: daemon (engine ephemeral, rex) pid 1234` / `backend: builtin`).

#### 2. Protocol `awman.frontend.v1`
- **Transport.** `POST /v1/engine/instances` → `{id: "ins_…"}`, then a WebSocket upgrade on `/v1/engine/instances/{id}/frontend` with subprotocol `awman.frontend.v1`. Local listener only (0140/0142). Frames are JSON messages with `{type, …}` and a per-direction `seq`.
- **Frontend → engine:**
  - `Hello{kind: Tui|Cli, role: Primary|Secondary, annotations: Annotations, cwd, argv, env, term_size, stdin_is_tty, protocol}`. `annotations` is the generic integration metadata (0142 §4): detected annotations plus any given with the hidden entry point's `--annotation k=v`. There is no integration-specific field in any frame;
  - `OpenTab{dir, options}`;
  - `Submit{ParsedCommandBoxInput}`;
  - `DialogResponse{dialog_id, response}`;
  - `Cancel`;
  - `Resize{cols, rows}`;
  - `UiRequest{OpenTab{dir} | NextTab | PrevTab | CloseTab | SetContainerWindow{slot, state} | OpenOverview | FocusWorkflowWindow | FocusWork{target_id}}`. This is the same enum, in snake_case (`open_overview`, `focus_work`, …), that `POST /v1/engine/instances/{id}/ui_requests` accepts (0142, 0139). `FocusWork` names a squad task or worker job (`task_…`/`cmd_…`/`lease_…`) that a presenter may show (0155);
  - `Quit`.
- **Engine → frontend:**
  - `InstanceSnapshot` (tabs, sessions, phases, slots, git summaries, labels, colours, yolo and stuck state), then `InstanceDelta`s;
  - `CommandOutput` (message-sink lines, with stream and level);
  - `DialogRequest{dialog_id, request}`;
  - `DialogClosed{dialog_id, answered_by}`;
  - `ProgramStatus` (0141 `EffectiveStatus` per slot), preserving the builtin status behavior and the OSC states already implemented in 0141;
  - `Notice`;
  - `UiAck{request_seq, claimed_by_host: bool}`;
  - `Detach{reason}`.
- **The cut line.** On the engine side, each tab's command thread is unchanged. A bridge task reads its `command_result_rx`, `dialog_request_rx` and `ContainerSlotEvent` queue and turns them into frames. Inbound `DialogResponse`s are written to `dialog_response_tx`. `TabSharedState` changes are diffed into `InstanceDelta`s.
- **On the client side,** Layer 2's `RemoteInstanceBackend` owns transport, reconnect, sequence and command/event mapping. A Layer 3 `RemoteTabView` adapter (`src/frontend/tui/remote_tab.rs`) feeds the existing TUI channel/view shape; `Tab` and render code consume it unchanged. The adapter cannot select commands, resolve defaults or drive a Layer 1 engine.
- **Dialogs** move from `src/frontend/tui/dialogs/mod.rs` to `src/data/dialog.rs` with serde derives, and the TUI re-exports them. Audit of the variants:
  - plain data (`YesNo`, `YesNoCancel`, `TextInput`, `MultilineInput`, `ListPicker`, `KindSelect`, `QuitConfirm`, `CloseTabConfirm`, `WorkflowCancelConfirm`, `Loading`, `Custom`, `KeySetupNotice`) derive directly;
  - state structs (`WorkflowControlBoardState`, `WorkflowYoloCountdownState`, `WorkflowStepConfirmState`, `AgentSetupState`, `MountScopeState`, `AgentAuthState`, `ReviewState`, `ConfigShowState`, the squad states) must be checked for handles, `Sender`s and closures, which are replaced by IDs resolved engine-side;
  - `DialogResponse::Secret(SecretString)` is serialised only on the local socket, with redacted `Debug`, and never logged;
  - `DialogResponse::ConfigEdit(ConfigEditRequest)` is a typed request, applied engine-side with the instance's env.
- **Several frontends:** every frontend of an instance gets every `DialogRequest`. The first `DialogResponse` wins, and the others get `DialogClosed{answered_by}`. Focus and scroll are per frontend.
- **Agent terminal bytes** never use this socket. A frontend subscribes to the core `GET /v1/terminals/{id}` (`term_…`, WebSocket `awman.terminal.v1`; 0141 framing: snapshot, output, resize, exit, lagged), and its vt100 parser is fed from that. Keystrokes into a maximized agent use the hub's local input grant, which the instance's frontends hold by default, as with typing into today's maximized view.
- **`TerminalRelease`/`TerminalRestore`** (awman hands the terminal to a host process such as an auth flow): the engine runs that process in a PTY with the instance's env and exposes it as a hub terminal. The frontend leaves raw mode and alternate screen, bridges its real terminal to that hub terminal, and restores afterwards. This is the same sequence the user sees today.
- **UiRequest claiming:**
  - The frontend sends a `UiRequest` for every window-management transition: tab open, next, previous or close, container-window state change, overview, workflow window.
  - With `claimed_by_host: true`, a host presenter (0154) does it, and the frontend only updates its state. For example, the tab bar stays collapsed and the maximized slot renders as Minimized because Rex shows it.
  - With `false`, the frontend does exactly what today's TUI does.
  - Without a host, every request is unclaimed, so behaviour is identical to builtin.
- **Hidden entry point** `awman view --instance <id> --tab <session-id> [--annotation k=v]...`: a secondary TUI frontend rendering one tab of an existing instance. Its `Hello.annotations` are its own detected annotations plus the given ones, which is how a presenter matches the view to the window it opened. It is used by host presenters, marked internal in the catalogue, and absent from `--help` and the docs.

#### 3. CLI frontends
- `awman chat`, `exec prompt|workflow`, `new …`, `init` and `ready` create an instance with `kind: Cli` and submit their parsed command.
  - `CommandOutput` is written to stdout and stderr exactly as the CLI's message sink does today (`src/frontend/cli/user_message.rs`, `output.rs`).
  - Prompts use `src/frontend/cli/command_frontend.rs`'s existing renderers over relayed `DialogRequest`s.
  - Ctrl-C sends `Cancel`, with the same double-Ctrl-C semantics as today.
  - The process exits with the command's `CommandOutcome` exit code.
  - `--headless` and non-TTY stdin behave identically, because the engine sees the same argv and the frontend reports `stdin_is_tty` in `Hello`.
- `agents …`, `squad …` and `daemon …` already work against the daemon or store, and are unchanged.
- Engine-free commands (`--help`, `--version`, `config …`, `integration <status|setup|remove>` from 0156) stay local and never trigger the hand-off.

#### 4. Reconnect and engine loss
- A socket error triggers a reconnect with backoff (50 ms → 1 s, 2 s total, matching 0142's primary grace period), then `InstanceSnapshot` re-sync. The user sees nothing.
- If the engine is gone, or the instance ended or was refused, the TUI shows today's fatal-error dialog (`Dialog::FatalError`) with the reason and exits with today's fatal exit code. The CLI prints the error and exits as an in-process engine failure would. This is the same experience as an engine-thread panic in the builtin engine.

### Edge Case Considerations:
- **Daemon start races** (two `awman` invocations at once): 0140's startup lock serialises them. The second connects to the first's daemon.
- **A frontend's terminal is resized mid-dialog:** `Resize` goes to the engine for PTY sizing and to the hub. Dialog layout is local.
- **A huge `InstanceSnapshot`** (many tabs, long git summaries): frames are capped at 1 MiB, with chunked snapshots. Oversized or unknown frames → a protocol error that closes the connection and triggers the fatal path, never a panic.
- **A frontend with an older protocol than the daemon** (the user runs an old binary against a new daemon): handled by the handshake in reverse. The old frontend is refused with "run the matching awman version, or `--set tui.backend=builtin`".
- **Scripts:** `awman exec … --headless` in a pipeline produces byte-identical stdout and stderr and the same exit code on either backend. This is verified in tests.
- **The env contains secrets:** see 0142 §8. `Hello` is never logged.
- **`tui.backend: daemon` outside any host** (or `--set tui.backend=daemon` for one run): the daemon engine is used for a plain terminal, enabled ephemerally if needed. This is how users try it before setting `daemon.features.engine.enabled`.

### Test Considerations:
- Codec: round-trip every frame type and every `DialogRequest`/`DialogResponse` variant through serde, with property tests over generated values. Unknown, oversized and out-of-order `seq` frames are rejected cleanly.
- Hand-off decision table (unit, pure) over `tui.backend` (from config, `AWMAN_TUI_BACKEND` and `--set`) × `tui.integrations.<name>.enabled` × host requirement × daemon state (none, running with or without engine, ephemeral forbidden, version mismatch idle or busy). Also check that no `--engine`, `--integration` or `--no-integrations` flag exists (clap rejects them).
- Parity: 0142's conformance suite runs the TUI and CLI suites with `AWMAN_TEST_BACKEND=daemon` through this protocol and compares outcome traces and frames with builtin. CLI end-to-end tests compare stdout, stderr and exit codes byte for byte.
- Multi-frontend: two secondaries and one primary. A dialog reaches all three, the first answer wins, and the others get `DialogClosed`.
- `UiRequest` claiming: unclaimed → native behaviour, identical frames to builtin. Claimed (fake presenter) → state updated, no native window change.
- `TerminalRelease` flow with a fake host process through a hub terminal.
- Reconnect within grace → no visible change. Engine loss → `FatalError` frame snapshot equals the builtin panic path.
- Latency: the p95 round trip for `Submit` → first `CommandOutput`, and for `DialogRequest` → `DialogResponse`, is under 5 ms on the Unix socket.

### Codebase Integration:
- follow established conventions, best practices, testing, and architecture patterns from the project's aspec.
- **Layer 0:** `src/data/dialog.rs` (moved dialog types), `src/data/frontend_protocol.rs` (frames), `src/data/annotations.rs` (shared with 0142).
- **Layer 2:** `src/command/engine_handoff.rs`.
  - `src/command/dispatch/mod.rs` chooses a daemon-backed or local `CommandFrontend` from the hand-off result.
  - Add a `FrontendKind` variant for relayed frontends (`catalogue/mod.rs`; the match has no default arm, so every command declares whether it supports relay). Engine-free commands are marked local-only.
- **Layer 3:**
  - `src/frontend/tui/remote_tab.rs` (`RemoteTabView`, presentation adapter over the Layer 2 `RemoteInstanceBackend`).
  - `src/frontend/tui/mod.rs` (`tui::run()` picks the backend).
  - `src/frontend/tui/dialog_router.rs` (unchanged, now fed remotely).
  - `src/frontend/cli/command_frontend.rs` and `per_command/*` (relay adapters).
  - `src/frontend/tui/event_loop.rs` (terminal release and restore around relayed host processes).
- Catalogue: no new global flags (0139's `--set` covers `tui.backend` and integration switches). Add the `tui.backend` row to the key table (`src/data/config/fields.rs`, section struct `src/data/config/tui.rs`, shared with 0154 and 0141), and the hidden `view` command (`--instance ins_…`, `--tab tab_…`, `--annotation k=v`).
- Reuse 0141's `attach_protocol.rs` framing and terminal hub for local subscriptions; do not duplicate either.

### Documentation

After implementation is complete, update user-facing documentation in `docs/` to reflect the current state of the tool:

- **Update existing feature docs** (e.g., if implementing headless features, update `docs/08-headless-mode.md`)
- **Create new user guides only if a new user-visible feature warrants it** (e.g., `docs/10-my-feature.md`)
- **Never create work-item-specific docs** (e.g., no "WI 0123 implementation guide" in published docs)
- **Keep all technical/implementation details in work item specs or code comments**, not in `docs/`
- **Docs are for end users**, not for developers trying to understand implementation

See `CLAUDE.md` for more guidance on documentation standards.

Specifically for this item:
- `docs/07-configuration.md`: `tui.backend` (a generated row).
- New `docs/19-engine-and-frontends.md` (shared with 0142): choosing the builtin or daemon backend (`tui.backend`, `--set tui.backend=…`), upgrade behaviour, fallback notices, and switching integrations with `tui.integrations.<name>.enabled`.
- `docs/16-daemon.md` (0140): a pointer to `docs/19-engine-and-frontends.md`.
- `docs/02-using-the-tui.md`: a short note that the TUI behaves identically on either engine.
