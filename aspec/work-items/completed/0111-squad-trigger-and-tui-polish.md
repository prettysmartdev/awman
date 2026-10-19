# Work Item: Bug + Feature

Title: squad — modal key hints, non-blocking daemon start, `squad trigger`, card
labels, tick-log noise, where failed actions are reported, the Layer-1 import
violation, and the missing bearer key
Issue: (reported directly)

## Summary:
- Eight squad changes reported after WI 0110:
  1. **Bug** — not every modal in the task-edit interview shows its key
     bindings.
  2. **Bug** — the daemon-start confirmation appears to hang for several
     seconds, then closes without a working squad tab.
  3. **Feature** — a `trigger` command and `t` key binding that evaluate a
     task now, regardless of its schedule.
  4. **Polish** — the task cards want grey labels for the description and for
     the last-run timestamp.
  5. **Polish** — the `squad scheduler tick` log line fires every 30 seconds
     and is noise.
  6. **Polish** — a failed squad action is reported above the card grid; it
     belongs in the hint bar above the command box.
  7. **Bug** — `architecture-lint` fails: the scheduler (Layer 1) imports the
     task gateway (Layer 2) to read a task's `config.json`.
  8. **Feature** — the TUI says nothing useful when it holds no squad bearer
     key, and the process that mints one cannot use it.

## User Stories

### User Story 1:
As a: user

I want to: see which keys each task-interview modal accepts, on the modal

So I can: answer an interview step without guessing whether Enter submits, Esc
cancels, or the box wants something else.

### User Story 2:
As a: user

I want to: keep a responsive TUI while a squad daemon starts, and be told when
one fails to start

So I can: tell the difference between "this is taking a moment" and "this did
not work", instead of watching a modal freeze and then vanish.

### User Story 3:
As a: user

I want to: trigger one task's evaluation immediately from the CLI or the TUI

So I can: test a task I have just written, or re-run one after fixing what it
depends on, without editing its interval and putting it back afterwards.

### User Story 4:
As a: user

I want to: read a task card as a labelled record

So I can: tell at a glance which line is the description and which timestamp is
the last run, rather than inferring it from position.

### User Story 5:
As a: administrator

I want to: read a daemon log that only says things that happened

So I can: find the line about a real event without scrolling past a heartbeat
emitted twice a minute since the daemon started.

### User Story 6:
As a: user

I want to: see a failed squad action reported where the TUI reports everything
else

So I can: keep the card grid in one place instead of having it shift down a row
whenever an action fails.

### User Story 7:
As a: developer

I want to: `make pre-push` to pass on the layering lint

So I can: push without hand-checking whether a violation is mine, and keep the
four-layer rule meaning something.

### User Story 8:
As a: user

I want to: be told when squad has a key I do not, and be offered a way out

So I can: recover from a lost key without hunting through docs, instead of
opening a squad tab that reports `401 Unauthorized` forever.

## Implementation Details:

### 1. Modal key hints (root-caused by arithmetic, not by omission)
Every dialog *composed* a hint row; two dialog kinds sized themselves too small
to draw it, and the clipping was silent because a `Paragraph` simply stops at
its rect.

- `TextInput` (the interval, leader agent, leader model, and every overlay
  step) laid its hint out at `input_area.y + 3 + 1`, which is exactly
  `inner.y + inner.height` for a `prompt_lines + 9` dialog — one row past the
  bottom. Guarded by `if hint_y < inner.y + inner.height`, so it rendered
  *never*, for any prompt. Height is now `prompt_lines + 10`: the literal sum
  of prompt + spacer + 3-row input + spacer + hint, plus the frame's 4.
- `render_yes_no` (and the `YesNoCancel` copy of its layout) sized itself
  `body_h + 5`, one short of the `body_h + 2` rows of content plus 4 of frame.
  A one-line body was saved by the `.max(7)` floor, which is why the existing
  test passed; every squad confirmation has a two- or three-line body and lost
  its hint. Now `body_h + 6`, floor `8`.
- `KindSelect` (the workspace choice) counted the hint but not the blank line
  before it, and was two short. Now `options + 2` body rows.

The other dialogs the interview raises — `MultilineInput`, `ListPicker`,
`MountScope`, `AgentSetup`, `AgentAuth`, `SquadRemoveConfirm`,
`SquadStartConfirm` — were audited and already size themselves to fit their
hint row.

### 2. The daemon-start confirmation
Two independent faults, one behind each half of the report.

*"Hangs for several seconds":* `build_and_install_squad_tab` ran
`build_squad_tab` inline on the event-loop thread, which blocks on a channel
while `SquadSupervisor::ensure_running` spawns a daemon and polls for its
endpoint for up to 10 seconds. `active_dialog` was cleared first, but no redraw
can happen while the loop is blocked — so the confirmation stayed painted for
the whole wait and then vanished.

`build_squad_tab` is split into the half that waits and the half that does not:

- `App::ensure_squad_gateway()` — async; the runtime guard, the supervisor, and
  `ensure_running`, returning `(RemoteTaskGateway, Option<key_setup>)`.
- `App::assemble_squad_tab(...)` — sync and local; the synthetic session, the
  `Tab`, and its poller. Nothing here waits on the daemon.

`build_and_install_squad_tab` now checks the runtime tier synchronously (an
immediate refusal must not be dressed up as a wait), spawns
`ensure_squad_gateway` on the runtime, stores the receiver in
`App::squad_startup_rx`, and puts a `Dialog::Loading` on screen. A new
`App::poll_squad_startup`, called first in `tick_all_tabs`, drains the result:
on success it assembles and installs the tab, on failure it raises a
`Dialog::Notice` carrying the reason. `squad_startup_rx.is_some()` is also what
makes a second `y` inert, so two presses cannot spawn two daemons.

`build_squad_tab` keeps its blocking shape for `InitialTab::Squad` (bare
`awman squad`), which runs before the event loop exists and therefore has no
screen to freeze.

*"Does not seem to work":* `ensure_running` accepted a `server.json` written by
a daemon that is no longer running. A daemon killed with SIGKILL never clears
its sidecar, `check_already_running` cleans the *pidfile* but not the sidecar,
and the post-spawn wait therefore returned the dead daemon's port on its first
iteration — a gateway whose every request is refused, while the daemon just
spawned came up on a different port and was never used. Reproduced directly:
`kill -9` the daemon, then any `squad` command answers
`remote connection refused ... :33241`.

`SquadSupervisor::discard_stale_endpoint` clears the sidecar immediately before
the spawn — after the PID check has already established nothing is listening —
so the wait blocks for the new daemon's own endpoint.

### 3. `squad trigger`
New Layer-2 subcommand `squad trigger <name>`, catalogued beside `pause` and
`resume`, `api_allowed`, one required `name` argument and no flags.

Store: a `trigger_requested_at TEXT` column, added through the existing
`add_column_if_missing` path. `TaskStore::request_trigger(name, now)` sets it
and clears `backoff_until`; `due_for_evaluation` admits a task with a pending
request as if its interval had elapsed; `start_run` clears the request in the
same statement that stamps `last_run_at`, so a trigger fires exactly one
evaluation and cannot be served twice. The ordering clause puts triggered tasks
first within a tick.

What a trigger deliberately does **not** do:

- It does not write `status`, so a paused task stays out of
  `due_for_evaluation`. `LocalTaskGateway::trigger` refuses a paused task
  outright, naming `squad resume`, because the store would happily record a
  request the scheduler would then ignore — leaving the user watching a task
  they triggered do nothing.
- It does not touch `last_run_at`. Backdating it would have been the cheap way
  to satisfy the interval predicate, and would have made the card's "Last run"
  timestamp a lie.
- It does not relax the not-already-running rule; a pending request simply
  waits for the in-flight evaluation to finish.

`TaskGateway::trigger` is implemented locally (as above) and remotely (re-issue
as the `squad trigger` argv the daemon parses), and reported as
`SquadOutcome::Triggered { name }` — distinct from `Ok` so the CLI can name the
task and say *when* it will run rather than printing a bare success.

`frontend/cli/mod.rs` gains `trigger` in both squad match arms: the runtime-tier
fast-fail list and the gateway-provisioning list. Missing the second one is a
silent failure mode — the command parses, reaches Layer 2, and reports "squad
tasks are served by the squad daemon" — and was caught only by running the
built binary against a live daemon.

TUI: `Action::SquadTrigger` on `t` in `FocusContext::SquadList`, dispatched
through the same `squad_dispatch_by_name` path as pause/resume, plus `t` in the
detail modal scoped to the modal's own task. `Ctrl-T` keeps its global new-tab
meaning, because the global `ctrl` block in `map_key` runs first.

### 4. Card labels
`render_task_card` was a bare description line, `Last run: <outcome>`, an
unlabelled indented `<timestamp>` continuation, and `Next: <next>`.

Every value now carries a grey label. The description's label sits on its own
row rather than inline: `CARD_MIN_WIDTH` is 30, so an inline `Description: `
would leave under half a minimum card for the text it introduces — measured, a
100-cell terminal already truncated a 23-character description. That costs one
row, so `CARD_MIN_HEIGHT` goes 6 → 7. `labelled_card_line` measures the value
against the width its label leaves, not the whole card, so a long value can no
longer overflow the border by the length of its own label.

`next_evaluation` reports a pending trigger as `triggered — next tick`, ahead of
the schedule it overrides.

### 5. The tick log line
`tracing::info!(tick, at, "squad scheduler tick")` is removed. `last_tick` and
`tick_count` are still recorded — `squad status` reports both — and
`log_running_agents`, which already emits one line per running container and
nothing when none are running, remains the tick's user-facing output.

### 6. Where a failed action is reported
`failed_action_line` is removed from `render/squad.rs`; `render_status_bar`
gains a `tab.is_squad` branch that renders the same text in red. This also
retires the execution-window hints ("Exit code: -1", "press ↑ to focus the
window") on a tab whose body is a card grid and which has no execution window
to focus.

### 7. The Layer-1 → Layer-2 import
`task_squad_config` lived beside the gateway, and the scheduler reached across
a layer boundary to call it. Every *input* it touches is already Layer 0: the
path from `SquadPaths::task_config_file`, the parse and validation from
`GlobalConfig::load_path`, the merge from `SquadConfig::layered_over`. Only its
error type was Layer 2, and that alone was what forced the import.

Moved verbatim to `data/config/global.rs`, beside `load_path` — whose doc
comment already explains the task-config case — returning `DataError`. The
scheduler now calls it directly; nothing in Layer 2 called it at all, so no
wrapper was left behind. `CommandError: From<DataError>` already covers the
gateway's own error surface.

Deliberately not done: relaxing the lint, or moving the *scheduler* instead.
The function was in the wrong place; the boundary was right.

### 8. The missing bearer key
The squad key is displayed once and stored only as a SHA-256 hash. That makes
"this process holds no key" a state a frontend must be able to report, not
something to discover as a 401 on every request. Three states, decided by
`SquadSupervisor::key_state`:

- `Ready` — `AWMAN_SQUAD_KEY` is set, or the running daemon serves
  unauthenticated.
- `Minted { setup, key }` — this process minted the key, so it must be shown.
- `Missing` — a hash exists, nothing holds the key, auth is on.

The precedence between them is `decide_key_state`, a pure function of the four
facts, so it is exhaustively testable without a daemon or a filesystem. The
`Minted` arm consumes the one-shot disclosure, so asking twice reports `Ready`
rather than printing a secret twice.

**No hash at all is `Ready`, not `Missing`.** Nothing has minted yet, and the
next start will; reporting it would put the recovery dialog in front of a user
on their first run.

TUI: `poll_squad_startup` raises `Dialog::SquadKeyMissing` on `Missing` and
assembles **no tab** — one whose every poll is refused shows nothing but that.
`y` runs `App::start_squad_key_refresh`, which shares `squad_startup_rx` and
`poll_squad_startup` with an ordinary start, because a refresh ends in exactly
what a first run ends in: a live gateway and a `Minted` key to display. One
drain, one progress modal, one error path. `n` opens no tab and says so.

The pre-event-loop path (bare `awman squad`) needs the same answer with no TUI
to raise it over, so `build_squad_tab` returns `SquadTabStart::KeyMissing` and
`tui::run` opens on the working directory with the dialog up — the same
degradation its existing error arm already performs.

**`refresh_key` mints in-process and starts the daemon normally, rather than
spawning `squad start --refresh-key`.** The two produce identical on-disk state,
but `--refresh-key` mints inside the detached child, whose stdout is
`~/.awman/squad/awman.log` — a file `awman squad logs` prints verbatim. This is
the same rule `provision_key` already documents. Verified empirically: after a
refresh the plaintext key appears nowhere in the daemon log, the old key is
refused with `Invalid API key`, and the new one works.

**Auto-injection.** `publish_key_to_process_env` sets `AWMAN_SQUAD_KEY` in the
minting process. The user is told to export the key, but cannot export it into
a process that is *already running* — and the TUI that starts the daemon is
exactly that process. Without it, the one process guaranteed to have seen the
key is the only one that cannot use it for anything it builds later
(`squad attach`, a Dispatch command, a second supervisor), because each reads
the live environment. It is a `setenv` in a threaded process, which is
tolerable only because of when it happens: during daemon startup, before any
squad gateway exists to read the variable, exactly once per minted key.

CLI: the same `Missing` state is reported before the request rather than after,
because the request's own answer is a bare `HTTP 401: API key required` that
names neither the variable nor the fact that the key cannot be read back.

## Edge Case Considerations:
- A trigger on a task whose evaluation is already running stays pending rather
  than being dropped, and is honoured when that run finishes.
- A daemon restart does not lose a pending trigger: it is a column, not
  in-memory state.
- `request_trigger` on a name that does not exist returns `false`, and the
  gateway turns that into the same "was not found" error every other verb uses.
- A second `y` on the daemon-start confirmation while the first start is still
  in flight is inert, so two presses cannot spawn two daemons.
- `poll_squad_startup` dismisses only a `Dialog::Loading`: an error or notice
  raised while the start was running belongs to the user.
- A startup channel that is dropped without a result (the runtime task died)
  ends the wait with an explicit "interrupted" message rather than leaving the
  progress modal up forever.
- `discard_stale_endpoint` is idempotent, and only ever runs after the PID
  check has found nothing running.
- A card narrower than its label renders the label and nothing else, rather
  than overflowing the border: `truncate_to_width` returns empty at width 0.
- Existing task rows predate `trigger_requested_at`; the column is nullable and
  `NULL` reads as "no request", so no migration backfill is needed.
- A key minted by one frontend and already disclosed reports `Ready` on the next
  read, not a second disclosure of the same secret.
- A first run with no hash at all is `Ready`, so the recovery dialog never
  appears before anything has been minted.
- A skip-auth daemon needs no key, so `Missing` is impossible while one is
  running and no dialog is raised.
- A second `y` on the recovery while a refresh is in flight is inert; two
  presses cannot mint two keys or restart the daemon twice.
- `refresh_key` stops the daemon before minting: `run_start` refuses to start a
  second daemon, and the running one authenticates against the old hash.
- Declining the recovery leaves the daemon running and untouched — it was
  already up, or was just started successfully; only the key is the problem.

## Test Considerations:
- Render: every modal the task interview raises shows its key bindings — the
  test fails if `TextInput`'s height, `YesNo`'s height, or `KindSelect`'s body
  height regresses (verified by reverting each).
- Render: cards label the description and the last-run timestamp, and labelling
  does not cost the description the width it needs; a pending trigger renders
  instead of the scheduled time; a failed squad action renders in the hint bar
  directly above the command box and not in the grid header, with no exit-code
  hint on a squad tab.
- Store: a trigger overrides the interval and the backoff but not pause and not
  a running run; it is recorded, consumed by `start_run`, and does not re-fire
  on the following tick.
- Gateway: triggering refuses a paused task with a message naming
  `squad resume`, refuses an unknown one, and leaves every other field —
  including `last_run_at` — untouched.
- CLI: `squad trigger <name>` is exactly one `trigger` gateway call; the
  catalogue and API-allowed sets include it.
- TUI: `t` triggers the selected task from the grid and the modal's own task
  from the modal, is a no-op on an empty list, and `Ctrl-T` still opens a tab.
- Daemon: a dead daemon's endpoint sidecar is discarded rather than handed out.
- TUI: a start already in flight makes a second confirmation inert; the
  progress modal survives until the start ends and is taken away by the tick,
  not by a key. Deliberately **not** tested: pressing `y` against a real
  daemon, which would spawn a background process from a unit test.
- Key state: `decide_key_state` covers every combination that matters — minted
  and undisclosed beats a hash and an environment key; minted and already
  disclosed is `Ready`; an env key or a skip-auth daemon is `Ready`; a hash with
  neither is `Missing`; no hash at all is `Ready`. Plus the supervisor-level
  path, which reaches `Missing` from a written hash and returns to `Ready` when
  the key is in the snapshot.
- A minted key is published into the process environment, so a later
  `Env::from_process()` in the minting process finds it.
- TUI: `Missing` raises the recovery and opens no tab; declining opens no tab
  and says so; a refresh already in flight makes a second `y` inert; a completed
  refresh opens the tab and displays the new key. The accepting press is
  deliberately **not** exercised against a real daemon — it terminates and
  restarts one.
- Render: the recovery dialog names `AWMAN_SQUAD_KEY`, says the key was shown
  only once, offers both answers, and states the cost before it is accepted.
- Architecture: `tools/architecture-lint.sh` passes.

## Codebase Integration:
- follow established conventions, best practices, testing, and architecture patterns from the project's aspec.

## Documentation
- `docs/12-squad.md`: a "When the key is missing" section (the CLI refusal, the
  TUI recovery dialog, what accepting costs) and the self-injection note under
  `AWMAN_SQUAD_KEY`; a "Triggering a task now" section (what it overrides, what
  it does not, and how it differs from editing the interval), `squad trigger` in
  the CRUD list, `t` in the key table and the detail-modal hints, the labelled
  card layout, the failed-action line's new home, the non-blocking daemon start,
  and the daemon log no longer carrying a tick heartbeat.
- `docs/02-using-the-tui.md`: the labelled card fields, `t` and `e` in the
  detail modal's footer, where a failed squad action is reported, and the
  missing-key modal.
