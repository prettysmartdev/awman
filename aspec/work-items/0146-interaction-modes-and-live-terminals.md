# Work Item: Feature

Title: Interactive or non-interactive agents in every daemon mode, with live terminals over the network
Issue: issuelink

## Summary:
- How agents run is fixed by where the command came from:
  - squad runs every agent in a PTY (`SquadRunFrontends::take_io`, fixed 120×40, `non_interactive: false`), with no non-interactive option;
  - the API forces `--non-interactive` (`API_FLAG_DEFAULTS` in `projections/raw_args.rs`) and pipes every agent (`initial_size: None`);
  - `api.alwaysNonInteractive` is documented but unused (`EffectiveConfig::always_non_interactive` is only called from tests).
- Make interaction a setting of each daemon mode (0145), and overridable per command or work item where the mode allows it:
  - `interactive`: the agent runs in a PTY, exactly as in the TUI. Unattended progress comes from yolo stuck-detection and auto-advance, as squad does today.
  - `non-interactive`: piped print mode, as the API does today.
- Make interactive agents **watchable from anywhere**. Today a running agent's PTY can only be attached locally (`squad attach`, over unix sockets via `attach_protocol.rs`). Add a daemon-owned **terminal hub** that keeps a screen mirror per agent container and serves it:
  - to local attach, as today;
  - to API clients over a WebSocket;
  - to work servers over the worker's outbound connection (0147).
  A late viewer sees the current screen, not a blank one.

## User Stories

### User Story 1:
As a: API client that shows runs to people (e.g. infact's webapp)

I want to:
submit `exec workflow --dynamic --interactive` and open `wss://…/v1/commands/{id}/terminals/{terminal_id}`

So I can:
show the agent's real, live TUI (colours, spinners, tool calls, diffs) instead of a final message printed at exit

### User Story 2:
As a: developer using squad

I want to:
set `daemon.modes.squad.interaction` to `non-interactive` for cheap, quiet evaluation runs

So I can:
skip PTYs and screen mirrors for tasks nobody watches

### User Story 3:
As a: developer

I want to:
`awman attach <command-id>` to a run that an API client or a work server started on my machine

So I can:
watch it, or step in when it is stuck, with the same keys as `squad attach`

## Implementation Details:
- **Settings.** `daemon.modes.<mode>.interaction` is `interactive` or `non-interactive`. Defaults keep today's behaviour: squad `interactive`, api `non-interactive`, worker `interactive`. `daemon.modes.<mode>.allowOverride` (default true) lets a command or work item choose otherwise.
  - The API's forced `non-interactive=true` becomes a default when the mode allows overrides. `exec workflow` and `exec prompt` accept `--interactive` over the API.
  - `api.alwaysNonInteractive` maps to `daemon.modes.api = { interaction: "non-interactive", allowOverride: false }` and is then removed.
- **Interactive unattended runs.**
  - Daemon frontends set `AgentIo.initial_size` from `daemon.terminal.{cols,rows}` (default 120×40), so `spawn_pty_bridged` is used.
  - Dialogs that would wait for a person (step confirmation, control board) take the yolo answers.
  - Stuck agents get the existing 60 s countdown, then auto-advance.
  - A person attached with input rights can cancel a countdown exactly as in the TUI.
- **Terminal hub** (`src/engine/terminal_hub/`). One per daemon, built by the 0145 core.
  - Every PTY-bridged container registers a terminal with `{terminal_id, command_id, source, agent, model, attempt, cols, rows, state}`. `source` is 0137's event-envelope object (phase, step name and index, container, `step_attempt`), and `attempt` is the command's execution number (0136).
  - The hub tees PTY output into a `vt100::Parser`, the same crate the TUI's `container_view.rs` uses, plus a bounded ring buffer, then fans it out to subscribers.
  - Slow subscribers get `Lagged` and a fresh snapshot rather than unbounded buffering.
  - Today's attach socket and exec bridge become hub subscribers, so local attach works for every mode.
- **API.**
  - `GET /v1/commands/{id}/terminals` lists terminals, including finished ones while the command record exists.
  - `GET /v1/commands/{id}/terminals/{terminal_id}` upgrades to a WebSocket with subprotocol `awman.terminal.v1`.
  - Server-to-client frames:
    - `snapshot{cols,rows,screen}`: the screen as ANSI that redraws it, sent on connect and after `lagged`;
    - `output{bytes}`;
    - `resize{cols,rows}`;
    - `exit{code}`;
    - `lagged`.
  - Client-to-server frames: `input{bytes}` and `resize{cols,rows}`. Both are accepted only from a client holding the input grant, below.
  - The frame set mirrors `attach_protocol.rs` so one codec serves local and network viewers.
- **Input over the network is off by default.** Two things must both allow it: `daemon.terminal.remoteInput` and a per-command `allow_remote_input`. When allowed, the first client that sends `take_control` holds the input grant, and others stay read-only. Every grant and release is written to the command's event log (`TerminalControl{terminal_id, holder}`), with the holder named by the client's `x-awman-actor` header. Keystrokes are not logged.
- **Events.** Add `TerminalOpened{terminal_id, agent, model, cols, rows}` and `TerminalClosed{terminal_id, exit_code}` to `ExecutionEvent`, so SSE followers know when to open a socket. Step, phase, container and retry come from the event envelope's `source` (0137), not from fields repeated in these payloads. Attributed output lines (0137) continue for clients that only want text.
- **CLI and TUI.** `awman attach <command-id> [--step NAME]` opens any hub terminal on this machine. `squad attach` becomes a shortcut for it.

## Edge Case Considerations:
- Resize: the agent's PTY size is owned by the input holder, or by the daemon setting when nobody holds input. Read-only viewers scale their own view and never resize the PTY.
- A command that falls back to non-interactive (an agent with no PTY support, or ACP-launched agents): `terminals` is empty and the client uses the text stream. ACP agents may later get a structured viewer, but that is out of scope here.
- Secrets on screen: terminals carry exactly what the agent prints, which is the same exposure as the log stream. Access uses the same key and session scope as `/logs`.
- Daemon restart: re-queued commands get new terminals with a new `attempt`. Old terminal IDs answer 410.

## Test Considerations:
- A squad task with `interaction: non-interactive` runs piped. An API command with `--interactive` runs in a PTY. With `allowOverride: false`, the override is refused at submit with 400.
- The terminal WebSocket: a snapshot on connect matches the vt100 state, output streams, a late joiner gets the current screen, and a slow consumer gets `lagged` plus a snapshot.
- Input refused without both settings. `take_control` is exclusive, and control events appear in the event log.
- A yolo countdown on a stuck interactive agent auto-advances with no viewer attached.

## Codebase Integration:
- follow established conventions, best practices, testing, and architecture patterns from the project's aspec.
- `src/engine/container/{process,attach_protocol,attach_socket}.rs`, `src/frontend/{api,squad}/`, `src/frontend/tui/container_view.rs` (reuse its vt100 handling), `projections/raw_args.rs`.

## Documentation
- `docs/03-agent-sessions.md` (interaction per mode), `docs/09-api-and-remote-mode.md` (terminals API, input grant), `docs/16-daemon.md`.
