# Work Item: Feature

Title: squad — Ctrl-S hint moves into the New Tab tooltip, a pinned squad
health indicator on the bottom row, status-coloured dashed task cards with a
solid tab-style selected card, a permanently inactive command box on the
squad tab, per-step log files plus log paths in every failure line, and a
run status that follows the workflow's real outcome (yolo kills excepted)
Issue: (reported directly)

## Summary:
- Four TUI changes and two daemon changes to the squad feature, reported
  after WI 0111:
  1. **Polish** — the New Tab dialog says "Press Ctrl-S to open squad" in its
     body text. It belongs in the key-hint row under the text box, next to
     `[Enter]` and `[Esc]`.
  2. **Feature** — a `squad` indicator, pinned to the far right of the bottom
     row (the `CWD:` / `Using worktree:` line under the command box), on every
     tab, always. A coloured circle reports daemon health at a glance: grey
     (not running), green (running, this TUI can talk to it), yellow (running
     but unreachable, for any reason including auth), blue (a task is
     executing right now), red (some task's most recent run failed).
  3. **Feature** — task cards in the squad tab take their border colour from
     the task's state (grey paused, green active, red failed, blue running,
     yellow never run, magenta triggered-and-waiting). Selection stops being
     a colour: every card has a dashed outline, and the selected card alone is
     drawn with a solid outline and marked the way the active tab is — an `➡`
     arrow in the title on the top edge.
  4. **Bug** — on the squad tab the command box is dead weight: nothing typed
     there does anything, yet the tab opens with the box focused, so the card
     grid needs `↑` before the arrow keys work, and `Esc` throws focus back to
     a box that has no use. The box is permanently inactive on that tab, the
     grid always holds focus, `Esc` is a no-op, and the box explains itself.
  5. **Bug** — when a task's generated workflow has `setup` / `teardown`
     steps, their stdout/stderr goes nowhere: the unattended frontend leaves
     the step-output hooks at their no-op defaults, so only agent containers
     get a `<container-name>.log` under the run directory. And when any step
     fails — setup, agent, or teardown — the daemon log records the failure
     without saying where the output is. Every step now writes a log file
     under `~/.awman/squad/tasks/<name>/runs/<run-id>/`, and every failure
     line in the daemon log names the file to open.
  6. **Bug** — a run whose generated workflow ends with a non-zero exit code
     is still recorded as `workflow executed`: the task never backs off, and
     the new indicator and card colours would read green for it. A run's
     status now follows the workflow's real outcome — a container that
     **exits non-zero on its own** fails the run and backs the task off,
     while a container **killed by a yolo countdown** is the engine's normal
     auto-advance and is never a failure. The same rule applies to the
     evaluation leader.

## User Stories

### User Story 1:
As a: user

I want to: read the New Tab dialog's key bindings in one place, under the
text box, including the `Ctrl-S` shortcut to the squad tab

So I can: see every key the dialog accepts where the other dialogs put theirs,
instead of a stray sentence in the prompt.

### User Story 2:
As a: user

I want to: see whether my squad daemon is up, whether this TUI can reach it,
and whether anything is running or has failed, from any tab, without opening
the squad tab

So I can: notice a failed task or a dead daemon while I am working in a
project tab, and go look only when the colour says there is something to see.

### User Story 3:
As a: user

I want to: tell a paused, failed, running, never-run, or triggered task apart
by its card's colour, and find the selected card by an unmistakable marker
(a solid outline and an arrow) rather than by which card is a slightly
different shade

So I can: scan a grid of a dozen cards and pick out the one that failed, and
never lose track of where my selection is.

### User Story 4:
As a: user

I want to: land on the squad tab with the arrow keys already driving the card
grid, with no command box to escape from or into

So I can: start navigating immediately, and not be dumped into a text box
that accepts nothing.

### User Story 5:
As a: user

I want to: find the full output of any step a squad run executed — a
`clone_repo` in setup, the agent itself, a `create_pr` in teardown — in a
file under the run's directory, and have the daemon log tell me which file
when a step fails

So I can: read `awman squad logs`, see "step X failed, see /path/to/file",
open that file, and debug, instead of guessing which step broke and having
nothing but the exit code to go on.

### User Story 6:
As a: user

I want to: see a task go red, back off, and record `failed` when a step of
its workflow genuinely crashes, but not when the engine's yolo countdown
simply moved past an idle agent

So I can: trust the indicator and the card colours — red means something
broke, and a run that auto-advanced past a quiet agent is not noise I have
to investigate.

## Implementation Details:

Parts 1–4 live in `src/frontend/tui/`; Part 5 lives in
`src/frontend/squad/unattended.rs` and `src/engine/squad/scheduler.rs`;
Part 6 lives in `src/engine/squad/scheduler.rs`, `src/engine/squad/launcher.rs`
and `src/command/commands/squad/evaluation.rs`.
Nothing below touches the gateway trait or the task store schema. Every
colour named here is a plain `ratatui::style::Color` variant so it matches
the existing tab-bar palette (`tabs::tab_color`).

### Part 1 — the Ctrl-S hint moves into the New Tab dialog's hint row

- `key_handler.rs` builds the New Tab dialog as `Dialog::TextInput` with the
  prompt `"Working directory:\nPress Ctrl-S to open squad"`. The prompt
  becomes `"Working directory:"` alone.
- The `TextInput` renderer in `render/dialog.rs` draws a fixed hint row,
  `[Enter] submit   [Esc] cancel`. For the New Tab dialog only, the hint row
  becomes `[Enter] submit   [Esc] cancel   [Ctrl+S] open squad`.
- The renderer decides "this is the New Tab dialog" exactly the way the key
  handler already does — by the dialog title. Pull that string literal into
  one `pub(crate) const NEW_TAB_DIALOG_TITLE: &str = "New Tab"` (in
  `dialogs/mod.rs`) and use it from both the Ctrl-S intercept in
  `key_handler.rs` and the renderer, so the two can never disagree. Do not add
  a field to `Dialog::TextInput` for this: the variant is shared with the
  command-frontend interview dialogs and every match site would have to
  change for one hint.
- The dialog height is `prompt_lines + 10`; with a one-line prompt the box
  gets one row shorter, which is the correct outcome, not a regression.
- The existing key-handler test `ctrl_t_new_tab_dialog_shows_press_ctrl_s_hint`
  asserts on the prompt text; it is replaced by a render test that asserts
  the hint row (see Test Considerations).

### Part 2 — the pinned squad indicator on the bottom row

**Where.** `render/command_box.rs::render_suggestion_row` draws the 1-row line
under the command box (`chunks[6]` in `render.rs`). The indicator is appended
to that row, right-aligned with a one-cell right margin, on every tab and in
both of the row's modes (suggestions showing, or the `CWD:` / `Using
worktree:` fallback). It is the last thing on the row and never scrolls off:
the suggestion spans and the path are truncated to `area.width - indicator
width` rather than the other way round. When the row is narrower than the
indicator itself, only the circle is drawn; when narrower than one cell,
nothing.

**What.** The text is `squad ●` — the word in `DarkGray`, the circle
(`U+25CF`) in the state colour:

| State | Colour | Meaning |
|---|---|---|
| `NotRunning` | `DarkGray` | No squad daemon process is running (pidfile check). |
| `Unreachable` | `Yellow` | A daemon is running but this TUI cannot get a successful answer from it: no bearer key in hand (`SquadKeyState::Missing` equivalent), no endpoint sidecar, connection refused, timeout, HTTP 401/403, any other transport or HTTP error. |
| `Failed` | `Red` | Reachable, and at least one task's `last_run_status` is `Failed`. |
| `Running` | `Blue` | Reachable, and at least one task's `last_run_status` is `Running`. |
| `Healthy` | `Green` | Reachable and neither of the above. |
| `Unknown` | `DarkGray` | Before the first probe completes. Rendered identically to `NotRunning`. |

Precedence is top-down: a daemon that is not running is grey even if the last
snapshot had failures; unreachable beats every task-derived colour; **red
beats blue** (a failure needs attention and persists; a running task is
transient and will show once the failure is cleared or another run starts).
Decided: red over blue.

**Data source.** A new app-level poller, `src/frontend/tui/squad_indicator.rs`,
independent of the squad tab: the indicator must work on a TUI that has never
opened the squad tab, and the squad tab's own `SquadTaskPoller` deliberately
fetches only while that tab is focused. Shape:

- `pub enum SquadIndicator { Unknown, NotRunning, Unreachable, Failed, Running, Healthy }`
  plus `pub type SharedSquadIndicator = Arc<Mutex<SquadIndicator>>`, a new
  `App.squad_indicator: SharedSquadIndicator` field defaulting to `Unknown`.
- `SquadIndicatorPoller::start(shared, cancel) -> JoinHandle<()>`, a tokio
  task ticking every `SQUAD_INDICATOR_INTERVAL = 10s` (`MissedTickBehavior::Delay`),
  started from `tui::run` in `mod.rs` after `App` is built — **not** from
  `App::new`, which the unit tests construct freely and which must not touch
  `~/.awman/squad`. Its `CancellationToken` is cancelled when the event loop
  exits.
- Each tick, on the tokio runtime (never on the event-loop thread):
  1. `SquadSupervisor::from_env(&Env::from_process())`. `Env` is re-read every
     tick so a key minted mid-session (which `publish_key_to_process_env`
     puts into the process environment) is picked up without a restart.
  2. `daemon_is_running()` false → `NotRunning`. Error → `Unreachable`.
  3. Build a probe gateway. **This must not mint a key.** `gateway_from_meta`
     calls `provision_key`, which on a missing hash generates and writes one.
     Add `SquadSupervisor::probe_gateway(&self) -> Result<Option<RemoteTaskGateway>, CommandError>`
     that resolves the key with the existing read-only steps only (env key,
     then the process-minted key, else `None`) and delegates to the private
     `gateway_from_meta_with`. `None` (no sidecar) → `Unreachable`.
  4. `gateway.list().await`. `Err(_)` of any kind → `Unreachable`. `Ok(tasks)`
     → `Failed` / `Running` / `Healthy` by the table above.
  The whole probe is wrapped in a `tokio::time::timeout` of `2s` so a hung
  daemon cannot stack ticks; a timeout is `Unreachable`.
- The classification `fn classify(daemon_running: bool, probe: Result<Vec<Task>, _>) -> SquadIndicator`
  is a pure function, unit-tested without a daemon.
- The renderer reads the shared value with `lock().ok()` and falls back to
  `Unknown` on a poisoned mutex, matching how `render_squad_body` reads its
  snapshot.

**Layering.** The poller lives in the frontend and calls only
`SquadSupervisor` and `TaskGateway` (Layer 2 command-side types the TUI
already depends on for the squad tab). Run `make lint` /
`architecture-lint` — WI 0111 §7 fixed one Layer-1→2 import and this must
not add another.

### Part 3 — status-coloured dashed cards and a solid selected card

In `render/squad.rs::render_task_card`. Two independent axes: the task's
state picks the **colour**; selection picks the **line style** (dashed or
solid) and the title marker. Neither axis ever expresses the other.

**Card colour** is a pure function `fn card_status(task: &Task) -> CardStatus`
with `enum CardStatus { Paused, Running, Triggered, Failed, NeverRun, Active }`,
evaluated in this order (first match wins):

| Order | Condition | `CardStatus` | Border colour |
|---|---|---|---|
| 1 | `task.status == Paused` | `Paused` | `DarkGray` |
| 2 | `last_run_status == Some(Running)` | `Running` | `Blue` |
| 3 | `trigger_requested_at.is_some()` | `Triggered` | `Magenta` |
| 4 | `last_run_status == Some(Failed)` | `Failed` | `Red` |
| 5 | `last_run_at.is_none()` | `NeverRun` | `Yellow` |
| 6 | otherwise (`NotTriggered`, `WorkflowExecuted`, `Interrupted`) | `Active` | `Green` |

Rationale for the order: paused is a user decision and outranks everything
(the card should read "you switched this off" even if its last run failed —
see Edge Cases). A running task cannot honour a trigger until it finishes, so
running outranks triggered. Triggered outranks failed because the user has
just acted on the task and wants to see the trigger acknowledged; the red
returns if the triggered run fails too.

**Line style.** Every card is drawn with a **dashed** outline; the selected
card alone is drawn with the ordinary **solid** rounded outline. Ratatui
`0.30` has no dashed `BorderType`, so the dashed style uses
`Block::border_set` with a custom `symbols::border::Set`: the rounded corners
(`╭ ╮ ╰ ╯`) with dashed edges `╌` (U+254C) horizontally and `┆` (U+2506)
vertically. `container_view.rs` already builds a custom border set this way.
Define it once as `const DASHED_ROUNDED: symbols::border::Set` in
`render/squad.rs`. The selected card keeps `BorderType::Rounded`. Dashed
versus solid means *only* "not selected" versus "selected"; a paused task's
card is dashed for the same reason every other unselected card is, and turns
solid when selected like any other.

**Selection marker.** `is_selected` no longer changes the border colour. The
selected card's title becomes `" ➡ {name} "` (`U+27A1`, the same glyph
`render_tab_bar` uses) and is `BOLD` in the card's status colour; unselected
titles stay bold in the default foreground. The border colour is the status
colour whether or not the card is selected.

**No flash.** The selected card does not animate. `App` gains no clock, and
the renderer stays a pure function of state.

**Docs and hint line.** The key-hint line under the grid is unchanged. A
one-line legend is *not* added to the tab; the colours are documented in
`docs/12-squad.md` (see Documentation).

### Part 4 — the command box is permanently inactive on the squad tab

Scope: the squad tab while **no attach session owns its slots**
(`tab.is_squad && tab.container_slots.is_empty()`), exactly the condition
`render.rs`, `status_bar.rs` and the key handler already use to mean "the
card grid is on screen". During an attach session the tab behaves as a
normal container tab and none of this applies.

- **Focus is forced onto the grid.** `handle_key_event` selects
  `FocusContext::SquadList` for that condition regardless of `app.focus`
  (today it also requires `app.focus == ExecutionWindow`). Additionally,
  `App::tick_all_tabs` normalises `self.focus = Focus::ExecutionWindow`
  whenever the active tab meets the condition, so every path that lands on
  the squad tab — `open_or_focus_squad_tab`, `build_and_install_squad_tab` /
  `poll_squad_startup`, `switch_to_prev_tab` / `switch_to_next_tab`,
  `close_active_tab`, the `awman squad` startup path in `mod.rs`, and the
  post-detach `app.focus = Focus::CommandBox` in `key_handler.rs` — ends up
  on the grid without each being patched individually. The tick runs before
  input is polled, so the very first key after a tab switch already reaches
  the grid.
- **Leaving the squad tab restores the command box.** When the active tab
  changes *from* the squad tab *to* a non-squad tab, focus becomes
  `Focus::CommandBox`. Implemented in the same tick normalisation by
  remembering the previous active index (`App.last_active_tab: usize`).
  A normal-to-normal switch keeps today's behaviour (focus is untouched).
- **`Esc` is a no-op.** `keymap::map_squad_list_key` maps `KeyCode::Esc` to
  `Action::None` instead of `Action::FocusCommandBox`. `Action::None`'s
  fallback (return focus to the command box when the tab is done/errored)
  is gated on `FocusContext::ExecutionWindow` and does not fire for
  `SquadList`. `↑`/`↓`/`←`/`→`/`Enter` and the letter bindings are unchanged.
- **Rendering.** `render_command_box` gets a squad branch before the
  `focused` computation: title `" command (inactive) "`, `DarkGray` border,
  body `"  Use ↑ ↓ ← → and Enter to navigate the squad tab"` in `DarkGray`,
  no cursor, no ghost text, no input echo. Ratatui only shows the cursor
  where `set_cursor_position` is called, so omitting it is enough. Any
  `app.command_input.text` typed on another tab is preserved untouched and
  reappears when the user returns to that tab.
- **Suggestion row.** `render_suggestion_row` shows suggestions only when
  `app.focus == CommandBox`; with focus forced to the grid, the squad tab
  always shows the context fallback (the synthetic session's squad root path)
  plus the Part 2 indicator. No change needed beyond Part 2.
- **Mouse.** `mouse_handler.rs` has no squad-specific handling and no click
  path that sets `Focus::CommandBox` today, so nothing to change; the tick
  normalisation covers any future click that does.

### Part 5 — every step logs to the run directory, and every failure names its file

**How it works today.** A squad run's agent containers are logged by
`UnattendedFrontend` (`src/frontend/squad/unattended.rs`): the
`AgentStatus::Running { container_name }` callback opens
`<run_log_dir>/<container-name>.log` and `take_io` drains the container's
stdout/stderr into it. Setup and teardown steps take a different path
entirely: `ExecWorkflowCommand` starts a plain background container per step
(`runtime.start_background`) and the engine runs the step's shell command
through `exec_streaming`, handing each output line to
`WorkflowFrontend::on_setup_step_output` / `on_teardown_step_output`. The
unattended frontend does not implement those hooks, so the lines are
dropped. On failure the engine calls `on_setup_step_failed` /
`on_teardown_step_failed` (also unimplemented — silent) and, for agent
steps, `report_step_status(step, Failed { exit_code })`, which the frontend
logs at `info` as a generic "lifecycle transition" with no path. The
scheduler then records the run outcome at `info` with the status and, for a
failed evaluation, keeps the error text only in the run row.

**Per-step log files.** All under the existing run directory
`~/.awman/squad/tasks/<name>/runs/<run-id>/`, next to the agent logs and the
verdict file:

| Step kind | File | Written by |
|---|---|---|
| Evaluation leader, agent workflow step | `<container-name>.log` (unchanged) | `AgentStatus::Running` + `take_io`, as today |
| Setup step *n* (1-based, in definition order) | `setup-<n>-<slug>.log` | new `on_setup_step_*` implementations |
| Teardown step *n* | `teardown-<n>-<slug>.log` | new `on_teardown_step_*` implementations |

`<slug>` is the step description from `setup_step_description` /
`teardown_step_description` (e.g. `clone_repo`, `run_shell`, `create_pr`)
lower-cased, with every character outside `[a-z0-9]` collapsed to `-` and
truncated to 40 characters, so the filename is safe and readable. The number
prefix keeps two identical steps apart and preserves execution order in a
directory listing. Implementation in `UnattendedFrontend`:

- New field `phase_log: Option<PhaseLog>` where
  `struct PhaseLog { path: PathBuf, file: File, header_written: bool }`, and
  counters `setup_steps_seen: usize`, `teardown_steps_seen: usize`.
- `on_setup_step_started(desc)`: increment the counter, derive the path,
  open it append-mode (same `OpenOptions` as `prepare_container_log`), write
  a header line `# setup step <n>: <desc>` followed by the ISO timestamp,
  store it in `phase_log`, and log
  `squad setup step started` at `info` with `step`, `log_path`. An open
  failure logs `squad failed to open setup step log` at `error` with the
  path and error, leaves `phase_log = None`, and the step still runs.
- `on_setup_step_output(line)`: `writeln!` to `phase_log` and flush per
  line, the same durability rule `spawn_file_drain` applies to agent output.
  No-op when `phase_log` is `None`.
- `on_setup_step_fixing(desc, attempt, of)`: write a separator line
  `# on_failure remediation attempt <attempt>/<of>` into the *same* file, so
  a remediated step's original output and each retry sit together in
  chronological order. The remediation agent itself is a container and gets
  its own `<container-name>.log` through the agent path; the separator line
  names it once `AgentStatus::Running` arrives (the frontend records the
  most recent container name for this purpose).
- `on_setup_step_completed(desc)`: flush, close, log
  `squad setup step succeeded` at `info` with `log_path`.
- `on_setup_step_failed(desc, exit_code, stderr)`: append the engine's
  `stderr` argument to the file (it is the error text when the command never
  launched, and would otherwise be lost), flush, close, then log
  `squad setup step failed` at **`error`** with `task`, `run_id`, `step`,
  `exit_code`, `log_path`, and the first line of `stderr` as `error`.
- The four `on_teardown_step_*` hooks mirror these with the teardown counter
  and prefix.
- `Drop` for `UnattendedFrontend` flushes and closes an open `phase_log` so a
  frontend torn down mid-step (workflow aborted) loses nothing buffered.

**Failure lines name the file.** Every place the daemon log records a step
failing gets a `log_path` field:

- Setup / teardown: the `on_*_step_failed` lines above.
- Agent workflow step: `report_step_status(step, Failed { exit_code })` is
  logged at **`error`** instead of `info`, with `log_path` set to the
  `<container-name>.log` the frontend opened for that step. To know which,
  `UnattendedFrontend` gains `current_step: Option<String>` (set by
  `report_step_status(step, Running)`, which the engine fires *before*
  `execution_for_step` launches the container) and
  `step_logs: HashMap<String, PathBuf>` (inserted by `prepare_container_log`
  under `current_step` when present). The engine's existing
  "Recent output saved to `~/.awman/logs/…`" tail dump still arrives through
  `write_message` and is left as-is; the squad line is the one that names
  the full log.
- Evaluation leader: the leader's frontend is a `Box<dyn AgentFrontend>`
  moved into `SquadAgentLauncher::run_leader`, so the evaluator cannot read
  the container name back from it, and `AgentExitInfo` does not carry one.
  The launcher does know it — it stamps the name in
  `SquadContainerIdentity::stamp` — so `run_leader` returns
  `LeaderExit { exit: AgentExitInfo, container_name: String }` instead of a
  bare `AgentExitInfo`. `evaluation.rs` then logs `squad evaluation
  container finished` with `log_path = <run_log_dir>/<container_name>.log`
  on every attempt, and when the exit code is non-zero or
  `decide_from_verdict` yields `VerdictDecision::Failed`, the existing
  `warn!("squad evaluation produced no usable verdict")` becomes `error!`
  carrying the same `log_path`, and the error string placed in
  `EvaluationOutcome::Failed { error }` is suffixed with
  ` (log: <path>)` so the run row and the TUI detail modal show it too.
- Scheduler (`evaluate_task` in `scheduler.rs`): when `classify` yields
  `RunStatus::Failed`, the `squad task run finished` line is logged at
  **`error`** with the `error` text and `log_dir = run_log_dir`, so even a
  failure that happened before any step ran (workflow validation exhausted,
  config unreadable) points at the directory to look in. A non-failed run
  keeps its `info` line.
- Run row: beyond the leader suffix above, `RunDetail.error` is unchanged
  and there is no schema change. The TUI detail modal already shows the
  run's error and run id, and the run directory follows from those.

**Message shape.** All new lines are `tracing` events with structured fields
(`task`, `run_id`, `step`, `exit_code`, `log_path`), matching the existing
`squad agent container launched` line, so `awman squad logs` shows the path
in the same `log_path=/…` form users already see for agent containers.

### Part 6 — the run status follows the workflow's real outcome; yolo kills are not failures

**How it works today.**
- The engine's yolo auto-advance (`yolo_advance_parallel` and the
  single-step equivalent) cancels the idle container, reports
  `KILLED_EXIT_CODE` (137) to the frontend, marks the step
  `StepState::Succeeded`, and continues. The workflow's overall exit code
  is therefore unaffected by a countdown kill: `exec workflow` derives it
  from `WorkflowOutcome` (`Completed` → 0, `Failed { exit_code }` → that
  code, `Aborted`/`CompletedTeardownFailed`/engine error → 1).
- `evaluate_inner` wraps that exit code in
  `EvaluationOutcome::WorkflowExecuted { exit_code }` and the scheduler's
  `classify` maps every `WorkflowExecuted` to `RunStatus::WorkflowExecuted`,
  no backoff, regardless of the code. So a genuinely crashed step is
  recorded as success.
- The evaluation leader is driven by `drive_unattended_agent`, which on
  countdown expiry cancels the container and returns exit 137. The
  evaluator only logs the exit code; the verdict file decides. A leader
  killed before it wrote a verdict is `VerdictError::Missing` →
  `EvaluationOutcome::Failed` → backoff. That is a countdown kill being
  recorded as a failure.

**The rule.** One sentence, applied everywhere: *a container that exits
non-zero on its own fails the run; a container awman killed because its
yolo countdown expired does not.*

**Generated workflow.** `classify` in `scheduler.rs` gains the run log
directory as a second argument and maps `WorkflowExecuted`:

| `exit_code` | `RunStatus` | `RunDetail.error` | backoff |
|---|---|---|---|
| `Some(0)` | `WorkflowExecuted` | `None` | no |
| `Some(n)`, n ≠ 0 | `Failed` | `generated workflow exited with code <n>; see <run_log_dir>` | yes |
| `None` (paused — cannot happen unattended, the frontend never pauses) | `WorkflowExecuted` | `None` | no |

`workflow_path` and `workflow_state_path` stay on the row in the failed
case, so the TUI detail modal and the daemon's workflow route still find
the state file. Nothing about yolo needs handling here: a countdown-killed
step is already `Succeeded` in the engine and never reaches a non-zero
overall exit code. What *does* reach a non-zero code is exactly the set of
things that should: a step container exiting non-zero on its own
(`WorkflowOutcome::Failed`), an aborting setup step (`setup_failed`), any
teardown step failure (`CompletedTeardownFailed`), an `abort_on_failure`
abort, or an engine error.

**Evaluation leader.** `SquadAgentLauncher::run_leader` already knows when
the countdown expired — it is the `CountdownOutcome::Expired` arm of
`drive_unattended_agent`. The `LeaderExit` struct Part 5 introduces gains
`killed_by_countdown: bool`, set only on that arm (a 137 the container
produced on its own — an OOM kill, an external `docker kill` — leaves it
`false`). The evaluator then decides, per attempt, in this order:

| Leader exit | Verdict on disk | Outcome |
|---|---|---|
| 0 | valid | as today: triggered → run workflow, else `NotTriggered` |
| 0 | missing/unparseable | `Failed` (protocol violation, as today) |
| killed by countdown | valid | honoured, as today — the leader finished its job before going idle |
| killed by countdown | missing | **`NotTriggered`**, reason `leader idle: killed by the yolo countdown before writing a verdict`, logged at `warn` with `log_path`, no backoff |
| non-zero, not countdown | any | **`Failed`**, error `leader agent exited with code <n> (log: <path>)`, backoff |

The last row is the rule applied strictly: a leader that crashes after
writing `triggered: true` does not get its workflow run, because a
crashing leader is a real error the user asked to see. Repair attempts
(`leader-repair-<n>`) follow the same table.

**Startup-grace kills.** `StuckEvent::StartupGraceExpired` means the
container produced no output at all before its startup grace ran out, and
the runtime bridge killed it. The engine finalises that step as `Failed`
(137, with `awman_killed` set only to suppress the tail dump). This is
**not** a yolo countdown and stays a failure: an agent that never started
producing output is a real problem, not an idle agent being moved past.
Decided.

**Effect on the rest of this work item.** `RunStatus::Failed` now covers
workflow failures, so the Part 2 indicator turns red and the Part 3 card
turns red for them, and the Part 5 error lines and the run row's error
text are what the user reads to find out why. The failure-count backoff in
`evaluate_task` is unchanged; it simply now sees these failures.

## Edge Case Considerations:

**Part 1**
- The Ctrl-S intercept and the hint are both keyed off `NEW_TAB_DIALOG_TITLE`;
  a command-frontend interview that happened to title a `TextInput` "New
  Tab" would gain the hint and the shortcut. No such interview exists; the
  const makes the coupling greppable.
- The hint row is `62` cells wide; the dialog's minimum width is `50`, and
  the row is clipped (not wrapped) by the buffer at narrower widths, as the
  existing `[Ctrl+Enter / Ctrl+S] submit …` hint already is. Acceptable.

**Part 2**
- The indicator is on every tab, including a remote-bound tab and the squad
  tab itself. On the squad tab it duplicates what the card grid shows; that
  is intended ("pinned there always").
- **Red vs blue** when one task is running and another's last run failed:
  red wins (decided).
- A task with `last_run_status == Some(Interrupted)` (the daemon restarted
  under it) is *not* red for the indicator and *not* red for its card. Only
  `Failed` is red (decided).
- `Unreachable` covers `SquadKeyState::Missing` without calling
  `key_state()`: that method consumes the one-shot key disclosure
  (`take_generated_key_setup`) and must never be called from a poller. The
  probe gateway resolves the key read-only and lets the daemon's 401 speak.
- The TUI's own squad tab may hold a live `app.squad_gateway` while the
  probe builds a fresh one from the sidecar each tick. Sharing the tab's
  gateway would tie the indicator's lifetime to the tab's; a fresh gateway is
  one JSON sidecar read per tick and is the simpler invariant.
- A `NotRunning` daemon with a stale `server.json` sidecar: the pid check
  comes first, so the stale endpoint is never dialled.
- A sandbox-class container runtime (no daemon possible): the pid check
  still answers honestly; `runtime_guard` is not consulted. The indicator
  reads grey on such a host, which is correct.
- The 10 s cadence plus a 2 s probe timeout means a hung daemon costs one
  in-flight request at a time, never a pile-up.
- Suggestions row: with the indicator pinned right, a long suggestion list
  is truncated to the remaining width; it was previously allowed to run to
  the edge. Truncate the *spans*, not the indicator.
- The `+A -D` git summary lives on the status bar (one row up), not this row,
  so the two right-pinned widgets never collide.

**Part 3**
- **Paused + failed** reads grey, not red. The user paused it; the
  `Outcome: failed` body line still says what happened (decided).
- **Triggered + failed** reads magenta until the trigger is honoured, then
  blue while running, then whatever the run produced.
- A snapshot refresh that reorders tasks moves the arrow and the solid
  outline with the linear `selected` index exactly as today.
- Terminals that do not render `╌`/`┆` (rare; both are in the Box Drawing
  block alongside the `╭` corners already required) show a fallback glyph on
  every unselected card; the selected card's solid outline and `➡` still
  single it out, and the colours are unaffected.
- With exactly one task the only card is always selected and therefore
  always solid; that is correct, not a special case.
- Colour-blind users: every status also appears as text in the card body
  (`Next: paused`, `Outcome: failed`, `Next: triggered — next tick`,
  `Last run: —`), so colour is never the only channel. Selection is likewise
  not colour: it is the arrow and the solid-versus-dashed line.

**Part 4**
- The `awman squad` startup path builds the app with a squad tab and
  `focus: Focus::CommandBox` (the `App::new` default); the first tick fixes it
  before the first key is read. The `KeyMissing` and `Err` fallbacks open a
  normal tab and are unaffected.
- After `Ctrl-\` detaches an attach session the tab returns to the grid;
  the tick normalisation moves focus there on the same tick, so the explicit
  `Focus::CommandBox` assignment in the detach branch becomes harmless. It is
  still updated to `ExecutionWindow` for clarity.
- `q` on the squad tab: with focus never on the command box there is no `q
  to quit` ghost. `Ctrl-C` (close-tab / quit confirm) is global and still
  works; the hint line under the grid does not need to mention it.
- Typing a letter that is not a squad binding is now silently swallowed on
  the squad tab rather than landing in the command box. That is the point.
- `last_active_tab` must be updated after the focus decision in the tick
  and must be clamped when tabs are removed (`close_active_tab`), or a
  removed index could compare equal to a new tab's index.

**Part 5**
- A failed agent step used to leave the run recorded as `workflow
  executed`; Part 6 changes that. Part 5's error line and Part 6's status
  change are deliberately separate so a best-effort step failure (see Part
  6 edge cases) still gets its log path in the daemon log even though it
  does not fail the run.
- `PollCi` setup/teardown steps run natively (no container, no shell
  output): they still get a log file containing the header, the poll
  messages the engine routes through `write_message`, and on failure the
  error text, so the numbering stays contiguous and "every step has a file"
  holds.
- A setup step whose container fails to *start* (`container_for_step`
  errors) never calls `on_setup_step_output`; `on_setup_step_failed` still
  fires with the launch error as `stderr`, which is why that text is
  appended to the file before it is closed.
- `on_setup_step_started` and `on_setup_step_output` carry no step index,
  so the frontend counts starts. The engine calls them strictly in
  definition order and never concurrently (setup/teardown are sequential
  phases), so the counter is reliable. A remediation retry re-runs the same
  step through `run_single_setup_step` **without** a fresh
  `on_setup_step_started`, which is exactly why output during remediation
  lands in the still-open file under a separator rather than a new one.
- Two steps with the same description (two `run_shell` steps) get
  `setup-1-run-shell.log` and `setup-2-run-shell.log`; the index is the
  disambiguator, never the slug.
- The slug is derived from the *description*, which for `run_shell`
  includes the command text; the 40-character cap plus `[a-z0-9-]`
  collapsing keeps it a safe filename component. A description that
  collapses to empty yields just `setup-<n>.log`.
- The unattended frontend is shared by the whole workflow run: agent steps
  and phase steps interleave (setup → agents → teardown), so `phase_log`
  being `None` outside a phase step is the normal state, and
  `on_*_step_output` arriving with no open file is silently dropped rather
  than an error — it cannot happen through the engine, but the frontend
  must not panic if it does.
- Log lines already include `task` and `run_id`; the run directory is
  `~/.awman/squad/tasks/<task>/runs/<run_id>/`, so even a line without
  `log_path` (the scheduler's early failures) is locatable. The
  `log_dir` field on the scheduler's error line makes it explicit anyway.
- Nothing here changes the non-squad `exec workflow` path: `CliFrontend`
  and the TUI frontends keep their own implementations of the step hooks
  (streaming to the terminal), and the engine's `~/.awman/logs` failure
  tail dump is untouched.

**Part 6**
- **Best-effort setup steps.** A setup step with `abort_on_failure: false`
  that fails leaves the step `Failed` in the state file, but the engine
  continues and the workflow can still complete with exit 0. The run then
  reads `workflow executed`: the author marked that step best-effort, and
  the overall exit code is the engine's own definition of success. The
  Part 5 error line with its log path is still in the daemon log. Teardown
  has no such exemption — any teardown failure yields
  `CompletedTeardownFailed` → exit 1 → `Failed`, matching how `exec
  workflow` already reports it.
- **Startup-grace kill counts as a failure** (decided). awman did kill the
  container, but because it never produced output, not because a countdown
  moved past a working agent. Treating it as success would hide a broken
  agent image forever.
- **Leader crash after a valid verdict is a failure** (decided), applied
  strictly from the rule.
- A leader killed by the countdown that *had* written `triggered: true`
  runs its workflow exactly as today. The kill is not distinguishable from
  "finished and went idle" and should not be.
- `killed_by_countdown` is set by the launcher on the `Expired` arm only.
  The synthetic 137 the launcher fabricates when the backend's wait errors
  after the kill also sets it — that path only exists inside the `Expired`
  arm.
- A repeatedly idle leader now produces a string of `not triggered` runs
  with a `warn` line each, and no backoff. That is the requested behaviour
  (a countdown kill is not a failure); the `warn` line and its `log_path`
  are what make the pattern visible in `awman squad logs`.
- The daemon-restart `Interrupted` status is untouched.
- Backoff semantics are unchanged; more runs now qualify. The existing
  `backoff_secs` cap of six hours still bounds a task whose workflow fails
  every time.
- `classify` remains a pure Layer 1 function; the run log directory it
  needs is already in `EvaluateArgs`.

## Test Considerations:

Unit tests sit next to the code (`#[cfg(test)]`), render assertions in
`src/frontend/tui/tests/render_tests.rs`, key flow in
`src/frontend/tui/tests/key_handler_tests.rs`, following the existing
`push_squad_tab` / `set_squad_tasks` / `fake_task` helpers. Extend `fake_task`
with a builder-ish helper (`fake_task_with(name, status, last_run_status,
last_run_at, trigger)`) rather than copying the struct literal per test.

**Part 1**
- Render test: `Ctrl-T` opens the dialog; the rendered buffer contains
  `[Ctrl+S] open squad` on the hint row and does **not** contain `Press
  Ctrl-S` in the prompt. Replaces `ctrl_t_new_tab_dialog_shows_press_ctrl_s_hint`.
- Render test: a command-frontend `TextInput` with another title shows the
  plain `[Enter] submit   [Esc] cancel` hint only.
- Existing `ctrl_s_in_new_tab_dialog_*` tests continue to pass unchanged.

**Part 2**
- Unit tests for `classify`: every row of the state table, the red-over-blue
  precedence, `Interrupted` not counted as failed, an empty task list is
  `Healthy`, every error kind (`RemoteConnectionRefused`, `RemoteTimeout`,
  `RemoteHttpStatus { 401 }`, `RemoteTransport`) is `Unreachable`.
- Unit test for `SquadSupervisor::probe_gateway`: with no key hash on disk,
  no env key, and auth on, it returns `Ok(None)`-or-a-keyless-gateway and
  writes **no** `squad_key.hash` (assert the file is absent afterwards).
  Uses the temp-dir `SquadPaths` fixture the daemon tests already use.
- Render tests (one per colour): set `app.squad_indicator` directly, render,
  assert the last non-space cells of the bottom row are `squad ●` with the
  circle cell styled in the expected colour, on a normal tab **and** on the
  squad tab.
- Render test: suggestions showing and a narrow width — the indicator is
  still at the right edge, and the suggestion text is truncated before it.
- Render test: a long `CWD:` path is truncated so the indicator fits.
- Render test: width below the indicator's width draws only the circle.

**Part 3**
- Unit tests for `card_status`: every row of the precedence table, plus the
  paused+failed, running+triggered, and triggered+failed combinations.
- Render tests: for each status, the selected and an unselected card's border
  cells carry the expected colour; every unselected card's top edge uses `╌`
  and its side edges `┆`; the selected card's edges use `─` and `│`; the
  selected title starts with `➡ `; the unselected title does not; a paused
  card is dashed when unselected and solid when selected, in `DarkGray` both
  times.
- The existing tests `squad_task_cards_render_rounded_borders_and_the_last_run_outcome`
  and `squad_task_cards_label_the_description_and_the_last_run_timestamp`
  are updated where they assert the old cyan selection border.

**Part 4**
- Key test: after `push_squad_tab` with `app.focus = Focus::CommandBox`, one
  `tick_all_tabs` sets focus to `ExecutionWindow`; `↓` then moves the
  selection (not the scroll offset) without a preceding `↑`.
- Key test: `Esc` on the squad list leaves focus on the grid and the
  selection unchanged.
- Key test: a plain letter with no squad binding (`x`) does not append to
  `app.command_input.text`.
- Key test: `Ctrl-A` from the squad tab to a normal tab sets focus to
  `CommandBox`; `Ctrl-D` back to the squad tab sets it to `ExecutionWindow`
  after the tick; a normal→normal switch leaves focus alone.
- Key test: closing a tab so the squad tab becomes active focuses the grid.
- Key test: after `detach_squad_attach` the next tick focuses the grid.
- Render test: on the squad tab the command box title is
  `command (inactive)`, the body contains `Use ↑ ↓ ← → and Enter`, and the
  frame reports no cursor position; on a normal tab the box is unchanged.
- Render test: during an attach session (non-empty `container_slots`) the
  command box renders as it does today.
- Keymap unit test: `map_key(Esc, SquadList) == Action::None`; the existing
  `map_execution_window_key` Esc test is untouched.

**Part 5** (in `src/frontend/squad/unattended.rs`'s test module, using its
existing `captured_tracing` helper and a `tempfile` run directory)
- Setup step happy path: `on_setup_step_started("clone_repo …")`, three
  `on_setup_step_output` lines, `on_setup_step_completed` → the file
  `setup-1-clone-repo….log` exists, starts with the header line, contains
  the three lines in order, and the daemon log has an `info` line naming
  `log_path`.
- Setup step failure: `on_setup_step_failed(desc, 1, "fatal: not a git
  repository")` → the file ends with that stderr, and the captured tracing
  contains an `ERROR` line with `step`, `exit_code=1`, and `log_path`
  equal to the file's path.
- Two steps with the same description produce `setup-1-…` and `setup-2-…`.
- Teardown mirrors setup with the `teardown-` prefix and its own counter
  (a workflow with two setup and one teardown step yields `setup-1`,
  `setup-2`, `teardown-1`).
- Remediation: `started` → `output` → `fixing(1, 2)` → `output` →
  `completed` writes everything to one file with the separator between the
  two output runs.
- Agent step failure: `report_step_status(step, Running)`,
  `report_status(AgentStatus::Running { container_name })`,
  `report_step_status(step, Failed { exit_code: 2 })` → an `ERROR` line with
  `log_path = <run_dir>/<container_name>.log`. A `Succeeded` transition
  stays `INFO`.
- Slug helper unit tests: lower-casing, collapsing runs of punctuation and
  whitespace to a single `-`, trimming leading/trailing `-`, 40-character
  cap, empty result.
- `Drop` with an open `phase_log` flushes the file (write a line, drop the
  frontend, read the file back).
- Launcher: `run_leader` returns the stamped container name; the existing
  launcher tests that assert the `awman-squad-<slug>-<hex>` name pattern
  are extended to assert it on the returned `LeaderExit`.
- Evaluator: a leader exit with a non-zero code yields
  `EvaluationOutcome::Failed { error }` whose text ends with
  `(log: <run_log_dir>/<container_name>.log)` (exercised through the
  existing fake-launcher evaluator tests).
- Scheduler: `evaluate_task` with a `Failed` outcome emits an `ERROR`
  `squad task run finished` line carrying `error` and `log_dir`; a
  `WorkflowExecuted { exit_code: Some(0) }` outcome stays `INFO` (extends
  the existing `classify`/`evaluate_task` tests with `captured_tracing`).

**Part 6**
- `classify` unit tests (`scheduler.rs`): `WorkflowExecuted { Some(0) }` →
  `WorkflowExecuted`, no backoff, paths kept; `WorkflowExecuted { Some(1) }`
  and `Some(137)` → `Failed`, backoff, paths kept, error text names the
  code and the run log directory; `WorkflowExecuted { None }` →
  `WorkflowExecuted`; `NotTriggered` and `Failed` unchanged.
- Scheduler integration (existing fake-evaluator harness): a
  `WorkflowExecuted { Some(2) }` outcome increments the task's failure
  count and sets `backoff_until`; a following `Some(0)` clears it.
- Engine invariant: the existing
  `parallel_group_yolo_expiry_launches_queued_step` test already drives a
  step through countdown expiry and asserts the workflow ends
  `WorkflowOutcome::Completed`. That is the property Part 6 relies on; no
  new engine test is needed.
- Launcher (`launcher.rs`, `AgentExecution::finished` fakes): an execution
  that exits on its own returns `killed_by_countdown = false`, including
  one that exits 137 on its own. The `Expired` arm cannot be reached with a
  pre-finished fake, so the countdown-kill branch is covered through the
  pure `decide_leader_outcome` below rather than a live countdown.
- Evaluator: the leader decision is the pure `decide_leader_outcome` /
  `leader_exit_failure` pair in `evaluation.rs`, unit-tested one case per
  row of the leader table: clean exit + valid verdict → as today; clean
  exit + missing verdict → `Failed` with the log path suffixed; countdown
  kill + valid `triggered` verdict → workflow runs; countdown kill +
  missing verdict → `NotTriggered` with `LEADER_IDLE_KILL_REASON`;
  non-zero non-countdown exit + any verdict → `Failed` with the code and
  log path in the error.
- TUI: a task whose `last_run_status` is `Failed` because of a workflow
  exit code renders a red card and a red indicator — covered by the Part 2
  and Part 3 tests, which do not care *why* the status is `Failed`.
- `make test` and `architecture-lint` green.

## Codebase Integration:
- follow established conventions, best practices, testing, and architecture
  patterns from the project's aspec.
- Render modules read shared state via `lock().ok()` with a default on
  poison (as `render_squad_body` does). Pollers hold no policy: `classify`
  and `card_status` are pure and live beside the state they describe.
- No new global key bindings. The only key change is `Esc` inside
  `FocusContext::SquadList`.
- Part 5 adds no new engine hooks: it implements `WorkflowFrontend` methods
  that already exist with no-op defaults, and widens one launcher return
  type. The engine stays agent- and frontend-agnostic.
- Colours reuse the `tab_color` palette; no RGB colours are introduced.
- The indicator poller is started from `tui::run`, never from `App::new`, so
  the unit-test `App` constructors stay free of filesystem side effects.

## Documentation

After implementation is complete, update user-facing documentation in `docs/`
to reflect the current state of the tool:

- `docs/12-squad.md` — "The squad tab": replace the sentence about the New
  Tab prompt's second line with the hint-row binding; add a short "Card
  colours" table (grey paused, green active, red failed, blue running,
  yellow never run, magenta triggered) and say that cards are dashed except
  the selected one, which is solid and carries the `➡` marker; in "Key
  bindings in the task list" remove `Esc`
  (or note it does nothing) and say the grid is always focused so the arrows
  work immediately; add a "Squad indicator" subsection with the five colours
  and what each means, noting it is visible on every tab.
- `docs/12-squad.md` — "Logs for the daemon and its agents": after the
  per-container log block, add that setup and teardown steps write
  `setup-<n>-<step>.log` / `teardown-<n>-<step>.log` in the same run
  directory, one file per step in execution order, with remediation
  attempts appended to the original step's file; and state that when any
  step fails (setup, agent, or teardown) the daemon log's failure line
  names the log file as `log_path=…`, and a failed evaluation's error (as
  shown by `squad runs` and the task detail modal) ends with the leader's
  log path. Update the sentence "The evaluation agent and every
  generated-workflow container use this layout" to cover phase steps.
- `docs/12-squad.md` — the run outcomes (the `Outcome` field description
  under "The squad tab", and the "Guardrails for unattended execution"
  bullet on yolo auto-advance): state that a run is `failed` when its
  generated workflow exits non-zero — a step container that crashes, an
  aborting setup step, any teardown failure — and that the task then backs
  off; and that an agent moved past by the yolo countdown is *not* a
  failure, for workflow steps and for the evaluation leader alike (a leader
  killed before writing a verdict records `not triggered` and a warning in
  the daemon log). Mention that a best-effort setup step
  (`abort_on_failure: false`) failing does not fail the run.
- `docs/02-using-the-tui.md` — the layout overview (the bottom rows) gains
  the `squad ●` indicator with a pointer to `12-squad.md`; the "The squad
  tab" subsection's description of the New Tab prompt is updated to the
  hint-row wording; the command box description notes it is inactive on the
  squad tab.
- No new doc file; no work-item-specific docs.
