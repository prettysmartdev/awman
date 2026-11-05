# Work Item: Task

Title: Host-terminal hygiene: agent output filter, OSC 7501 emission, terminal env deny-list
Issue: issuelink

Depends on: 0159 (config naming) for the key; nothing else to start. OSC 7501 emission of agent states uses 0152's parser types; until 0152 lands, emit only the states awman itself originates.

## Summary:
- Agent bytes reach the user's **host terminal** in several places:
  - `awman agents attach`;
  - `awman squad attach`;
  - 0146's `awman attach`;
  - the TUI's maximized view, when it passes sequences through.
- Today those bytes are forwarded with almost no filtering, so an agent in a container can use control sequences the host terminal acts on:
  - writing the host clipboard (OSC 52);
  - spoofing the host shell's cwd (OSC 7);
  - raising desktop notifications (OSC 9/99/777);
  - triggering file transfers (OSC 1337, kitty);
  - tunnelling sequences through DCS/APC passthrough.
- In the other direction, containers can inherit host-terminal variables (`TMUX`, `REX_*`, `TERM_PROGRAM`, …) that describe and sometimes address the host's multiplexer, and an exotic host `TERM` (`xterm-rex`, `xterm-kitty`) has no terminfo entry inside the image.
- awman knows each agent's state but tells the host terminal nothing. OSC 7501, the Program Status Protocol (0152), lets a terminal show "working / blocked / done" in its tab bar.
- This item adds three small, independent pieces. Each can ship before the daemon and Rex work (0145, 0151, 0154–0158), and those items build on them:
  1. a **host-terminal output filter** applied wherever agent bytes reach a host terminal;
  2. **OSC 7501 emission** from the TUI to its own terminal;
  3. a **container env deny-list** for host-terminal variables, and `TERM` normalisation.

## User Stories

### User Story 1:
As a: security-conscious user

I want to:
be sure an agent's output can never write my clipboard, spoof my shell's directory, raise notifications or start file transfers in my terminal when I attach to it

So I can:
attach to agents freely without trusting what they print.

### User Story 2:
As a: user of a terminal or multiplexer that supports OSC 7501 (Rex and others)

I want to:
see in my terminal's tab bar whether the awman tab is working, waiting on me, done or failed

So I can:
notice an agent waiting on me even when the awman tab isn't focused.

### User Story 3:
As a: user running awman inside tmux, zellij or Rex

I want to:
my agents' containers to see neither my multiplexer's variables nor a `TERM` they can't render

So I can:
get correct rendering in the container, with nothing about my host terminal leaking into it.


## Implementation Details:

### 1. Host-terminal output filter: `src/engine/agent_runtime/host_output_filter.rs` (Layer 1, pure)
- `HostOutputFilter` is a streaming state machine: `fn feed(&mut self, chunk: &[u8], out: &mut Vec<u8>)`, plus `fn finish(&mut self, out)`.
  - It holds a partial escape sequence across chunks, capped at 4 KiB. A sequence that exceeds the cap is dropped up to its terminator, and plain text is never held back.
  - It never rewrites printable text, CSI sequences (colour, cursor, modes), or ordinary OSC 0/1/2 titles.
- **Stripped by default:**
  - OSC 52 (clipboard write and query);
  - OSC 7 (cwd);
  - OSC 9, OSC 99 and OSC 777 (desktop notifications);
  - OSC 1337 (iTerm2 file transfer, `SetUserVar`, etc.);
  - OSC 5113 / kitty file-transfer and APC `_G` graphics transfer;
  - all DCS (`ESC P … ST`, including tmux passthrough `ESC Ptmux;`), SOS, PM and APC strings.
- **OSC 7501** reports are parsed with 0152's parser and **re-encoded** from the sanitized record, never forwarded raw. OSC 7501 queries are dropped, because awman's bridge answers them (0152). Until 0152 lands, OSC 7501 is stripped.
- `HostOutputPolicy { allow_notifications: bool, program_status: Reencode | Strip }` is passed in by the caller. The default policy strips notifications. The caller builds it from config. An integration can contribute through 0154's registry when its detection is `Present` in the current terminal: Rex (0151) maps `tui.integrations.rex.forwardAgentNotifications` onto `allow_notifications`. No flag selects a policy.
- **Where it is applied** (every place agent bytes reach a host terminal):
  - `awman agents attach`: the CLI writer in `src/frontend/cli/per_command/container_frontend_marker.rs`, before raw stdout.
  - `awman squad attach`: `src/frontend/cli/per_command/squad_attach.rs` and `src/frontend/tui/squad_attach.rs`.
  - 0146's `awman attach` (once it lands). The filter and OSC 7501 re-encoding are applied **whenever its stdout is a TTY**, with no flag. This is what agent blocks opened by host presenters (0151's Rex blocks, 0157) rely on.
  - The TUI's own renderer is unaffected: it parses agent bytes into `vt100` and never forwards raw OSC to the host. Add a regression test that it still doesn't.
- The existing CLI ACP OSC 52 strip (`acp_frontend.rs`) is replaced by the shared filter, so there is one implementation.

### 2. OSC 7501 emission: `src/engine/agent_runtime/program_status_emit.rs` (Layer 1, pure) + TUI hook
- `ProgramStatusEmitter::reports(state: &EmitInput) -> Vec<Osc7501Report>` is pure. `EmitInput` lists each tab and its agent slots, with their 0152 `EffectiveStatus` and any awman-originated states.
  - **Root record:** `app=awman`. Its state is the aggregate across all tabs and agents, by priority **blocked > error > working > idle > done**. `msg` is the highest-priority record's message.
  - **Child records:** one per agent, `id=tab<N>/<slot>`, with `title` = the tab label and `msg` = the agent's sanitized message. On `blocked` they carry `kind` and `progress`.
  - **awman-originated states**, which need no 0152:
    - a dialog waiting on the user → `blocked`, `kind=question`;
    - an agent-auth dialog → `blocked`, `kind=auth`;
    - a running command → `working`;
    - a command finished → `done`; failed → `error`.
  - It encodes base64 `msg`/`title` and enforces the protocol caps: 4096-byte sequence, 192-byte `title`, 2048-byte `msg`, 128-byte `id`, at most 64 records.
- **TUI hook** (`src/frontend/tui/event_loop.rs`): after each tick, if the emitter's output changed, write the sequences straight to the TUI's stdout, outside the ratatui frame buffer.
  - **Coalesced**: at most 4 writes per second, and only on change.
  - **Cleared** with `state=clear` (no `id`) on normal exit and in the existing panic hook, so the terminal never keeps a stale "working".
- **Feature-detect gating:** at startup the TUI sends `OSC 7501 ; ? ST` followed by `CSI c` (primary device attributes).
  - It emits only if the 7501 reply arrives before the DA reply.
  - No reply, or DA first, means no emission ever, so terminals that would print junk never see the sequences.
  - The probe reuses the event loop's input reader. The replies are consumed and never reach the key handler.
- **Config:** `tui.programStatus.emit` (bool, default `true`, scope `repo`: global and repo), one row in the 0159 key table. For one run: `--set tui.programStatus.emit=false` (or `AWMAN_TUI_PROGRAM_STATUS_EMIT=false`); there is no dedicated flag. Emission is also off under `TERM=dumb` and when stdout isn't a TTY.

### 3. Container env deny-list and `TERM` normalisation
- `src/engine/container/host_env_denylist.rs` (pure) has `fn is_host_terminal_var(name) -> bool` for:
  - `REX_*`, `TMUX`, `TMUX_*`, `ZELLIJ`, `ZELLIJ_*`, `STY`, `WINDOW`;
  - `TERM_PROGRAM`, `TERM_PROGRAM_VERSION`, `TERM_SESSION_ID`;
  - `WEZTERM_*`, `KITTY_*`, `ITERM_*`, `LC_TERMINAL*`, `ALACRITTY_*`, `GHOSTTY_*`, `VTE_VERSION`, `WT_SESSION`, `WT_PROFILE_ID`.
- It is applied at the single point where each runtime backend's container run spec gets its env: Docker, Apple `container`, builtin and sandbox, in `src/engine/container/` and `src/engine/sandbox/`.
  - It applies after overlays and agent env are merged, so even an overlay that requests one of these names gets a notice and the variable is not passed.
  - Explicit per-session secrets (0141) are unaffected, because those names never match.
- `TERM` is always `xterm-256color` inside containers, whatever the host `TERM` (`xterm-rex`, `xterm-kitty`, `tmux-256color`, …). The current pass-through (`src/engine/auth/host_secret_command.rs` passes `TERM` for host-side helpers only) is unchanged, because that is a host process. `COLORTERM=truecolor` is passed only when the host sets it.

## Edge Case Considerations:
- **A sequence split across chunk boundaries** (`ESC ]` in one chunk, `52;c;…BEL` in the next): the filter buffers it and still strips it.
- **An unterminated OSC** (an agent prints `ESC ]52;` and never terminates it): after 4 KiB, or at `finish()` / detach, the buffered bytes are dropped, never flushed raw.
- **Both `ESC \` and BEL terminators**, and 8-bit C1 forms (`0x9D` OSC, `0x90` DCS, `0x9C` ST), must be handled.
- **An agent relying on OSC 8 hyperlinks:** allowed through, since they are inert until clicked. Revisit if a terminal is found that auto-opens them.
- **The TUI inside tmux:** tmux swallows unknown OSC unless `allow-passthrough` is on. Feature-detect then fails, so there is no emission, which is correct.
- **Several awman TUIs in one terminal multiplexer:** each emits for its own terminal only. Records are per terminal, so they don't collide.
- **A user who wants clipboard writes from an agent:** not supported here, on purpose. Agents should write files to `/awman/output` instead.
- **A deny-listed variable the user needs in a container:** they must re-export it under a different name via an overlay `env()`. The deny-list is not configurable, so the safety default can't be bypassed by a repo config.

## Test Considerations:
- **Filter unit tests:**
  - each stripped family, with both terminators and C1 forms;
  - sequences split at every byte offset;
  - an oversized unterminated sequence;
  - CSI, SGR and OSC 0/2/8 preserved byte for byte;
  - OSC 7501 re-encoded (with 0152) or stripped (without);
  - `allow_notifications` on and off.
- **Property test:** for arbitrary input, the output contains no byte sequence starting OSC 52/7/9/99/777/1337 or DCS/APC, and the printable text is preserved.
- **Emitter unit tests:**
  - aggregate priority across mixed states;
  - child IDs and titles;
  - the caps (truncation stays valid UTF-8);
  - base64 encoding;
  - coalescing (no write when unchanged; ≤4/sec);
  - the clear sequence on exit and in the panic path.
- **Feature-detect:** a fake terminal answering 7501 then DA → emits; DA only → never emits; a 7501 reply after DA → never emits; replies never reach the key handler.
- **Env:** container specs built under an `EnvSnapshot` full of the deny-listed names contain none of them, for every backend. `TERM` is `xterm-256color` whatever the host `TERM`. An overlay requesting `TMUX` produces a notice and no variable.
- **End-to-end:** gated by `AWMAN_TEST_DOCKER`, `awman agents attach` to a container that prints OSC 52, OSC 7, OSC 9 and DCS shows none of them on the captured host stdout.

## Codebase Integration:
- follow established conventions, best practices, testing, and architecture patterns from the project's aspec.
- The filter and emitter are pure Layer 1 modules with no I/O. Frontends own writing to stdout. Reuse 0152's `ProgramStatusReport` and `EffectiveStatus` instead of duplicating the protocol model.
- The deny-list is one function, called from each backend's run-spec builder. Don't scatter checks.
- These modules are reused by 0151 (Rex agent blocks), 0155 (TUI frontends) and 0157.

## Documentation

After implementation is complete, update user-facing documentation in `docs/` to reflect the current state of the tool:

- **Update existing feature docs** (e.g., if implementing headless features, update `docs/08-headless-mode.md`)
- **Create new user guides only if a new user-visible feature warrants it** (e.g., `docs/10-my-feature.md`)
- **Never create work-item-specific docs** (e.g., no "WI 0123 implementation guide" in published docs)
- **Keep all technical/implementation details in work item specs or code comments**, not in `docs/`
- **Docs are for end users**, not for developers trying to understand implementation

See `CLAUDE.md` for more guidance on documentation standards.

Specifically for this item:
- `docs/04-security-and-isolation.md`: which control sequences from agents are filtered, and which host-terminal variables never reach containers.
- `docs/02-using-the-tui.md`: terminal tab status (OSC 7501), and `tui.programStatus.emit` (and `--set tui.programStatus.emit=false` for one run).
- `docs/07-configuration.md`: `tui.programStatus.emit`.
- `docs/03-agent-sessions.md`: the filtering applies to `agents attach` and `squad attach`.
