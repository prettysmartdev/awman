# Work Item: Feature

Title: Engine feature: a singleton awman instance for many frontends
Issue: issuelink

Depends on: 0159 (config and API naming, route tree), 0145 (one daemon, one API, features), 0146 (terminal hub in the shared core, relayed dialogs). Lands together with 0155 (frontend protocol), which follows it in the order. Used by: 0153 (Rex integration), 0157 (daemon work in Rex), 0158 (Rex action pack).

## Summary:
- Today every `awman` TUI process builds its own awman instance in-process. `tui::run()` creates an `App` that holds:
  - an in-memory `SessionManager` (`src/data/session_manager.rs`);
  - one `Tab` per session with its own command thread and channels;
  - the git poll;
  - the `SquadIndicatorPoller`;
  - container slots fed by `ContainerLifecycle` (`src/frontend/tui/tabs.rs`).
  
  The CLI does the same for each command. Nothing outside that process can drive or present its work.
- **Add `engine`, the third feature of the 0145 daemon:** a singleton awman instance host that runs this same non-UI half of awman **once, in the daemon**, and drives any number of **frontends**: one or more TUI instances, the Rex integration (0153), and any future awman frontend (a web UI, an IDE extension…). Frontends speak the frontend protocol (0155) over the daemon's local listener.
- **Instance model, unchanged from today.**
  - The engine hosts **instances**. An instance is exactly what one TUI process holds in-process today.
  - Typing `awman` creates a new instance. Two `awman`s are two independent instances, exactly as today.
  - An instance has one **primary** frontend (the process the user started) and any number of **secondary** frontends (for example, Rex tab windows running `awman view --instance <id> --tab <session-id>`).
- **The parity contract is the point.** A TUI or CLI on the daemon `engine` must be **indistinguishable** from one on the builtin in-process engine. That covers keys, dialogs, commands, output, errors, exit codes, environment, config resolution and lifecycle.
  - A conformance suite runs the existing TUI and CLI behaviour tests against both backends and requires identical outcomes.
  - The builtin engine stays the default outside host integrations, and stays fully supported.
- Each instance runs its host-side operations with the **launching frontend's environment, cwd and argv**, never the daemon's. So `PATH`, Docker context, SSH agent, credential helpers and git config are the user's shell's, just as in-process.

## User Stories

### User Story 1:
As a: user

I want to:
run awman's TUI with the daemon `engine` as its backend and notice no difference at all from the normal TUI

So I can:
get host integrations such as Rex (0153) and future frontends without relearning anything or losing any behaviour.

### User Story 2:
As a: frontend author (Rex integration, a future web or IDE frontend)

I want to:
attach to an awman instance hosted by one long-lived engine, receive its state and dialogs, and claim its window-management requests

So I can:
present awman natively in my host without re-implementing sessions, workflows, dialogs or container lifecycle.

### User Story 3:
As a: security-conscious user

I want to:
know that `engine` is reachable only from my own user on this machine unless I explicitly expose it, and that my shell environment is never written to disk or forwarded to containers

So I can:
enable it, or let a host integration enable it ephemerally, without widening awman's attack surface.


## Implementation Details:

### 1. The feature
- `EngineFeature` implements 0145's `DaemonFeature` (`start(&Core)`, `drain()`, `stop_now()`, `status()`, `routes()`) in `src/engine/daemon/features/engine.rs`.
- **Routes.** These are the `engine` rows of 0159 §5. They follow its conventions: `ins_`/`tab_` ID prefixes, `{items, next_cursor}` lists, `{error, code}` errors and `snake_case` fields.

  | Route | Purpose |
  |---|---|
  | `POST /v1/engine/instances` | Create an instance. Returns `{id: "ins_…"}`. |
  | `GET /v1/engine/instances?annotation=k=v` | List instances (repeatable filter): `id`, `primary_frontend_kind`, `annotations`, `tab_count`, `running_jobs`, `created_at`. The filter is how integrations find "the instance for this window". |
  | `GET /v1/engine/instances/{id}` | One instance, with its frontends. |
  | `DELETE /v1/engine/instances/{id}` | Quit, with today's exit policy. |
  | `PATCH /v1/engine/instances/{id}` | `{annotations}` only. |
  | `GET /v1/engine/instances/{id}/frontend` | WebSocket `awman.frontend.v1` (0155). |
  | `GET /v1/engine/instances/{id}/tabs`, `GET\|PATCH /v1/engine/instances/{id}/tabs/{id}` | Tabs with their session, container slots and annotations. `PATCH` accepts `{annotations}` only. |
  | `GET /v1/engine/instances/{id}/dialogs`, `POST /v1/engine/instances/{id}/dialogs/{id}/answer` | Pending dialogs, and answering one with `{response}`. The first answer wins and every frontend gets `DialogClosed{answered_by}`, exactly as when a frontend answers. |
  | `POST /v1/engine/instances/{id}/ui_requests` | `{request: "open_overview" \| "focus_workflow_window" \| …}`, the same `UiRequest` enum as the frontend protocol (0155). It is routed to the claiming host presenter, or handled by the primary frontend. |
  
  - Agent terminals of engine sessions are the core `GET /v1/terminals?owner_id=ins_…` and `GET /v1/terminals/{id}` (0146, 0159). There is no engine-specific terminal route.
  - There is **no `actions` route**. Friendly names such as `instances.list`, `dialogs.answer`, `steps.approve` and `overview.open` are client-side `awman daemon api` actions over these routes (0159 §6).
- **Local listener only by default.** The `/v1/engine/*` routes are served on the network listener only when `engine` is **persistently** enabled *and* listed in `daemon.network.features`. Even then they are explicitly out of scope until a remote frontend has its own security review. The docs say so.
- **Config** (scope `global`, rows in the 0159 key table):
  ```json
  "daemon": { "features": { "engine": { "enabled": false, "allowEphemeral": true, "idleTimeout": "15m", "maxConcurrentJobs": 4 } } }
  ```
  `awman config set daemon.features.engine.enabled true` turns it on persistently and applies live to a running daemon (0159 §4).
- **Ephemeral enablement** (0145): a local frontend's hand-off (0155) may send `PATCH /v1/features/engine {enabled: true, ephemeral: true}`, a core route on the local listener.
  - The ephemeral engine is never served on the network listener.
  - It disables itself after `idleTimeout` with no frontends, no instances, no running engine work and no presenter hold. A host presenter may hold the engine while it presents other features' work (0157). If no feature is left, the daemon exits.
  - `allowEphemeral: false` forbids it, and hand-offs then fall back to builtin under `auto` (0155).
  - `awman daemon status` and `GET /v1/status` show the feature state `ephemeral` (with the reason, e.g. `rex`), `on`, `off` or `draining`.
- **Executor share.** Engine sessions' commands go through the shared executor under `daemon.features.engine.maxConcurrentJobs`, inside the host-wide `daemon.maxConcurrentJobs`, with round-robin claims across features, so a flood of `worker` jobs can't starve interactive sessions and vice versa.
- **Interaction.** Engine sessions are always `interactive` (0146): agents run in a PTY at the frontend's terminal size, and dialogs are relayed to frontends (0155) instead of taking yolo answers. Yolo sessions keep their countdowns, exactly as in-process.

### 2. Instance model and lifecycle (parity)
- `EngineInstance` (`src/engine/instance/mod.rs`) owns what `App` owns today, minus rendering and input:
  - `SessionManager`;
  - the ordered tab list, each with its session, `execution_phase`, `TabSharedState`, command thread and dialog channels;
  - container slots and their `ContainerLifecycle` / `ContainerSlotEvent` queue;
  - per-tab git poll;
  - `SquadIndicatorPoller`;
  - yolo and stuck state;
  - the active-tab index as a *hint* (each frontend keeps its own focus).
- **Creation.** `POST /v1/engine/instances` is followed by the primary frontend's `Hello` (0155), which supplies `env`, `cwd`, `argv` and `term_size`. The initial tab is opened exactly as `tui::run()` does today (`InitialTab::Normal | Squad`, then auto-spawning `ready` or `agents status --watch`).
- **Frontends.**
  - Exactly one **primary** per instance.
  - **Secondaries** attach to an existing instance (`role: Secondary`) and may render one tab (`awman view --instance <id> --tab <session-id>`) or all of them.
  - A second `Primary` hello for the same instance is refused (`409 primary_exists`).
- **Lifecycle mapping, identical to today:**
  - Quit from any frontend (`Quit`, after `QuitConfirm` as today) → today's TUI-exit policy for the whole instance. All frontends get `Detach{reason: "quit"}`.
  - The primary frontend disconnects without `Quit` (killed or hung up) → treated as today's TUI process being killed: the same exit policy, with no prompt. A short grace period (2 s) covers a reconnect after a transient socket error (0155), so parity holds for genuine exits only.
  - A secondary frontend disconnects → nothing happens to the instance. Host presenters may map it to a tab action (0153 maps closing a Rex tab window to close-tab).
  - Close tab → today's close-tab semantics (cancel the running command, end the session). Closing the last tab quits the instance, as today.
  - `Ctrl-\` detach containers → identical to today.
- **Nothing survives that wouldn't survive in-process**, and nothing is killed that wouldn't be. There is no "detached instance" state.

### 3. Per-instance environment, cwd, argv and config
- `Hello.env` (the frontend's full environment), `cwd` and `argv` are kept **in memory only**, in the instance, as an `EnvSnapshot` (`src/data/config/env.rs`).
- Every host-side spawn for that instance goes through the instance's `EnvSnapshot` instead of the daemon process environment: docker, container, sbx, git, gh, credential helpers, host secret commands (`src/engine/auth/host_secret_command.rs`), editor launches, and mount-scope checks.
  - This needs an audit of every `std::env::var` / `Command::new` on the session path. Route them through `EnvSnapshot` and `host_cli::program` (`src/engine/host_cli.rs`), adding explicit `env_clear().envs(snapshot)` where a process is spawned.
  - A lint-style test (grep in CI) forbids new direct `std::env` reads under `src/engine` and `src/command` outside an allow-list.
- Container environment rules are **unchanged**. Containers get exactly what they get today from an in-process TUI with the same environment, plus the 0156 deny-list.
- **Config** is resolved per session exactly as in-process, with 0159's precedence: default → global (`$AWMAN_HOME/config.json`, or the XDG/`HOME` location, taken from the instance's env) → repo `GITROOT/.awman/config.json` → `AWMAN_*` variables from the instance's env → `--set` from its `argv`. Repo-scope `tui.*` keys resolve against each tab's repo. Config reload behaviour matches today.
- **Docker context is per instance.** Two instances launched from shells with different `DOCKER_HOST`/`DOCKER_CONTEXT` each talk to their own Docker, as two TUIs do today. Runtime handles in the shared core are keyed by the resolved runtime endpoint, so the core may hold several.

### 4. Annotations, the integration registry, and host presenters
- **Annotations** are the only way integrations attach metadata. No integration-specific type appears in the protocol, the store or `src/data/`.
  - The type is `Annotations` (`src/data/annotations.rs`), a `BTreeMap<String, String>` with keys namespaced by integration (`rex/session`, `rex/block`, `rex/window`).
  - Keys are ≤63 characters of `[a-z0-9./-]`. Values are ≤256 bytes with no control characters. There are at most 32 entries per object. Anything else is rejected with `400 invalid_annotation`.
  - They live on:
    - the frontend connection, from `Hello.annotations` (0155);
    - the instance;
    - each tab;
    - each terminal subscription, from `awman attach --annotation k=v` (0146) and `awman view --annotation k=v` (0155).
  - They are listed by `awman daemon api instances.list annotation.<k>=<v> --json` (client-side, 0159 §6), which calls `GET /v1/engine/instances?annotation=k=v`. Presenters set them through their engine hook, and API clients through `PATCH` on the instance or tab.
  - They are kept in memory with the instance and dropped when it ends.
- **Integration registry** (`src/engine/integrations/mod.rs`). This is the compiled-in list of integrations. Each one provides:
  - `name()` (e.g. `"rex"`), which is also its annotation namespace and its `tui.integrations.<name>` config key;
  - a frontend-side `detect(env, config) -> HostRequirement` (`None | Optional(Annotations) | Required(Annotations)`), called by 0155's hand-off;
  - an optional `setup` / `remove` / `status`, used by the generic `awman integration …` command (0158);
  - an optional engine-side `HostPresenter`.
- `trait HostPresenter: Send + Sync`:
  - `fn name(&self) -> &'static str`. A presenter **claims every frontend whose `Hello.annotations` contain keys in its namespace**.
  - `fn on_ui_request(&self, instance, frontend, req: UiRequest) -> UiOutcome`.
  - `fn on_instance_event(&self, instance, ev: InstanceEvent)`, for tab opened/closed, container-window state changed, terminal opened/closed/detached (with the subscription's annotations and the reason, for example `hangup`), program status and instance exit.
  - Through `PresenterHandle`, it can set or clear annotations **in its own namespace only** on instances, tabs and terminals. For example, Rex records `rex/window` on a tab.
- The engine routes a frontend's `UiRequest`s to the presenter that claims it. If none does, it replies `UiAck{claimed_by_host: false}`, and the frontend handles the request natively (a tab switch in its own tab bar). That means a frontend with no host behaves exactly like today's TUI.
- Presenters run under the feature's supervisor. A presenter panicking or erroring never affects the instance: its requests fall back to `claimed_by_host: false`, and a single `Notice` goes out.

### 5. Splitting `App` into UI state and instance state (refactor plan)
1. **Identify** `App` fields (`src/frontend/tui/app.rs`) and `Tab` fields (`src/frontend/tui/tabs.rs:789`) that are UI-only, and keep them in the frontend:
   - focus and `active_dialog` rendering state;
   - the command-box editor;
   - scroll state;
   - the vt100 parser (fed by the hub);
   - mouse state;
   - `container_window_state` (Hidden/Minimized/Maximized), as a view choice reported to the engine for presenters.
2. **Move** everything else behind an `InstanceBackend` trait (`src/frontend/tui/instance_backend.rs`) with two implementations:
   - `LocalInstanceBackend`, which builds `EngineInstance` in-process (today's behaviour, the builtin engine);
   - `RemoteInstanceBackend` (0155), which talks to the daemon.
3. `Tab` keeps its existing channel types (`command_result_rx`, `dialog_request_rx`, `dialog_response_tx`, `TabSharedState`, `ContainerSlotEvent`). Only the producer side moves, so most TUI code is untouched.
4. `tui::run()` picks a backend from 0155's hand-off result and nothing else changes.

This order keeps every step shippable: step 2 with only `LocalInstanceBackend` is a pure refactor, verified by the existing test suite.

### 6. Parity contract and conformance suite
- **Contract.** For any input sequence (keys, mouse, resize, CLI argv and stdin), the daemon engine must produce the same engine-side outcomes as the builtin engine:
  - sessions created;
  - commands run, with their arguments;
  - dialogs raised, their order and content;
  - container lifecycle;
  - git and worktree effects;
  - stdout, stderr and exit codes.
  
  The rendered TUI frames must also be identical.
- **Allowed differences:** none in the TUI or CLI. `awman daemon status` and `awman daemon api instances.list` show engine state. Host presenters may add host-native presentation (0153), but never change outcomes.
- **Suite** (`tests/engine_parity/`):
  - Run the existing TUI tests (`src/frontend/tui/tests/*`, `tabs/tests.rs`) and CLI end-to-end tests twice, with a backend switch (`AWMAN_TEST_BACKEND=builtin|daemon`).
  - The daemon run starts an in-process daemon core on a temp `<data>` with a Unix socket.
  - Fakes are shared: fake runtimes and fake agents.
  - Assertions compare recorded engine-side outcome traces and frame snapshots between the two runs.
- **Latency budget:** under 5 ms added per command or dialog round trip on the local listener (p95, measured in the suite). Rendering never waits on the socket.

### 7. Instance listing
- `awman daemon api instances.list [annotation.<k>=<v>]... [--json]` shows instances, their tabs, frontends and annotations. It is a client-side action (0159 §6) over the read-only local route `GET /v1/engine/instances`, and `instances.get id=ins_…` maps to `GET /v1/engine/instances/{id}`. The instance count also appears in `awman daemon status`.
- `awman agents status` / `list` include engine-instance agents exactly as they include in-process ones today.

### 8. Security
- `/v1/engine/*` is served on the **local listener only** by default (0145): the Unix socket at 0600 with a peer-UID check, or the loopback owner key.
- `Hello.env` may contain secrets (tokens in env vars).
  - It travels only over the peer-UID-checked local socket, or loopback with the owner key.
  - It is held in memory, never logged (its `Debug` is redacted) and never written to the store, events or crash reports.
  - It is dropped when the instance ends.
- `SecretInput` dialog responses (`SecretString`) cross the socket under the same rules and stay redacted in every log.
- Containers receive nothing beyond today's rules and the 0156 deny-list. The daemon socket, owner key and sidecar are never mounted into containers.
- An ephemeral engine is local-only, whatever `network.features` says.

## Edge Case Considerations:
- **The daemon crashes:** every frontend loses its backend. The TUI shows today's fatal-error dialog (as if the engine thread panicked) and exits with today's code. Containers follow today's crash behaviour. On restart, the engine does not resurrect instances; there is no detached state.
- **A frontend crashes:**
  - The primary → today's killed-TUI exit policy after the 2 s grace period.
  - A secondary → no effect on the instance.
- **Two primaries for one instance:** the second gets `409 primary_exists`. A new `awman` always creates a new instance, so this only happens with a misused hidden entry point.
- **Env with secrets:** handled as in §8. Tests assert that secrets never appear in logs or the store.
- **Docker context differs between instances:** each instance uses its own (§3). A runtime handle cache keyed by endpoint avoids rebuilding clients.
- **Squad tab in the engine:** the `InitialTab::Squad` and squad-tab views read the daemon store directly, as today. When the daemon itself has `squad` enabled, the instance's squad view and the `squad` feature share the store with no extra IPC.
- **Daemon upgraded while instances run:** handled by 0155's version handshake. Running instances keep the old daemon until they quit.
- **`engine` disabled while instances run** (`awman config set daemon.features.engine.enabled false`): this drains, and running commands finish. Frontends get `Detach{reason: "engine disabled"}` and the TUI shows the same fatal-style notice as a crash, so a user never keeps working on a half-dead engine. Cancelling immediately is the explicit `awman daemon api jobs.cancel feature=engine` (0159 §4).
- **Host-side interactive subprocesses** (`DialogRequest::TerminalRelease`/`TerminalRestore`, used when awman hands the terminal to a host process such as an auth flow): the process runs in the daemon in a PTY with the instance's env, and its bytes are relayed to the frontend's terminal through a hub terminal (0146). The behaviour the user sees is the same as today. See 0155.
- **`idleTimeout` while a frontend is reconnecting:** the grace period counts as "has frontends".

## Test Considerations:
- The parity suite (§6) is the acceptance gate for this item and for 0155.
- Unit:
  - `EngineInstance` lifecycle state machine (quit, primary loss with grace, secondary loss, close last tab);
  - primary/secondary admission;
  - ephemeral idle timing, including reconnect grace;
  - annotation validation (key grammar, size and count limits, namespace ownership: a presenter can't write another namespace);
  - `HostPresenter` routing by annotation namespace, with fallback to `claimed_by_host: false` when the presenter errors.
- `EnvSnapshot` routing: run a fake `docker`/`git` on a per-instance `PATH` through `host_cli::program`, and check that two instances with different `PATH`/`DOCKER_HOST` each hit their own fake. **Never mutate the process-wide `PATH`** (per the recent CI fix).
- Security:
  - the engine routes refuse non-loopback peers, wrong owner keys and wrong peer UIDs;
  - an ephemeral engine is never on the network listener;
  - an env containing `GITHUB_TOKEN=secret` never appears in logs, events, the store or crash output.
- Refactor safety: after step 2 of §5 (`LocalInstanceBackend` only), the full existing suite passes unchanged.

## Codebase Integration:
- follow established conventions, best practices, testing, and architecture patterns from the project's aspec.
- **Layer 1** `src/engine/instance/` (`EngineInstance`, lifecycle, events), `src/engine/daemon/features/engine.rs`, `src/engine/integrations/mod.rs` (`HostPresenter`).
- **Layer 0** `src/data/annotations.rs`, plus instance and event types that cross the protocol.
- **Layer 3** `src/frontend/tui/instance_backend.rs` (trait + `LocalInstanceBackend`); `app.rs` and `tabs.rs` split as in §5.
- `src/data/session_manager.rs` is unchanged and simply lives in the instance.
- Reuse `ContainerLifecycle` / `ContainerSlotEvent`, `TabSharedState` and the command-thread spawn in `App::spawn_command` as they are; move them, don't rewrite them.
- Coordinate with 0129's architecture findings: land its daemon and API read-model cleanups (A01–A03) first, so `engine` builds on them.
- Catalogue: no new command. This item adds the `instances.list|get`, `dialogs.list|answer` and `ui.request` rows to the client-side action table of `awman daemon api` (`src/command/commands/daemon/actions.rs`, 0159 §6). 0158 adds the `steps.approve` and `overview.open` compositions. The server gains no action registry. Engine-served commands use `FrontendKind::Daemon { feature: Engine, listener: Local }` (0145).

## Documentation

After implementation is complete, update user-facing documentation in `docs/` to reflect the current state of the tool:

- **Update existing feature docs** (e.g., if implementing headless features, update `docs/08-headless-mode.md`)
- **Create new user guides only if a new user-visible feature warrants it** (e.g., `docs/10-my-feature.md`)
- **Never create work-item-specific docs** (e.g., no "WI 0123 implementation guide" in published docs)
- **Keep all technical/implementation details in work item specs or code comments**, not in `docs/`
- **Docs are for end users**, not for developers trying to understand implementation

See `CLAUDE.md` for more guidance on documentation standards.

Specifically for this item:
- The daemon guide (`docs/16-daemon.md`, from 0145): the `engine` feature, what it is for, persistent versus ephemeral, `daemon.features.engine.*` (`idleTimeout`, `allowEphemeral`, `maxConcurrentJobs`), `awman daemon api instances.list`, turning it on with `awman config set daemon.features.engine.enabled true`, and the fact that it behaves exactly like the normal TUI.
- `docs/04-security-and-isolation.md`: engine is local-only, and how the shell environment is handled.
- `docs/14-command-reference.md`: the `instances.*`, `dialogs.*` and `ui.request` actions of `awman daemon api`.
- `docs/09-api-and-remote-mode.md`: the `/v1/engine/*` routes in the 0159 route reference.
- New `docs/19-engine-and-frontends.md` (shared with 0155): running TUIs on the daemon engine, instances and frontends, and how integrations use annotations.
