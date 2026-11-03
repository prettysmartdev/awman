# Cleaning Up

As you use awman it leaves behind stopped containers (or builtin microVM sessions), workflow data files, database migration backups and images. `awman clean` removes them safely. It does not remove worktrees: those are handled when a worktree run ends (see [Worktree isolation](04-security-and-isolation.md#worktree-isolation)).

---

## What gets removed

`awman clean` targets six categories of resources:

1. **Stopped containers** — awman containers from previous runs that have exited or died, on the configured runtime.

2. **Completed repo workflow files** — Workflow state files in `<git_root>/.awman/workflows/` that have finished (all steps succeeded, failed, skipped, or were cancelled). In-progress workflows are left untouched.

3. **Completed workflow context directories** — Per-invocation context directories under `~/.awman/context/workflows/` that belong to terminal (completed) workflows. These contain logs, temporary files, and other state from the workflow run. Directories are considered complete when their UUID matches a terminal workflow in the current repo, or when they contain a `completed` marker file.

4. **Dangling images** — Awman-labeled Docker images that Docker reports as dangling, usually because a newer build replaced the same tag. If Docker refuses to remove an image because a container still references it, awman reports that item as a deletion error and continues.

5. **Pre-migration database backups** — The `awman.db.pre-migration` backup (and its `-wal`/`-shm` sidecars) left in `~/.awman/api/` when the database moved to its current location. awman never reads it. The live database at `~/.awman/data/awman.db` is never touched.

6. **Squad daemon environment** — The OS keychain item (`awman-squad` / `daemon-env`) where the squad daemon persists the values it holds for tasks' `env()` names, so a daemon the OS restarts comes back armed. It is removed through `security` (macOS) or `secret-tool` (Linux), and is the only category that may hold a secret — see [Squad: Persisting values across a restart](12-squad.md#persisting-values-across-a-restart). Removing it does **not** affect a running daemon, which keeps the values it holds in memory; it only removes what a future restart would read back. With no keychain, or no `secret-tool`, the category never appears.

### Builtin runtime state

With `runtime` set to `builtin-experimental`, `awman clean` applies to the builtin runtime's own state like this:

- **Stopped containers** lists and removes builtin sessions that have stopped or crashed (their VM records and disks). Running and paused sessions are never listed.
- **Dangling images** is always empty: the builtin runtime only tracks images it imported by name, and `awman clean` never runs a global image prune against the store.
- **Downloaded archive cache.** Importing an image keeps a validated copy of its archive in `<builtin state dir>/oci-cache/` (`~/.awman/builtin/oci-cache/` by default). It is read only while an image is being acquired: during `awman ready`, or when a launch with `builtin.builder.enabled` prepares a missing agent image. `awman clean` does not remove it. To reclaim the space, delete that directory while no `awman ready` or launch is preparing an image; the next import simply re-acquires the image from its source.
- **Everything else.** To remove all builtin state (imported images, session disks, the cache), stop or finish every builtin session, then delete the state directory (`builtin.stateDir`, or `AWMAN_BUILTIN_STATE_DIR`). The next `awman ready` re-imports your images. Do not delete it while sessions are running.
- **Sessions from another awman version.** Sessions started by an awman with a different internal protocol version are neither adopted nor cleaned up (`builtin runtime worker protocol mismatch`); stop them with the awman that started them. An older state directory is upgraded in place automatically (see [Upgrading an older state directory](11-runtimes.md#session-lifecycle)); one that cannot be upgraded must be moved or deleted, after which `awman ready` re-imports your images.
- **Catalog upgrade backups.** When a new awman upgrades an older builtin state directory in place, it keeps the previous catalog as `<builtin state dir>/db.pre-<protocol>/` (for example `~/.awman/builtin/db.pre-0.7.2_18_awman-1/`). `awman clean` does not remove it, because it is the only way to roll back to the older awman. Once you are happy with the upgrade, delete that directory yourself while no awman is using the state directory. Hidden leftovers named `.db.pre-*.partial-*`, `.db.restore-*` or `.db.discard-*` can only come from a crash. Once the upgrade has completed (no `awman-catalog-restore` file is left in the state directory), they are also safe to delete then. A `.sandboxes.retired` directory is removed by awman itself the next time it opens the upgraded directory.

---

## Running the command

The simplest form lists what would be removed and prompts for confirmation:

```bash
awman clean
```

This displays an itemized list of all removable items grouped by category, then waits for your confirmation. If stdin is not a terminal (a script or pipe), the command aborts unless you pass `--yes`.

---

## Flags

### `--yes`, `-y`

Skip the confirmation prompt and delete immediately. Use in scripts and automation:

```bash
awman clean --yes
```

The itemized list is still printed.

### `--dry-run`

List what would be removed without deleting anything. Useful for previewing the impact:

```bash
awman clean --dry-run
```

When `--dry-run` is set, the command displays the full itemized list and reports the count of items that would be deleted. No deletion occurs, and no confirmation is needed.

---

## Behavior in different contexts

### Interactive terminal

When you run `awman clean` in an interactive terminal:

```
Stopped containers (2):
  - awman-abc123 (abc123def456)
  - awman-test-1 (def456ghi789)

Completed repo workflow files (1):
  - repohash8-workflow-name.json

Dangling images (1):
  - sha256:abc123 <none>:<none> (25MB)

Delete the above? [y/N]: 
```

Type `y` or `Y` to proceed, or anything else (or press Enter) to abort. After deletion:

```
Deleted 4 items. 0 errors.
```

### Non-TTY / scripting contexts

Without `--yes`, the command aborts:

```bash
awman clean </dev/null
# error: stdin is not a TTY; provide --yes on the command line
# exit code: 2
```

Pass `--yes` to proceed:

```bash
awman clean --yes
# Deleted 4 items. 0 errors.
# exit code: 0
```

Deletions that fail are counted and reported. If any deletions failed, the exit code is 1:

```bash
awman clean --yes
# Deleted 3 items. 1 errors.
#   /some/path: permission denied
# exit code: 1
```

### Nothing to clean

If no items match the deletion criteria:

```bash
awman clean
# Nothing to clean.
# exit code: 0
```

No confirmation prompt is shown; the command exits immediately.

### Container runtime unavailable

If the container runtime is unreachable or not configured, container and image categories are skipped with a warning, but filesystem cleanup (completed workflow files and context directories) still runs:

```bash
awman clean
# clean: container runtime unavailable; skipping container and image cleanup
# 
# Completed repo workflow files (2):
#   - repohash8-workflow-1.json
#   - repohash8-workflow-2.json
# 
# Delete the above? [y/N]: y
# Deleted 2 items. 0 errors.
```

The exit code is 0 if all filesystem deletions succeed.

---

## TUI behavior

When you run `awman clean` inside the TUI (e.g., via a command session), a modal dialog appears instead of a terminal prompt:

```
┌─────────────────────────────────────────┐
│         Confirm clean                   │
├─────────────────────────────────────────┤
│                                         │
│ Stopped containers (1):                 │
│   - awman-session (abc123def456)        │
│                                         │
│ Completed repo workflow files (2):      │
│   - repohash8-wf-1.json                 │
│   - repohash8-wf-2.json                 │
│                                         │
│                      [Yes]    [No]      │
└─────────────────────────────────────────┘
```

Select `[Yes]` to proceed or `[No]` to abort. The `--yes` flag also skips this dialog in the TUI.

---

## Exit codes

| Exit code | Meaning |
|-----------|---------|
| 0 | Deletion succeeded (or nothing to clean, or dry-run). |
| 1 | One or more deletions failed. Summary and per-item errors are reported. |
| 2 | Interactive input was required but unavailable (e.g., non-TTY without `--yes`). |

---

## Safety and design

`awman clean` is conservative by design:

- **Confirmation always required** (unless `--yes` or `--dry-run`). No silent deletions.
- **In-progress workflows are protected.** Only workflows that have reached a terminal state (completed, failed, or cancelled) are marked for deletion.
- **Per-item failures don't stop the process.** If one container fails to delete, the command continues and reports all failures at the end.
- **Context directories are verified.** A global context directory is only deleted if it matches a terminal workflow in the current repo or carries an explicit `completed` marker.
- **An unavailable runtime is not an error.** If the container runtime is unreachable, filesystem cleanup still runs, and the exit code is 0 if those deletions succeed.
- **A keychain that can't be reached is not an error either.** The probe for the squad daemon's stored environment is capped, so it can never hang the command; a platform with no keychain, a missing `secret-tool`, or a locked collection all resolve to "nothing to offer".

---

## Common patterns

### Clean up after a batch of workflows

Run a few workflows, then clean up:

```bash
awman exec workflow some-workflow.toml
awman exec workflow another-workflow.toml
awman clean --yes
```

### Preview before deleting in CI

Use `--dry-run` in CI logs to see what would be cleaned:

```bash
awman clean --dry-run
awman clean --yes
```

### Keep your machine tidy with a cron job

Schedule periodic cleanup:

```bash
0 2 * * * awman clean --yes >/dev/null 2>&1
```

This runs `awman clean` at 2 AM daily, deleting all completed resources without output.

### Dry-run in the TUI

Open a command session and type:

```
awman clean --dry-run
```

The command output lists what would be removed; no confirmation dialog is shown and nothing is deleted.

---

[← squad](12-squad.md) · [Back to contents](contents.md)
