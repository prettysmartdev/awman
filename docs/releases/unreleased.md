# Unreleased

Changes on `main` that have not yet been cut into a release. Grouped by what
they mean for you, not by where they were made.

## Breaking and behaviour changes

### `awman chat` now exits with the agent's exit code

A `chat` session whose agent exited non-zero reported that code in the TUI but
exited `0` from the CLI and reported success over the API. All three now use
the same rule.

**Who is affected:** scripts and CI that run `awman chat` and branch on `$?`,
and API clients that read a command's status. A chat session that looked
successful because its agent crashed or was killed is now reported as failed.
Nothing changes for a chat session whose agent exits `0`.

### The TUI reports a failed workflow as failed

A failed `exec workflow` wrote "Command 'exec workflow' completed
successfully." into the tab's status log and coloured the tab as a clean
finish. The log line and the tab colour now reflect the real result. TUI only;
no scripting surface changes.

### `--non-interactive` on a terminal is honoured by every CLI prompt

`awman <command> --non-interactive` (and `--json`, which implies it) on a real
terminal used to prompt anyway for the mount-scope, agent-setup, agent-auth,
worktree-lifecycle, `init`, `ready` and `clean` questions: each one checked
whether stdin was a terminal rather than whether you had asked for a
non-interactive run. They now take the headless answer, which is what the
command already believed was happening.

### A malformed `config.json` fails a daemon start

`awman api start` and `awman squad start` with a malformed
`~/.awman/config.json` now fail with `config parse error in <path>: <reason>`
— naming the file, the line and the offending key — instead of starting on
default settings you did not write. Nine other config reads that silently
defaulted a bad file away are gone.

### The squad daemon reads `authRefresh` from the global config only

The squad daemon builds its credential-refresh monitor when it boots, from the
global config and the environment. A repo-level `authRefresh` block is no
longer honoured by the daemon, and credential leases begin at daemon boot
rather than at the first request. See
[Configuration](../07-configuration.md#control-credential-refresh-authrefresh).

### `persistence` is omitted rather than empty when no daemon answered

`DaemonStatus.env_persistence` (`GET /v1/status`) and `awman squad env`'s
`persistence` field are now **absent** rather than `""` when no live daemon
answered. `""` and an explicit `none` opt-out were previously
indistinguishable.

### Interactive prompts no longer answer themselves on a pipe

Several prompts used to pick an answer for you when there was nobody to ask:

- `awman squad add --interview` through a pipe took a workspace, a mount
  scope, an agent pool and an overlay set that nothing displayed. Each of
  those steps now fails with "interactive input unavailable" rather than
  choosing. A run that reaches them needs a terminal, or the equivalent flags.
- The work-item kind question (`awman new spec`, `awman specs`) accepted
  `feature`, `bug` and `enhancement` in the CLI but not in the TUI. Both now
  accept only the digits both display, and dismissing the question abandons
  the interview instead of silently filing a Task.

## Fixes

### Running the test suite no longer touches your keychain or your squad daemon

On macOS, `make test` read your real Claude credential from the keychain, and
the `awman clean` and squad daemon tests could overwrite or delete the squad
daemon's stored environment (`squad.envPersistence`). Tests that started a
squad daemon did so through launchd (`systemd --user` on Linux) under the same
fixed label as your real daemon, stopping it and leaving a test daemon
registered in its place. The TUI copy tests also overwrote your clipboard.

`make test` now runs with a throwaway home directory and no global git config,
and awman uses an in-memory keychain and clipboard and starts daemons as plain
child processes. Tests that drive Docker, Apple's `container` or `sbx` no
longer build, run or remove images and containers in your own daemon: they are
skipped unless you opt in with `AWMAN_TEST_DOCKER=1` (or
`AWMAN_TEST_APPLE_CONTAINER=1`, `AWMAN_TEST_SBX=1`). `make test-full` opts into
Docker.

If `make test` ran on your machine before this fix, check your daemon with
`launchctl print gui/$(id -u)/io.awman.squad` (or `systemctl --user status
awman-squad`). If it is not running, or its `AWMAN_SQUAD_ROOT` points into a
temporary directory, restart it with `awman squad start` from a shell that has
your task variables exported, so it stores them again.

### A running daemon is no longer mistaken for a stale one on macOS

awman checks that a daemon's PID still belongs to awman with `ps`, which
truncated the executable path to 79 columns. With `awman` installed at a longer
path, a running API or squad daemon looked stale: awman cleared its PID file
and started a second daemon beside it. The full path is now read.

### GitHub issue references resolve from more remote URL shapes

`--issue` derives `owner/repo` from your `origin` remote. It now also
recognises `ssh://git@github.com/owner/repo.git` and a host written in any
case (`https://GitHub.com/...`), alongside the `git@`/`https://` spellings it
already handled. This widens what is accepted and narrows nothing.

### A repo-local git identity counts

The warning `exec workflow` prints before a `commit_changes` teardown step
("git user.name / user.email not set") probed git with no working directory,
so it only ever saw the global identity. A repository that sets its own
`user.name`/`user.email` no longer gets the warning.

### "did you mean" works for nested commands

`awman exec wrkflow` — and the same typo in the TUI command box — got no
suggestion, because both frontends searched only the top-level command list.
Suggestions now come from the command catalogue and cover subcommands.

### A remote session's tab is magenta again

The documented magenta tab colour for a remote (auto-cloned) session never
appeared. It does now.

### A setup step that fails gets the same failure file a teardown step gets

A **setup** step that fails and has an `on_failure:` agent now writes the
failed command's captured output to a file the remediation agent can read, and
its prompt points at that file — behaviour only teardown steps had. The file
lands in the same per-invocation run directory, and its name now says which
phase it came from: `setup-failure-<step>.txt` / `teardown-failure-<step>.txt`
(it was `teardown-failure-<step>.txt` either way, which could not tell a setup
and a teardown step of the same name apart).

`StatusMessage.phase` on the API event stream is unchanged: still exactly
`setup` / `teardown`.

## Interface changes

### The TUI command box rejects what the API rejects

The command box used to accept input the API refused:

- `chat --launch-mode banana` was carried through as the string `banana`; it
  is now rejected at the box, with the reason and the allowed set, and your
  text is kept so you can correct it.
- `squad start --port abc` likewise.
- A `-`-prefixed token is no longer accepted as a flag's value.
- `--flag=false` on a boolean flag is now honoured, as it already was
  elsewhere.
- `ready -ab` now reads `unknown flag: -ab` — what the CLI and the API already
  said — instead of `short-flag bundle '-ab' is not supported by the command
  box`.

### Typed enums on the wire

Six event and session fields that were free-form strings are now enums. **No
wire value is renamed** — every variant serialises to exactly the string its
predecessor carried. The enumerated values are listed in
[API and Remote Mode](../09-api-and-remote-mode.md).

| Field | Values |
|---|---|
| `WorkflowStepTransition.from_status` / `.to_status` | `pending`, `running`, `succeeded`, `failed`, `cancelled`, `skipped` |
| `CommandStatus.status` | `done`, `paused`, `aborted`, `error` |
| `WorkflowPhaseTransition.status` | `running`, `succeeded`, `failed`, `paused`, `teardown_failed` |
| session `type` (create body, `session_type` column) | `local`, `remote` |
| `awman squad env` row `state` | `set`, `unmet` |

### One `ready` summary everywhere

The `ready` summary box showed different rows in different places: the CLI
omitted the "Image rebuild", "aspec folder" and "Work items config" rows the
TUI showed, and a remote session showed them under different labels plus one
row neither had. All three now show the same list, and
`awman remote session start` draws the same box `ready` draws.

### The API-key and squad-key banners are drawn per frontend

`awman api start --refresh-key` and the first-run key mint drew a
box-drawing banner that every frontend received verbatim, so the TUI got a box
inside its own frame and the API serialised the box characters into JSON. The
CLI still draws the box; the TUI and the API state the key as text. The key
itself is unchanged, and is still shown exactly once.

### The mount-scope and workspace questions read the same in the CLI and the TUI

`awman squad add --interview` asked for the mount scope as `[gitroot]/cwd?` in
the CLI and as "Mount the entire git root? (No = current directory only)" in
the TUI. Both now ask the same question with the same options. The same
applies to the Dockerfile-setup question `awman init` asks, which also now
shows the path it looked at on its own line.

### One worktree-creation failure message

`exec workflow` printed one of three messages when it could not create a
worktree — `failed to create worktree for issue:`, `... for work item:` or
`... for workflow:` — depending on how you named the run. There is now one:

```
exec workflow: failed to create worktree: <reason>
```

The reason text is unchanged. Only a log grep for the longer forms is
affected.

### The "INTERACTIVE mode" banner is gone

Launching `chat` or `exec` printed a box-drawn `INTERACTIVE mode` banner
before the agent started, in both the CLI and the TUI. No frontend shows it.
The agent launches identically, and the container overlay still shows the
agent name and elapsed time in its title.

## Known issues

### `awman ready --agent antigravity` reports the local agent as not installed

The host-side ready ping for `antigravity` runs a binary named `antigravity`,
but antigravity ships `agy` — which is what awman uses to launch it. The ping
therefore always fails, and credential refresh for antigravity cannot succeed.
This is long-standing, not new; fixing it changes behaviour on the one
sanctioned host-side agent execution path and is tracked separately.

## New environment variables

- `AWMAN_TEST_ISOLATION` — set to `1` to keep awman off your per-user OS
  resources: an in-memory keychain and clipboard, daemons started as plain
  child processes rather than through launchd or `systemd --user`, and no
  downloads from the internet, and `docker`, `container` and `sbx` treated as
  not installed. For test runs; `make test` sets it.
- `AWMAN_TEST_DOCKER`, `AWMAN_TEST_APPLE_CONTAINER`, `AWMAN_TEST_SBX` — with
  `AWMAN_TEST_ISOLATION`, set one to `1` to let awman use that real CLI.
  `make test-full` sets `AWMAN_TEST_DOCKER`.

## New environment variables documented

No behaviour changed; these were previously read but undocumented. See
[Configuration](../07-configuration.md).

- `AWMAN_ATTACH_DIR` — overrides the attach-socket directory
  (`~/.awman/attach/` by default).
- `AWMAN_API_VERBOSE_SETUP` — set to `0`, `false`, `no` or `off` to demote the
  API server's per-session setup logging from `info` to `debug`. Verbose is
  the default; the parse is case- and whitespace-insensitive.
- `PATH`, `HOME`, `RUST_LOG` — forwarded to a systemd/launchd daemon job.

---

[Back to contents](../contents.md)
