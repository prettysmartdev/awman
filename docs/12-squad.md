# squad

Your squad is a group of agents that work on your behalf while you're doing
something else. You give the squad tasks — "when a new issue is opened, triage
it and post a plan" — and it watches for each one and runs a workflow when it
fires.

Your squad lives inside `awman`: there's no separate program to install or run,
and you direct it from the same CLI and TUI as everything else.

---

## When to build a squad

Give your squad the routine, repeatable work you'd otherwise have to trigger by
hand every time it comes up:

- Triaging new issues as they're opened
- Reacting to a comment or label on a GitHub issue or PR
- Watching for failing tests on open PRs and pushing a fix
- Any other "when X happens in my repo, do Y" rule you want handled without
  babysitting it

For a single task you want to do right now yourself, use `awman chat` or
`awman exec prompt` instead — see [Agent Sessions](03-agent-sessions.md) and
[API mode](09-api-and-remote-mode.md#one-shot-scripted-execution-exec).

---

## Tasks

A **task** is a job you hand your squad: an "if... then..." rule saying what to
watch for and what you want done when it fires. Some examples:

- "Whenever a new issue is opened in the awman repo, analyze it, draft a
  plan, and comment the plan on the issue."
- "Whenever I comment `/squad` on an issue, research the comment and post a
  followup with findings and/or an updated plan."
- "When the `ready-to-implement` label is added to an issue, implement the
  approved plan and open a PR."
- "If any open PR has failing tests, check out the branch, fix the failure,
  and push the fix."

On a regular interval (6 hours by default, configurable per task), your squad
puts a **task-evaluation agent** on the task, with access to the task's durable
workspace at `~/.awman/squad/tasks/<name>/workspace/`. That agent decides whether
the task is currently met and, if so, writes or reuses a workflow file describing
what to do. The squad then validates that workflow and runs it unattended, the
same way `awman exec workflow --dynamic` would.

Every task captures its effective workspace and mount scope at creation time.
The effective workspace may be the durable default workspace or a custom
folder/repository; see [Task workspaces](#task-workspaces) and [Guardrails for
unattended execution](#guardrails-for-unattended-execution) below.

---

## Giving your squad a task

### From the CLI

```sh
awman squad add \
  --name issue-triage \
  --description "Whenever a new issue is opened, analyze it and post a plan as a comment." \
  --workspace default \
  --overlay "env(GITHUB_TOKEN)" \
  --interval 10m \
  --mount-scope gitroot
```

| Flag | Description |
|------|-------------|
| `--name <slug>` | Required. The task's identifier — lowercase letters, digits, and hyphens only (no leading/trailing hyphen), used to name its data directory and its containers. |
| `--description <text>` | Required. The natural-language "if... then..." rule the evaluation agent reads every tick. |
| `--workspace <default\|path>` | Bind the task to its durable default workspace, or to an existing custom folder/repository. If omitted, the default workspace is used. `--repo <path>` remains a legacy synonym for a custom workspace. |
| `--overlay <spec>` | Add a `dir()`, `ssh()`, `env()`, or `skill()` overlay to every container for this task. Repeat the flag for multiple overlays; malformed syntax is rejected when the task is created. See [Overlays](08-overlays.md). |
| `--interval <duration>` | How often the task is evaluated (default `6h`). |
| `--agent <name>` / `--model <name>` | Override the agent/model used for this task's evaluations, taking priority over the global `squad` config (see [Configuring squad](#configuring-squad) below). |
| `--agent-models <agent>=<model>[,<model>…]` | Give this task its own pool of agents and models, written to its `config.json`. Repeat the flag for more agents. See [Choosing a task's agents and models](#choosing-a-tasks-agents-and-models). |
| `--mount-scope <cwd\|gitroot>` | Only meaningful for a custom workspace that is a Git repository root, where both values name that same root (default `gitroot`). Captured once and never changed later. Every other workspace — the default durable workspace, a plain directory, or a subdirectory of a repository — is mounted directly, as given. |
| `--interview` | Collect the task fields through the interactive interview, including the multiline description, workspace choice, and overlays. |
| `-n, --non-interactive` | Never prompt: refuse anything that would need a confirmation instead of asking. Cannot be combined with `--interview`. |

Task names are lowercase slugs: letters `a`–`z`, digits `0`–`9`, and
hyphens, starting and ending with a letter or digit. A name with uppercase
letters, underscores, spaces, or a leading or trailing hyphen is rejected
before the task is created.

Once created, manage it with the rest of the CRUD subcommands:

```sh
awman squad list                   # table of every task
awman squad show issue-triage      # description, schedule, and run history (with verdict reasons)
awman squad edit issue-triage      # change it — see below
awman squad trigger issue-triage   # evaluate it now, ignoring its schedule
awman squad cancel issue-triage    # stop the run in progress
awman squad pause issue-triage     # stop evaluating it without deleting it
awman squad resume issue-triage
awman squad remove issue-triage    # delete it
```

Add `--json` to `list`/`show`/`status` for machine-readable output
(`--json` implies non-interactive mode).

### Editing a task

`awman squad edit <name>` changes an existing task in place, keeping its durable
workspace and its run history — unlike deleting and recreating it, which throws
both away.

```sh
awman squad edit issue-triage \
  --description "Whenever an issue is opened or reopened, analyze it and post a plan." \
  --interval 30m \
  --agent claude --model claude-opus-4-8
```

| Flag | Description |
|------|-------------|
| `--description <text>` | Replace the task's "if… then…" rule. |
| `--interval <duration>` | Replace the evaluation interval. Held to the same 60s–24h bounds `squad add` applies. |
| `--agent <name>` / `--model <name>` | Replace the task's own leader agent/model. |
| `--clear-agent` / `--clear-model` | Drop the task's own agent/model so it falls back to the squad defaults. |
| `--overlay <spec>` | Replace the task's overlay list. Repeat the flag for several; the given set replaces the stored one rather than adding to it. |
| `--clear-overlays` | Remove every overlay from the task. |
| `--agent-models <agent>=<model>[,<model>…]` | Replace the task's agent pool. Repeat the flag for more agents. |
| `--clear-agent-models` | Remove the task's own agent pool, so it inherits the global `squad` settings again. |
| `--interview` | Collect the fields interactively, each prefilled with the task's current value. |
| `-n, --non-interactive` | Never prompt. Cannot be combined with `--interview`. |

**A task's name, workspace, and mount scope cannot be edited.** They are captured
once when the task is created and define its identity, its data directory, its
container names, and whether its runs are worktree-isolated — see [Guardrails for
unattended execution](#guardrails-for-unattended-execution). Changing any of them
means creating a new task.

An edit with no field flags and no `--interview` is refused rather than reported
as a successful no-op.

### Triggering a task now

A task normally waits for its interval to elapse. `awman squad trigger <name>`
puts your squad on it at the next tick instead, whatever the interval says and
whatever backoff an earlier failure left outstanding:

```sh
awman squad trigger issue-triage
# Triggered task issue-triage; it will be evaluated on the next scheduler tick.
```

Your squad checks its tasks every 30 seconds, so a trigger takes effect within
half a minute — it does not start an evaluation the instant the command returns.

Triggering changes nothing about the task. Its interval, agent, model,
overlays, and last-run timestamp are all untouched, and the trigger is consumed
by the one evaluation it asks for: afterwards the task is back on its ordinary
schedule. That is the difference between `trigger` and shortening the interval
with [`squad edit`](#editing-a-task) — use `trigger` for "run this once, now",
and `edit` to change how often the task runs from here on.

Two things a trigger deliberately does not override:

- **A paused task is refused**, with a message naming `squad resume`. Pausing
  tells your squad to leave the task alone, which is not a schedule, and
  silently accepting a trigger the squad would then ignore would leave you
  watching a task you triggered do nothing.
- **A task your squad is already working on** is not picked up a second time in
  parallel. The trigger stays pending and is honoured once that run finishes.

While a trigger is pending, the task's card in the TUI reads
`Next: triggered — next tick`, and `squad show --json` reports a
`trigger_requested_at` timestamp.

In the TUI, **t** triggers the selected task from the card grid, and from
inside a task's detail modal, after a `[y]es / [n]o` confirmation.

### Canceling a run in progress

`awman squad cancel <name>` stops the run a task is executing right now —
whether its leader is still deciding or its generated workflow is already
running:

```sh
awman squad cancel issue-triage
# Canceled the in-progress run of task issue-triage; its containers are being stopped.
```

The run is recorded as `canceled` in the task's
[run history](#run-history) straight away, and every agent container the run
started — the evaluation leader and any workflow steps — is stopped, which
takes a few seconds per container. A cancel is your decision, not a failure, so
it never backs the task off: the task keeps its schedule and is evaluated again
when it is next due. A task with no run in progress is refused with a message
saying so.

In the TUI, **c** cancels the selected task's run from the card grid, and from
inside a task's detail modal, after a `[y]es / [n]o` confirmation.

### Choosing a task's agents and models

By default a task uses whatever agents and models the global `squad` block
allows. A task can instead carry its own pool, stored in its own config file:

```text
~/.awman/squad/tasks/<name>/config.json
```

That file has the same shape as every other awman config file, and only its
`squad` block matters:

```json
{
  "squad": {
    "agentsToModels": {
      "claude": ["claude-opus-4-8", "claude-sonnet-4-6"],
      "codex": ["gpt-5"]
    }
  }
}
```

Set it from the command line with `--agent-models` on `squad add` or
`squad edit`:

```sh
awman squad add --name issue-triage --description "…" \
  --agent-models 'claude=claude-opus-4-8,claude-sonnet-4-6' \
  --agent-models 'codex=gpt-5'
```

…or answer the questions in the interactive interview, which asks:

1. **Whether to use the global squad settings** — only when a global `squad`
   block exists. Answering yes writes no task config and the task inherits that
   block, exactly as tasks did before per-task configs existed.
2. **Extra models for the leader agent** — one at a time, blank to finish.
3. **Extra agents this task may use**, each followed by its own models.

Answering "no more" to everything writes no file at all.

Fields the task file omits are inherited from the global block, so a task can
narrow its agent pool while keeping the standing `guidance` every task gets; see
[Configuration: Per-task squad settings](07-configuration.md#per-task-squad-settings).

Every agent in the pool gets a container image built automatically at the start
of each run, before the leader launches — so the leader can pick any of them for
a step of the workflow it generates without paying for a build mid-run. Images
that already exist are not rebuilt.

### Task workspaces

By default, a task uses its own durable workspace:
`~/.awman/squad/tasks/<name>/workspace/`. It is created when the task is
created and reused for every evaluation and workflow run. Files written there
survive between runs, including files left by a task whose agent stops
unexpectedly. The workspace is removed only when the task itself is removed
and you confirm deletion (or use `--yes`).

With `--workspace <path>`, the path must already exist. A path that is the
*root* of a Git repository is worktree-isolated for runs. Any other path — a
plain directory, or a subdirectory inside a repository — is mounted directly,
exactly as it was given: squad never widens a run's view from the folder you
picked to its enclosing repository. In the interactive interview, a custom
path that is not a Git repository root is kept only after a warning and
confirmation. A missing path is an error and is never created automatically.
These choices are captured when the task is created; squad does not silently
change workspace mode later.

If the custom path is a *parent* of the directory you are standing in, squad
asks you to confirm the wider mount scope first — the same confirmation every
other awman mount-scope flow applies. This holds whether you answered the
interview or passed `--workspace` on the command line; with `-n` the widening
is refused outright instead of being asked about.

A task whose workspace is a plain directory (the default workspace, or a
custom folder that is not a Git repository) has no project of its own to build
agent images from, so on its first run squad writes a `Dockerfile.dev` and a
`.awman/Dockerfile.<agent>` into that directory from the same bundled
templates `awman init` uses. Both are written only if absent: edit either one
to control how the task's containers are built, and squad will leave your
version alone from then on.

Images built from a default task workspace are tagged with the task's own
name — `awman-squad-<name>:latest` for the base image and
`awman-squad-<name>-<agent>:latest` per agent — so two tasks never share or
overwrite each other's images. (A custom workspace that is a repository uses
the same folder-derived tags as any other awman project in that repository.)

Whether the task uses the default workspace or a custom path, the durable
task workspace is also available to its containers through the
`context(workflow)` location. This gives custom-workspace tasks a stable place
for task-scoped files and data. Overlay specifications configured on a task
are additive with global, repository, environment, and workflow overlays; see
[Overlays](08-overlays.md) for the syntax and merge rules.

### What the leader agent is told about them

Each run, before it designs anything, the leader agent is given the merged
overlay set as an inventory in its prompt: which host directories are mounted
and where, which skills it can call, which context directories the steps will
get, and — split into *set* and *declared but not set* — which environment
variables the task named. Names only, never values.

This matters in both directions. A leader not told it has `ssh()` and
`env(GITHUB_TOKEN)` won't design a workflow that pushes a branch and opens a
pull request, though the task allows exactly that; one that assumes a key it
lacks writes steps that fail at runtime. The inventory comes from the same
resolution the containers launch with, so the two cannot disagree.

### From the TUI

Every action available on the command line is also available as a key
binding inside the squad tab (see [The squad tab](#the-squad-tab) below) —
pressing **n** to create a task, **e** to edit one, **t** to trigger one,
**p**/**r** to pause/resume, and **d** to remove all dispatch through the same
commands the CLI uses, just from inside the tab instead of a terminal prompt.

Task creation is an all-or-nothing interview. The description uses a large
multiline editor with the prompt:

> Describe the new squad task including its triggering conditions and how
> squad should handle the task each time it is triggered

After the description and interval, choose **Default Task Workspace** or
**Custom Folder / Repo**. The custom choice asks for an existing path; a path
that is not the root of a Git repository produces a warning and offers
**keep this path** or **choose a different path**. You can then add overlays
one at a time using the same syntax as other awman commands; submit a blank
overlay entry when finished. Last come the agent and model questions described
in [Choosing a task's agents and models](#choosing-a-tasks-agents-and-models).
Submitting an empty box is an answer — it keeps the documented default, or ends
a list — but pressing **Esc** dismisses the interview entirely, and nothing is
saved.

Pressing **e** on a task runs the same interview against that task, with every
box prefilled with what the task currently carries. Leaving a box untouched
changes nothing; clearing the agent or model box drops the task's own value so it
falls back to the squad defaults. Overlays and the agent pool are offered as
replace-or-keep, since a list has no meaningful "edit in place" prompt. An
interview you walk through without changing anything is refused as an empty edit
rather than silently rewriting the task with its own values.

---

## Task environment values

A task's `env(VAR)` overlay (see [Overlays](08-overlays.md#overlay-types)) names a
host environment variable the squad daemon must hand to that task's
containers. Squad tracks, for every such name across every task, whether it
currently holds a value, and warns you when it doesn't.

### How the daemon gets its environment

The squad daemon is a long-lived background process — often started once, at
login or the first time any `awman squad` command needed it, and left running
for days. Like any other process, it doesn't see a variable you `export` in a
shell afterward; nothing does, short of restarting it.

Instead, any `awman squad` command that needs a working daemon — `add`,
`edit`, `list`, `show`, `trigger`, `pause`/`resume`, `remove`, `attach`, and
`squad env` itself, as well as opening the squad tab — first compares what the
daemon already holds against what your current shell can see, and pushes
anything new or different before doing its own work. Nothing is ever re-sent
once the daemon already has it: both sides compare a digest, never the value
itself, so this happens on every command at effectively no cost once your
shell is caught up. The digest is a salted hash the daemon publishes per name
it holds, over the daemon's authenticated local socket; it is what lets a
client tell "the daemon already has this exact value" from "it has a different
one" without either side putting a secret on the wire. This is why exporting a
variable and then running *any*
squad command — not just `awman squad env --push` — is enough for the daemon
to pick it up on the very next command that reaches it.

The one exception is `awman squad status`: it's a lightweight liveness probe
and deliberately pushes nothing, so checking status never has the side effect
of arming a task.

### The "daemon has no value for" warning

`squad add` and `squad edit` print this once, right after creating or
updating a task, if the task you just defined declares an `env()` name the
daemon doesn't currently have a value for:

```
⚠ Task "nightly-triage" was created, but the squad daemon has no value for ANTHROPIC_KEY.

  Its containers will start without it until it is supplied. To fix:
      read -rs ANTHROPIC_KEY && export ANTHROPIC_KEY      # in any shell; keeps it out of shell history
      awman squad env --push       # or just run any awman squad command

  Check state at any time with `awman squad env`.
Created task nightly-triage.
```

It's a warning, not a rejection — the task above **is** created, and the same
warning fires (with "updated" in place of "created") from `squad edit`. The
task's containers simply start without that variable until one is supplied.
Every `env()` name is treated the same way — there are no exempt names.

The leader agent is told the same thing each run: a name the daemon still has
no value for is listed in its prompt as declared but not set, so it plans
around the gap instead of writing a workflow that dies on it, and can name what
was missing in the run's verdict reason.

To clear it, export the missing variable in any shell and run any
`awman squad` command — or run `awman squad env --push` if you want to be
sure the push happens right away, rather than waiting for the next command
you'd run anyway.

### `awman squad env`

Reports every `env()` name the daemon needs, across every task, the daemon's
own config, and `AWMAN_OVERLAYS`: whether it currently has a value, where that
value came from, and how long a missing one has been missing. It starts the
daemon if one isn't already running, the same as `squad list`.

```sh
awman squad env
```

```
Daemon env coverage (persistence: keychain)

  NAME           STATE    SOURCE      SINCE
  ANTHROPIC_KEY  ✓ set    this shell  —
  AWS_PROFILE    ⚠ unmet  —           just now

  AWS_PROFILE is required by task "deploy-preview".
  Export it and run `awman squad env --push`.
```

No value is ever printed — only whether one is present, and, when one is,
which of `this shell`, `pushed` (an earlier push, from this or another
shell), or `keychain` (loaded from persisted storage at daemon startup) it
came from. The trailing "is required by" block lists every unmet name and the
task(s) that need it, and is omitted entirely once nothing is unmet.

| Flag | Description |
|------|-------------|
| `--push` | Push every required value your current shell has, whether or not the daemon already matches it — the flag to reach for right after rotating a token. |
| `--clear` | Remove whatever the daemon has persisted to the OS keychain. Leaves the running daemon's in-memory values untouched — see [Persisting values across a restart](#persisting-values-across-a-restart) below. |
| `--json` | Machine-readable output, same envelope as `list`/`show`/`status`. |

In `--json` output, each row's `state` is `"set"` or `"unmet"` (the wire form
of the `✓`/`⚠` glyphs above), and the header's `persistence` field is the
`--clear`/keychain state described below. If no daemon could be reached to
answer at all, `persistence` is omitted from the JSON entirely rather than
sent as `""` — the two are not the same thing: `""` would mean "asked, got
nothing back," where an absent field means "couldn't ask."

`--push` re-sends every required name your shell has and re-reads the table,
so a row you just fixed shows the update immediately:

```sh
AWS_PROFILE=preview awman squad env --push
```

```
Daemon env coverage (persistence: keychain)

  NAME           STATE   SOURCE      SINCE
  ANTHROPIC_KEY  ✓ set   pushed      —
  AWS_PROFILE    ✓ set   this shell  —
```

`--clear` removes the persisted keychain item and says so as a second line
between the header and the table, but leaves every row's `STATE` exactly as
it was — clearing what's *stored* must not disarm a daemon that's running
fine:

```sh
awman squad env --clear
```

```
Daemon env coverage (persistence: keychain)

  Stored env item removed. The running daemon keeps the values it already holds.

  NAME           STATE   SOURCE  SINCE
  ANTHROPIC_KEY  ✓ set   pushed  —
  AWS_PROFILE    ✓ set   pushed  —
```

If nothing was stored to begin with, that line instead reads
`No stored env item to remove (this daemon persists nothing).`

### Persisting values across a restart

By default (`squad.envPersistence: "keychain"` — see [Configuration: Squad
daemon configuration](07-configuration.md#squad-daemon-configuration)), squad
stores the values it holds for `env()` names in the OS keychain, so a daemon
the OS restarts on your behalf — launchd at login, systemd after a crash —
comes back already armed instead of needing every task's variables pushed
again from a live shell. A pushed value always overwrites a stored one; the
keychain is only a cold-start hint, never the source of truth for a running
daemon.

Know what the keychain does and does not defend against before leaving this
on. It protects the stored values against **other users** on the machine, and
against an offline copy of the disk while the keychain is locked. It does
**not** protect them against a process running as *you*: anything under your
own account can read the item back with `security find-generic-password -s
awman-squad -a daemon-env -w` (macOS) or `secret-tool lookup service
awman-squad account daemon-env` (Linux), with no prompt. That unprompted read
is exactly what lets the daemon come back armed without asking you for
anything at login. If that trade isn't one you want on a particular machine,
set `squad.envPersistence` to `"none"` there.

Only names some task actually declares are ever requested, held, or stored.
A daemon with no tasks requires nothing, transmits nothing, and persists
nothing.

If the keychain isn't usable — `secret-tool` not installed, a locked login
keychain, no keychain backend on the platform, or a keychain call that fails
outright — the daemon falls back to persisting nothing rather than failing to
start. `awman squad env` reports this in its header as
`persistence: unavailable(<reason>)`, and `awman squad status` appends
`; env persistence unavailable(<reason>)` to its one-line summary so you see
it without having to go looking. This isn't a failure: squad keeps
working exactly as it did before persistence existed, and every regular
`awman squad` command still resupplies the daemon as described above — the
only thing lost is a value surviving an OS-initiated restart. If you'd rather
never see the fallback and never have anything persisted, set
`squad.envPersistence` to `"none"` explicitly; an explicit `"none"` is never
warned about.

To remove a stored item, `awman clean` is the recommended route: it
discovers the daemon's stored keychain item — listed as "Squad daemon
environment" — alongside everything else it offers to remove, and clearing
it is one of the choices in its confirmation summary; see [Cleaning
Up](13-cleaning-up.md). This only removes what a future restart would read
back — it does not touch what the *running* daemon currently holds in
memory.

If you need to remove it without going through awman, the manual commands
are:

```sh
# macOS
security delete-generic-password -s awman-squad -a daemon-env

# Linux
secret-tool clear service awman-squad account daemon-env
```

---

## The squad tab

Rather than being a separate program, your squad gets a dedicated, singleton
tab inside the ordinary multi-tab `awman` TUI — the same TUI you use for
project work. Unlike a normal tab it isn't bound to a working directory: it
shows what your squad is working on and lets you direct it from wherever you
happen to have `awman` open.

### Opening it

Two ways to get there:

- **`Ctrl-S` from the New Tab dialog.** Press **Ctrl-T** to open a new tab;
  the key-hint row under the text box reads `[Enter] submit   [Esc] cancel
  [Ctrl+S] open squad`. Pressing **Ctrl-S** while that dialog is focused
  closes the dialog and opens (or focuses) the squad tab instead of creating
  a directory-bound tab. This doesn't change what
  `Ctrl-S` does anywhere else — outside that dialog it keeps its usual
  meanings (cycling parallel container slots, submitting multiline dialogs).
- **Bare `awman squad` in a terminal.** Run `awman squad` with no subcommand
  in a TTY (and without `-n`/`--json`) and awman opens the TUI pre-focused
  on the squad tab.

There is at most one squad tab per running `awman` process — opening it again
just focuses the existing one rather than creating a second. You can run
more than one `awman` process, each with its own squad tab; the squad daemon
itself is what enforces there's only ever one instance of the daemon.

Opening the tab with **Ctrl-S** when no squad daemon is running asks first:

```
┌ Start squad daemon? ───────────────────────────────┐
│  The squad daemon is not running.                  │
│                                                    │
│  Start it in the background and open the squad tab?│
│                                                    │
│  [y] start   [n / Esc] cancel                      │
└────────────────────────────────────────────────────┘
```

**y** starts the daemon in the background and opens the tab; **n** or **Esc**
opens no tab and starts nothing. Starting a daemon takes a moment, so **y**
replaces the question with a `Starting squad daemon` modal and the TUI stays
responsive while it waits; the tab appears when the daemon is ready, and if it
never becomes ready the reason is shown in a modal of its own rather than
leaving you with no tab and no explanation. With a daemon already running there
is nothing to consent to and no question is asked. Bare `awman squad` still starts the
daemon without asking — you named squad on the command line, and there is no TUI
yet in which to raise the question.

### What it looks like

The tab has its own colour — cyan — so it's never mistaken for a normal
project tab, and its label is always the fixed word `squad` regardless of
where squad's data actually lives on disk. Otherwise it behaves like any
other tab: it participates in `Ctrl-A`/`Ctrl-D` tab cycling, closes with the
usual close-tab flow, and keeps its state while you're on another tab.

The body of the tab is a generously spaced grid of rounded **task cards**.
The task name is the card's title; every value below it carries a grey label:

```
╭ issue-triage ───────────────────╮
│ Description                     │
│ Whenever an issue is opened or… │
│ Last run: 2026-09-02 09:00      │
│ Outcome: workflow executed      │
│ Next: 2026-09-02 15:00          │
╰─────────────────────────────────╯
```

`Outcome` is what the last run actually did (`workflow executed`,
`not triggered`, `failed`, `interrupted`, `canceled`, `running`, or `never run`), and
`Next` is the next scheduled evaluation — which reads `paused` while the task
is paused, and `triggered — next tick` while a [trigger](#triggering-a-task-now)
is waiting to be honoured. A task that declares an `env(VAR)` name the daemon
doesn't currently have a value for gets one more line, appended after `Next`:
`Env: ⚠ VARNAME unmet` (several names are comma-joined). The row is absent
whenever nothing is unmet, and it's informational only — it doesn't change
the card's colour; see [Task environment values](#task-environment-values)
above and [Card colours](#card-colours) below. The same line appears in the
task's detail modal (**Enter**) and in its [run history](#run-history) (**h**)
when a past run had an unmet name at the time it started. Cards reflow as the terminal
is resized, at most three to a row, so a card is always at least a third of the
tab's width and its description summary stays readable on a wide terminal. Use
the arrow keys to move in two dimensions, including across the final partially
filled row. Polling refreshes automatically every couple
of seconds while the tab is focused. If the daemon isn't reachable, the tab
shows that clearly above whatever tasks it last saw, rather than quietly
showing an empty list. With no tasks, the grid shows an empty-state prompt to
press **n** and create one.

`Ctrl-G` (the git sidebar) is a no-op on the squad tab, since it isn't bound
to a repository.

### Key bindings in the task list

The card grid always has focus on the squad tab: the arrow keys work the
moment the tab opens, with no need to press **↑** first. The command box
under the grid is permanently inactive there — it reads `command (inactive)`
and says to use the arrows and **Enter** — and **Esc** does nothing, since
there is nothing to hand focus to. Switching back to a normal tab returns
focus to that tab's command box.

| Key | Action |
|-----|--------|
| **↑ / ↓ / ← / →** | Move between task cards |
| **Enter** | Open a detail modal for the selected task — description, workspace, mount scope, interval, overlays, agent/model, unmet `env()` values, and timestamps |
| **h** | Open the [run history](#run-history) for the selected task |
| **a** | Attach to the task's currently running container(s) — see [Attaching](#attaching-to-a-running-task) |
| **n** | Create a new task |
| **e** | Edit the selected task (the creation interview, prefilled) |
| **t** | Trigger the selected task — evaluate it on the next tick, ignoring its schedule (asks for confirmation first) |
| **c** | [Cancel](#canceling-a-run-in-progress) the selected task's run in progress (asks for confirmation first) |
| **p** | Pause the selected task (asks for confirmation first) |
| **r** | Resume the selected task |
| **d** | Remove the selected task (opens a `[y]es / [n]o` confirmation first) |

The detail modal includes the same task-scoped action hints: **h** history,
**a** attach, **e** edit, **t** trigger, **c** cancel, **p** pause, **r** resume, **d**
delete, and **Esc** close. Those keys act on the task shown in the modal, even
if the underlying card list has changed. **t**, **c**, **p**, and **d** ask for
confirmation before acting, from the modal and from the card grid alike.

### Run history

A task's run history is a modal of its own, so a long task description can
never push it off the bottom of the detail modal. **h** opens it, from either
place:

- **From the card grid**, it shows the selected task's runs. **Esc** closes it
  and returns you to the grid — it does not open the detail modal.
- **From the detail modal**, it replaces that modal. **Esc** closes the history
  and puts the detail modal back, so you can page between the two with **h**
  and **Esc**.

The table lists each run's start time, outcome (`running`, `not triggered`,
`executed`, `failed`, `interrupted`, `canceled`), the reason the evaluation leader gave for
its verdict (why the task did or didn't trigger — kept even when the run later
fails or is canceled; `—` when it gave none or the
run never reached a verdict), finish time, and error, plus an `Unmet env`
column when any run in the history started with an unmet `env()` name. The
modal widens to fit a long reason or error, up to the terminal's width. `awman
squad show` prints the same `Reason` column. **↑ / ↓** and **PgUp / PgDn** scroll it. A task that has never run says
so instead of showing an empty table. Like the detail modal, the history keeps
refreshing from the daemon while it is open.

If one of these actions fails — the daemon rejects it, or it cannot reach the
daemon at all — the reason is shown in red in the hint bar directly above the
command box, so a key never just appears to do nothing.

Global shortcuts (**Ctrl-T**, **Ctrl-A**, **Ctrl-D**, **Ctrl-M**, **Ctrl-O**,
**Ctrl-W**, **Ctrl-,**, **Ctrl-C**) keep their usual meaning while the task list has
focus — none of them are repurposed for squad. You can also type
`squad <subcommand> ...` into the command box of any *other* tab; the keys
above are shortcuts over the same path, not a separate one.

### Card colours

Each card's outline colour is the task's state, and the selected card is the
one drawn with a **solid** outline and a `➡` before its name — every other
card is drawn dashed. Colour and selection are independent: a selected card
keeps its state colour, and a paused card is dashed or solid for the same
reason any other card is.

| Colour | Meaning |
|--------|---------|
| Grey | Paused |
| Blue | A run is in progress right now |
| Magenta | A [trigger](#triggering-a-task-now) is waiting for the next tick |
| Red | The most recent run failed |
| Yellow | Has never run |
| Green | Active, last run finished normally |

The first matching row wins from the top: a paused task is grey even if its
last run failed, a running task is blue even if a trigger is pending, and a
triggered task is magenta until the trigger is honoured. Every colour is also
spelled out in the card's body (`Next: paused`, `Outcome: failed`,
`Last run: —`, `Next: triggered — next tick`), so nothing depends on colour
alone.

### The squad indicator

The bottom row of the TUI — the one showing `CWD:` under the command box —
always ends with `squad ●`, on every tab, whether or not the squad tab is
open. The circle's colour is the daemon's health, re-probed every ten
seconds:

| Colour | Meaning |
|--------|---------|
| Grey | No squad daemon is running |
| Yellow | A daemon is running but this awman cannot get an answer from it — no bearer key for it (see [Authenticating to the daemon](#authenticating-to-the-daemon)), a connection refused, a timeout, or any other error |
| Red | Reachable, and some task's most recent run failed |
| Yellow | Reachable, nothing failed, and some task has an `env()` name the daemon doesn't currently hold a value for — see [Task environment values](#task-environment-values) |
| Blue | Reachable, and a task is executing right now |
| Green | Reachable, nothing failed, nothing running, nothing unmet |

Red wins over blue when both apply: a failure needs attention and persists,
while a running task is transient. An unmet variable ranks below a failure
but above a running task, for the same reason: it needs attention and
persists across ticks. Yellow is deliberately shared between "can't reach
the daemon" and "some task has an unmet variable" — both mean "worth a
look, not broken" — and the two can never actually coincide, since an
unreachable daemon has no way to report what's unmet in the first place. A
persisted-storage fallback (see [Persisting values across a
restart](#persisting-values-across-a-restart)) never turns this indicator
yellow by itself — only an actual unmet name does — because that fallback is
the ordinary steady state on headless Linux and on Windows, and a
permanently yellow indicator would just teach you to ignore it. The
indicator is only a summary — open the squad tab (or run `awman squad list`
/ `awman squad env`) to see which task or name.

---

## Attaching to a running task

Only a **currently running** agent can be attached to — there's no way to
replay a finished run. A task has two things that can be running at
once:

- **The evaluation agent** — the agent deciding whether the task is
  met, before any workflow has been generated.
- **The generated workflow's containers** — once the evaluation agent has
  decided to act.

### From the CLI

```sh
awman squad attach issue-triage
```

If exactly one container is running for the task, this attaches your
terminal directly to the running agent's terminal UI. Squad-launched agents
run with a PTY even when nobody is attached, so attach reconnects to the
actual agent process rather than opening a shell beside it. If the task has
no run in progress right now,
the command fails immediately rather than pretending an old run is still
live. If more than one container is running (a generated workflow with
parallel steps), the command lists each one's short ID and label and asks
you to disambiguate:

```sh
awman squad attach issue-triage --container a1b2c3d4e5f6
```

Detaching (`Ctrl-C`, closing the terminal) only ends your local attach
session — the container and the daemon are left running, and you can
attach again later. Note that `Ctrl-C` here ends the *client*; from the TUI, use
`Ctrl-\` instead, which never reaches the agent (see below).

### From the TUI

Pressing **a** in the squad tab attaches to every running container for the
selected task at once, showing the actual agent TUIs and reproducing the same view you'd get from
`awman exec workflow --dynamic`: the Workflow Overview across the
bottom, one container maximized and the rest as minimized bars, and
**Ctrl-S** to cycle focus between them. If the task is still in its
evaluation phase (no workflow yet), you instead see the single evaluation
container with no Workflow Overview.

**Press Ctrl-\\ to detach.** While you are attached, every other key — including
**Ctrl-C** — is forwarded straight into the focused agent's terminal, which is
what makes an attached agent usable and what makes Ctrl-C the wrong way out:
it interrupts the agent's unattended work. Ctrl-\\ never reaches the agent. It
ends your local attach session, leaves every container running, and returns you
to the task grid, where **a** reattaches. The hint bar above the command box
shows `ctrl-\ detach` whenever keys are going to a container.

Ctrl-\\ is not squad-specific: it detaches from any container view in the TUI.
On an ordinary tab it minimizes the running command's container — the command
keeps running, output keeps streaming into its status bar, and **Ctrl-M** brings
the view back.

While attached, the daemon dying doesn't interrupt anything already
streaming — those are direct connections to the container runtime, not
proxied through the daemon — but the Workflow Overview freezes and the tab
shows a "daemon not reachable" indicator until the daemon comes back.

If the local attach client itself dies (for example, the container stopped a
moment earlier), the session ends and the client's exit code and final output
are written to the tab's status log, so a failed attach explains itself
rather than silently returning to the task grid.

Attach works on both runtimes, through different plumbing with the same
semantics. On **docker**, attach is a native `docker attach` to the agent's
TTY. Apple's `container` CLI has no attach verb, so on **apple-containers**
the awman process that launched the agent (the squad daemon, for squad tasks)
serves the agent's live terminal on a local, user-private socket, and attach
connects to that. Either way you reach the real agent TUI, several clients
can attach at once, and detaching never stops the container.

The one Apple-specific caveat: the launching process is the only holder of
the agent's terminal there, so if it has exited (say, the daemon was
restarted mid-run), attach reports that there is no live attach endpoint —
the agent's per-run log file still has everything it printed.

---

## Guardrails for unattended execution

Because your squad works with nobody watching, it applies a fixed set of
guardrails to every run rather than leaving them optional:

- **Mount scope is captured once, at creation, and never widened later.**
  A custom workspace that is a repository root is the repository root, so
  `--mount-scope cwd` and `--mount-scope gitroot` name the same directory
  there. Every other workspace mounts directly; there is no repository root to
  widen to.
- **Worktree isolation follows the workspace type.** A custom workspace that
  is a Git repository *root* always runs in its own isolated worktree, exactly
  as `--worktree` does for a manual `exec workflow` run — see [Security &
  Isolation](04-security-and-isolation.md#worktree-isolation). The default
  durable workspace, custom non-Git directories, and subdirectories of a
  repository are mounted directly and never use a worktree. This decision is
  made when the task is created.
- **Every run is autonomous and PTY-backed.** There's no human around to
  answer a permission prompt, so the evaluation leader and every generated
  workflow run under the same auto-advance guardrails as `--yolo` — see
  [Permission modes](03-agent-sessions.md#permission-modes). An agent that
  goes quiet starts the standard stuck detection and 60-second yolo
  countdown; if it stays idle the run advances past it automatically, exactly
  as a dynamic workflow would. The countdown's start, cancellation, and
  auto-advance are recorded in the daemon log (never each tick). Agents still
  run in a terminal-sized PTY so attaching later shows the real interactive
  agent interface.
- **A run's status follows what actually happened.** A run is recorded as
  `failed` — and the task backs off — when its generated workflow exits
  non-zero: a step container that crashes, a setup step marked
  `abort_on_failure` that fails, any teardown step failure, or an engine
  error. A container that the yolo countdown moved past is *not* a failure,
  for workflow steps and for the evaluation leader alike: the step is marked
  succeeded and the run continues. If the countdown kills the leader before
  it has written a verdict, the run is recorded as `not triggered` with a
  warning in the daemon log rather than as a failure. A leader that exits
  non-zero on its own is a failed run even if it had already written a
  verdict. A setup step *without* `abort_on_failure` that fails does not fail
  the run — the workflow author marked it best-effort — but its failure is
  still logged with the path to its output.
- **The durable workspace is preserved.** Squad never clears task files
  between runs. The task workspace is also mounted at the stable
  `context(workflow)` location, including for custom-workspace tasks.
- **Credentials are only ever injected at container startup**, the same as
  any other agent container, and are never written into a task's
  persistent directory where they'd survive across runs.
- **A sandbox runtime (`docker-sbx-experimental`) is refused entirely.**
  squad's task-directory mounts, its evaluation-agent handshake, and
  workflow setup/teardown steps all depend on a real container runtime.
  Every squad command — including daemon lifecycle commands such as `start`,
  `stop`, and `logs`, task commands, and the TUI — fails with a clear error
  naming the configured runtime rather than degrading silently.
  Set `runtime` to `docker` or `apple-containers` to use squad.

---

## Configuring squad

An optional `squad` block in the **global** config
(`~/.awman/config.json`) sets which agents and models your squad may use, and
the standing instructions every task evaluation must follow:

```json
{
  "squad": {
    "agentsToModels": {
      "claude": ["claude-opus-4-8", "claude-sonnet-4-6"]
    },
    "maxConcurrentEvaluations": 2,
    "defaultLeader": "claude::claude-opus-4-8",
    "guidance": ["Keep automated changes focused."]
  }
}
```

| Key | Meaning |
|-----|---------|
| `agentsToModels` | The agents and models squad is allowed to schedule. A task's own `--agent`/`--model` override still wins if set; this is the default pool, not a hard allowlist against per-task overrides. |
| `maxConcurrentEvaluations` | How many task evaluations can run at once across the whole daemon. |
| `defaultLeader` | The `agent::model` used for a task's evaluation when the task doesn't specify its own. |
| `guidance` | Standing instructions applied to **every** task evaluation and every workflow it generates — the same mechanism as [dynamic workflow guidance](06-dynamic-workflows.md#guidance), injected as a bulleted "Developer Guidance" block in the agent's prompt. |

This is all global rather than per-repo, because one squad daemon watches
tasks across every repo you point it at. Editing this block takes
effect on the daemon's next scheduling tick — no restart needed. See
[Configuration: Squad daemon configuration](07-configuration.md#squad-daemon-configuration)
for the full field reference, validation rules, and how to edit it with
`awman config set`.

---

## squad and `awman api`

The squad daemon and the `awman api` server are both long-lived processes
that hold the same shared database open, so **only one of them can run on a
machine at a time**. Starting either one while the other is running fails
immediately, before any port is bound or the database is opened, and the
error names the other process and its PID.

To switch from one to the other:

```sh
# Currently running awman api, want to use squad instead
awman api kill
awman squad start

# Currently running squad, want to use awman api instead
awman squad stop
awman api start --port 9876 --workdirs /path/to/repo
```

Any squad CLI command or TUI entry point that needs the daemon (bare
`awman squad`, `squad add`/`list`/`show`/etc., opening the squad tab) starts it
automatically if it isn't already running — you don't have to run
`awman squad start` yourself first, unless you want to pass daemon-specific
flags like `--port`. See [API server and squad daemon](09-api-and-remote-mode.md#api-server-and-squad-daemon)
for the same guarantee from the API server's side.

---

## Daemon lifecycle

`squad start`/`stop`/`status`/`logs` manage the daemon directly, mirroring
`awman api`:

```sh
awman squad start              # explicit start (usually unnecessary — see above)
awman squad status             # liveness and PID
awman squad logs               # the daemon's log output
awman squad stop               # alias: awman squad kill
```

Daemon runtime files live under `~/.awman/squad/`, a sibling of `~/.awman/api/`:

```text
~/.awman/squad/
  awman.pid
  awman.log
  squad_key.hash
```

Task and run records live in the same
shared database as API mode, at `~/.awman/data/awman.db` — see
[Storage layout](09-api-and-remote-mode.md#storage-layout).

### Logs for the daemon and its agents

`awman squad logs` tails the daemon log. It contains the chronological squad
lifecycle — administrator actions, task decisions, container launches, workflow
steps, worktrees, and outcomes — but not the raw output of agents.

The scheduler's own 30-second heartbeat is deliberately not logged: a line
every half minute, forever, buried the lines that say something actually
happened. What *is* logged on a tick is one line per currently running agent —
its task, container, agent, model, image, and elapsed time — and nothing at all
when no agent is running. Use `awman squad status` for the scheduler's
liveness (its last tick and how many evaluations are in flight).

Each run keeps the output of each container in its own file:

```text
~/.awman/squad/tasks/<name>/runs/<run-id>/<container-name>.log
```

The evaluation agent and every generated-workflow agent container use this
layout. A workflow's setup and teardown steps write to the same directory,
one file per step in execution order:

```text
~/.awman/squad/tasks/<name>/runs/<run-id>/setup-<n>-<step>.log
~/.awman/squad/tasks/<name>/runs/<run-id>/teardown-<n>-<step>.log
```

`<n>` is the step's position in its phase and `<step>` is derived from the
step's description (`setup-1-clone-repo.log`, `teardown-2-create-pr.log`).
Each file starts with a header naming the step, then carries the command's
full stdout and stderr; an `on_failure` remediation attempt is appended to
the original step's file under a separator, so a step's whole history reads
top to bottom in one place.

All of these files are written as the run progresses, so output remains
available even if a run stops unexpectedly. The daemon log and these per-step
logs are separate: use the latter when you need an agent's or a step's
detailed output.

When any step fails — setup, agent, or teardown — the daemon log records it
as an error line that names the file to open, in the same `log_path=…` form
used when a container launches:

```text
ERROR squad setup step failed task=nightly run_id=… step=clone_repo exit_code=128 log_path=/home/you/.awman/squad/tasks/nightly/runs/…/setup-1-clone-repo.log
```

A failed evaluation's error text (as shown by `awman squad runs <name>` and
by the task detail modal in the TUI) ends with the leader's log path for the
same reason, and a failed run's closing `squad task run finished` line names
the run directory even when the failure happened before any step ran.

Container *image build* output is kept out of the daemon log the same way.
Each build a task triggers writes its full output to its own file:

```text
~/.awman/squad/builds/<name>/<run-id>-<n>.log
```

The daemon log records one lifecycle line when a build starts and one when it
finishes or fails, each naming the image and the path of that build's log
file.

At the end of an evaluation, the leader records whether the task was
triggered for that specific run. An older `workflow.toml` by itself is not
enough to trigger a later run: an explicit not-triggered result wins, and a
missing or invalid result is reported as a failed evaluation.

### When the daemon does not come up

Anything that starts the daemon for you — the squad tab, a `squad` CLI
command — waits ten seconds for it to publish its endpoint, then reports one of
two failures. They mean different things:

- **"squad daemon started but did not publish its endpoint within 10 seconds."**
  The daemon ran and something went wrong inside it. The reason is in
  `~/.awman/squad/awman.log`; read it with `awman squad logs`.
- **"the squad daemon process never started."** The OS process manager
  (launchd on macOS, systemd on Linux) accepted the request but ran nothing, so
  there is no daemon output to read. Start it in the foreground with
  `awman squad start` — with a terminal attached, the daemon prints directly to
  it — and check the job itself:

  ```sh
  launchctl print gui/$(id -u)/io.awman.squad   # macOS
  systemctl --user status awman-squad           # Linux
  ```

  On macOS, a job switched off under **System Settings › General › Login Items
  & Extensions** stays off; re-enable it there, or run
  `launchctl bootout gui/$(id -u)/io.awman.squad` and start squad again.

A background daemon writes its output to `~/.awman/squad/awman.log` on every
platform, including when the OS process manager is unavailable and awman falls
back to starting the process itself. If launchd or systemd declines to run the
job, the reason it gave is recorded in that same log.

---

## Authenticating to the daemon

squad serves its task data over a small HTTP surface on loopback, and the
CLI and TUI are clients of it. By default that surface requires a bearer key.

### The key and `AWMAN_SQUAD_KEY`

The first time the daemon starts, it mints a key, stores only its SHA-256 hash
in `~/.awman/squad/squad_key.hash`, and prints the plaintext **once** together
with the shell snippet that makes it usable:

```
╔════════════════════════════════════════════════════════════════════╗
║  squad API key (store this — it will not be shown again)            ║
║  954ec30c6719074e0ea952588461d079f97675424b8ecb53b5cbe76a9f06c96b  ║
╚════════════════════════════════════════════════════════════════════╝

Add this to ~/.zshrc so the awman CLI and TUI can authenticate to squad:

    export AWMAN_SQUAD_KEY=954ec30c6719074e0ea952588461d079f97675424b8ecb53b5cbe76a9f06c96b
```

Add that line to your shell startup file and reload it. Every later
`awman squad` command — and the squad TUI tab — reads `AWMAN_SQUAD_KEY` from the
environment and sends it as the bearer token. Without it the daemon answers
`401 Unauthorized`.

The process that mints the key also sets `AWMAN_SQUAD_KEY` for **itself**, so it
keeps working before you have exported anything. This matters most in the TUI:
the session that starts the daemon is the one session you cannot retroactively
export a variable into, and without this it would be the only one unable to talk
to the squad it just started. The injection lasts for that process only — a new
terminal still needs the line in your startup file.

The snippet is tailored to your shell: zsh gets `~/.zshrc`, bash gets
`~/.bashrc`, and fish gets `set -gx AWMAN_SQUAD_KEY …` for
`~/.config/fish/config.fish`.

The box-drawn banner above is the CLI's own rendering; the daemon itself only
ever hands a frontend the key, the shell, and the export line, and each
frontend draws them its own way. In the TUI this appears as a modal in the
TUI's own dialog style (no box-drawing characters — the modal's border is
the frame), whose text can't be selected with the mouse:

```
╭ squad authentication ────────────────────────────────────────────────╮
│                                                                       │
│  squad API key (store this — it will not be shown again):            │
│                                                                       │
│    954ec30c6719074e0ea952588461d079f97675424b8ecb53b5cbe76a9f06c96b  │
│                                                                       │
│  Add this to ~/.zshrc so the awman CLI and TUI can authenticate to    │
│  squad:                                                               │
│                                                                       │
│    export AWMAN_SQUAD_KEY=954ec30c6719074e0ea952588461d079f97675424… │
│                                                                       │
╰───────────────────────────────────────────────────────────────────────╯
```

Press `c` to copy just the key to the clipboard, or `z` to copy the shell
snippet (e.g. `export AWMAN_SQUAD_KEY=…`) — either can be pressed as many
times as you like before dismissing the modal with `Enter`.

### When the key is missing

Only the hash is stored, so a lost key cannot be recovered. Both frontends say
so rather than letting it surface as a bare `401`.

From the CLI, any `awman squad` subcommand that needs the daemon refuses up
front:

```
awman: squad requires a bearer key and none is set in this shell.

The key is shown only once, when it is minted, and only its hash is stored — so
it cannot be read back. Set AWMAN_SQUAD_KEY if you saved it, or mint a new one
with:
    awman squad start --refresh-key
which invalidates the previous key, so any shell still exporting it must be
updated too.
```

In the TUI, opening the squad tab raises the recovery instead of opening a tab
that could only ever show a 401:

```
╭ squad authentication ──────────────────────────────────────────────────╮
│                                                                        │
│   The squad daemon requires a key, and this session has none.          │
│                                                                        │
│   AWMAN_SQUAD_KEY is not set here, and squad's key was shown           │
│   only once when it was minted — only its hash is stored, so           │
│   the key itself cannot be read back.                                  │
│                                                                        │
│   Mint a new key and restart the squad daemon onto it?                 │
│   Any other shell still exporting the old key will stop working.       │
│                                                                        │
│   [y] mint a new key and restart squad   [n / Esc] cancel              │
│                                                                        │
╰────────────────────────────────────────────────────────────────────────╯
```

**y** mints a new key, restarts the daemon onto it, injects it into the running
session, opens the squad tab, and then shows you the key and its shell snippet —
the same display a first run gives. **n** or **Esc** opens no squad tab and says
so in the status bar; the daemon is left running and untouched.

Minting a new key invalidates the old one, so any other shell still exporting it
stops working until you update it.

To do the same thing from a terminal:

```sh
awman squad stop
awman squad start --refresh-key   # prints a fresh key and snippet, then exits
awman squad start
```

### Running without a key — `--dangerously-skip-auth`

If you would rather not manage a key at all:

```sh
awman squad start --dangerously-skip-auth
```

This mints no key, writes no hash, and accepts unauthenticated requests. It is
a reasonable choice on a single-user machine because **the squad daemon binds to
127.0.0.1 exclusively** — there is no flag to expose it on another interface,
so nothing off the machine can reach it. What it does give up is isolation from
other local processes and users on the same host: any of them can drive squad,
which means launching agent containers against your repo. Prefer the key on
shared or multi-user machines.

The flag applies to the run it is passed to. It leaves any existing
`squad_key.hash` untouched, so a later plain `awman squad start` requires a key
again. While a skip-auth daemon is running, the CLI and TUI notice and send no
bearer token rather than minting a key you would never see.

---

## Watching what your squad is running

Containers your squad launches — both the evaluation agent and the workflow it
generates — are visible in `awman status` like any other agent container,
marked with an `squad:<task>` source instead of `session`, so you can always
tell what your squad is doing versus what you started yourself. See
[Agent Sessions: Monitoring running agents](03-agent-sessions.md#monitoring-running-agents).

---

[← Runtimes](11-runtimes.md) · [Next: Cleaning Up →](13-cleaning-up.md)
