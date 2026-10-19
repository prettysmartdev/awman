# Work Item: Bug + Feature

Title: squad task management — delete cascade, task editing, per-task agent config, card width, daemon-start confirmation
Issue: (reported directly)

## Summary:
- Five squad changes reported after WI 0108:
  1. **Bug** — deleting a task from the TUI silently does nothing once the task
     has any run history.
  2. **Feature** — a task can be edited from both the CLI and the TUI.
  3. **Feature** — task creation can add extra models for the leader agent and
     extra available agents, stored in a task-specific `config.json` with the
     same shape as every other awman config file; images for every configured
     agent are built automatically so the leader can use them in its workflows.
  4. **Polish** — task cards in the TUI are at least one third of the grid
     width.
  5. **Feature** — opening the squad tab in the TUI asks before starting a
     daemon that is not already running.
  6. **Feature** — a way to leave an attached container view that does not
     signal the agent the way Ctrl-C does.

## User Stories

### User Story 1:
As a: user

I want to: delete a squad task from the TUI and have it actually disappear

So I can: retire tasks without dropping to a terminal, and see why when a
squad action fails instead of watching nothing happen.

### User Story 2:
As a: user

I want to: edit an existing task's description, schedule, agent, model,
overlays and agent pool from the CLI and the TUI

So I can: correct a task without deleting and recreating it (which would throw
away its durable workspace and run history).

### User Story 3:
As a: user

I want to: give one task its own set of agents and models when I create it

So I can: let that task's leader pick between several agents in the workflows
it generates, without changing the global squad settings every other task uses.

### User Story 4:
As a: user

I want to: see task cards that are wide enough to read

So I can: read a task's description summary on a wide terminal instead of a
row of narrow columns.

### User Story 5:
As a: user

I want to: be asked before awman starts a squad daemon in the background

So I can: open the squad tab without silently acquiring a long-lived
background process I did not ask for.

### User Story 6:
As a: user

I want to: detach from an attached agent without interrupting it

So I can: look in on unattended work and leave again, instead of having Ctrl-C —
which is forwarded straight into the agent's terminal — as my only way out.

## Implementation Details:

### 1. Delete (root-caused empirically)
`TaskStore::delete` issued a bare `DELETE FROM squad_tasks WHERE name = ?`.
`squad_runs.task_id` carries `REFERENCES squad_tasks(id)`, so as soon as a task
had recorded a run the delete failed with `FOREIGN KEY constraint failed`. The
daemon answered `400`, the TUI wrote the error to the tab's status log — which
the squad tab never renders, because its body is the card grid — and the task
stayed in the list with no visible explanation. `awman squad remove` from a
terminal looked fine, because a task removed before its first tick has no runs.

Two fixes:
- `TaskStore::delete` now removes the task's runs and the task itself inside a
  single transaction, keyed on the task's `id` looked up from its `name`.
  Removing a task is the only operation allowed to discard its run history
  (`squad remove` already deletes the durable workspace), so nothing else in the
  store cascades.
- The squad tab renders the last failed squad command above the card grid, in
  the same header area the "daemon not reachable" state uses. A squad action
  that fails now says so where the user is looking.

### 2. `squad edit`
New Layer-2 subcommand `squad edit <name>`, catalogued beside `add`, with the
same flags a task can carry after creation: `--description`, `--interval`,
`--agent`/`--model` (with `--clear-agent`/`--clear-model` to fall back to the
squad defaults), `--overlay` (repeatable; replaces the stored list) and
`--agent-models`. `--interview` collects the same fields interactively with the
current values as defaults.

Deliberately **not** editable: `name`, `workspace` and `mount-scope`. Those are
capture-once identity and isolation decisions (WI 0106 §6); changing them would
move a task's durable workspace or widen a mount after the fact. Editing them
means creating a new task.

`TaskGateway` gains `update(&self, name, UpdateTask)`, implemented by
`LocalTaskGateway` (validation reuses `validate_create`'s interval, overlay and
agent rules) and by `RemoteTaskGateway` (which re-serialises the request as the
`squad edit` argv the daemon parses). `TaskStore::update` writes only the fields
the request carries and always bumps `updated_at`.

TUI: `e` on the task grid and in the detail modal dispatches
`squad edit <name> --interview` through the same Layer-2 path as every other
key binding.

### 3. Per-task agent configuration
A task may own `<squad root>/tasks/<name>/config.json` — the same shape as
`~/.awman/config.json` and `GITROOT/.awman/config.json`, of which only the
`squad` block is meaningful for a task:

```json
{ "squad": { "agentsToModels": { "claude": ["claude-opus-4-8"] } } }
```

- `SquadPaths::task_config_file(name)` resolves it through the same
  `validate_under_root` guard `task_dir` uses.
- `GlobalConfig::load_path` loads and validates any config file at an explicit
  path, so the task file cannot diverge from the global file's shape or rules.
- `SquadConfig::overlay_onto` layers the task block over the global one
  field-by-field (a field the task sets wins; a field it omits inherits).
  `maxConcurrentEvaluations` stays daemon-wide and is read from the global
  config only — a per-task value would not mean anything.
- The scheduler applies the overlay per task inside its tick, so an edited task
  config takes effect on the next tick with no restart, exactly like the global
  block.

Collection: the interview asks for extra models for the leader agent and for
extra agents (each with its own models), and only when the answers are
non-empty is a task config written. When the global config already has a
`squad` block, the interview first asks whether to use those global settings —
answering yes skips the agent/model questions and writes no task config, so the
task keeps inheriting the global pool. Scripted callers use `--agent-models
<agent>=<model>[,<model>…]` (repeatable) on `add` and `edit`; `--agent-models`
with no value on `edit` clears the task config.

Images: `evaluate_inner` builds an image for **every** agent in the task's
effective pool before the leader launches, not just the leader's. The
generated-workflow step images were already built after generation (WI 0108 §3);
doing the whole pool up front is what lets the leader pick any configured agent
for a step without paying for a build mid-run. `validate_create` also validates
Dockerfiles for the task pool's agents, not only the global pool's.

### 4. Card width
`render/squad.rs` caps the card grid at three columns
(`MAX_CARD_COLUMNS`), so a card is never narrower than a third of the grid
width. The existing half-width cap (WI 0108 §8) and `CARD_MIN_WIDTH` floor are
unchanged, so one or two cards still do not stretch across the tab and a narrow
terminal still gets a readable card.

### 5. Daemon-start confirmation
`App::open_or_focus_squad_tab` now checks whether a daemon is already running
(`SquadSupervisor::daemon_is_running`) before building the tab. When one is
running, nothing changes. When none is, the TUI opens a `y`/`n` confirmation
and only builds the tab — which starts the daemon in the background — on `y`;
`n`/`Esc` leaves no tab and says so in the status bar.

The runtime refusal still comes first: a sandbox-class runtime is reported
without asking a question whose answer could not be honoured.

Bare `awman squad` (`InitialTab::Squad`) is unchanged and still starts the
daemon: the user named squad on the command line, and there is no TUI yet in
which to raise a dialog.

### 6. `Ctrl-\` detach
While a container is maximized every key — Ctrl-C included — is forwarded to the
agent's PTY, which is what makes an attached agent usable and what makes Ctrl-C
the wrong way out: it interrupts unattended work. `Ctrl-\` (FS, 0x1c) is
intercepted in `keymap.rs`'s global `ctrl` block, alongside Ctrl-O and Ctrl-G, so
it is claimed **before** the `ForwardToPty` path in every focus context and can
never reach an agent.

The binding is global rather than squad-specific, per the reporter's request:

- On a squad tab with an attach session, `detach_squad_attach` ends the session.
  `SquadTabState` gained an attach-scoped `CancellationToken` — a *child* of the
  tab's — so cancelling it kills the local attach clients and the session's
  workflow poller while leaving the task-list poller (which shares the tab)
  running. `Tab::end_attach_session` drops the slots, the queued slot events, the
  workflow snapshot and the summary bar, and re-arms
  `suppress_container_auto_open` so an in-flight slot event cannot immediately
  reopen the overlay the user just left. The daemon's containers are never
  touched — the same semantics as detaching from the CLI's `squad attach`.
- On any other tab the maximized container is merely minimized and focus returns
  to the command box. The command keeps running and streaming into its status
  bar; Ctrl-M brings the view back. Nothing is signalled.

Discoverability: `ctrl-\ detach` is added to the status-bar hint above the
command box in both maximized-container states — the exact place where Ctrl-C
would otherwise look like the only exit.

The attach session's workflow poller also moved off `set_poll_handle` onto its
own `set_attach_handle`, so ending an attach can no longer leave the task list
unpolled (previously the attach poller's handle overwrote the list poller's).

## Edge Case Considerations:
- Deleting a task whose runs are still `running` removes them with the task;
  the containers themselves are the runtime's, not the store's, and were
  already outside `squad remove`'s reach.
- `squad edit` with no field flags and no `--interview` is rejected, rather than
  writing an update that changes nothing but `updated_at`.
- Editing an interval still goes through `parse_squad_interval` and the
  gateway's 60s–24h bounds, so an edit cannot install a value creation refuses.
- A task config whose `squad` block is absent or empty behaves exactly like no
  task config at all.
- A malformed task `config.json` fails that task's tick with a named error
  rather than silently falling back to the global pool — the opposite of the
  global config's tolerant `unwrap_or_default`, because a task-scoped file the
  user wrote by hand should not be ignored.
- Three columns is a maximum, not a target: two tasks still render two cards.
- The daemon-start dialog is skipped entirely when a daemon is already running,
  so the common case never gains a keystroke.
- `Ctrl-\` on a tab with nothing attached and no maximized container is a no-op,
  not an error.
- Detaching a squad attach session must not stop the tab's task-list poller;
  that is why the attach token is a child token rather than the tab's own.

## Test Considerations:
- Unit: `TaskStore::delete` removes a task that has runs (the regression), and
  leaves other tasks' runs alone; `SquadConfig::overlay_onto` precedence;
  `SquadPaths::task_config_file` escape rejection; `--agent-models` parsing;
  card-width/column-cap arithmetic; `squad edit` argument round-trip through the
  remote gateway's argv.
- TUI: `e` opens the edit interview from the grid and from the detail modal; a
  failed squad command renders above the grid; the daemon-start confirmation
  opens instead of a tab and builds the tab on `y` only; `Ctrl-\` maps to detach
  in every focus context while `Ctrl-C` still forwards to the PTY, minimizes an
  ordinary tab's container without dropping it, and ends a squad attach session
  leaving the grid behind.
- Gateway: an edit changes only the fields it carries; `Some(None)` clears an
  agent/model where an absent field does not; overlays replace wholesale and are
  still validated; an edited interval is held to the creation bounds; an empty
  edit is refused before any gateway call; the task pool round-trips through
  `config.json` and an emptied pool removes the file.
- E2E (docker-gated): unchanged suites plus a task-pool image build assertion.

## Codebase Integration:
- follow established conventions, best practices, testing, and architecture patterns from the project's aspec.

## Documentation
- `docs/12-squad.md`: `squad edit`, the task `config.json` and the interview
  questions that write it, the per-task agent pool and its image builds, the
  `e` key binding, the failed-action line, the card-width behaviour, the
  daemon-start confirmation, and `Ctrl-\` detach.
- `docs/07-configuration.md`: the task-scoped config file alongside the global
  and per-repo ones.
- `docs/02-using-the-tui.md`: `Ctrl-\` in the three key tables plus a
  "Detaching from a container" section, since the binding is global rather than
  squad-specific.
