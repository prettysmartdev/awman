# Security & Isolation

amux is built around a simple principle: agents never run on your host machine. Every agent session runs inside a Docker container that only sees what you explicitly give it. This section explains the isolation mechanisms and when to opt into elevated access.

---

## The containment model

By default, an agent container:

- Sees only your current Git repository, mounted at `/workspace`
- Receives your credentials as environment variables (never as mounted files)
- Has no access to your home directory, SSH keys, or Docker daemon
- Is removed when the session ends (`--rm`)

This means a misbehaving agent can't access your SSH keys, can't run arbitrary containers on your behalf, and can't touch files outside the project. The worst case is that it makes bad edits inside the repo — which git can undo.

### Transparency

Every time amux runs a container command, the full CLI invocation is printed before it executes:

```
$ docker run --rm -it -v /home/user/myproject:/workspace -w /workspace \
    -e CLAUDE_CODE_OAUTH_TOKEN=*** amux-myproject:latest claude "..."
```

Credential values are masked (`***`), but everything else is visible. You can always see exactly what amux is doing.

---

## Worktree isolation

The `--worktree` flag runs agent sessions in an isolated Git worktree rather than your main working directory. The agent's changes land on a separate branch, completely isolated from your current work until you decide what to do with them.

```sh
amux exec workflow path/to/workflow.md --worktree
```

### Why use it

- The agent can make sweeping changes without your working branch becoming unstable mid-implementation
- You can review the full diff as a coherent unit before it touches your main tree
- If the output isn't useful, discard it with a single keypress — no `git reset` needed
- Works with `--workflow`: all steps in the workflow share the same isolated worktree

### How it works

1. amux creates a branch `amux/work-item-NNNN` from your current `HEAD`
2. A worktree is checked out at `~/.amux/worktrees/<repo-name>/<NNNN>/`
3. The agent container mounts the worktree instead of your repo root
4. After the agent exits, you choose what to do with the branch

### Post-run options (command mode)

When a worktree run completes (or is aborted), the worktree is preserved on disk:

```
Worktree branch `amux/work-item-0030` is ready. Merge into current branch? [y/n/s]
```

| Key | Action |
|-----|--------|
| `y` | Merge into current branch (`git merge --no-ff`), remove worktree and branch |
| `n` | Discard — remove worktree and delete branch |
| `s` | Keep worktree and branch for manual review; prints the path |

If you abort the workflow (Ctrl+C or the **Abort** action in the workflow control board), amux shows the same merge/discard dialog. The worktree is never automatically deleted on abort — your completed steps' changes are preserved and ready for review.

### Post-run dialog (TUI mode)

```
╭─── Worktree: Merge or Discard? ───────────────────────╮
│                                                        │
│  Branch 'amux/work-item-0030' completed.               │
│  (or was aborted — changes preserved on disk)         │
│                                                        │
│  [m/y] Merge into current branch                       │
│  [d]   Discard (delete branch + worktree)              │
│  [s/Esc] Keep worktree branch as-is                    │
│                                                        │
╰────────────────────────────────────────────────────────╯
```

The same dialog appears whether the workflow completed successfully or was aborted. Your partially completed work is preserved, allowing you to review, manually continue, or discard as you choose.

### Interrupted runs

If a worktree already exists (previous run was interrupted), amux detects it:

```
Worktree already exists at ~/.amux/worktrees/myrepo/0030.
[r]esume — reuse existing worktree
[R]ecreate — remove it and start fresh
```

### Merge conflicts

If the merge fails, amux prints a recovery message and leaves the worktree in place:

```
Merge failed with conflicts — resolve manually in /path/to/repo,
then run: git branch -d amux/work-item-0030 && git worktree remove ~/.amux/worktrees/myrepo/0030
```

### Commit signing (GPG, SSH, S/MIME)

When Git commit signing is enabled, amux **suspends the TUI** around each `git commit` it runs, allowing your passphrase prompt to work normally. After the commit completes (or fails), the TUI is restored. Users without signing configured see no change.

### Edge cases

| Situation | Behaviour |
|-----------|-----------|
| `git` < 2.5 | Error before launch: "git ≥ 2.5 is required for --worktree support" |
| Detached HEAD | Warning printed; worktree created from current commit; continues |
| Branch exists, no worktree dir | Worktree created using the existing branch |
| Merge conflict | Error with manual resolution instructions; worktree kept |
| Combined with `--workflow` | All workflow-step containers share the same worktree |
| Combined with `--mount-ssh` | Both flags apply independently |

### Examples

```sh
amux exec workflow path/to/workflow.md --worktree                    # isolated run; prompt to merge after
amux exec workflow path/to/workflow.md --worktree --mount-ssh        # worktree + SSH keys in container
```

---

## Overlay mounts

The `--overlay` flag mounts additional host resources into the agent container beyond the default Git repository mount. Supported overlay types:

- `skill()` — mount your global amux skills directory (`~/.amux/skills/`) as slash commands
- `dir(host_path:container_path[:ro|rw])` — mount a host directory

This lets you give an agent access to a personal skills library, a reference dataset, a shared prompts directory, or any other host resource without permanently modifying any config file.

### Directory overlay format

```
dir(host_path:container_path[:ro|rw])
```

| Field | Description |
|-------|-------------|
| `host_path` | Absolute path on the host. Leading `~` is expanded to your home directory. |
| `container_path` | Absolute path inside the container where the directory will appear. |
| `ro` / `rw` | Mount permission. Defaults to `ro` when omitted. |

### Skills overlay

```
skill()
```

Mounts `~/.amux/skills/` read-only into the agent's native skills directory (determined by agent type). No arguments allowed.

### Basic examples

```sh
# Mount your personal skills library
amux exec workflow path/to/workflow.md --overlay "skill()"

# Mount a reference dataset read-only
amux exec workflow path/to/workflow.md --overlay "dir(/data/reference:/mnt/reference:ro)"

# Mount a shared prompts directory read-write
amux chat --overlay "dir(~/prompts:/mnt/prompts:rw)"

# Skills + directories (repeated flag or comma-separated — both are equivalent)
amux exec workflow path/to/workflow.md --overlay "skill()" --overlay "dir(/data/ref:/mnt/ref:ro)" --overlay "dir(~/snippets:/mnt/snippets)"
amux exec workflow path/to/workflow.md --overlay "skill(),dir(/data/ref:/mnt/ref:ro),dir(~/snippets:/mnt/snippets)"
```

Available on all agent-launching commands: `chat`, `exec prompt`, and `exec workflow`.

### `AMUX_OVERLAYS` environment variable

Set `AMUX_OVERLAYS` in your shell profile to apply overlays automatically to every agent session regardless of which repo you're working in. It uses the same format as `--overlay` — a comma-separated list of typed overlay expressions:

```sh
export AMUX_OVERLAYS="skill(),dir(~/personal-prompts:/mnt/prompts),dir(/data/shared-fixtures:/mnt/fixtures:ro)"
```

### Config-based overlays

Overlays can be declared in config files so they are applied automatically without requiring any flags each time. Both the per-repo and global configs support an `overlays` object with optional `skills` and `directories` fields:

**Per-repo config** (`aspec/.amux.json`):
```json
{
  "overlays": {
    "skills": true,
    "directories": [
      { "host": "/data/fixtures", "container": "/mnt/fixtures", "permission": "ro" },
      { "host": "~/shared-prompts", "container": "/mnt/prompts" }
    ]
  }
}
```

**Global config** (`~/.amux/config.json`):
```json
{
  "overlays": {
    "skills": true,
    "directories": [
      { "host": "~/personal-prompts", "container": "/mnt/prompts", "permission": "ro" }
    ]
  }
}
```

**Skills field:**

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `skills` | boolean | `false` | When `true`, mount `~/.amux/skills/` read-only into the agent's native skills directory. |

**Directory field:**

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `host` | string | yes | Host path (absolute or `~`-prefixed). |
| `container` | string | yes | Container path (absolute). |
| `permission` | string | no | `"ro"` or `"rw"`. Defaults to `"ro"` when omitted. |

### Priority and conflict resolution

Overlays are **additive**: all four sources contribute entries, then conflicts are resolved.

**For directory overlays**, the priority order, from lowest to highest:

1. Global config (`~/.amux/config.json`)
2. Per-repo config (`aspec/.amux.json`)
3. `AMUX_OVERLAYS` environment variable
4. `--overlay` CLI flags (highest priority)

Unlike `envPassthrough` (where the per-repo list replaces the global list entirely), directory overlay sources are merged — entries from all four sources appear in the final mount list unless they conflict.

**For skills overlay**, there is no priority hierarchy. If *any* source sets `"skills": true` or includes `skill()`, the mount is enabled. It's an **additive OR** operation.

**Conflict resolution rules for directories:**

When two sources specify the same host path:
- The **higher-priority source** wins for the container path.
- The **more restrictive permission always wins** — `:ro` beats `:rw` regardless of which source is higher priority. A warning is logged whenever permissions are downgraded. This prevents a CLI flag from silently escalating a read-only global config entry to read-write.

When two sources specify different host paths that map to the **same container path**, both mounts are applied and a warning is logged (Docker will shadow one with the other; the last mount in the list wins).

**Skills overlay mounts are always read-only** and cannot be modified by the agent, even if you attempt to override with `:rw`.

### Missing host paths

If a configured host path does not exist when the container launches, amux logs a warning and skips that overlay — it does not abort the session. This matches the behaviour of other optional mounts (SSH keys, Docker socket).

```
WARN overlay host path '/data/reference' does not exist; skipping
```

### Security note

Overlay mounts extend the base isolation model: the agent still cannot access anything outside your Git repo **plus the explicitly listed overlay directories and skills**. Directory `:ro` mounts prevent the agent from modifying the overlaid directory. Only use `:rw` when the task genuinely requires the agent to write to that directory, and only with agent images you trust.

Skills overlays are always mounted read-only, whether skills are provided by global config, per-repo config, environment variable, or CLI flag. The agent cannot modify any skill files.

Like `--mount-ssh` and `--allow-docker`, overlay mounts are always printed in the Docker command before execution so you can see exactly what is mounted.

### TUI usage

In the TUI command box, use comma-separated syntax when specifying multiple overlays — the TUI flag parser stores one value per flag, so repeating `--overlay` keeps only the last value:

```
# Correct: comma-separated in one value
exec workflow path/to/workflow.md --overlay "skill(),dir(/data/ref:/mnt/ref:ro),dir(~/prompts:/mnt/prompts)"

# Incorrect in TUI (second value silently overwrites first):
exec workflow path/to/workflow.md --overlay "skill()" --overlay "dir(/data/ref:/mnt/ref:ro)"
```

On the CLI, both repeated flags and comma-separated syntax are equivalent.

---

## Docker socket access

The `--allow-docker` flag mounts the host Docker daemon socket into the agent container. This lets the agent build and run Docker containers itself.

### When to use it

Use `--allow-docker` when the task requires the agent to:

- Build Docker images (e.g. testing your app's Dockerfile)
- Run Docker containers (e.g. starting a local database for testing)
- Interact with the Docker daemon in any other way

### What happens

Before launching the container, amux verifies the socket exists and prints a warning:

```
Docker socket: /var/run/docker.sock (found)
WARNING: --allow-docker: mounting host Docker socket into container
(/var/run/docker.sock:/var/run/docker.sock). This grants the agent elevated host access.
```

| Platform | Mount |
|----------|-------|
| Linux / macOS | `-v /var/run/docker.sock:/var/run/docker.sock` |
| Windows | `--mount type=npipe,source=\\.\pipe\docker_engine,target=\\.\pipe\docker_engine` |

### Security note

Mounting the Docker socket gives the agent root-equivalent access to your host — it can start containers, delete images, and interact with any running container. Only use `--allow-docker` for tasks that genuinely require it and when you trust the agent and work item. amux will never mount the socket without this explicit flag.

### Examples

```sh
amux exec workflow path/to/workflow.md --allow-docker  # workflow that needs to build a Docker image
amux chat --allow-docker                               # freeform session with Docker access
amux ready --refresh --allow-docker                    # Dockerfile audit with Docker access
```

---

## SSH key mounting

The `--mount-ssh` flag mounts your host `~/.ssh` directory read-only into the container, so the agent can authenticate with remote Git servers using your existing SSH keys.

### When to use it

Use `--mount-ssh` when the task requires the agent to:

- Clone private repositories over SSH
- Push branches or tags to a remote
- Run `git fetch` / `git pull` against SSH remotes

### What happens

Before launching the container, amux verifies `~/.ssh` exists and prints a warning:

```
WARNING: --mount-ssh: mounting host ~/.ssh into container (read-only). Ensure you trust the agent image.
```

The directory is mounted as `-v /home/user/.ssh:/root/.ssh:ro`. The `:ro` flag prevents the agent from modifying your host SSH keys.

`~/.ssh` is never mounted without this explicit flag — there is no config option to enable it silently.

### Security notes

- The mount is read-only; the agent can use your keys but cannot modify them
- SSH key permissions must be correct on the host (`600` for private keys); Docker bind mounts inherit host permissions
- Only use `--mount-ssh` with agent images you trust

### Examples

```sh
amux exec workflow path/to/workflow.md --mount-ssh              # agent can push/pull over SSH
amux chat --mount-ssh                                           # freeform session with SSH access
amux exec workflow path/to/workflow.md --worktree --mount-ssh   # combine with worktree isolation
```

When used with `--workflow`, the SSH directory is mounted into every workflow-step container.

---

## Container transparency

Every `docker build` and `docker run` command amux issues is printed before it executes — in command mode to stdout, in TUI mode as the first line of the execution window.

```
$ docker build -t amux-myapp:latest -f Dockerfile.dev /path/to/repo
$ docker run --rm -it \
    -v /path/to/repo:/workspace \
    -w /workspace \
    -e CLAUDE_CODE_OAUTH_TOKEN=*** \
    amux-myapp:latest claude "Implement work item 0001..."
```

With the Apple Containers runtime, the same commands are shown with `container` instead of `docker`. Credential values are always masked.

---

[← Agent Sessions](02-agent-sessions.md) · [Next: Workflows →](04-workflows.md)
