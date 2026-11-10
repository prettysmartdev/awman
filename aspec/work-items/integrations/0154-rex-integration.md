# Work Item: Feature

Title: Rex integration
Issue: issuelink

Depends on: 0143 (the complete 0139–0143 daemon, including the finalized config/API surface, terminal hub, OSC status, host-terminal hygiene, singleton engine and frontends). Used by: 0155 (daemon work in Rex), 0156 (Rex action pack).

## Summary:
- Rex (Superlogical, beta) is a terminal multiplexer driven by a server. It organises work as **sessions → windows (tabs) → blocks (terminals)**, with **clients** (the macOS and iOS apps, the `rex` CLI, scripts) attached to a long-lived server. All of it is scriptable through a typed Server API (https://www.superlogical.com/rex/docs/automate/server-api), through `rex api call <method> <json>` and through Lua. Rex is also where OSC 7501, the Program Status Protocol that WI 0141 adopts, was defined.
- Inside every Rex terminal, Rex sets `REX_SESSION`, `REX_BLOCK` and `REX_SERVER`. That makes detection cheap and reliable. A live probe (`server.version`, `session.describe_block`) confirms the variables are not stale.
- Today, awman inside Rex is a multiplexer running inside a multiplexer. The awman tab bar duplicates Rex's tabs, a vt100 emulator duplicates Rex's terminal engine, and agent screens can't be seen from the Rex mobile app, searched with Rex scrollback, or captured with `rex capture`.
- **This work item:** when awman detects that it is running in Rex, it **uses the awman daemon's `engine` feature as its backend** and **presents through Rex**:
  - awman tabs become Rex windows;
  - maximized agent containers become Rex terminal blocks;
  - parallel workflow steps become a tiled Rex window.

  The daemon's `engine` is the singleton awman instance behind every awman invocation in Rex. No awman process in Rex runs its own engine. The Layer 3 Rex presenter owns Rex windows and calls the Rex API through a Layer 2 facade.
- **Parity is the core requirement.** Using awman in Rex, or using the normal TUI with the daemon `engine` as its backend, must be **indistinguishable from running the TUI normally with the builtin in-process engine**. That covers keys, dialogs, commands, output, lifecycle, errors and environment. The only difference in Rex is *where* things are drawn: Rex windows instead of awman's tab bar, and Rex blocks instead of the maximized container view. Rex-native extras are strictly additive: status in Rex's tab bar, `rex capture`, palette actions.
- **Config:** one set of keys, `tui.integrations.rex.*`, resolved per session by Layer 0; Layer 2 applies integration policy and supplies the effective typed values to the TUI and Rex frontend presenter. `tui.integrations.rex.enabled` is `auto` by default. Outside Rex nothing changes.
- **Security is unchanged.**
  - Only the Rex frontend presenter calls the Rex API through the Layer 2 facade.
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
- **OSC 7501** is defined by Rex. Rex shows it as tab spinners, blocked symbols and session-picker indicators, and later as mobile push. WI 0141 makes awman parse it.

### 1. Work item breakdown and dependencies
The full design spans several work items. This item (0154) owns the Rex presenter; the related work items and their dependencies are below.

| WI | Scope | What 0154 needs from it |
|---|---|---|
| **0140**: one daemon, one API, features | Replace *modes* with **one API** whose **features** are enabled and disabled independently at runtime:<br>• **`squad`**: today's squad functionality, unchanged;<br>• **`worker`**: jobs pushed (today's API server and `awman remote`) or pulled (work servers, infact pull mode);<br>• **`engine`**: a singleton awman instance.<br>Also: a local listener (Unix socket with peer-UID check, or a named pipe) plus an optional network listener with `network.features`, **ephemeral enablement** (local-only, idles out), `daemon.features.*` config (written with `awman config set`, applied live), and the minimal `awman daemon start\|stop\|status\|logs\|api` command surface. There are no compatibility spellings. | Ephemeral, local-only `engine` enablement from a frontend's hand-off. `engine` never exposed on the network listener unless persistently listed. |
| **0143**: `worker` feature, push and pull | **Push** covers today's API server, moved under `/v1/worker/*` (`sessions`, `commands`, `runs`, `workdirs`; 0139 §5), and `awman remote exec`. **Pull** covers Work Server Protocol claims (infact pull mode), with work servers configured in the top-level `workServers.<name>` map (0139). Both share one job model, executor share and event stream. | Nothing directly. Rex presentation of worker jobs is WI 0155. |
| **0141, terminal part**: terminal hub and interaction | The terminal hub and screen mirrors move into the daemon's shared core and serve both `worker` and `engine`. Per-feature interaction defaults. **Dialogs relayed to engine frontends** (not only yolo answers). Terminals are the core `/v1/terminals/{id}` (0139). `awman attach <terminal-id>` gains `--read-only` and the generic `--annotation k=v`, and reports hang-ups. | Hub terminals for every engine-session agent, late-joiner snapshots, `awman attach`, and the input grant. |
| **0141, status part**: OSC 7501 parsing | Already specified. | Per-agent `EffectiveStatus`. |
| **0142, engine host**: `engine` feature (singleton awman instance) | Host everything `tui::run()` builds in-process today, once, in the daemon: `SessionManager`, tab command threads, dialogs, workflows, git polling and the squad indicator. Includes:<br>• the **TUI-instance model**: sessions belong to the frontend instance that opened them, exactly as with the builtin engine;<br>• the **per-session environment**: a session runs host-side operations with the launching frontend's `EnvSnapshot` and cwd;<br>• config resolved per session exactly as in-process;<br>• the **parity contract** and its conformance suite. | The Layer 2 instance host that the Rex presenter observes through a typed interface. |
| **0142, frontend part**: frontend protocol, and the TUI and CLI as engine frontends | `awman.frontend.v1` on `/v1/engine/instances/{id}/frontend` over the local listener. It serialises the existing `Tab` ↔ command-thread channels (`command_result_rx`, `dialog_request_rx`, `dialog_response_tx`, `TabSharedState`, `ContainerSlotEvent`). Also:<br>• `RemoteTabBackend`;<br>• moving `DialogRequest`/`DialogResponse` to `src/data/` with serde;<br>• CLI relay (output, prompts, Ctrl-C, exit codes);<br>• the hand-off (`tui.backend: auto\|daemon\|builtin`, overridable for one run with `--set`; starting or ephemerally enabling the daemon; the version handshake);<br>• generic `annotations` on `Hello` (namespaced keys such as `rex/session`), plus a `UiRequest` channel for window-management requests (`OpenTab`, `NextTab`, `PrevTab`, `CloseTab`, `SetContainerWindow{slot, state}`, `OpenOverview`, `FocusWorkflowWindow`, `FocusWork`) that a host presenter may claim. There are no integration flags; switches are `tui.integrations.<name>.enabled`, overridable with `--set`. | A frontend that carries the Rex annotations and hands window-management requests to the engine. |
| **0141, hygiene part**: host-terminal hygiene | Ships with the terminal and status foundation.<br>• A host-terminal output filter for agent bytes written to any host terminal: strip OSC 52 / 7 / 9 / 99 / 777 / 1337, kitty file transfer, DCS/APC; re-encode OSC 7501.<br>• OSC 7501 **emission** from the TUI (aggregate + per-agent records, awman-originated `blocked` for waiting dialogs).<br>• A container env deny-list for host-multiplexer variables (`REX_*`, `TMUX*`, `ZELLIJ*`, `TERM_PROGRAM`).<br>• `TERM` normalisation for containers. | The filter and emitter used by Rex agent blocks and TUI frontends. |
| **0154 (this item): Rex integration** | Detection, the hand-off rule "in Rex → engine", the Rex presenter, sessions as Rex windows, agents as Rex blocks, workflow layouts, the `tui.integrations.rex` config section, the Rex-specific security scrub, and Rex parity tests. | none |
| **0155: daemon work in Rex** | Present `squad` tasks and `worker` jobs (push and pull) in a dedicated Rex session, configured with `tui.integrations.rex.daemonWork.{show,session,server,allowInput}`. Includes read-only attach by default (with 0141 `take_control` auditing when `allowInput` is on), a 12-window cap with an overflow window, an ephemeral `engine` hold, and squad tab or `squad attach` → "focus task window". | Builds on 0154's presenter and ledger (`DaemonWork{feature, id}`). |
| **0156: Rex action pack** | The generic `awman integration <status\|setup\|remove> [<name>]` command. For Rex, `setup` generates a Lua pack under `~/.awman/integrations/rex/` and prints the `dofile` line for `init.lua`, which awman never edits. Rex palette actions call the awman CLI, using client-side `awman daemon api` actions (0139 §6): `instances.list annotation.rex/block=$REX_BLOCK`, and the `steps.approve` and `overview.open` compositions over `/v1/engine/instances/{id}/dialogs/{id}/answer` and `/ui_requests`. | Builds on 0154 and 0142. |

**Order:** complete the entire 0139–0143 daemon block, including the 0139 config/API audit, before this Rex integration. The reference GitHub work server (0153) is separate. WI0155 and WI0156 follow this presenter and can proceed independently.

### 2. Parity contract (Rex-specific part)
WI0142 defines the general contract: the TUI against the daemon `engine` behaves exactly like the TUI with the builtin engine. Its conformance suite runs the existing TUI and CLI behaviour tests against both backends. This item extends that contract to Rex: **Rex changes where things are drawn, never what awman does.**

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
- **Same output, errors and environment.** Sessions run with the launching frontend's environment and cwd (0142), so `PATH`, Docker context, SSH agent, credential helpers and git config are the ones from the user's Rex shell. Config resolution, mount-scope prompts, prerequisites, errors and exit codes are identical.
- **No new UI noise.** In Rex the TUI shows no "running on engine" banner. Notices appear only when something differs from the user's expectation: fallback to in-process, or a version-mismatch restart. The engine state is visible in `awman config show` / `awman daemon status`, not in the TUI.
- **Feel.** Rendering stays in the frontend process. The command box, scrolling and dialogs are local, and only command and dialog traffic crosses the local socket. Agent keystrokes go to the hub. Budget: under 5 ms of added latency per round trip on the local listener, measured in the conformance suite.
- **Allowed differences (additive only):**
  - status in Rex's tab bar and session picker;
  - Rex labels (`awman · <repo>`, `agent:<id>`) that `rex capture`/`rex send`/`rex wait` can address;
  - native Rex scrollback, search and zoom inside agent blocks;
  - workflow windows (§5), which open only *in addition to* today's workflow overview and never replace a dialog.

### 3. Detection and the hand-off rule
- Rex is one entry in 0142's Layer 2 integration registry (`src/command/integrations/mod.rs`). Detection returns the Rex **annotations** that travel in 0142's `Hello.annotations`:
  - `rex/server` (when `REX_SERVER` is set), `rex/session`, `rex/block` and `rex/version`;
  - `rex/client` when `REX_CLIENT` is set.

  `RexContext { server: Option<String>, session_id, block_id, client: Option<String>, version: RexVersion }` (`src/data/rex/context.rs`) is parsed from and rendered to those annotations. Rex-specific fields stay out of the generic frontend protocol; its serializable values belong in Layer 0. `RexDetection` is `Absent | Disabled(reason) | Unavailable(reason) | Present(RexContext)`.
- Detection is a Layer 2 decision over an injected `EnvSnapshot` and a Layer 1 probe trait (`src/command/integrations/rex/detect.rs`), run once per invocation before dispatch picks a backend:
  1. `tui.integrations.rex.enabled == "off"` (from config, `AWMAN_TUI_INTEGRATIONS_REX_ENABLED`, or `--set tui.integrations.rex.enabled=off` for one run) → `Disabled`, meaning today's behaviour.
  2. `REX_SESSION` or `REX_BLOCK` unset or malformed (must match `session:`/`block:` + UUID) → `Absent`. With `"on"`, this is an error.
  3. `TMUX`, `ZELLIJ` or `STY` set (a nested multiplexer) → `Unavailable("nested multiplexer")`.
  4. `rex` resolved through `host_cli::program("rex")` (add an `AWMAN_TEST_REX` opt-in) → otherwise `Unavailable("rex CLI not found")`.
  5. Probe `server.version`, then `session.describe_block {session_id, block_id}`. This proves the variables are live, not inherited by a `nohup`'d or detached process. Each call has a 1.5 s timeout.
  6. Version gate `MIN_REX_VERSION`. A missing optional method (`method_not_found`) turns off that one part of the integration only.
  7. `server.whoami` must map to the current OS user. A Rex *shared server* may run blocks as the `rex` account, which can't reach the user's daemon or Docker → `Unavailable`.
- **The rule:** `Present` → this invocation **must** use the daemon `engine`, whatever `tui.backend` says (0142's `auto`/`daemon`). Detection reports `HostRequirement::Required(annotations)` to 0142's hand-off, which starts the daemon or enables `engine` ephemerally (0140) and sends the annotations in `Hello`.
  - The only exception is an explicit request for the builtin engine, with `tui.backend: "builtin"` (in config, or `--set tui.backend=builtin` for one run). That **disables the Rex integration** for the invocation, the same as `tui.integrations.rex.enabled: "off"`, and shows a one-line notice ("rex integration off: builtin engine requested").
  - If the hand-off fails under `auto` (for example, a legacy daemon is running or the engine is disabled by policy), awman runs in-process with today's TUI, as if Rex weren't there. It shows one status-bar or stderr notice with the reason.
  - Under `on`, it exits nonzero with that reason.
- `awman config show` reports one line (`rex: present v0.x · engine pid 1234`).

### 4. The Rex presenter (Layer 3) — `src/frontend/rex/`
- **`RexPresenter`** (`presenter.rs`) implements the Layer 2 `HostPresenter` contract (0142) and is named `rex`. It uses a Layer 2 Rex client facade for Rex operations; it never calls engine objects directly.
  - It claims every frontend whose `Hello.annotations` contain the `rex/` namespace, along with their `UiRequest`s.
  - It records what it creates as **annotations on engine objects** through 0142's presenter hook: `rex/window` on a tab, and `rex/block` on a terminal view. Those annotations are visible in `awman daemon api instances.list --json` and can be filtered on with `annotation.<k>=<v>`, which calls `GET /v1/engine/instances?annotation=k=v` (0142, 0139). For example, `awman daemon api instances.list annotation.rex/block=$REX_BLOCK --json` returns the instance behind the current Rex block.
  - It observes typed Layer 2 instance and terminal projections: tabs opened and closed, container-window state changes, 0141 terminal events, 0137 step and phase envelopes, and 0141 status. It also reads `rex events` through the Rex client facade.
  - It emits `RexOp`s (`NewWindow`, `NewSplit`, `NewLayer`, `CloseBlock`, `CloseWindow`, `Zoom`, `SetLabel`, `Focus`) through one executor per Rex server.
  - Everything is keyed by `(server, rex_session_id, instance_id)`.
  - Future host integrations (tmux, zellij, WezTerm…) add sibling presenters under `src/frontend/`.
- **`RexApi` client** (`src/command/integrations/rex/client.rs`):
  - `trait RexApi: Send + Sync`, with typed serde structs in `src/data/rex/types.rs`. The facade calls Layer 1 host CLI transport; unknown fields are ignored and missing optional fields default.
  - `CliRexApi` runs `rex [-S <server>] api call <method> -` with JSON on stdin, so label text stays out of argv, and maps a nonzero exit to `RexError { BadRequest | MethodNotFound | NotFound | SessionNotFound | Conflict | Other }`.
  - Calls go through one queue per server, with last-write-wins coalescing per target and method, because a `rex` spawn costs about 10–30 ms.
  - `FakeRexApi` (cfg(test)) records calls and returns scripted responses.
- **Events** (`src/command/integrations/rex/events.rs`): one `rex -S <server> events -s <session> --json` child per presented Rex session via the Layer 1 host CLI transport. If it dies, it restarts with backoff (max 5 tries); after that the presenter reconciles by polling `session.view` every 5 s.
- **Layouts** (`src/data/rex/layout.rs`, pure, with golden JSON tests):
  - `RexLayout::block(argv, cwd, keep_open)`, `hsplit`, `vsplit` and `tiled`.
  - **Always `shell: "none"`**, with an argv built from the daemon's own `current_exe()`, fixed subcommands and IDs validated against awman's ID grammar.
- **Ledger** (`src/data/rex/ledger.rs`):
  - `<data>/rex/<sha16(server+rex_session)>.json` (0600, under the private-file lock helpers) maps `window_id/block_id → {kind: Tab | AgentView{terminal_id} | Workflow{command_id}, instance_id, awman_session_id}`.
  - Layer 0 persists it under a Layer 2 ownership policy; only the presenter requests records for objects it created. It lets a restarted daemon re-adopt those objects.
- **Reconciliation** reports typed host events to the Layer 2 instance host, which applies the parity lifecycle in §2:
  - `window_closed` for a `Tab` → Layer 2 applies close-tab semantics, or instance-exit semantics for the instance's first window.
  - `block_closed` for an `AgentView` → Layer 2 changes the slot to Minimized.
  - `session_destroyed` → Layer 2 applies instance-exit semantics for every instance presented there.
  - A daemon restart re-adopts what is still live.

### 5. What is presented in Rex
- **Tabs as Rex windows** (`tui.integrations.rex.tabs: "windows"`, the default).
  - The instance's first frontend keeps its own block as tab 1. The presenter labels the window with the tab label + status glyph, at most 2/sec and only on change, and labels the block `awman · <repo>`.
  - `UiRequest::OpenTab{dir}` (after the unchanged Ctrl-T path dialog and session creation) → `session.new_window {layout: block([awman, view, --instance, <id>, --tab, <session-id>], cwd=dir), window_label}` in the instance's Rex session. `awman view` is a hidden 0142 entry point that renders one tab of an existing instance. It runs inside the new Rex terminal, so its own detection produces the `rex/*` annotations for that window's block, and the presenter matches them to the window it just opened.
  - `NextTab`/`PrevTab` → `session.focus_window` over the instance's `Tab` windows in tab order.
  - `CloseTab` → today's close-tab semantics, then `session.close_window`.
  - The TUI hides awman's tab bar (`render/tab_bar.rs` collapses to zero rows).
  - `tabs: "native"` keeps awman's tab bar in one block and doesn't open tab windows. Only agent views and workflow windows use Rex.
- **Maximized agents as Rex blocks.**
  - When a slot enters Maximized (Ctrl-M, or the maximize action), the presenter opens the slot's hub terminal as a block running `[awman, attach, <terminal-id>, --annotation, rex/view=<ledger-id>]` (`term_…`). That is plain 0141 `awman attach` (0139 §7) with a generic annotation and no Rex-specific flag.
  - `agentPlacement` picks the placement: `"split"` (beside the awman block, ratio 0.6, the default), `"window"` (a Rex window `🤖 <agent> · <tab>`) or `"float"` (`session.new_layer`).
  - Leaving Maximized closes the block (with `"split"`/`"float"`) or returns focus to the awman window (with `"window"`).
  - The awman block shows the slot exactly as Minimized does today, so the summary, Ctrl-Y copy and the overview are unchanged.
  - When an agent finishes while its block is open, the block keeps its final screen with a `✓`/`✗` label, just as today's maximized view shows the final screen. `closeFinishedAgents: true` closes it.
- **What the agent block gets from generic mechanisms:**
  - **`awman attach` writing to a TTY** always applies 0141's host-terminal output filter and OSC 7501 re-encoding. That is generic hygiene and has no flag.
  - **Block labels:** the presenter labels the block `agent:<short-id>` through the Rex API (`session.set_block_label`).
  - **Hang-up:** SIGHUP (block closed) makes `awman attach` report a hang-up, which the engine delivers to presenters as 0141's `TerminalViewerDetached{reason: hangup, annotations}`. The presenter recognises its `rex/view` annotation and treats it as "un-maximize".
  - **Input:** the attach holds the input grant for the instance's own agents, exactly as typing into today's maximized view does, unless `agentInput: false` adds the generic `--read-only`.
- **Workflow windows** (additive).
  - While a workflow with parallel steps runs, the presenter keeps a window `⚙ <workflow> · <tab>`. A pure planner, `layout_plan::plan(dag_snapshot, existing_blocks) -> Vec<RexOp>`, tiles the running steps' terminals (≤3 columns, then rows) and zooms a step that reports `blocked`.
  - The workflow overview (Ctrl-O), control board (Ctrl-W) and every step dialog stay in the awman block, unchanged.
  - The window never steals focus from a non-awman Rex window the user is in (check `active_window_changed`), and closes when the workflow ends.
  - `workflowWindows: false` turns it off.
- **Status.** TUI frontends emit 0141's OSC 7501 aggregate to their own block, and agent blocks emit their agent's state. Rex shows these as tab spinners, blocked badges and session-picker indicators. When Rex ships mobile push, the user gets it with no awman change.

### 6. Security design (aspec/architecture/security.md)
- **Only the Rex presenter talks to Rex**, through the `rex` CLI as the daemon's own user. It is a host control-plane binary like `docker`, so running it is not running an agent on the host. TUI frontends run `rex` only for the read-only detection probe.
- **Frontends reach the engine only over the daemon's local listener** (0140/0142). The Rex hand-off enables `engine` ephemerally, which keeps it off the network listener.
- **No Rex leakage into containers.**
  - 0141's env deny-list covers `REX_*` and `TERM_PROGRAM=Rex`.
  - Never mount the Rex runtime dir or socket (`$REX_RUNTIME_DIR`, `/run/rex`, `~/Library/…`).
  - **This item adds the Rex skill scrub:** if the user ran `rex skill setup`, Rex has added `~/.claude/skills/rex` and hooks in `~/.claude/settings.json`. The Claude overlay scrub (`src/engine/overlay/claude.rs` / `agent_settings.rs`) drops that skill directory and any hook that invokes `rex`.
- **Agent bytes never reach Rex's control API.** Agent output reaches a Rex *terminal* only through 0141's filter.
  - Every string in a Rex API call is sanitized: control characters and bidi stripped, a 64-character cap, and an allow-list of label characters. That covers labels built from agent titles, branch names, workflow names and repo names.
  - Argv comes only from awman's binary path, fixed subcommands and validated IDs.
- **Input reach.** Rex's own remote access (Tailscale, login server, iOS) can carry keystrokes into an agent block from another device. That is the same reach as typing into today's maximized view from whatever terminal hosts the TUI. The docs must state it, and `tui.integrations.rex.agentInput: false` makes agent blocks read-only (`--read-only`), leaving agent input to the awman block.
- **Remote Rex servers.** When `REX_SERVER` is remote, the presenter uses `rex -S` against it, but only if `server.whoami` and the server's host identity show that its blocks run on the daemon's machine as the daemon's user. Otherwise detection returns `Unavailable` and awman runs in-process.

### 7. Configuration: `tui.integrations.rex`
- Rex settings are one set of keys under a new top-level `tui` section. Layer 0 resolves them per session against that session's repo config; Layer 2 applies hand-off and presentation policy and supplies effective typed values to the TUI and Rex presenter. Future host-terminal integrations become siblings under `tui.integrations`. Daemon feature settings belong to 0140 (`daemon.features.*`), and daemon-work presentation keys (`daemonWork.*`) are added by 0155.
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
  | `forwardAgentNotifications` | bool | `false` | `repo` (global + repo) | Pass agents' desktop-notification OSCs to Rex (through 0141's filter). |
- Global + repo keys resolve per session, leaf by leaf. `enabled` in a repo config is a validation error naming the key. Unknown keys and unknown integration names are rejected with the valid list.
- For one invocation, use 0139's generic `--set tui.integrations.rex.enabled=off` (or any other key here, e.g. `--set tui.integrations.rex.agentPlacement=window`), or the derived `AWMAN_TUI_INTEGRATIONS_REX_*` variables. There is no integration flag of any kind.
- **Code shape:** `src/data/config/tui.rs` defines `TuiConfig { scrollbackLines, backend, programStatus, worker, integrations: Option<TuiIntegrationsConfig> }`, `TuiIntegrationsConfig { rex: Option<RexIntegrationConfig> }`. Each integration owns its config struct in its own config module (`src/data/config/integrations/rex.rs`, so Layer 0 stays free of engine imports), and there is one field per compiled-in integration in the registry. A shared `IntegrationMode { Auto, On, Off }` lives in `tui.rs`. It is the `tui` section of 0139's single `ConfigFile` struct. Each key is one row in the key table (`fields.rs`), which drives validation, scope, `config show/get/set/unset`, `--set` and `AWMAN_*`, and `effective.rs` resolves it.

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
- **The daemon restarts while the instance runs:** that is the engine going away, which 0142 treats as the backend dying. The TUI reports it as today's fatal error dialog would, and the presenter re-adopts or cleans up ledger entries on start.
- **Maximized agent finished before its block opened:** the hub snapshot shows the final screen, the same as maximizing a finished slot today.
- **Sandbox (dsbx) runtime:** output is already plain (ANSI and OSC stripped), the same as today's maximized view. **ACP agents:** their maximized state renders inside the awman block as today; no Rex block.
- **`TERM=xterm-rex`:** 0141 normalises the containers' `TERM`.
- **Windows:** Rex uses named pipes. Detection must not assume Unix socket paths.
- **Scripts in a Rex terminal:** `awman exec … --headless` goes through the engine with identical stdout, stderr and exit codes (0142). `--set tui.integrations.rex.enabled=off` (or `--set tui.backend=builtin`) keeps the whole invocation in-process.

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
  2. daemon `engine` with a plain frontend (0142's suite);
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
- Follow `aspec/architecture/2026-grand-architecture.md` and WI0142's HostPresenter contract. Host-specific presentation belongs in Layer 3; generic command and ownership policy stays in Layer 2; transport mechanics stay in Layer 1; Rex wire and ledger data stay in Layer 0.
- follow established conventions, best practices, testing, and architecture patterns from the project's aspec.
- **Layering:**
  - **Layer 0** `src/data/rex/` owns Rex IDs, `RexContext`, annotation codec, API structs, layouts, `RexOp`, sanitizer and ledger persistence; `src/data/annotations.rs` owns generic annotations; `src/data/config/{tui,integrations/rex}.rs` owns config. These exchangeable types have no command or frontend imports.
  - **Layer 1** `src/engine/host_cli.rs` owns Rex CLI process transport and probe mechanics. It has no Rex presentation, command policy or frontend imports.
  - **Layer 2** `src/command/integrations/{mod,rex/}` owns detection and hand-off policy, the `RexApi` facade, event subscriptions, ledger ownership and typed host-presenter contract. It exposes instance and terminal projections and typed UI requests to the presenter; the frontend never calls Layer 1 objects directly.
  - **Layer 3** `src/frontend/rex/{presenter,layout_plan}.rs` owns Rex windows, blocks, labels, status display, reconciliation and host interaction. It implements 0142's `HostPresenter` contract. The existing TUI also adapts presentation:
    - `render/tab_bar.rs` collapses when the host presents tabs;
    - `app.rs` sends maximize, tab and close transitions as 0142 `UiRequest`s when a host presenter claims them;
    - otherwise the existing code paths run unchanged.

  **No business logic branches on Rex in the TUI.**
- **Reuse existing seams.** The container-window state machine (`container_window_state`, Hidden/Minimized/Maximized) and tab operations (`add_tab`, `switch_to_*_tab`, `close_active_tab`) are the hook points. The presenter mirrors their transitions and doesn't add new states.
- **Host CLI:** add `"rex"` to `src/engine/host_cli.rs` with an `AWMAN_TEST_REX` opt-in. Every `rex` spawn goes through it.
- **Catalogue:** nothing Rex-specific. There are no integration flags (0139 `--set` covers them). The hidden `view` entry point comes from 0142, `attach <terminal-id> --annotation/--read-only` comes from 0141, and `awman integration …` comes from 0156.
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
- New `docs/18-rex-integration.md` (0140 claims `16-daemon.md`, 0143 claims `17-workers-and-work-servers.md`, and 0142 claims `19-engine-and-frontends.md`). It covers:
  - what looks different in Rex (tabs as Rex windows, maximized agents as Rex blocks, workflow windows, status in Rex), and the fact that everything else behaves exactly as in a normal terminal;
  - the `tui.integrations.rex.*` keys, and switching them for one run with `--set`;
  - input reach through Rex remote access;
  - fallback notices;
  - runtime limits (ACP, sandbox).
- `docs/02-using-the-tui.md`: a short "In Rex" note on Ctrl-T, Ctrl-A/D and Ctrl-M.
- `docs/07-configuration.md`: the `tui` section.
- `docs/04-security-and-isolation.md`: only the daemon's presenter talks to Rex, and agents never get Rex access.
- `docs/14-command-reference.md`: nothing Rex-specific. `--set` is documented by 0139.
- `docs/contents.md`: link the new guide.
