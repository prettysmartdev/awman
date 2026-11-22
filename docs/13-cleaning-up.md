# Cleaning up

`awman clean` lists resources awman can remove and asks before deleting them:

```sh
awman clean            # review and confirm
awman clean --dry-run  # preview only
awman clean --yes      # delete without a prompt
```

Use `--yes` in scripts or when stdin is not a terminal. In the TUI, confirmation appears in a dialog. If nothing qualifies, the command exits without prompting.

## What it removes

- Stopped awman containers or builtin microVM sessions, including their session disks.
- Completed workflow state files and their per-run context directories. Running workflows are left alone.
- Dangling awman Docker images and unused images built for pulled worker leases. Images used by active leases are left alone.
- Old database migration backups. The live database is left alone.
- The saved squad daemon environment in the OS keychain, if present. A running daemon retains its current values; a future restart will need them again. See [Squad task environment values](12-squad.md#task-environment-values).

`awman clean` does not remove Git worktrees. A [worktree run](04-security-and-isolation.md#worktree-isolation) handles those when it ends. The command lists each removable item before confirmation, so check the list if you use `--yes`.

## Builtin runtime state

With `runtime.kind=builtin`, clean removes stopped sessions but leaves imported images and the downloaded archive cache. To reclaim cache space, delete `<builtin state dir>/oci-cache/` while no image preparation is running; the next import downloads it again. To remove all builtin state, finish every session and delete the configured `runtime.builtin.stateDir` (by default `~/.awman/builtin/`). The next `awman ready` imports images again. See [Builtin runtime](11-runtimes.md#builtin-microvm-experimental).

A settings profile left `closing` after an uncertain shutdown is retained. Once the runtime is available, close it with `awman agents profile close NAME --agent AGENT` before deleting it. Sessions from a different builtin protocol version need the awman version that started them for cleanup.

## Failures and exit status

If the runtime is unavailable, awman warns and still cleans up eligible files. A deletion failure does not stop the remaining items; awman reports every failure at the end.

| Status | Meaning |
|---|---|
| `0` | Completed, nothing to remove, or dry run |
| `1` | One or more deletions failed |
| `2` | Confirmation was needed but no interactive input was available |

[← Squad](12-squad.md) · [Back to contents](contents.md)
