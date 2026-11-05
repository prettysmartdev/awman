# Work Item: Feature

Title: Rex integration
Issue: issuelink

Depends on: 0159 (config and API naming, route tree), 0145 (one daemon, one API, features), 0146 (terminal hub, `awman attach`), 0152 (OSC 7501 parsing), 0154 (`engine`, annotations, integration registry, host presenters), 0155 (frontend protocol, hand-off), 0156 (host-terminal hygiene). Used by: 0157 (daemon work in Rex), 0158 (Rex action pack).

## Summary:
- Rex (Superlogical, beta) is a terminal multiplexer driven by a server. It organises work as **sessions → windows (tabs) → blocks (terminals)**, with **clients** (the macOS and iOS apps, the `rex` CLI, scripts) attached to a long-lived server. All of it is scriptable through a typed Server API (https://www.superlogical.com/rex/docs/automate/server-api), through `rex api call <method> <json>` and through Lua. Rex is also where OSC 7501, the Program Status Protocol that WI 0152 adopts, was defined.
- Inside every Rex terminal, Rex sets `REX_SESSION`, `REX_BLOCK` and `REX_SERVER`. That makes detection cheap and reliable. A live probe (`server.version`, `session.describe_block`) confirms the variables are not stale.
- Today, awman inside Rex is a multiplexer running inside a multiplexer. The awman tab bar duplicates Rex's tabs, a vt100 emulator duplicates Rex's terminal engine, and agent screens can't be seen from the Rex mobile app, searched with Rex scrollback, or captured with `rex capture`.
- **This work item:** when awman detects that it is running in Rex, it **uses the awman daemon's `engine` feature as its backend** and **presents through Rex**:
  - awman tabs become Rex windows;
  - maximized agent containers become Rex terminal blocks;
  - parallel workflow steps become a tiled Rex window.
  
  The daemon's `engine` is the singleton awman instance behind every awman invocation in Rex. No awman process in Rex runs its own engine or opens Rex windows itself. Only the engine's Rex presenter calls the Rex API.
- **Parity is the core requirement.** Using awman in Rex, or using the normal TUI with the daemon `engine` as its backend, must be **indistinguishable from running the TUI normally with the builtin in-process engine**. That covers keys, dialogs, commands, output, lifecycle, errors and environment. The only difference in Rex is *where* things are drawn: Rex windows instead of awman's tab bar, and Rex blocks instead of the maximized container view. Rex-native extras are strictly additive: status in Rex's tab bar, `rex capture`, palette actions.
- **Config:** one set of keys, `tui.integrations.rex.*`, read by both the TUI frontends and the engine's Rex presenter. `tui.integrations.rex.enabled` is `auto` by default. Outside Rex nothing changes.
- **Security is unchanged.**
  - Only the engine's Rex presenter calls the Rex API.
  - Agents in containers never get Rex variables, the Rex socket, daemon keys, or a path for their output to reach Rex's control API.
  - Agent-supplied text that reaches Rex is sanitized.
- **This item is one of several.** The daemon features, the `engine` feature, the frontend protocol and terminal hygiene are prerequisites, specified in their own work items (see §1). This item covers only the Rex-specific parts on top of them.

## User Stories

### User Story 1:
As a: user who lives in Rex

I want to:
run `awman` in a Rex terminal and get exactly the awman I know (same keys, dialogs, commands and behaviour), with its tabs as Rex windows and its agents as Rex terminal blocks, and with status spinners in Rex's tab bar

So I can:
use my multiplexer's own keys, splits, zoom, scrollback search and `rex capture` on awman's work, without learning a second tab system and without anything behaving differently from the normal TUI.

### User Story 2:
As a: user running parallel workflows

I want to:
see every running step's agent tiled in one Rex window, with blocked or finished agents flagged in Rex's tab headers and session picker

So I can:
watch a whole workflow at a glance, including from the Rex iOS app, and jump straight to the agent that needs me.

### User Story 3:
As a: security-conscious user or admin

I want to:
be sure the Rex integration never gives an agent container a way to drive my terminal multiplexer or the awman daemon, and be able to turn it off with one config key or flag

So I can:
adopt Rex without weakening awman's isolation guarantees.


## Implementation Details:

### 0. Research findings (Rex surface this design relies on)
- **Taxonomy:** a session holds windows (tabs). A window holds a layout of blocks (splits, plus floating layers). Each block is a terminal of creator `com.superlogical.terminal`, flavor `com.superlogical.terminal.shell`. IDs look like `block:01a1…`, never change and are never reused. Sessions, windows and blocks can also carry labels.
- **Environment variables in every Rex terminal:** `REX_SESSION`, `REX_BLOCK`, `REX_SERVER`. Also `REX_CLIENT` (default client for client actions) and `REX_TERM` (terminfo override; Rex terminals may report `TERM=xterm-rex`).
- **No `REX_*` needed to drive Rex.** The `rex` CLI talks to the user's local or home server by default, and accepts `-S <server>` / `REX_SERVER` for another one. Every `session.*` method takes an explicit `session_id`. A daemon under systemd or launchd can therefore act as a normal Rex client, given the context a frontend sends it.
- **Methods awman needs.** All are listed by `rex api list`, and `rex api describe <m> --json` gives JSON Schemas.
  - `server.version`, `server.whoami`.
  - `session.view` (windows, layouts and blocks in one snapshot), `session.describe_block`, `session.list_blocks`.
  - `session.new_window {session_id, layout, window_label, focus}`.
  - `session.new_split {session_id, direction, side, layout, anchor_id|window_id, ratio, focus}`.
  - `session.new_layer`, `session.focus_window`, `session.focus_block`, `session.move_window`, `session.zoom_block`, `session.close_window`, `block.close`.
  - `session.set_window_label`, `session.set_block_label`.
  - `block.method` for terminal methods: `title`, `write`, `program_status`, `format`, `size`.
- **Layout shape:** `{block:{flavor, options:{command:[argv…], shell:"none", cwd, exit:{on_completion:false}}}}`, combined with horizontal and vertical split nodes that take a `ratio`, mirroring the Lua `rex.layout.*` builders. Confirm the exact JSON with `rex api describe session.new_window --json` while implementing, and pin it in golden tests.
- **Events:** `rex events -s <session> --json` prints NDJSON. Events include `window_created`/`window_closed`/`window_label_changed`, `active_window_changed`, `block_created`/`block_closed`, `session_view_changed`, `session_destroyed`, `client_connected`, and `terminal.child_exited`, `terminal.program_status_changed`, `terminal.title_changed`.
- **Transport:** a local Unix socket (named pipe on Windows), plus optional HTTP, Tailnet or WebSocket listeners. Only the CLI and Lua surfaces are documented as public. **Decision for v1:** shell out to the `rex` CLI (`rex api call`, `rex events`) as a host CLI, like `docker`. A native socket client waits until the wire protocol is documented.
- **OSC 7501** is defined by Rex. Rex shows it as tab spinners, blocked symbols and session-picker indicators, and later as mobile push. WI 0152 makes awman parse it.

### 1. Work item breakdown and dependencies
The full design spans several work items. This item (0151) is only the Rex layer. The proposed split is below; the numbers for new items are suggestions.

| WI | Scope | What 0151 needs from it |
|---|---|---|
| **0145 (amend)**: one daemon, one API, features | Replace *modes* with **one API** whose **features** are enabled and disabled independently at runtime:<br>• **`squad`**: today's squad functionality, unchanged;<br>• **`worker`**: jobs pushed (today's API server and `awman remote`) or pulled (work servers, infact pull mode);<br>• **`engine`**: a singleton awman instance.<br>Also: a local listener (Unix socket with peer-UID check, or a named pipe) plus an optional network listener with `network.features`, **ephemeral enablement** (local-only, idles out), `daemon.features.*` config (written with `awman config set`, applied live), and the minimal `awman daemon start\|stop\|status\|logs\|api` command surface. There are no compatibility spellings. | Ephemeral, local-only `engine` enablement from a frontend's hand-off. `engine` never exposed on the network listener unless persistently listed. |
| **0147 (amend)**: `worker` feature, push and pull | **Push** covers today's API server, moved under `/v1/worker/*` (`sessions`, `commands`, `runs`, `workdirs`; 0159 §5), and `awman remote exec`. **Pull** covers Work Server Protocol claims (infact pull mode), with work servers configured in the top-level `workServers.<name>` map (0159). Both share one job model, executor share and event stream. | Nothing directly. Rex presentation of worker jobs is WI 0157. |
| **0146 (amend)**: terminal hub and interaction | The terminal hub and screen mirrors move into the daemon's shared core and serve both `worker` and `engine`. Per-feature interaction defaults. **Dialogs relayed to engine frontends** (not only yolo answers). Terminals are the core `/v1/terminals/{id}` (0159). `awman attach <terminal-id>` gains `--read-only` and the generic `--annotation k=v`, and reports hang-ups. | Hub terminals for every engine-session agent, late-joiner snapshots, `awman attach`, and the input grant. |
| **0152**: OSC 7501 parsing | Already specified. | Per-agent `EffectiveStatus`. |
| **New 0154: `engine` feature (singleton awman instance)** | Host everything `tui::run()` builds in-process today, once, in the daemon: `SessionManager`, tab command threads, dialogs, workflows, git polling and the squad indicator. Includes:<br>• the **TUI-instance model**: sessions belong to the frontend instance that opened them, exactly as with the builtin engine;<br>• **the per-session environment**: a session runs host-side operations (docker/git/credential helpers) with the launching frontend's `EnvSnapshot` and cwd, not the daemon's environment;<br>• config resolved per session exactly as in-process;<br>• the **parity contract** and its conformance suite. | The session host that the Rex presenter observes, and the place the presenter lives. |
| **New 0155: frontend protocol, and the TUI and CLI as engine frontends** | `awman.frontend.v1` on `/v1/engine/instances/{id}/frontend` over the local listener. It serialises the existing `Tab` ↔ command-thread channels (`command_result_rx`, `dialog_request_rx`, `dialog_response_tx`, `TabSharedState`, `ContainerSlotEvent`). Also:<br>• `RemoteTabBackend`;<br>• moving `DialogRequest`/`DialogResponse` to `src/data/` with serde;<br>• CLI relay (output, prompts, Ctrl-C, exit codes);<br>• the hand-off (`tui.backend: auto\|daemon\|builtin`, overridable for one run with `--set`; starting or ephemerally enabling the daemon; the version handshake);<br>• generic `annotations` on `Hello` (namespaced keys such as `rex/session`), plus a `UiRequest` channel for window-management requests (`OpenTab`, `NextTab`, `PrevTab`, `CloseTab`, `SetContainerWindow{slot, state}`, `OpenOverview`, `FocusWorkflowWindow`, `FocusWork`) that a host presenter may claim. There are no integration flags; switches are `tui.integrations.<name>.enabled`, overridable with `--set`. | A frontend that carries the Rex annotations and hands window-management requests to the engine. |
| **New 0156: host-terminal hygiene** | Independent; can ship first.<br>• A host-terminal output filter for agent bytes written to any host terminal: strip OSC 52 / 7 / 9 / 99 / 777 / 1337, kitty file transfer, DCS/APC; re-encode OSC 7501.<br>• OSC 7501 **emission** from the TUI (aggregate + per-agent records, awman-originated `blocked` for waiting dialogs).<br>• A container env deny-list for host-multiplexer variables (`REX_*`, `TMUX*`, `ZELLIJ*`, `TERM_PROGRAM`).<br>• `TERM` normalisation for containers. | The filter and emitter used by Rex agent blocks and TUI frontends. |
| **0151 (this item): Rex integration** | Detection, the hand-off rule "in Rex → engine", the Rex presenter, sessions as Rex windows, agents as Rex blocks, workflow layouts, the `tui.integrations.rex` config section, the Rex-specific security scrub, and Rex parity tests. | none |
| **New 0157: daemon work in Rex** | Present `squad` tasks and `worker` jobs (push and pull) in a dedicated Rex session, configured with `tui.integrations.rex.daemonWork.{show,session,server,allowInput}`. Includes read-only attach by default (with 0146 `take_control` auditing when `allowInput` is on), a 12-window cap with an overflow window, an ephemeral `engine` hold, and squad tab or `squad attach` → "focus task window". | Builds on 0151's presenter and ledger (`DaemonWork{feature, id}`). |
| **New 0158: Rex action pack** | The generic `awman integration <status\|setup\|remove> [<name>]` command. For Rex, `setup` generates a Lua pack under `~/.awman/integrations/rex/` and prints the `dofile` line for `init.lua`, which awman never edits. Rex palette actions call the awman CLI, using client-side `awman daemon api` actions (0159 §6): `instances.list annotation.rex/block=$REX_BLOCK`, and the `steps.approve` and `overview.open` compositions over `/v1/engine/instances/{id}/dialogs/{id}/answer` and `/ui_requests`. | Builds on 0151, 0154 and 0155. |

**Order** (as in the work items README): 0156 (any time) → 0145 → 0146, then 0147 (and 0148) → 0152 → 0154 → 0155 → **0151** → 0157 and 0158.

### 2. Parity contract (Rex-specific part)
0154 and 0155 define the general contract: the TUI against the daemon `engine` behaves exactly like the TUI with the builtin engine. Their conformance suite runs the existing TUI and CLI behaviour tests against both backends. This item extends that contract to Rex: **Rex changes where things are drawn, never what awman does.**

- **Same instance model.** Typing `awman` in a Rex terminal starts a new **TUI instance**, exactly as typing it in a new terminal does today. Its sessions are its own. Typing `awman` in another Rex terminal starts another independent instance, as today; it does not join the first one's sessions.
  - The Rex windows an instance opens (Ctrl-T) all belong to that instance. The ledger (§4) records `instance_id` per window. Tab order, Ctrl-A/D cycling and quit apply to that instance's windows only.
- **Same keys, same meaning.**
  - `Ctrl-T` opens the same path dialog and creates the same session; the new tab appears as a Rex window instead of in the tab bar.
  - `Ctrl-A`/`Ctrl-D` switch to the previous or next tab, which is now the instance's previous or next Rex window.
  - `Ctrl-M` cycles the container window as today. Its *maximized* state is a Rex block showing that agent, and leaving maximized closes or unfocuses that block, so the cycle stays Hidden → Minimized → Maximized.
  - `Ctrl-C` and `CloseTabConfirm` / `QuitConfirm` behave as today. Closing a tab closes its Rex window. Quitting closes all the instance's windows and applies today's TUI-exit policy to its agents.
  - `Ctrl-\` detach behaves as today.
  - Every other key and every dialog is unchanged and is rendered inside the awman block.
- **Same lifecycle when Rex closes things.** Closing a Rex window from Rex (its close button, `rex close`) means the same as that tab's terminal going away today:
  - A tab window closed → the engine applies today's close-tab semantics without the confirm prompt, the same as a hang-up today.
  - The instance's first window closed → the same as the TUI process being killed today: today's exit policy for the whole instance.
  - An agent block closed → only the extra *view* of the agent goes away, and the slot returns to Minimized, the same as un-maximizing.
  
  Nothing survives that wouldn't survive today, and nothing is killed that wouldn't be killed today.
- **Same output, errors and environment.** Sessions run with the launching frontend's environment and cwd (0154), so `PATH`, Docker context, SSH agent, credential helpers and git config are the ones from the user's Rex shell. Config resolution, mount-scope prompts, prerequisites, errors and exit codes are identical.
- **No new UI noise.** In Rex the TUI shows no "running on engine" banner. Notices appear only when something differs from the user's expectation: fallback to in-process, or a version-mismatch restart. The engine state is visible in `awman config show` / `awman daemon status`, not in the TUI.
- **Feel.** Rendering stays in the frontend process. The command box, scrolling and dialogs are local, and only command and dialog traffic crosses the local socket. Agent keystrokes go to the hub. Budget: under 5 ms of added latency per round trip on the local listener, measured in the conformance suite.
- **Allowed differences (additive only):**
  - status in Rex's tab bar and session picker;
  - Rex labels (`awman · <repo>`, `agent:<id>`) that `rex capture`/`rex send`/`rex wait` can address;
  - native Rex scrollback, search and zoom inside agent blocks;
  - workflow windows (§5), which open only *in addition to* today's workflow overview and never replace a dialog.

### 3. Detection and the hand-off rule
- Rex is one entry in 0154's integration registry (`src/engine/integrations/mod.rs`). Its frontend-side `detect()` returns the Rex **annotations** that travel in 0155's `Hello.annotations`:
  - `rex/server` (when `REX_SERVER` is set), `rex/session`, `rex/block` and `rex/version`;
  - `rex/client` when `REX_CLIENT` is set.
  
  Inside the integration module, `RexContext { server: Option<String>, session_id, block_id, client: Option<String>, version: RexVersion }` (`src/engine/integrations/rex/context.rs`) is parsed from and rendered to those annotations. No Rex type appears in the frontend protocol or in shared `src/data/` types. `RexDetection` is `Absent | Disabled(reason) | Unavailable(reason) | Present(RexContext)`.
- Detection is a pure decision over an injected `EnvSnapshot` and a probe trait (`src/engine/integrations/rex/detect.rs`), run once per invocation before dispatch picks a backend:
  1. `tui.integrations.rex.enabled == "off"` (from config, `AWMAN_TUI_INTEGRATIONS_REX_ENABLED`, or `--set tui.integrations.rex.enabled=off` for one run) → `Disabled`, meaning today's behaviour.
  2. `REX_SESSION` or `REX_BLOCK` unset or malformed (must match `session:`/`block:` + UUID) → `Absent`. With `"on"`, this is an error.
  3. `TMUX`, `ZELLIJ` or `STY` set (a nested multiplexer) → `Unavailable("nested multiplexer")`.
  4. `rex` resolved through `host_cli::program("rex")` (add an `AWMAN_TEST_REX` opt-in) → otherwise `Unavailable("rex CLI not found")`.
  5. Probe `server.version`, then `session.describe_block {session_id, block_id}`. This proves the variables are live, not inherited by a `nohup`'d or detached process. Each call has a 1.5 s timeout.
  6. Version gate `MIN_REX_VERSION`. A missing optional method (`method_not_found`) turns off that one part of the integration only.
  7. `server.whoami` must map to the current OS user. A Rex *shared server* may run blocks as the `rex` account, which can't reach the user's daemon or Docker → `Unavailable`.
- **The rule:** `Present` → this invocation **must** use the daemon `engine`, whatever `tui.backend` says (0155's `auto`/`daemon`). Detection reports `HostRequirement::Required(annotations)` to 0155's hand-off, which starts the daemon or enables `engine` ephemerally (0145) and sends the annotations in `Hello`.
  - The only exception is an explicit request for the builtin engine, with `tui.backend: "builtin"` (in config, or `--set tui.backend=builtin` for one run). That **disables the Rex integration** for the invocation, the same as `tui.integrations.rex.enabled: "off"`, and shows a one-line notice ("rex integration off: builtin engine requested").
  - If the hand-off fails under `auto` (for example, a legacy daemon is running or the engine is disabled by policy), awman runs in-process with today's TUI, as if Rex weren't there. It shows one status-bar or stderr notice with the reason.
  - Under `on`, it exits nonzero with that reason.
- `awman config show` reports one line (`rex: present v0.x · engine pid 1234`).

### 4. The Rex presenter (inside `engine`) — `src/engine/integrations/rex/`
- **`RexPresenter`** (`presenter.rs`) is a `HostPresenter` (0154) named `rex`. It is the only code that calls the Rex API.
  - It claims every frontend whose `Hello.annotations` contain the `rex/` namespace, along with their `UiRequest`s.
  - It records what it creates as **annotations on engine objects** through 0154's presenter hook: `rex/window` on a tab, and `rex/block` on a terminal view. Those annotations are visible in `awman daemon api instances.list --json` and can be filtered on with `annotation.<k>=<v>`, which calls `GET /v1/engine/instances?annotation=k=v` (0154, 0159). For example, `awman daemon api instances.list annotation.rex/block=$REX_BLOCK --json` returns the instance behind the current Rex block.
  - It observes engine events: tabs opened and closed, container-window state changes, 0146 `TerminalOpened`/`TerminalClosed`, 0137 step and phase envelopes, and 0152 status. It also reads `rex events` streams.
  - It emits `RexOp`s (`NewWindow`, `NewSplit`, `NewLayer`, `CloseBlock`, `CloseWindow`, `Zoom`, `SetLabel`, `Focus`) through one executor per Rex server.
  - Everything is keyed by `(server, rex_session_id, instance_id)`.
  - Future host integrations (tmux, zellij, WezTerm…) add sibling presenters under `src/engine/integrations/`.
- **`RexApi` client** (`client.rs`):
  - `trait RexApi: Send + Sync`, with typed serde structs in `src/engine/integrations/rex/types.rs` (pure). Unknown fields are ignored and missing optional fields default.
  - `CliRexApi` runs `rex [-S <server>] api call <method> -` with JSON on stdin, so label text stays out of argv, and maps a nonzero exit to `RexError { BadRequest | MethodNotFound | NotFound | SessionNotFound | Conflict | Other }`.
  - Calls go through one queue per server, with last-write-wins coalescing per target and method, because a `rex` spawn costs about 10–30 ms.
  - `FakeRexApi` (cfg(test)) records calls and returns scripted responses.
- **Events** (`events.rs`): one `rex -S <server> events -s <session> --json` child per presented Rex session. If it dies, it restarts with backoff (max 5 tries); after that the presenter reconciles by polling `session.view` every 5 s.
- **Layouts** (`layout.rs`, pure, with golden JSON tests):
  - `RexLayout::block(argv, cwd, keep_open)`, `hsplit`, `vsplit` and `tiled`.
  - **Always `shell: "none"`**, with an argv built from the daemon's own `current_exe()`, fixed subcommands and IDs validated against awman's ID grammar.
- **Ledger** (`ledger.rs`):
  - `<data>/rex/<sha16(server+rex_session)>.json` (0600, under the private-file lock helpers) maps `window_id/block_id → {kind: Tab | AgentView{terminal_id} | Workflow{command_id}, instance_id, awman_session_id}`.
  - It is owned by the presenter alone. It lets a restarted daemon re-adopt the Rex objects it created, and keeps it from touching anything it didn't create.
- **Reconciliation** follows the parity lifecycle in §2:
  - `window_closed` for a `Tab` → close-tab semantics, or instance-exit semantics for the instance's first window.
  - `block_closed` for an `AgentView` → the slot returns to Minimized.
  - `session_destroyed` → instance-exit semantics for every instance presented there.
  - A daemon restart re-adopts what is still live.

### 5. What is presented in Rex
- **Tabs as Rex windows** (`tui.integrations.rex.tabs: "windows"`, the default).
  - The instance's first frontend keeps its own block as tab 1. The presenter labels the window with the tab label + status glyph, at most 2/sec and only on change, and labels the block `awman · <repo>`.
  - `UiRequest::OpenTab{dir}` (after the unchanged Ctrl-T path dialog and session creation) → `session.new_window {layout: block([awman, view, --instance, <id>, --tab, <session-id>], cwd=dir), window_label}` in the instance's Rex session. `awman view` is a hidden 0155 entry point that renders one tab of an existing instance. It runs inside the new Rex terminal, so its own detection produces the `rex/*` annotations for that window's block, and the presenter matches them to the window it just opened.
  - `NextTab`/`PrevTab` → `session.focus_window` over the instance's `Tab` windows in tab order.
  - `CloseTab` → today's close-tab semantics, then `session.close_window`.
  - The TUI hides awman's tab bar (`render/tab_bar.rs` collapses to zero rows).
  - `tabs: "native"` keeps awman's tab bar in one block and doesn't open tab windows. Only agent views and workflow windows use Rex.
- **Maximized agents as Rex blocks.**
  - When a slot enters Maximized (Ctrl-M, or the maximize action), the presenter opens the slot's hub terminal as a block running `[awman, attach, <terminal-id>, --annotation, rex/view=<ledger-id>]` (`term_…`). That is plain 0146 `awman attach` (0159 §7) with a generic annotation and no Rex-specific flag.
  - `agentPlacement` picks the placement: `"split"` (beside the awman block, ratio 0.6, the default), `"window"` (a Rex window `🤖 <agent> · <tab>`) or `"float"` (`session.new_layer`).
  - Leaving Maximized closes the block (with `"split"`/`"float"`) or returns focus to the awman window (with `"window"`).
  - The awman block shows the slot exactly as Minimized does today, so the summary, Ctrl-Y copy and the overview are unchanged.
  - When an agent finishes while its block is open, the block keeps its final screen with a `✓`/`✗` label, just as today's maximized view shows the final screen. `closeFinishedAgents: true` closes it.
- **What the agent block gets from generic mechanisms:**
  - **`awman attach` writing to a TTY** always applies 0156's host-terminal output filter and OSC 7501 re-encoding. That is generic hygiene and has no flag.
  - **Block labels:** the presenter labels the block `agent:<short-id>` through the Rex API (`session.set_block_label`).
  - **Hang-up:** SIGHUP (block closed) makes `awman attach` report a hang-up, which the engine delivers to presenters as 0146's `TerminalViewerDetached{reason: hangup, annotations}`. The presenter recognises its `rex/view` annotation and treats it as "un-maximize".
  - **Input:** the attach holds the input grant for the instance's own agents, exactly as typing into today's maximized view does, unless `agentInput: false` adds the generic `--read-only`.
- **Workflow windows** (additive).
  - While a workflow with parallel steps runs, the presenter keeps a window `⚙ <workflow> · <tab>`. A pure planner, `layout_plan::plan(dag_snapshot, existing_blocks) -> Vec<RexOp>`, tiles the running steps' terminals (≤3 columns, then rows) and zooms a step that reports `blocked`.
  - The workflow overview (Ctrl-O), control board (Ctrl-W) and every step dialog stay in the awman block, unchanged.
  - The window never steals focus from a non-awman Rex window the user is in (check `active_window_changed`), and closes when the workflow ends.
  - `workflowWindows: false` turns it off.
- **Status.** TUI frontends emit 0156's OSC 7501 aggregate to their own block, and agent blocks emit their agent's state. Rex shows these as tab spinners, blocked badges and session-picker indicators. When Rex ships mobile push, the user gets it with no awman change.

### 6. Security design (aspec/architecture/security.md)
- **Only the Rex presenter talks to Rex**, through the `rex` CLI as the daemon's own user. It is a host control-plane binary like `docker`, so running it is not running an agent on the host. TUI frontends run `rex` only for the read-only detection probe.
- **Frontends reach the engine only over the daemon's local listener** (0145/0155). The Rex hand-off enables `engine` ephemerally, which keeps it off the network listener.
- **No Rex leakage into containers.**
  - 0156's env deny-list covers `REX_*` and `TERM_PROGRAM=Rex`.
  - Never mount the Rex runtime dir or socket (`$REX_RUNTIME_DIR`, `/run/rex`, `~/Library/…`).
  - **This item adds the Rex skill scrub:** if the user ran `rex skill setup`, Rex has added `~/.claude/skills/rex` and hooks in `~/.claude/settings.json`. The Claude overlay scrub (`src/engine/overlay/claude.rs` / `agent_settings.rs`) drops that skill directory and any hook that invokes `rex`.
- **Agent bytes never reach Rex's control API.** Agent output reaches a Rex *terminal* only through 0156's filter.
  - Every string in a Rex API call is sanitized: control characters and bidi stripped, a 64-character cap, and an allow-list of label characters. That covers labels built from agent titles, branch names, workflow names and repo names.
  - Argv comes only from awman's binary path, fixed subcommands and validated IDs.
- **Input reach.** Rex's own remote access (Tailscale, login server, iOS) can carry keystrokes into an agent block from another device. That is the same reach as typing into today's maximized view from whatever terminal hosts the TUI. The docs must state it, and `tui.integrations.rex.agentInput: false` makes agent blocks read-only (`--read-only`), leaving agent input to the awman block.
- **Remote Rex servers.** When `REX_SERVER` is remote, the presenter uses `rex -S` against it, but only if `server.whoami` and the server's host identity show that its blocks run on the daemon's machine as the daemon's user. Otherwise detection returns `Unavailable` and awman runs in-process.

### 7. Configuration: `tui.integrations.rex`
- Rex settings are one set of keys under a new top-level `tui` section. The TUI frontends read them, and so does the engine's presenter, which resolves them per session against that session's repo config. Future host-terminal integrations become siblings under `tui.integrations`. Daemon feature settings belong to 0145 (`daemon.features.*`), and daemon-work presentation keys (`daemonWork.*`) are added by 0157.
  ```json
  {
    "tui": {
      "integrations": {
        "rex": {
          "enabled": "auto",
          "tabs": "windows",
          "agentPlacement": "split",
          "agentInput": true,
          "closeFinishedAgents": false,
          "workflowWindows": true,
          "forwardAgentNotifications": false
        }
      }
    }
  }
  ```
  | Key | Values | Default | Scope | Meaning |
  |---|---|---|---|---|
  | `enabled` | `"auto"` \| `"on"` \| `"off"` | `"auto"` | `global` | `auto`: in Rex, use the engine and present through Rex, else in-process with a notice. `on`: a failure is an error. `off`: never probe. |
  | `tabs` | `"windows"` \| `"native"` | `"windows"` | `repo` (global + repo) | Tabs as Rex windows, or awman's own tab bar. |
  | `agentPlacement` | `"split"` \| `"window"` \| `"float"` | `"split"` | `repo` (global + repo) | Where a maximized agent opens. |
  | `agentInput` | bool | `true` | `repo` (global + repo) | Whether agent blocks accept input. |
  | `closeFinishedAgents` | bool | `false` | `repo` (global + repo) | Close an agent's block when it stops. |
  | `workflowWindows` | bool | `true` | `repo` (global + repo) | Show the additive tiled window for parallel steps. |
  | `forwardAgentNotifications` | bool | `false` | `repo` (global + repo) | Pass agents' desktop-notification OSCs to Rex (through 0156's filter). |
- Global + repo keys resolve per session, leaf by leaf. `enabled` in a repo config is a validation error naming the key. Unknown keys and unknown integration names are rejected with the valid list.
- For one invocation, use 0159's generic `--set tui.integrations.rex.enabled=off` (or any other key here, e.g. `--set tui.integrations.rex.agentPlacement=window`), or the derived `AWMAN_TUI_INTEGRATIONS_REX_*` variables. There is no integration flag of any kind.
- **Code shape:** `src/data/config/tui.rs` defines `TuiConfig { scrollbackLines, backend, programStatus, worker, integrations: Option<TuiIntegrationsConfig> }`, `TuiIntegrationsConfig { rex: Option<RexIntegrationConfig> }`. Each integration owns its config struct in its own config module (`src/data/config/integrations/rex.rs`, so Layer 0 stays free of engine imports), and there is one field per compiled-in integration in the registry. A shared `IntegrationMode { Auto, On, Off }` lives in `tui.rs`. It is the `tui` section of 0159's single `ConfigFile` struct. Each key is one row in the key table (`fields.rs`), which drives validation, scope, `config show/get/set/unset`, `--set` and `AWMAN_*`, and `effective.rs` resolves it.

### 8. Phases within this item
1. Detection, the hand-off rule, the config section, the Rex skill scrub, and window and block labels and status. Tabs and agents are still drawn inside the awman block (`tabs: "native"` behaviour).
2. Maximized agents as Rex blocks.
3. Tabs as Rex windows, with the full §2 lifecycle mapping.
4. Workflow windows.

Each phase is a separate PR that must pass the Rex parity suite (Test Considerations).

## Edge Case Considerations:
- **Stale variables:** `REX_*` inherited by `nohup`, `screen`, `ssh` or a detached process whose block is gone. The probe fails → in-process.
- **Nested multiplexers:** tmux, zellij or screen inside Rex, or Rex inside tmux. No hand-off; in-process with a notice.
- **The hand-off fails** (a legacy daemon, engine disabled by policy, a version mismatch the user declines): in-process with today's TUI. The user gets standard awman, never a half-integrated one.
- **Rex closes the instance's first window while other tab windows are open:** this is instance exit (§2), so the presenter closes the instance's remaining windows and agent blocks. That matches today, where killing the TUI terminal ends every tab.
- **The user drags an awman tab window into another Rex session, or reorders windows:** tab order follows Rex's window order within the instance's Rex session. A window moved to another session stays the instance's tab, and the ledger tracks it by ID. awman re-applies labels only on its own changes, never on `window_label_changed`, and stops relabelling a window the user renamed.
- **Several instances in one Rex session (two `awman`s typed in two terminals):** each owns only its own ledger entries. Ctrl-A/D never crosses into another instance's windows, exactly as two TUIs today never share tabs.
- **Several Rex sessions or servers using one daemon:** everything is keyed by `(server, rex_session, instance)`.
- **Focus stealing:** the presenter never focuses a window while the user is in a non-awman Rex window, except as the direct result of their own keypress (Ctrl-T, Ctrl-A/D, Ctrl-M).
- **The Rex server restarts while the instance runs:** the awman block itself is gone, so this is instance exit, the same as a terminal dying today. On the presenter side, the event stream backs off, the ledger is pruned, and nothing is left dangling.
- **The daemon restarts while the instance runs:** that is the engine going away, which 0155 treats as the backend dying. The TUI reports it as today's fatal error dialog would, and the presenter re-adopts or cleans up ledger entries on start.
- **Maximized agent finished before its block opened:** the hub snapshot shows the final screen, the same as maximizing a finished slot today.
- **Sandbox (dsbx) runtime:** output is already plain (ANSI and OSC stripped), the same as today's maximized view. **ACP agents:** their maximized state renders inside the awman block as today; no Rex block.
- **`TERM=xterm-rex`:** 0156 normalises the containers' `TERM`.
- **Windows:** Rex uses named pipes. Detection must not assume Unix socket paths.
- **Scripts in a Rex terminal:** `awman exec … --headless` goes through the engine with identical stdout, stderr and exit codes (0155). `--set tui.integrations.rex.enabled=off` (or `--set tui.backend=builtin`) keeps the whole invocation in-process.

## Test Considerations:
- **Unit (pure, Layer 0/1):**
  - The detection table over `EnvSnapshot` × probe results. Malformed or injected IDs are rejected.
  - The hand-off rule: `Present` → engine; a failure under `auto` → in-process; under `on` → error.
  - `RexLayout` golden JSON. `RexApi` request serialization against the documented schemas. Tolerant response parsing. Error-code mapping. The `RexEvent` NDJSON parser.
  - The lifecycle mapping: each `RexEvent` (`window_closed` for tab N, for the first tab, `block_closed` for an agent view, `session_destroyed`) → the expected engine action (close tab, exit instance, un-maximize).
  - `layout_plan::plan`: 1, 2, 3, 5 and 8 parallel steps; blocked → zoom; idempotence.
  - The label sanitizer and argv builder.
  - Config: `tui.integrations.rex.*` parse and defaults; per-leaf resolution; repo `enabled` rejected.
  - Ledger read, write and prune, with instance isolation.
- **Rex parity suite (integration):** this is the heart of the item. It runs the *existing* TUI key-handler, dialog and command-flow tests (`src/frontend/tui/tests/`, `tabs/tests.rs`) three ways:
  1. builtin in-process engine;
  2. daemon `engine` with a plain frontend (0154/0155's suite);
  3. daemon `engine` with a frontend whose `Hello.annotations` carry `rex/*` and `FakeRexApi`.
  
  For each test, assert that the **engine-side outcome is identical** across all three: sessions, commands run, dialogs raised and their answers, container lifecycle, and exit codes. The rendered awman-block frames must also be identical, except for the tab bar region, which is collapsed in Rex mode with `tabs: "windows"`. The Rex run additionally asserts the expected `RexOp` sequence.
  - Key flows: Ctrl-T → `new_window` with the right argv and cwd; Ctrl-A/D → `focus_window` within the instance; Ctrl-M cycle → open and close the agent block; Ctrl-C / `QuitConfirm` → close all instance windows and apply today's exit policy.
  - Rex-initiated closes map to today's hang-up semantics. A second instance in the same Rex session is fully isolated.
  - The fake `rex` binary lives in a test temp dir and is resolved through `host_cli::program("rex")`. It **must not mutate the process-wide `PATH`** (per the recent CI fix).
  - Fallback: every `RexApi` method failing → the instance still behaves exactly like case 1, with agents and tabs drawn in the awman block.
  - Security regressions:
    - the Claude scrub drops `skills/rex` and `rex` hooks;
    - no Rex paths in container mounts;
    - with `agentInput: false`, agent blocks are `--read-only`;
    - a remote `REX_SERVER` failing the identity check → in-process.
- **End-to-end:** a gated `AWMAN_TEST_REX=1` suite with a real Rex Linux server in a temp `--runtime-root` and a real daemon:
  - `awman` in a Rex terminal from `rex new` → `rex api call session.view` shows the expected windows, labels and blocks;
  - `rex capture -b agent:<id>` shows the agent's screen;
  - Ctrl-T, Ctrl-M and quit produce the same container and session state as the same script run in a plain terminal;
  - `--set tui.integrations.rex.enabled=off`, `AWMAN_TUI_INTEGRATIONS_REX_ENABLED=off`, `--set tui.backend=builtin` and `enabled: "off"` in config each give byte-identical behaviour to a plain terminal.
  
  It is not in the default `make test` (Rex is beta and not in CI images). The run instructions go in `aspec/devops/localdev.md` as a follow-up.

## Codebase Integration:
- follow established conventions, best practices, testing, and architecture patterns from the project's aspec.
- **Layering:**
  - **Layer 0**
    - No Rex types in `src/data/` apart from its config struct. The pure Rex types (IDs, `RexContext` and its annotation codec, API structs, layouts, `RexOp`, and the sanitizer) live in `src/engine/integrations/rex/{types,context}.rs`, which have no I/O, so they can be unit-tested like Layer 0. The generic `Annotations` type is in `src/data/annotations.rs`.
    - `src/data/config/tui.rs` and `src/data/config/integrations/rex.rs` (config only).
  - **Layer 1** `src/engine/integrations/rex/{mod,detect,client,events,layout,layout_plan,ledger,presenter}.rs`. The presenter registers with 0154's engine through its host-presenter hook and consumes 0146's terminal hub.
  - **Layer 2** The hand-off rule plugs into 0155's `engine_handoff` through the integration registry. Rex's `detect()` returns `HostRequirement::Required(annotations)`, with its `rex/*` annotations.
  - **Layer 3** Minimal and presentation-only:
    - `render/tab_bar.rs` collapses when the host presents tabs;
    - `app.rs` sends maximize, tab and close transitions as 0155 `UiRequest`s when a host presenter claims them;
    - otherwise the existing code paths run unchanged.
  
  **No business logic branches on Rex in the TUI.**
- **Reuse existing seams.** The container-window state machine (`container_window_state`, Hidden/Minimized/Maximized) and tab operations (`add_tab`, `switch_to_*_tab`, `close_active_tab`) are the hook points. The presenter mirrors their transitions and doesn't add new states.
- **Host CLI:** add `"rex"` to `src/engine/host_cli.rs` with an `AWMAN_TEST_REX` opt-in. Every `rex` spawn goes through it.
- **Catalogue:** nothing Rex-specific. There are no integration flags (0159 `--set` covers them). The hidden `view` entry point comes from 0155, `attach <terminal-id> --annotation/--read-only` comes from 0146, and `awman integration …` comes from 0158.
- **Security hooks:** the Rex skill and hook scrub in `src/engine/overlay/claude.rs` / `agent_settings.rs`, with tests beside `claude_scrub_tests.rs`. The ledger uses the private-file helpers.
- **Keep it simple** (design.md): Rex is a presenter, not a mode; one component calls Rex; v1 shells out to the documented `rex` CLI; parity tests guard every phase; and `auto` always has an in-process fallback.

## Documentation

After implementation is complete, update user-facing documentation in `docs/` to reflect the current state of the tool:

- **Update existing feature docs** (e.g., if implementing headless features, update `docs/08-headless-mode.md`)
- **Create new user guides only if a new user-visible feature warrants it** (e.g., `docs/10-my-feature.md`)
- **Never create work-item-specific docs** (e.g., no "WI 0123 implementation guide" in published docs)
- **Keep all technical/implementation details in work item specs or code comments**, not in `docs/`
- **Docs are for end users**, not for developers trying to understand implementation

See `CLAUDE.md` for more guidance on documentation standards.

Specifically for this item:
- New `docs/18-rex-integration.md` (0145 claims `16-daemon.md`, 0147 claims `17-workers-and-work-servers.md`, and 0154/0155 claim `19-engine-and-frontends.md`). It covers:
  - what looks different in Rex (tabs as Rex windows, maximized agents as Rex blocks, workflow windows, status in Rex), and the fact that everything else behaves exactly as in a normal terminal;
  - the `tui.integrations.rex.*` keys, and switching them for one run with `--set`;
  - input reach through Rex remote access;
  - fallback notices;
  - runtime limits (ACP, sandbox).
- `docs/02-using-the-tui.md`: a short "In Rex" note on Ctrl-T, Ctrl-A/D and Ctrl-M.
- `docs/07-configuration.md`: the `tui` section.
- `docs/04-security-and-isolation.md`: only the daemon's presenter talks to Rex, and agents never get Rex access.
- `docs/14-command-reference.md`: nothing Rex-specific. `--set` is documented by 0159.
- `docs/contents.md`: link the new guide.
