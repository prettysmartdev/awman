# Work Item: Task

Title: Support OSC 7501 status codes
Issue: https://github.com/prettysmartdev/awman/issues/30

Depends on: 0159 (config and API naming)

## Summary:
- OSC 7501 (the "Program Status Protocol", draft rev 0.4: https://www.superlogical.com/rex/docs/build/program-status) is an open terminal escape sequence. A program running in a PTY uses it to tell the hosting terminal whether it is `idle`, `working`, `done`, `blocked` (on `permission`, `question` or `auth`), or in `error`. Coding agents are starting to emit it, and Claude Code reportedly does.
- awman already acts as the terminal for every agent it runs: it owns the PTY or pipes in each runtime bridge. Today it guesses agent state from one signal, output silence (`StuckDetector`, 30s `agents.stuckTimeout`). This work item makes awman parse OSC 7501 reports from agent output. **Once an agent has sent a valid report, its reported status supersedes the built-in silence heuristic for the rest of that execution.** A self-report says what the agent is doing; silence only suggests it.
  - **Agents that report:** silence never marks them stuck and never starts a yolo countdown. `working` means busy, however long the agent stays quiet. `done` (and `idle`, meaning the turn has ended) means the step has finished. `blocked` and `error` mean there is a problem, and **the workflow never advances past a problem**: it pauses for the user, or, when no user is present, the step fails through the existing failure path.
  - **Agents that don't report:** behaviour is unchanged. Silence detection, the startup grace timer and the yolo countdown all work exactly as they do today.
  - **Basic information:** the TUI tab bar, container window title, workflow overview, `awman agents status`, the CLI message sink and the API event stream show the reported state, kind, progress and message.
- awman answers the protocol's feature-detection query (`OSC 7501 ; ? ST`) so agents that only report after detecting support will start reporting.
- No new commands. One new config key, `agents.programStatus.enabled` (bool, default `true`; named per 0159): set it to `false` to ignore reports and keep using silence detection only.

## User Stories

### User Story 1:
As a: user

I want to:
see in the TUI tab bar and in `awman agents status` whether each agent is working, done, or waiting on me for a permission, question or login, together with the agent's own one-line message

So I can:
go straight to the agent that needs me. I don't have to wait for a 30-second "stuck" guess, and an agent that is quietly running a long build is never flagged stuck by mistake.

### User Story 2:
As a: user running `awman exec workflow --yolo`

I want to:
have a step advance as soon as its agent reports `done`, and have the workflow stop advancing when the agent reports `error` or `blocked`

So I can:
trust unattended workflows. A finished step moves on right away, with no 30s silence plus 60s countdown. A failing or stuck-on-a-question step never gets pushed forward as if it had succeeded.

### User Story 3:
As a: user running a non-yolo workflow

I want to:
have the workflow control board open as soon as the step's agent reports that it has finished, needs help or has failed, with the agent's reason shown

So I can:
act at the moment a decision is needed. Agents that don't support OSC 7501 keep today's behaviour.


## Implementation Details:

### 1. Protocol model and parser (Layer 0, pure): `src/data/program_status.rs`
- Types:
  ```rust
  pub enum ProgramState { Idle, Working, Done, Blocked, Error }
  pub enum BlockedKind { Permission, Question, Auth }
  pub struct ProgramStatusReport {
      pub id: Option<String>,          // path grammar; None = root record
      pub state: ReportState,          // ProgramState | Clear
      pub kind: Option<BlockedKind>,   // only kept for Blocked
      pub progress: Option<u8>,        // 0..=100, only for Working/Blocked
      pub app: Option<String>,
      pub title: Option<String>,       // decoded, sanitised
      pub msg: Option<String>,         // decoded, sanitised
  }
  pub enum ParsedOsc7501 { Query, Report(ProgramStatusReport), Ignored }
  pub fn parse_body(body: &[u8]) -> ParsedOsc7501   // body = bytes after "7501;"
  ```
- Follow the spec's parsing rules exactly:
  - Pairs are `key=value`, separated by `:`, with whitespace trimmed. Keys match `[a-z]+` and are at most 16 bytes. Values match `[A-Za-z0-9_.,+/=-]*`.
  - A malformed pair is skipped and the rest of the report is still used. Unknown keys are ignored. When a key repeats, the last value wins.
  - A missing or unknown `state` makes the whole report `Ignored`. An unknown `kind` is treated as absent. A `progress` that is not an integer from 0 to 100 is treated as absent. An invalid `app` is treated as absent.
  - A malformed `id` makes the whole report `Ignored`; it must **not** fall back to root. Segments match `[A-Za-z0-9_.+-]{1,32}`, with at most 8 levels and 128 bytes in total.
  - `msg` and `title` are standard base64 with optional padding. Check the encoded length (2732 for `msg`, 256 for `title`) **before** decoding. Then check the decoded length (2048 / 192) and that the bytes are valid UTF-8. Any C0, DEL or C1 control character makes the **whole report** invalid (`Ignored`).
  - After validation, strip bidi overrides and invisible formatting characters (U+202A–U+202E, U+2066–U+2069, U+200B–U+200F, U+FEFF) from `msg`/`title`. They are only ever rendered outside the terminal grid.
- `ProgramStatusRecord` = the last accepted report for one id. Add `ProgramStatusSet`:
  - Holds a map of records keyed by id, capped at 64 with least-recently-updated eviction.
  - `apply(report)`: replaces the whole record, so keys missing from the new report are dropped. `Clear` with an id removes that id and its descendants. `Clear` without an id removes everything.
  - `reset()` is called on RIS (`ESC c`).
  - `effective() -> Option<EffectiveStatus>` returns one aggregate for the agent. Priority is `blocked > error > working > idle > done`, so any problem in any record wins and a busy child keeps the agent from counting as finished. Kind, msg and progress come from the winning record. Child records inherit `app` from their nearest ancestor.
- Expose `ProgramState::as_str()`/`parse()` in the same style as `SessionStatus` (`src/data/session_status.rs`), so CLI/API JSON uses the spec spellings.

### 2. Stream scanner (Layer 1, runtime-neutral): `src/engine/agent_runtime/program_status.rs`
- `Osc7501Scanner` is a small state machine fed every output chunk, `fn feed(&mut self, chunk: &[u8]) -> ScanResult`. It finds `ESC ] 7501 ;` … terminated by BEL (0x07) or ST (`ESC \`), and it also spots RIS (`ESC c`).
  - It must handle sequences **split across chunk boundaries**. The reader threads read in 4 KiB blocks, so splits are routine.
  - It buffers at most 4096 bytes of one sequence. On overflow it drops that sequence and returns to ground state; it never grows without bound.
  - It returns `(forward: Cow<[u8]>, events: Vec<ParsedOsc7501>)`. All bytes are forwarded **unchanged** except query sequences (`7501;?`), which are removed so that an outer real terminal (CLI passthrough, attach) never answers as well, which would send a second reply into the agent's stdin. Reports keep flowing through, so a real terminal that supports OSC 7501 (Ghostty etc.) still sees them in CLI mode.
- It lives under `agent_runtime` because the container tier (`src/engine/container/io_bridge.rs`, `attach_socket.rs`, `builtin/exec_bridge.rs`) and the sandbox tier (`src/engine/sandbox/dsbx/io_bridge.rs`) may both import `agent_runtime`, while sandbox may not import container (WI 0090 layering rule).
- Query reply: when the scanner yields `Query` on the **PTY path**, the bridge writes `ESC ] 7501 ; ? ESC \` into the agent's stdin through the stdin sender it already keeps (`BridgeResult::stdin_injector`). The reply carries no `states=`/`kinds=` fields, which means the full default sets. Piped (non-interactive) mode does not answer the query, because the agent has no TTY. Reports that arrive anyway are still parsed.
- A query by itself does **not** count as support: only an accepted report does (see §3).
- Disabled via config (`agents.programStatus.enabled: false`): the scanner still removes queries, and the bridge neither replies nor publishes. awman then behaves exactly as it does today.

### 3. Supersede silence detection in the bridges and the `StuckDetector`
- Each bridge reader thread already calls `act.touch()` per chunk. It should also run that chunk through its own scanner (one per stream: stdout and stderr each get one in piped mode). Accepted events update a shared `ProgramStatusTracker` (`Arc<Mutex<…>>`) stored next to `OutputActivity`. The tracker holds:
  - the `ProgramStatusSet`
  - a `reporting: bool` latch, set by the first accepted report (any state, including `clear`) and kept for the rest of the execution.
- **Who decides: the agent's report or the silence heuristic.** This is a single rule, applied only inside `StuckDetector`, so every consumer gets the same behaviour without duplicated logic:
  - `reporting == false`: today's silence loop runs unchanged and emits `Stuck`/`Unstuck`.
  - `reporting == true` and `effective()` is `Some`: the agent's report decides. The silence loop **never emits `Stuck`**. If the agent was flagged stuck when the first report arrived, the detector emits `Unstuck` once.
  - `reporting == true` but `effective()` is `None` (the agent cleared all of its records): fall back to the silence loop, measured from the clear, until the next report arrives. The agent has withdrawn its status, so awman has nothing better to go on.
  - The **startup grace timer is unchanged** in every case: an agent that produces no output never gets to report. It ends on the first byte, as it does today.
- Extend `StuckEvent` (`src/engine/agent_runtime/execution.rs`). The name is kept to limit churn, and the doc comment is updated to say "attention event" and to state the supersede rule:
  ```rust
  pub enum StuckEvent {
      /// Silence heuristic. Only for agents not reporting OSC 7501.
      Stuck, Unstuck, StartupGraceExpired,
      /// The agent's effective OSC 7501 status changed (deduplicated on state+kind).
      Reported(EffectiveStatus),
  }
  ```
  The detector publishes `Reported` when `effective()` changes state or kind, debounced to at most one publish per 250 ms per agent. That protects consumers from a status flapping between two states. Progress or msg changes alone are published at most once per second, for display only.
- Drop all records and the `reporting` latch when the execution ends and at each new step/container. This follows the spec rule that `working`/`blocked` are dropped on process exit. A new container may run a different agent, so support is re-learned.
- `AgentExecution` gets `program_status() -> Option<EffectiveStatus>` and `reports_program_status() -> bool`, snapshot reads for frontends and `agents status`.

### 4. Workflow engine behaviour (`src/engine/workflow/single_step.rs`, `parallel.rs`, `phases.rs`; squad `src/engine/squad/launcher.rs`)
- Map `Reported(s)` events inside the step loops (the existing `Some(event) = Self::recv_stuck(..)` arms). These rules only ever apply to reporting agents, because silence `Stuck` is never emitted for them (§3). **Problems (`blocked`, `error`) never lead to an advance in any mode.**

  | Effective state | yolo | non-yolo (TUI) | no user (squad / API / `--headless`) |
  |---|---|---|---|
  | `working` | cancel any running countdown; close an auto-opened dialog | close an auto-opened control board unless the user is interacting with it | cancel any running countdown |
  | `done` | start `CountdownKind::AgentDone` (default 10s, `timing::agent_done_countdown_duration()`); on expiry kill the container and advance, exactly like `StuckStep` expiry. On the last step: complete the workflow | open the lightweight step-confirmation dialog (as if the step had completed) | same as yolo |
  | `idle` (turn ended, awaiting input) | same as `done`: the agent has stopped and is waiting, which in a workflow step means its turn is over | open the lightweight step-confirmation dialog | same as yolo |
  | `blocked` (any kind) | **no auto-advance.** Pause the workflow, show the kind + msg, and alert via the yolo dialog/tab bar in a "needs you" style. The user answers in the container or uses the control board. For `permission` the message also notes that yolo should have skipped prompts | open the control board immediately, showing kind + msg | start `CountdownKind::AgentBlocked` (normal yolo duration); on expiry kill the container and **fail the step** (`reported_problem`) so the existing unattended failure path (one retry, then fail) applies. Never advance |
  | `error` | **no auto-advance.** Start `CountdownKind::AgentError` (normal yolo duration); on expiry kill the container and **fail the step** so `on_failure` remediation and `FailureRetry` apply. Any output or a `working` report in the meantime cancels it | open the control board immediately, showing msg. Advancing is still possible but only as an explicit user action | same as yolo |

- Step finalisation: record a `reported_problem: Option<EffectiveStatus>` on the step slot. When the engine kills a container whose last effective state was `blocked`/`error`, the finaliser marks the step **Failed** (with the agent's msg as the reason), even though the exit code is `KILLED_EXIT_CODE`, which today is read as an awman-initiated advance. A process that exits by itself after reporting `error` is also marked Failed, even with exit code 0. A process that exits after `done` keeps its exit code.
- Add `AgentDone`, `AgentBlocked` and `AgentError` to `CountdownKind` (`src/engine/workflow/actions.rs`), with a doc comment on what each means to the reader. Frontends match on the kind to label the countdown ("Agent reported done: advancing in 8s", "Agent reported an error: failing step in 42s").
- Remove the `is_last_step()` skip for `AgentDone`. It still applies to `StuckStep`, the silence path.
- Parallel groups: `handle_parallel_stuck_event` gets the same mapping per step, and each container is tracked independently (the fan-in already tags events with the step name). One step's `error` pauses or fails only that step. The group advances only when every sibling has finished; that rule is unchanged.
- Respect `--plan`/`--auto`: only `--yolo` (and frontends with no user) auto-advance on `done`/`idle`, matching current rules.

### 5. Frontends (Layer 3, presentation only)
- **TUI**:
  - Store the `EffectiveStatus` on each tab/container slot, using the same path `git_poll.rs` uses for `StuckEvent`.
  - The tab label gets a state glyph: `◐` working (with `NN%` if progress is set), `?` blocked question, `🔒` blocked permission/auth, `✓` done, `✗` error.
  - Colours: blocked = yellow (it replaces the stuck yellow as the "needs you" signal), error = red, done = green, working/idle = default.
  - The yolo countdown colours still take priority while a countdown runs. Reporting agents never get the silence `⚠️` prefix.
  - Show the msg (truncated to fit) in the container window's title line and in the minimized container bar.
  - The workflow overview shows `blocked: permission` / `error` etc. under the step box.
  - **Active-tab suppression** also applies to reports. On the active tab, if the user typed or scrolled within the last `agents.stuckTimeout`, an `idle`/`done` event shows its indicator but **does not** auto-open the dialog or start the countdown until the user has been idle that long. This avoids a dialog on every turn of an interactive step. `blocked` and `error` are not deferred.
- **CLI**: status lines go to the message sink (stderr), at most one per state change ("Agent is waiting for permission: <msg>"). Reports still pass through to the user's terminal unchanged.
- **API**: a new `program_status` event in the command event stream (`GET /v1/worker/commands/{id}/events`, 0159) with `{state, kind, progress, msg, app}` (snake_case per 0159; values use the spec spellings). Add it alongside the existing agent output events in `src/frontend/api/command_frontend.rs`.
- **`awman agents status` / `agents ls`** (`src/command/commands/agents/snapshot.rs`):
  - Add `AgentState::Waiting` (blocked, glyph `◆`, sorted first), `AgentState::Errored` (glyph `✗`, sorted with Waiting) and `AgentState::Done` (glyph `✓`).
  - `Stuck` now only applies to non-reporting agents.
  - Add `program_status: Option<…>` to the tab snapshot (next to `is_stuck`), and add `kind`/`msg` fields to the JSON output.
  - The table STATUS column shows e.g. `waiting (permission)`.

### 6. Config
- `agents.programStatus.enabled` (bool, default `true`, scope `repo`: global and repo, repo wins) is one row in the 0159 key table (`src/data/config/fields.rs`), in the `agents` section struct (`src/data/config/agents.rs`). That one row gives validation, `config show/get/set/unset`, `--set agents.programStatus.enabled=false` and `AWMAN_AGENTS_PROGRAM_STATUS_ENABLED`. `false` = ignore reports and use silence detection only. It is the escape hatch for an agent whose reports turn out to be unreliable.
- Add it to `aspec/uxui/cli.md`'s config section. The `docs/07-configuration.md` reference table is generated from the key table (0159).
- The `AgentDone` countdown duration is a timing constant in `src/engine/container/timing.rs` (alongside `yolo_countdown_duration()`), not a config key.


## Edge Case Considerations:
- **Agents that never emit OSC 7501:** behaviour must be exactly what it is today. With no latch there are no events, and the silence detector, grace timer and countdowns are untouched. This is the default case and must stay covered by regression tests.
- **Hung agent after `working`:** silence detection no longer applies once an agent reports, and the protocol has no heartbeat. An agent that freezes mid-`working` therefore sits as `working` indefinitely. This is the accepted cost of trusting the agent:
  - The TUI shows "working · silent 12m" (information only; no action is taken), so the user can notice and use Ctrl+W.
  - Startup grace still catches agents that never start.
  - `agents.programStatus.enabled: false` restores the heuristic.
  - Note it in docs.
- **Agent stops reporting partway through:** the latch persists, so the last reported state stands. Only an explicit `clear` returns the agent to silence detection.
- **Flapping status** (e.g. done ↔ working every 100 ms, from a buggy or hostile agent): debounce transitions. `working` cancels a `done` countdown, so a flapping agent can at worst keep its own step alive, which it can already do today by printing output. It can never cause another step to advance.
- **`done` followed by process exit:** the exit is handled first (the `wait_rx` arm is biased first), and the countdown is cancelled. No double advance.
- **`error` followed by process exit with code 0:** the step is still Failed, because the report outranks the exit code.
- **`error` then `working`** (the agent recovers by itself): cancel the `AgentError` countdown and clear `reported_problem`. The step continues.
- **`done` while the user is mid-conversation on the active tab:** this is deferred by active-tab suppression (see §5).
- **`blocked` in yolo with the user present in the TUI:** pause indefinitely; never start an auto-advance. The user resolves it in the container.
- **`blocked`/`error` on the last step:** fail the workflow normally, so teardown-on-failure runs. Never mark it complete.
- **Parallel steps:** each container has its own scanner, tracker and latch, so one agent's `done` never advances a sibling and one agent's `error` never fails a sibling.
- **Nested records / subagents (`id=a/b`):** aggregate by priority so a problem in any child is never hidden by a `done` root. The 64-record cap uses LRU eviction.
- **RIS (`ESC c`)** clears all records but keeps the latch, so the agent falls back to silence detection until its next report (same as `clear`). Alternate-screen switches and DECSTR do not clear records.
- **Split sequences:** an introducer, body or terminator can be split across reads, including a lone trailing `ESC` at a chunk end. The scanner keeps state across reads, and the forwarded bytes must keep their order.
- **Oversized or unterminated sequence:** drop it once it passes 4096 bytes and resynchronise. Never buffer without a bound, and never hold normal output back waiting for a terminator: forward bytes as they arrive and only remove the query form.
- **Malformed reports:** these include a bad `id`, a missing or unknown `state`, bad base64, control characters in decoded text, and limits exceeded. Each is discarded as a whole with a `tracing::debug!`. It changes no state and **does not set the latch**, so garbage cannot switch off silence detection.
- **Unknown future states/keys/kinds:** ignore them as the spec says. A new state must never be treated as `idle` or `done`.
- **Double query replies:** removing queries from forwarded output prevents an outer terminal from replying as well. If the agent sends `CSI c` after the query (the spec's timeout trick), our reply is injected before the DA1 answer, because awman replies while it scans.
- **Piped / headless mode:** do not reply to queries. Still parse reports, and the latch still applies. Stdout and stderr have separate scanners feeding one tracker.
- **Attach / reattach (`attach_socket.rs`, squad attach):** status is per execution and lives in the engine, so a frontend that reattaches reads the current snapshot via `program_status()` rather than replaying history.
- **Sandbox tier (`dsbx`)** does not drive silence detection yet. Wire the scanner and publish `Reported` there, so reporting agents on that tier get done/problem handling even though silent ones still don't.
- **Apple start delay:** reports that arrive within `APPLE_CONTAINER_START_DELAY` are still parsed and still set the latch. Only the first-byte/activity bookkeeping is suppressed during that window.
- **Trust boundary:** a contained agent can now, by reporting, advance or fail **its own** step sooner. It cannot affect other steps, the host, or anything outside the existing workflow actions, and in yolo it can already end its step by exiting. Problem reports only ever make awman more conservative (pause or fail, never advance).
- **Security:** `msg`/`title` are agent-controlled, untrusted text:
  - Never interpret them as markup, shell input, prompt text or file paths.
  - Never echo them back into any PTY.
  - Strip bidi and invisible characters.
  - Truncate them for display.
  - The only bytes awman ever writes back in response are the fixed query reply, which contains no report data (spec security section). This adds no new host execution path; everything stays inside the existing container I/O boundary (aspec/architecture/security.md).

## Test Considerations:
- **Unit: `src/data/program_status.rs`**:
  - Each state, kind and progress bound (0, 100, 101, -1, `abc`).
  - Whitespace trimming, malformed pairs skipped, unknown keys ignored, last value wins.
  - A missing or unknown state is ignored. A bad `id` rejects the whole report (it does not fall back to root).
  - `id` limits (segment 33 bytes, 9 levels, 129 bytes).
  - base64 with and without padding, invalid base64, encoded and decoded length caps checked before decoding, invalid UTF-8, a control character (incl. `\n`, `\t`, C1) rejects the report.
  - Bidi stripping.
  - `ProgramStatusSet` replacement semantics (omitted keys removed), clear by id with descendants, global clear, RIS reset, LRU eviction at 64, `effective()` priority (`blocked > error > working > idle > done`) and `app` inheritance.
- **Unit: scanner**:
  - BEL and ST terminators. The sequence split at every byte offset (property-style loop over split points).
  - Lone trailing ESC. Overflow at 4097 bytes followed by resync.
  - Query removed from forwarded output while surrounding bytes are preserved. Reports forwarded byte-for-byte.
  - Other OSCs (0, 52, 9;4) untouched. Plain output with no ESC bytes returns `Cow::Borrowed` (no copy).
- **Unit: tracker + `StuckDetector`** (`io_bridge.rs` tests, with the shortened `poll_interval` like the existing detector tests):
  - No report → silence `Stuck` fires as today (regression).
  - First accepted report → the latch is set and silence never emits `Stuck`, even after many timeouts with `working`.
  - Already `Stuck` when the first report arrives → exactly one `Unstuck`.
  - `clear` → silence detection resumes from the clear, then the next report suppresses it again.
  - Malformed report → latch not set.
  - Grace expiry still fires for a silent agent.
  - Debounce/dedup of `Reported`.
  - Config off → no latch, no publish, no reply.
- **Unit: bridges:** a query produces exactly one reply on stdin on the PTY path and none on the piped path. Stdout and stderr both feed the one tracker.
- **Engine workflow tests** (`src/engine/workflow/tests/`, existing fake frontend + fake execution patterns):
  - Yolo:
    - `done` → `AgentDone` countdown → advance. `done` on the last step → complete. `idle` → same as `done`.
    - `working` with long silence → **no** countdown.
    - `error` → `AgentError` countdown → step Failed → `on_failure` remediation runs. It never advances.
    - `error` then `working` → countdown cancelled, step continues.
    - `blocked` with a user present → paused, no countdown, no advance.
  - No user (fake unattended frontend): `blocked` → `AgentBlocked` countdown → step Failed → one retry → workflow fails.
  - Non-yolo:
    - `done` → the lightweight confirmation opens without waiting for the stuck timeout.
    - `blocked`/`error` → the control board opens immediately.
  - Finaliser: a killed container with `reported_problem` → Failed. An exit 0 after `error` → Failed.
  - Parallel: one sibling's `done` or `error` does not affect another sibling.
  - Non-reporting agent → identical event sequence to today (regression).
- **Squad launcher tests:** unattended `done`/`blocked`/`error` mapping. A reporting `working` agent is never recovered by a silence countdown.
- **TUI tests** (`src/frontend/tui/tabs/tests.rs`, `render/tests.rs`):
  - Tab glyph/label/colour per state. A reporting agent never shows the `⚠️` stuck prefix. Countdown colours still win.
  - The "silent Nm" info for `working`.
  - The msg is truncated and sanitised in the window title.
  - Active-tab suppression defers `idle`/`done` but not `blocked`/`error`.
  - `vt100-ctt` renders nothing visible for an OSC 7501 sequence (guards against stray glyphs).
- **Command tests:** `agents status` snapshot with the `Waiting`/`Errored`/`Done` states, sort order, JSON fields (`tests/cli_parity/json_outputs.rs`).
- **API tests:** the `program_status` event shape and spellings (alongside `src/frontend/api/command_frontend/tests/`).
- **Integration / e2e** (Docker-gated, skip cleanly when unavailable, like `tests/squad_status_marker.rs`):
  - Fixture A runs through `exec workflow --yolo` in a real container. It reports `working`, sleeps longer than `agents.stuckTimeout`, then reports `done`. Assert that no stuck countdown happened during the sleep and that the step advanced shortly after `done`.
  - Fixture B reports `error`. Assert that the workflow does not advance to step 2 and that the step ends Failed.
  - Fixture C sends the query and asserts it reads back exactly `\033]7501;?\033\\` on stdin.
- **Config tests:** `agents.programStatus.enabled` parse, repo-over-global precedence, `config set/get` round trip. `false` restores silence-only behaviour end to end.

## Codebase Integration:
- Follow established conventions, best practices, testing, and architecture patterns from the project's aspec.
- **Layering** (aspec/architecture/design.md):
  - The pure protocol types and parser go in `src/data/`.
  - The scanner, tracker and supersede rule go in `src/engine/agent_runtime/` + `StuckDetector`, so both the container and sandbox tiers can use them without breaking the WI 0090 rule that sandbox must not import container.
  - Workflow policy goes in `src/engine/workflow/`. Frontends only render what they receive and hold no policy.
- **Apply the supersede rule in exactly one place:** `StuckDetector` stops emitting silence `Stuck` for reporting agents. Consumers must not re-implement "is this agent reporting?" checks; they just react to the events they receive.
- **Reuse the existing channels** rather than adding new plumbing:
  - Status changes ride the `StuckEvent` broadcast that every consumer already subscribes to (workflow single/parallel, squad launcher, TUI `git_poll.rs`, API).
  - Query replies use the bridge's retained stdin sender (`stdin_injector`).
  - Countdowns reuse the `yolo_countdown_*` frontend hooks with new `CountdownKind` variants.
  - Failing a step reuses the existing failure path (`on_failure`, `FailureRetry`, unattended single retry).
- Adding `StuckEvent::Reported` turns every exhaustive `match` into a compile error, which is intended: it lists every consumer that needs a decision. Grep `StuckEvent::` (≈20 sites across `src/engine/workflow/`, `src/engine/squad/launcher.rs`, `src/command/commands/exec_workflow/dynamic.rs`, `src/command/commands/agent_setup.rs`, `src/frontend/tui/`). Dynamic workflows should get the same mapping as static ones. Agent setup should ignore it explicitly, with a comment.
- Update the `AgentStatus`/`StuckEvent` doc comments (`src/engine/agent_runtime/frontend.rs`, `execution.rs`) and `DEFAULT_STUCK_TIMEOUT`'s comment in `timing.rs` so they describe what each runtime emits, the supersede rule, and the latch lifetime, keeping the existing "what a frontend can rely on" style.
- **Bridges to touch:** `src/engine/container/io_bridge.rs` (`bridge_pty`, `bridge_piped`), `src/engine/container/attach_socket.rs`, `src/engine/container/builtin/exec_bridge.rs`, `src/engine/sandbox/dsbx/io_bridge.rs`. Keep the per-chunk hot path allocation-free when a chunk contains no `ESC` byte.
- **Timing constants** live in `src/engine/container/timing.rs`. Add `agent_done_countdown_duration()` next to `yolo_countdown_duration()`, with test overrides the same way.
- **Config fields:** the 0159 key table (`src/data/config/fields.rs`) and `src/data/config/agents.rs`, with spec text in `aspec/uxui/cli.md`.
- Keep the code readable for an intermediate Rust developer: a plain state machine for the scanner, no parser-combinator crates. The base64 decode should use the crate the workspace already depends on, if there is one.

## Documentation

After implementation is complete, update user-facing documentation in `docs/` to reflect the current state of the tool:

- **Update existing feature docs** (e.g., if implementing headless features, update `docs/08-headless-mode.md`)
- **Create new user guides only if a new user-visible feature warrants it** (e.g., `docs/10-my-feature.md`)
- **Never create work-item-specific docs** (e.g., no "WI 0123 implementation guide" in published docs)
- **Keep all technical/implementation details in work item specs or code comments**, not in `docs/`
- **Docs are for end users**, not for developers trying to understand implementation

Specific docs to update for this item:
- `docs/02-using-the-tui.md`: the tab colour/label table, container window title, and the "Stuck detection" section. Explain that agents that report their status are judged on their reports instead of on silence.
- `docs/05-workflows.md`: "Stuck steps" and "Auto-advance when stuck (yolo mode)". Cover advance-on-done, never advancing past `blocked`/`error`, the new countdown labels, and the hung-agent caveat.
- `docs/03-agent-sessions.md`: a short note on agent status reporting and which bundled agents emit it.
- `docs/07-configuration.md`: the `agents.programStatus.enabled` reference row and example.
- `docs/09-api-and-remote-mode.md`: the `program_status` event.
- `docs/14-command-reference.md`: `agents status` states (`waiting`, `errored`, `done`).

See `CLAUDE.md` for more guidance on documentation standards.
