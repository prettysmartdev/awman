# Work Item: Feature

Title: Interactive or headless agents in every daemon feature, with live terminals over the network
Issue: issuelink

Depends on: 0145 (one daemon, one API, features), 0159 (naming rules, config tree, route tree).

## Summary:
- Today, how agents run is fixed by where the command came from:
  - squad runs every agent in a PTY (`SquadRunFrontends::take_io`, fixed 120×40, `non_interactive: false`), with no headless option;
  - the API forces `--non-interactive` (`API_FLAG_DEFAULTS` in `projections/raw_args.rs`) and pipes every agent (`initial_size: None`);
  - `api.alwaysNonInteractive` is documented but unused (`EffectiveConfig::always_non_interactive` is only called from tests).
- Make interaction a setting of each daemon feature (0145), overridable per command or work item where the feature allows it. The values are 0159's `interactive` | `headless`:
  - `interactive`: the agent runs in a PTY, exactly as in the TUI.
  - `headless`: piped print mode, as the API does today.
  
  | Feature | Interaction |
  |---|---|
  | `squad` | `daemon.features.squad.interaction`; defaults to `interactive`, as today. |
  | `worker` push | `daemon.features.worker.push.interaction`; defaults to `headless`, today's API behaviour. |
  | `worker` pull | `daemon.features.worker.pull.interaction`; defaults to `interactive`. |
  | `engine` (0154) | Always interactive, with dialogs **relayed to its frontends** (0155) instead of taking yolo answers. |
- Make interactive agents **watchable from anywhere.** Today a running agent's PTY can only be attached locally (`squad attach`, over unix sockets via `attach_protocol.rs`). Add a **terminal hub** to the daemon's **shared core** (0145). It keeps a screen mirror per agent container, for every feature, and serves it through one core resource, `/v1/terminals`:
  - to local attach (`awman attach`), as today;
  - to push clients over a WebSocket;
  - to work servers over the worker's outbound connection (0147);
  - to engine frontends, including Rex agent blocks (0153).
  
  A late viewer sees the current screen, not a blank one.

## User Stories

### User Story 1:
As a: API client that shows runs to people (e.g. infact's webapp)

I want to:
push `exec workflow --dynamic --interactive`, list `GET /v1/terminals?owner_id=cmd_…`, and open `wss://…/v1/terminals/term_…`

So I can:
show the agent's real, live TUI (colours, spinners, tool calls, diffs) instead of a final message printed at exit

### User Story 2:
As a: developer using squad

I want to:
`awman config set daemon.features.squad.interaction headless` for cheap, quiet evaluation runs

So I can:
skip PTYs and screen mirrors for tasks nobody watches

### User Story 3:
As a: developer

I want to:
`awman attach cmd_…` (or `--read-only`) to a run that a push client, a work server or squad started on my machine

So I can:
watch it, or step in when it is stuck, with the same keys as `squad attach`

## Implementation Details:
- **Settings** (all in 0159's key table, `global` scope, live):
  - `daemon.features.squad.interaction`, `daemon.features.worker.push.interaction` and `daemon.features.worker.pull.interaction` are each `interactive` or `headless`. The defaults keep today's behaviour: squad `interactive`, push `headless`, pull `interactive`.
  - `allowOverride` (default `true`) on each lets a command or work item choose otherwise.
  - `engine` has no setting: it is always interactive.
  - `daemon.terminal.cols` / `rows` (default 120×40) set the PTY size for unattended runs, and `daemon.terminal.remoteInput` (default `false`) gates network input (below).
  - The API's forced `--non-interactive` (renamed `--headless`, 0159) becomes push's default when overrides are allowed. `exec workflow` and `exec prompt` accept `--interactive` through `POST /v1/worker/commands`.
  - `api.alwaysNonInteractive` is removed with no migration (0159). Its effect is `daemon.features.worker.push = { interaction: "headless", allowOverride: false }`.
- **Interactive unattended runs** (squad, push, pull):
  - Daemon frontends set `AgentIo.initial_size` from `daemon.terminal.{cols,rows}`, so `spawn_pty_bridged` is used.
  - Dialogs that would wait for a person (step confirmation, control board) take the yolo answers.
  - Stuck agents get the existing 60 s countdown, then auto-advance.
  - A person attached with input rights can cancel a countdown exactly as in the TUI.
- **Engine sessions.** Agents run in a PTY at the frontend's terminal size. Dialogs are relayed to the instance's frontends over 0155's protocol and wait for an answer, exactly as the in-process TUI does. Yolo sessions keep their countdowns.
- **Terminal hub** (`src/engine/terminal_hub/`). One per daemon, built by the 0145 shared core and shared by every feature.
  - Every PTY-bridged container registers a terminal: `{id: "term_…", owner_id: "cmd_…" | "tab_…" | "lease_…", feature, source, agent, model, attempt, cols, rows, state, created_at}`.
    - `source` is 0137's event-envelope object (phase, step name and index, container, `step_attempt`).
    - `attempt` is the command's execution number (0136).
  - The hub tees PTY output into a `vt100::Parser`, the same crate the TUI's `container_view.rs` uses, plus a bounded ring buffer, then fans it out to subscribers.
  - Slow subscribers get `lagged` and a fresh snapshot rather than unbounded buffering.
  - Today's attach socket and exec bridge become hub subscribers, so local attach works for every feature.
- **API** (core routes, 0159 §5):
  - `GET /v1/terminals?owner_id=…` lists terminals as `{items, next_cursor}`, including finished ones while the owning record exists.
  - `GET /v1/terminals/{id}` returns the terminal record, or upgrades to a WebSocket with subprotocol `awman.terminal.v1`.
  - **Access** follows the owner's rules:
    - a command's terminals are served wherever `worker` is served, network listener included;
    - an engine tab's terminals are local only unless `engine` is persistently listed in `daemon.network.features` (0145);
    - a lease's terminals are relayed to its work server by 0147.
  - There are no per-feature terminal routes (`/v1/commands/{id}/terminals*` and `/v1/engine/terminals/*` are gone).
  - Server-to-client frames:
    - `snapshot{cols, rows, screen}`: the screen as ANSI that redraws it, sent on connect and after `lagged`;
    - `output{bytes}`;
    - `resize{cols, rows}`;
    - `exit{code}`;
    - `lagged`.
  - Client-to-server frames: `input{bytes}` and `resize{cols, rows}`. Both are accepted only from a client holding the input grant (below).
  - The frame set mirrors `attach_protocol.rs`, so one codec serves local and network viewers.
- **Input over the network is off by default.** Two things must both allow it: `daemon.terminal.remoteInput` and a per-command `allow_remote_input`.
  - When allowed, the first client that sends `take_control` holds the input grant, and others stay read-only.
  - Every grant and release is written to the owner's event log as `terminal_control{terminal_id, holder, annotations}`. The holder is named by the client's `x-awman-actor` header for network clients, or `local` for local attaches, and `annotations` are the subscription's annotations (below), which is how an integration such as Rex identifies its block. Keystrokes are not logged.
  - Local attaches to an engine instance's own terminals get the grant the same way the TUI's maximized view does today.
- **Events.**
  - Add `terminal_opened{terminal_id, agent, model, cols, rows}` and `terminal_closed{terminal_id, exit_code}` to the execution events. SSE followers of `/v1/worker/commands/{id}/events` then know when to open a socket.
  - Step, phase, container and retry come from the event envelope's `source` (0137), not from fields repeated in these payloads.
  - Attributed output lines (0137) continue for clients that only want text.
  - The engine publishes the same events to host presenters (0154), plus `terminal_viewer_detached{terminal_id, reason: user | hangup, annotations}` for local attach subscriptions.
- **CLI.**
  - `awman attach <terminal-id | command-id | task-id> [--step NAME] [--read-only] [--annotation k=v]...` (0159 §7). The ID prefix says what it is:
    - `term_…` opens that terminal;
    - `cmd_…` opens the command's current terminal, or with `--step` that step's;
    - `task_…` opens the squad task's running terminal.
  - `--annotation k=v` (repeatable) attaches generic integration metadata to the subscription. Keys and values follow 0159 §1 and 0154's limits. They appear in `terminal_control` and in the engine's terminal events, so a host presenter can recognise its own views. There is no integration-specific attach flag.
  - **Hang-up:** when the attach loses its terminal (SIGHUP, or stdout closed), it tells the hub `detached{reason: hangup}` before exiting. The engine forwards that to host presenters with the subscription's annotations (0153 maps it to "un-maximize"). A normal detach (Ctrl-C) reports `detached{reason: user}`.
  - **Host-terminal hygiene:** whenever stdout is a TTY, `awman attach` passes agent bytes through 0156's output filter and OSC 7501 re-encoding. There is no flag.
  - `--read-only` drops stdin, never requests the input grant, and never resizes the PTY. 0153 and 0157 use it for presented blocks.
  - `awman squad attach <task>` is a shortcut for `awman attach task_…`.

## Edge Case Considerations:
- **Resize:** the agent's PTY size is owned by the input holder, or by `daemon.terminal.{cols,rows}` when nobody holds input. Read-only viewers scale their own view and never resize the PTY. For engine sessions with no holder, the primary frontend's size wins, as in the TUI today.
- **A command that falls back to headless** (an agent with no PTY support, or ACP-launched agents): `GET /v1/terminals?owner_id=…` returns no items and the client uses the event stream. ACP agents may later get a structured viewer, but that is out of scope here.
- **Secrets on screen:** terminals carry exactly what the agent prints, which is the same exposure as the event stream. Access uses the same key and session scope as `/v1/worker/commands/{id}/events`.
- **Daemon restart:** re-queued commands get new terminals with a new `attempt`. Old terminal IDs answer `410 terminal_gone`.
- **A feature cancelled with `awman daemon api jobs.cancel feature=…`:** its terminals send `exit` and close. Viewers of other features' terminals are unaffected.
- **Changing `daemon.terminal.cols`/`rows` live:** new terminals use the new size, and running terminals keep theirs.

## Test Considerations:
- A squad task with `interaction: headless` runs piped. A pushed command with `--interactive` runs in a PTY. With `allowOverride: false`, the override is refused at submit with `400 interaction_override_refused`.
- A pulled item uses `daemon.features.worker.pull.interaction` unless the item's `exec.interaction` overrides it and overrides are allowed.
- Engine sessions always run in a PTY, and their dialogs reach the frontend, not the yolo answerer.
- `/v1/terminals`:
  - lists by `owner_id` with `items`/`next_cursor`;
  - IDs carry `term_`;
  - an engine tab's terminal is refused on the network listener unless `engine` is persistently listed;
  - the removed per-feature terminal paths return 404.
- The terminal WebSocket:
  - a snapshot on connect matches the vt100 state, and output streams;
  - a late joiner gets the current screen;
  - a slow consumer gets `lagged` plus a snapshot.
- Input is refused without both settings. `take_control` is exclusive, and `terminal_control` events appear in the event log.
- `awman attach`:
  - it resolves `term_`, `cmd_` (with and without `--step`) and `task_` IDs;
  - `--read-only` never sends input or resize frames;
  - `--annotation` values are validated and appear unchanged in `terminal_control` and `terminal_viewer_detached`;
  - SIGHUP produces `reason: hangup`, and Ctrl-C produces `reason: user`.
- With stdout a TTY, a container printing OSC 52 or DCS through `awman attach` shows none of it (0156).
- A yolo countdown on a stuck interactive agent auto-advances with no viewer attached.

## Codebase Integration:
- follow established conventions, best practices, testing, and architecture patterns from the project's aspec.
- **Code:**
  - `src/engine/container/{process,attach_protocol,attach_socket}.rs`;
  - `src/engine/terminal_hub/`, built in 0145's shared core, with the `/v1/terminals` core routes;
  - `src/frontend/tui/container_view.rs` (reuse its vt100 handling);
  - `projections/raw_args.rs`;
  - `awman attach` in `src/command/commands/attach.rs`, replacing `agents attach` and folding in `squad attach`'s resolver.
- **Config:** the interaction and terminal keys are rows in 0159's key table, `src/data/config/fields.rs`.
- **Related:**
  - 0154 and 0155: engine sessions and dialog relay;
  - 0153 and 0157: Rex blocks as hub subscribers;
  - 0156: the host-terminal output filter applied by `awman attach`;
  - 0159: naming, routes and IDs.

## Documentation
- `docs/03-agent-sessions.md`: interaction per feature (`interactive` | `headless`) and `awman attach`.
- `docs/09-api-and-remote-mode.md`: `/v1/terminals`, the WebSocket frames, and the input grant.
- `docs/16-daemon.md`: `daemon.terminal.*` and per-feature interaction.
