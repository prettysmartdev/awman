# Work Item: Feature

Title: Frontend protocol: the TUI and CLI as engine frontends
Issue: issuelink

Depends on: 0159 (config and API naming, route tree), 0154 (engine feature), 0145 (one daemon, one API, local listener, ephemeral enablement), 0146 (terminal hub, relayed dialogs). Used by: 0153 (Rex), 0157, 0158.

## Summary:
- 0154 hosts awman instances in the daemon's `engine` feature. This item defines how a TUI or CLI process becomes a **frontend** of such an instance, and when it does so.
- **Protocol `awman.frontend.v1`.** It is a WebSocket on `/v1/engine/instances/{id}/frontend`, on the daemon's local listener. It serialises the channels a `Tab` already uses to talk to its command thread (`command_result_rx`, `dialog_request_rx`, `dialog_response_tx`, `TabSharedState`, the `ContainerSlotEvent` queue). The TUI keeps its rendering and input code; only the producer side of those channels moves.
- **Hand-off.** Each invocation decides once, before dispatch, whether it runs on the **builtin** in-process engine or the **daemon** engine. The inputs are one config key, `tui.backend: "auto" | "daemon" | "builtin"` (overridable for one run with 0159's generic `--set tui.backend=…` or `AWMAN_TUI_BACKEND`), whether an enabled host integration requires the daemon (Rex, 0153), and each integration's `tui.integrations.<name>.enabled` (likewise overridable with `--set`). There are no dedicated flags.
  - Under `auto`, the daemon engine is used when a host integration requires it, or when `engine` is persistently enabled. Otherwise today's builtin engine is used.
  - Any failure under `auto` falls back to builtin with a one-line notice.
- **Parity.** A TUI or CLI frontend is indistinguishable from the builtin engine (0154's contract): the same keys, dialogs, output, prompts, Ctrl-C behaviour, exit codes and fatal-error handling.
- Agent terminal bytes travel over 0146 hub subscriptions, never over the frontend socket.

## User Stories

### User Story 1:
As a: user

I want to:
run `awman`, `awman chat` or `awman exec workflow …` against the daemon engine and see exactly the same screens, prompts, output and exit codes as with the builtin engine

So I can:
use host integrations such as Rex without anything else changing, including in scripts.

### User Story 2:
As a: user who upgrades awman while a daemon is running

I want to:
have the new binary quietly restart an idle daemon, or ask me before restarting one with running work

So I can:
upgrade without stale-daemon surprises or lost agents.

### User Story 3:
As a: frontend author (Rex today, a web or IDE frontend later)

I want to:
a documented, versioned protocol with typed frames and a host-claim mechanism for window-management requests

So I can:
present awman natively without forking its TUI.


## Implementation Details:

### 1. Hand-off — `src/command/engine_handoff.rs` (Layer 2)
- **Inputs:**
  - `tui.backend: "auto" | "daemon" | "builtin"`: default `"auto"`, scope `repo` (global and repo), one row in 0159's key table. For one run, use `--set tui.backend=builtin` or `AWMAN_TUI_BACKEND=builtin`; 0159 removes per-setting flags, so there is no `--engine`;
  - `tui.integrations.<name>.enabled: "auto" | "on" | "off"` for each registered integration, which `detect()` reads. For one run, use `--set tui.integrations.rex.enabled=off`. There is no integration-specific or integration-wide flag;
  - `HostRequirement` (`None | Optional(Annotations) | Required(Annotations)`), returned by each enabled integration's `detect()` in 0154's registry. Rex (0153) returns `Required` with its `rex/*` annotations when Rex is `Present`. Detected annotations from all integrations are merged into `Hello.annotations`; a namespace clash is a bug and is rejected.
- **Decision:**

  | `tui.backend` | Host requires daemon | `engine` persistently on | Result |
  |---|---|---|---|
  | `builtin` | any | any | builtin, and the host integration does not run |
  | `daemon` | any | any | daemon. It is started, or `engine` is enabled ephemerally, if needed. Failure → exit nonzero |
  | `auto` | yes | any | daemon, ephemeral if needed. Failure → builtin + notice |
  | `auto` | no | yes | daemon. Failure → builtin + notice |
  | `auto` | no | no | builtin (today) |
- **Steps to reach the daemon:**
  1. Read 0145's server-meta sidecar.
  2. If no daemon is running, start one in the background, as `awman daemon start --background` does, with `engine` enabled ephemerally (an internal local-only API call, not a CLI verb).
  3. If a daemon runs with `engine` off, `PATCH /v1/features/engine {enabled: true, ephemeral: true}` (a core route, local listener only, 0159). This is refused with `409 ephemeral_not_allowed` when `daemon.features.engine.allowEphemeral` is `false`.
- **Version handshake:** `GET /v1/status` returns `{build, protocols: {frontend: "awman.frontend.v1", terminal: "awman.terminal.v1"}, features: […]}` (0159). On a mismatch:
  - The daemon has no running work in any feature → stop it and start the new binary's daemon, silently.
  - It has running work → the TUI shows a `YesNo` dialog: "The awman daemon is from an older version and is running N jobs. Restart it now? Running work will be stopped." The CLI prints the same advice and exits 2 (or falls back to builtin under `auto`). A declined restart under `auto` falls back to builtin with a notice.
  - An incompatible daemon is never driven silently.
- **Fallback notice.** One line in the TUI status bar (CLI: stderr), for example `engine: unavailable: daemon failed to start: <data>/daemon/api.sock: permission denied`. When the daemon engine works, nothing is shown.
- `awman config show` shows `tui.backend` with its effective value and source, plus the resolved backend for this shell (`backend: daemon (engine ephemeral, rex) pid 1234` / `backend: builtin`).

### 2. Protocol `awman.frontend.v1`
- **Transport.** `POST /v1/engine/instances` → `{id: "ins_…"}`, then a WebSocket upgrade on `/v1/engine/instances/{id}/frontend` with subprotocol `awman.frontend.v1`. Local listener only (0145/0154). Frames are JSON messages with `{type, …}` and a per-direction `seq`.
- **Frontend → engine:**
  - `Hello{kind: Tui|Cli, role: Primary|Secondary, annotations: Annotations, cwd, argv, env, term_size, stdin_is_tty, protocol}`. `annotations` is the generic integration metadata (0154 §4): detected annotations plus any given with the hidden entry point's `--annotation k=v`. There is no integration-specific field in any frame;
  - `OpenTab{dir, options}`;
  - `Submit{ParsedCommandBoxInput}`;
  - `DialogResponse{dialog_id, response}`;
  - `Cancel`;
  - `Resize{cols, rows}`;
  - `UiRequest{OpenTab{dir} | NextTab | PrevTab | CloseTab | SetContainerWindow{slot, state} | OpenOverview | FocusWorkflowWindow | FocusWork{target_id}}`. This is the same enum, in snake_case (`open_overview`, `focus_work`, …), that `POST /v1/engine/instances/{id}/ui_requests` accepts (0154, 0159). `FocusWork` names a squad task or worker job (`task_…`/`cmd_…`/`lease_…`) that a presenter may show (0157);
  - `Quit`.
- **Engine → frontend:**
  - `InstanceSnapshot` (tabs, sessions, phases, slots, git summaries, labels, colours, yolo and stuck state), then `InstanceDelta`s;
  - `CommandOutput` (message-sink lines, with stream and level);
  - `DialogRequest{dialog_id, request}`;
  - `DialogClosed{dialog_id, answered_by}`;
  - `ProgramStatus` (0152 `EffectiveStatus` per slot);
  - `Notice`;
  - `UiAck{request_seq, claimed_by_host: bool}`;
  - `Detach{reason}`.
- **The cut line.** On the engine side, each tab's command thread is unchanged. A bridge task reads its `command_result_rx`, `dialog_request_rx` and `ContainerSlotEvent` queue and turns them into frames. Inbound `DialogResponse`s are written to `dialog_response_tx`. `TabSharedState` changes are diffed into `InstanceDelta`s.
- **On the frontend side,** `RemoteInstanceBackend` / `RemoteTabBackend` (`src/frontend/tui/remote_tab.rs`) recreate the same channel types locally. `Tab` and the render code consume them unchanged.
- **Dialogs** move from `src/frontend/tui/dialogs/mod.rs` to `src/data/dialog.rs` with serde derives, and the TUI re-exports them. Audit of the variants:
  - plain data (`YesNo`, `YesNoCancel`, `TextInput`, `MultilineInput`, `ListPicker`, `KindSelect`, `QuitConfirm`, `CloseTabConfirm`, `WorkflowCancelConfirm`, `Loading`, `Custom`, `KeySetupNotice`) derive directly;
  - state structs (`WorkflowControlBoardState`, `WorkflowYoloCountdownState`, `WorkflowStepConfirmState`, `AgentSetupState`, `MountScopeState`, `AgentAuthState`, `ReviewState`, `ConfigShowState`, the squad states) must be checked for handles, `Sender`s and closures, which are replaced by IDs resolved engine-side;
  - `DialogResponse::Secret(SecretString)` is serialised only on the local socket, with redacted `Debug`, and never logged;
  - `DialogResponse::ConfigEdit(ConfigEditRequest)` is a typed request, applied engine-side with the instance's env.
- **Several frontends:** every frontend of an instance gets every `DialogRequest`. The first `DialogResponse` wins, and the others get `DialogClosed{answered_by}`. Focus and scroll are per frontend.
- **Agent terminal bytes** never use this socket. A frontend subscribes to the core `GET /v1/terminals/{id}` (`term_…`, WebSocket `awman.terminal.v1`; 0146 framing: snapshot, output, resize, exit, lagged), and its vt100 parser is fed from that. Keystrokes into a maximized agent use the hub's input grant, which the instance's frontends hold by default, as with typing into today's maximized view.
- **`TerminalRelease`/`TerminalRestore`** (awman hands the terminal to a host process such as an auth flow): the engine runs that process in a PTY with the instance's env and exposes it as a hub terminal. The frontend leaves raw mode and alternate screen, bridges its real terminal to that hub terminal, and restores afterwards. This is the same sequence the user sees today.
- **UiRequest claiming:**
  - The frontend sends a `UiRequest` for every window-management transition: tab open, next, previous or close, container-window state change, overview, workflow window.
  - With `claimed_by_host: true`, a host presenter (0153) does it, and the frontend only updates its state. For example, the tab bar stays collapsed and the maximized slot renders as Minimized because Rex shows it.
  - With `false`, the frontend does exactly what today's TUI does.
  - Without a host, every request is unclaimed, so behaviour is identical to builtin.
- **Hidden entry point** `awman view --instance <id> --tab <session-id> [--annotation k=v]...`: a secondary TUI frontend rendering one tab of an existing instance. Its `Hello.annotations` are its own detected annotations plus the given ones, which is how a presenter matches the view to the window it opened. It is used by host presenters, marked internal in the catalogue, and absent from `--help` and the docs.

### 3. CLI frontends
- `awman chat`, `exec prompt|workflow`, `new …`, `init` and `ready` create an instance with `kind: Cli` and submit their parsed command.
  - `CommandOutput` is written to stdout and stderr exactly as the CLI's message sink does today (`src/frontend/cli/user_message.rs`, `output.rs`).
  - Prompts use `src/frontend/cli/command_frontend.rs`'s existing renderers over relayed `DialogRequest`s.
  - Ctrl-C sends `Cancel`, with the same double-Ctrl-C semantics as today.
  - The process exits with the command's `CommandOutcome` exit code.
  - `--headless` and non-TTY stdin behave identically, because the engine sees the same argv and the frontend reports `stdin_is_tty` in `Hello`.
- `agents …`, `squad …` and `daemon …` already work against the daemon or store, and are unchanged.
- Engine-free commands (`--help`, `--version`, `config …`, `integration <status|setup|remove>` from 0158) stay local and never trigger the hand-off.

### 4. Reconnect and engine loss
- A socket error triggers a reconnect with backoff (50 ms → 1 s, 2 s total, matching 0154's primary grace period), then `InstanceSnapshot` re-sync. The user sees nothing.
- If the engine is gone, or the instance ended or was refused, the TUI shows today's fatal-error dialog (`Dialog::FatalError`) with the reason and exits with today's fatal exit code. The CLI prints the error and exits as an in-process engine failure would. This is the same experience as an engine-thread panic in the builtin engine.

## Edge Case Considerations:
- **Daemon start races** (two `awman` invocations at once): 0145's startup lock serialises them. The second connects to the first's daemon.
- **A frontend's terminal is resized mid-dialog:** `Resize` goes to the engine for PTY sizing and to the hub. Dialog layout is local.
- **A huge `InstanceSnapshot`** (many tabs, long git summaries): frames are capped at 1 MiB, with chunked snapshots. Oversized or unknown frames → a protocol error that closes the connection and triggers the fatal path, never a panic.
- **A frontend with an older protocol than the daemon** (the user runs an old binary against a new daemon): handled by the handshake in reverse. The old frontend is refused with "run the matching awman version, or `--set tui.backend=builtin`".
- **Scripts:** `awman exec … --headless` in a pipeline produces byte-identical stdout and stderr and the same exit code on either backend. This is verified in tests.
- **The env contains secrets:** see 0154 §8. `Hello` is never logged.
- **`tui.backend: daemon` outside any host** (or `--set tui.backend=daemon` for one run): the daemon engine is used for a plain terminal, enabled ephemerally if needed. This is how users try it before setting `daemon.features.engine.enabled`.

## Test Considerations:
- Codec: round-trip every frame type and every `DialogRequest`/`DialogResponse` variant through serde, with property tests over generated values. Unknown, oversized and out-of-order `seq` frames are rejected cleanly.
- Hand-off decision table (unit, pure) over `tui.backend` (from config, `AWMAN_TUI_BACKEND` and `--set`) × `tui.integrations.<name>.enabled` × host requirement × daemon state (none, running with or without engine, ephemeral forbidden, version mismatch idle or busy). Also check that no `--engine`, `--integration` or `--no-integrations` flag exists (clap rejects them).
- Parity: 0154's conformance suite runs the TUI and CLI suites with `AWMAN_TEST_BACKEND=daemon` through this protocol and compares outcome traces and frames with builtin. CLI end-to-end tests compare stdout, stderr and exit codes byte for byte.
- Multi-frontend: two secondaries and one primary. A dialog reaches all three, the first answer wins, and the others get `DialogClosed`.
- `UiRequest` claiming: unclaimed → native behaviour, identical frames to builtin. Claimed (fake presenter) → state updated, no native window change.
- `TerminalRelease` flow with a fake host process through a hub terminal.
- Reconnect within grace → no visible change. Engine loss → `FatalError` frame snapshot equals the builtin panic path.
- Latency: the p95 round trip for `Submit` → first `CommandOutput`, and for `DialogRequest` → `DialogResponse`, is under 5 ms on the Unix socket.

## Codebase Integration:
- follow established conventions, best practices, testing, and architecture patterns from the project's aspec.
- **Layer 0:** `src/data/dialog.rs` (moved dialog types), `src/data/frontend_protocol.rs` (frames), `src/data/annotations.rs` (shared with 0154).
- **Layer 2:** `src/command/engine_handoff.rs`.
  - `src/command/dispatch/mod.rs` chooses a daemon-backed or local `CommandFrontend` from the hand-off result.
  - Add a `FrontendKind` variant for relayed frontends (`catalogue/mod.rs`; the match has no default arm, so every command declares whether it supports relay). Engine-free commands are marked local-only.
- **Layer 3:**
  - `src/frontend/tui/remote_tab.rs` (`RemoteInstanceBackend`, `RemoteTabBackend`) implementing 0154's `InstanceBackend`.
  - `src/frontend/tui/mod.rs` (`tui::run()` picks the backend).
  - `src/frontend/tui/dialog_router.rs` (unchanged, now fed remotely).
  - `src/frontend/cli/command_frontend.rs` and `per_command/*` (relay adapters).
  - `src/frontend/tui/event_loop.rs` (terminal release and restore around relayed host processes).
- Catalogue: no new global flags (0159's `--set` covers `tui.backend` and integration switches). Add the `tui.backend` row to the key table (`src/data/config/fields.rs`, section struct `src/data/config/tui.rs`, shared with 0153 and 0156), and the hidden `view` command (`--instance ins_…`, `--tab tab_…`, `--annotation k=v`).
- Reuse 0146's `attach_protocol.rs` framing for terminal subscriptions. Do not duplicate it.

## Documentation

After implementation is complete, update user-facing documentation in `docs/` to reflect the current state of the tool:

- **Update existing feature docs** (e.g., if implementing headless features, update `docs/08-headless-mode.md`)
- **Create new user guides only if a new user-visible feature warrants it** (e.g., `docs/10-my-feature.md`)
- **Never create work-item-specific docs** (e.g., no "WI 0123 implementation guide" in published docs)
- **Keep all technical/implementation details in work item specs or code comments**, not in `docs/`
- **Docs are for end users**, not for developers trying to understand implementation

See `CLAUDE.md` for more guidance on documentation standards.

Specifically for this item:
- `docs/07-configuration.md`: `tui.backend` (a generated row).
- New `docs/19-engine-and-frontends.md` (shared with 0154): choosing the builtin or daemon backend (`tui.backend`, `--set tui.backend=…`), upgrade behaviour, fallback notices, and switching integrations with `tui.integrations.<name>.enabled`.
- `docs/16-daemon.md` (0145): a pointer to `docs/19-engine-and-frontends.md`.
- `docs/02-using-the-tui.md`: a short note that the TUI behaves identically on either engine.
